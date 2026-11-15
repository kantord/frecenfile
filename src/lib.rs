use std::collections::HashSet;
use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::Result;
use chrono::Utc;
use directories::ProjectDirs;
use git2::{DiffOptions, Oid, Repository, Sort};
use rayon::prelude::*;
use rustc_hash::FxHashMap as HashMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CACHE_ENV_VAR: &str = "FRECENFILE_CACHE_DIR";

/// Calculates size penalty: 1 / (1 + sqrt(size_in_kib))
fn size_penalty(size_bytes: u64) -> f64 {
    let kib = (size_bytes as f64) / 1024.0;
    1.0 / (1.0 + kib.sqrt())
}

/// On-disk static data per commit: per-file penalties
#[derive(Clone, Serialize, Deserialize)]
struct CommitStatics {
    contribs: Vec<(PathBuf, f64)>,
}

/// Per-repo cache: commit id -> statics. Loaded from one JSON file, written back by `save`.
#[derive(Default)]
struct Cache {
    path: Option<PathBuf>,
    entries: Mutex<HashMap<String, CommitStatics>>,
}

impl Cache {
    fn load(path: Option<PathBuf>) -> Self {
        let entries = path
            .as_ref()
            .and_then(|p| fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path,
            entries: Mutex::new(entries),
        }
    }

    fn get(&self, key: &str) -> Option<CommitStatics> {
        self.entries.lock().ok()?.get(key).cloned()
    }

    fn insert(&self, key: String, value: CommitStatics) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(key, value);
        }
    }

    /// Best-effort atomic write; failures only cost us the cache.
    fn save(&self) {
        let Some(path) = &self.path else { return };
        let Ok(entries) = self.entries.lock() else {
            return;
        };
        let Ok(bytes) = serde_json::to_vec(&*entries) else {
            return;
        };
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        if fs::write(&tmp, bytes).is_err() || fs::rename(&tmp, path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }
}

/// Opens a cache file unique to this repo, in OS-appropriate cache dir.
/// Falls back to temp or no-cache if the directory is not writable.
fn open_repo_cache(repo_path: &Path) -> Cache {
    let cache_base = cache_base_from_env().or_else(default_cache_base);
    if let Some(base) = cache_base
        && let Some(path) = cache_file_at(repo_path, &base)
    {
        return Cache::load(Some(path));
    }

    if let Some(base) = temp_cache_base()
        && let Some(path) = cache_file_at(repo_path, &base)
    {
        return Cache::load(Some(path));
    }

    Cache::default()
}

fn cache_base_from_env() -> Option<PathBuf> {
    env::var_os(CACHE_ENV_VAR).map(PathBuf::from)
}

fn default_cache_base() -> Option<PathBuf> {
    ProjectDirs::from("com", "kantord", "frecenfile").map(|proj| proj.cache_dir().to_path_buf())
}

fn temp_cache_base() -> Option<PathBuf> {
    Some(env::temp_dir().join("frecenfile"))
}

fn cache_file_at(repo_path: &Path, cache_base: &Path) -> Option<PathBuf> {
    fs::create_dir_all(cache_base).ok()?;
    Some(repo_cache_path(cache_base, repo_path))
}

