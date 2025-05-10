use std::collections::HashSet;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use bincode;
use chrono::Utc;
use directories::ProjectDirs;
use git2::{DiffOptions, Oid, Repository, Sort};
use hex;
use rayon::prelude::*;
use rustc_hash::FxHashMap as HashMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sled;

/// Calculates size penalty: 1 / (1 + sqrt(size_in_kib))
fn size_penalty(size_bytes: u64) -> f64 {
    let kib = (size_bytes as f64) / 1024.0;
    1.0 / (1.0 + kib.sqrt())
}

/// On-disk static data per commit: per-file penalties
#[derive(Serialize, Deserialize)]
struct CommitStatics {
    contribs: Vec<(PathBuf, f64)>,
}

/// Opens (or creates) a sled cache DB unique to this repo, in OS-appropriate cache dir
fn open_repo_cache(repo_path: &Path) -> sled::Db {
    let proj = ProjectDirs::from("com", "kantord", "frecenfile")
        .expect("unable to get project directories");
    let cache_base = proj.cache_dir();
    fs::create_dir_all(cache_base).expect("failed to create cache directory");

    let absolute_path = repo_path
        .canonicalize()
        .expect("failed to canonicalize repo path");
    let mut hasher = Sha256::new();
    hasher.update(absolute_path.to_string_lossy().as_bytes());
    let path_hash = hex::encode(&hasher.finalize()[0..16]);

    let db_path = cache_base.join(format!("{}.sled", path_hash));
    sled::open(db_path).expect("failed to open sled cache")
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

    let scores = compute_scores_parallel(&oids, repo_path, &paths_arc, now_secs, cache);
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
    cache: Arc<sled::Db>,
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
    cache: &Arc<sled::Db>,
    size_cache: &mut HashMap<Oid, u64>,
) -> CommitStatics {
    let key = oid.to_string();

    if let Ok(Some(bytes)) = cache.get(&key) {
        return bincode::deserialize(&bytes).expect("deserialize cache bytes");
    } else {
        let contribs = compute_statics_for_commit(&repo, oid, size_cache).unwrap_or_default();
        let statics = CommitStatics { contribs };
        let serialized = bincode::serialize(&statics).expect("serialize statics");
        cache.insert(&key, serialized).expect("insert into cache");
        return statics;
    };
}

/// Worker: for each OID, load from cache or compute, then filter & weight
fn process_chunk(
    chunk: &[Oid],
    repo_path: &Path,
    paths: &Option<Arc<HashSet<PathBuf>>>,
    now_secs: i64,
    cache: Arc<sled::Db>,
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
            if paths.as_ref().map_or(true, |set| set.contains(&path)) {
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