fn repo_cache_path(cache_base: &Path, repo_path: &Path) -> PathBuf {
    let absolute_path = repo_path
        .canonicalize()
        .unwrap_or_else(|_| repo_path.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(absolute_path.to_string_lossy().as_bytes());
    let path_hash = hex::encode(&hasher.finalize()[0..16]);
    cache_base.join(format!("{}.json", path_hash))
}

/// Top-level: analyze repo at `repo_path`, optional filter paths, limit to max_commits newest commits
pub fn analyze_repo(
    repo_path: &Path,
    paths: Option<HashSet<PathBuf>>, // files to include; None = all
    max_commits: Option<usize>,
) -> Result<Vec<(PathBuf, f64)>> {
    let repo = Repository::discover(repo_path)?;
    let cache = Arc::new(open_repo_cache(repo_path));
    let oids = collect_commit_ids(&repo, max_commits)?;
    let now_secs = Utc::now().timestamp();
    let paths_arc = paths.map(Arc::new);

    let scores = compute_scores_parallel(&oids, repo_path, &paths_arc, now_secs, cache.clone());
    cache.save();
    Ok(scores.into_iter().collect())
}

/// Collect commit OIDs (newest first), up to max_commits
fn collect_commit_ids(
    repo: &Repository,
    max_commits: Option<usize>,
) -> Result<Vec<Oid>, git2::Error> {
    let mut revwalk = repo.revwalk()?;
    revwalk.push_head()?;
    revwalk.set_sorting(Sort::TIME)?;
    revwalk.simplify_first_parent()?;

    let limit = max_commits.unwrap_or(usize::MAX);
    let mut oids = Vec::with_capacity(limit.min(1024));
    for oid_res in revwalk.take(limit) {
        let oid = oid_res?;
        oids.push(oid);
    }
    Ok(oids)
}

/// Parallel scoring: chunk OIDs to workers
fn compute_scores_parallel(
    oids: &[Oid],
    repo_path: &Path,
    paths: &Option<Arc<HashSet<PathBuf>>>,
    now_secs: i64,
    cache: Arc<Cache>,
) -> HashMap<PathBuf, f64> {
    const COMMITS_PER_WORKER: usize = 250;

    oids.par_chunks(COMMITS_PER_WORKER)
        .map(|chunk| process_chunk(chunk, repo_path, paths, now_secs, cache.clone()))
        .reduce(HashMap::default, |mut acc, local| {
            for (k, v) in local {
                *acc.entry(k).or_default() += v;
            }
            acc
        })
}

fn get_commit_statistics(
    repo: &Repository,
    oid: Oid,
    cache: &Arc<Cache>,
    size_cache: &mut HashMap<Oid, u64>,
) -> CommitStatics {
    let key = oid.to_string();

    if let Some(statics) = cache.get(&key) {
        return statics;
    }

    let contribs = compute_statics_for_commit(repo, oid, size_cache).unwrap_or_default();
    let statics = CommitStatics { contribs };
    cache.insert(key, statics.clone());
    statics
}

/// Worker: for each OID, load from cache or compute, then filter & weight
fn process_chunk(
    chunk: &[Oid],
    repo_path: &Path,
    paths: &Option<Arc<HashSet<PathBuf>>>,
    now_secs: i64,
    cache: Arc<Cache>,
) -> HashMap<PathBuf, f64> {
    let repo = Repository::open(repo_path).expect("re-open repo inside worker");
    let mut size_cache: HashMap<Oid, u64> = HashMap::default();
    let mut local_scores: HashMap<PathBuf, f64> = HashMap::default();

    for oid in chunk {
        let commit = match repo.find_commit(*oid) {
            Ok(c) if c.parent_count() <= 1 => c,
            _ => continue,
        };
        let statics: CommitStatics = get_commit_statistics(&repo, *oid, &cache, &mut size_cache);
        let age_days = ((now_secs - commit.time().seconds()) / 86_400).max(0) as f64;
        let weight = 1.0 / (age_days + 1.0).powi(2);

        for (path, penalty) in statics.contribs.into_iter() {
            if paths.as_ref().is_none_or(|set| set.contains(&path)) {
                *local_scores.entry(path).or_default() += penalty * weight;
            }
        }
    }

    local_scores
}

/// Compute the static penalties for all files in a given commit
fn compute_statics_for_commit(
    repo: &Repository,
    oid: Oid,
    size_cache: &mut HashMap<Oid, u64>,
) -> Result<Vec<(PathBuf, f64)>, git2::Error> {
    let mut out = Vec::new();
    let commit = repo.find_commit(oid)?;
    if commit.parent_count() > 1 {
        return Ok(out);
    }
    let tree = commit.tree()?;

    let mut diff_opts = DiffOptions::new();
    diff_opts.context_lines(0);
    diff_opts.interhunk_lines(0);
    diff_opts.skip_binary_check(true);
    diff_opts.include_typechange(false);

    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut diff_opts))?;

    for delta in diff.deltas() {
        if let Some(path) = delta.new_file().path() {
            let blob_oid = delta.new_file().id();
            if blob_oid.is_zero() {
                continue;
            }
            let size_bytes = *size_cache.entry(blob_oid).or_insert_with(|| {
                repo.find_blob(blob_oid)
                    .map(|b| b.size() as u64)
                    .unwrap_or(0)
            });
            let penalty = size_penalty(size_bytes);
            out.push((path.to_path_buf(), penalty));
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    /// Tests mutate process-wide env vars, so they must not run concurrently.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        old: Option<OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: impl Into<OsString>) -> Self {
            let old = env::var_os(key);
            unsafe {
                env::set_var(key, value.into());
            }
            Self { key, old }
        }

        #[cfg(unix)]
        fn remove(key: &'static str) -> Self {
            let old = env::var_os(key);
            unsafe {
                env::remove_var(key);
            }
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(value) = self.old.take() {
                unsafe {
                    env::set_var(self.key, value);
                }
            } else {
                unsafe {
                    env::remove_var(self.key);
                }
            }
        }
    }

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let mut dir = env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        dir.push(format!("{}_{}_{}", prefix, std::process::id(), nanos));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn make_git_repo() -> PathBuf {
        let repo_dir = make_temp_dir("frecenfile_repo");
        let repo = Repository::init(&repo_dir).expect("init repo");
        let sig = git2::Signature::now("Test", "test@example.com").expect("signature");
        let file_path = repo_dir.join("file.txt");
        fs::write(&file_path, "hello").expect("write file");

        let mut index = repo.index().expect("index");
        index.add_path(Path::new("file.txt")).expect("add path");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .expect("commit");
        repo_dir
    }

    #[test]
    fn cache_env_override_is_used() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let repo_dir = make_git_repo();
        let cache_dir = make_temp_dir("frecenfile_cache");
        let _guard = EnvGuard::set(CACHE_ENV_VAR, cache_dir.as_os_str());
        analyze_repo(&repo_dir, None, Some(10)).expect("analyze");

        let expected = repo_cache_path(&cache_dir, &repo_dir);
        assert!(expected.exists(), "expected cache file to exist");
    }

    #[cfg(unix)]
    fn set_dir_readonly(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o555);
        fs::set_permissions(path, perms).expect("set read-only perms");
    }

    #[test]
    #[cfg(unix)]
    fn read_only_home_does_not_panic() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let repo_dir = make_git_repo();
        let home_dir = make_temp_dir("frecenfile_home");
        let xdg_dir = home_dir.join("xdg_cache");
        fs::create_dir_all(&xdg_dir).expect("create xdg dir");
        set_dir_readonly(&home_dir);
        set_dir_readonly(&xdg_dir);

        let _guard_home = EnvGuard::set("HOME", home_dir.as_os_str());
        let _guard_xdg = EnvGuard::set("XDG_CACHE_HOME", xdg_dir.as_os_str());
        let _guard_cache = EnvGuard::remove(CACHE_ENV_VAR);

        let result = analyze_repo(&repo_dir, None, Some(10));
        assert!(result.is_ok(), "expected analyze_repo to succeed");
    }

    #[test]
    #[cfg(windows)]
    fn invalid_cache_dir_falls_back() {
        let repo_dir = make_git_repo();
        let tmp_dir = make_temp_dir("frecenfile_cache_parent");
        let cache_file = tmp_dir.join("not_a_dir");
        fs::write(&cache_file, "not a dir").expect("write cache file");

        let _guard = EnvGuard::set(CACHE_ENV_VAR, cache_file.as_os_str());
        let result = analyze_repo(&repo_dir, None, Some(10));
        assert!(result.is_ok(), "expected analyze_repo to succeed");
    }
}
