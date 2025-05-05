use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use git2::{DiffOptions, Oid, Repository, Sort};
use rayon::prelude::*;
use rustc_hash::FxHashMap as HashMap;

fn size_penalty(size_bytes: u64) -> f64 {
    let kib = (size_bytes as f64) / 1024.0;

    1.0 / (1.0 + kib.sqrt())
}

/// Analyse the repository, looking at at most `max_commits` newest commits.
/// Pass `None` to inspect the full history.
pub fn analyze_repo(
    repo_path: &Path,
    paths: Option<HashSet<PathBuf>>,
    max_commits: Option<usize>,
) -> Result<Vec<(PathBuf, f64)>, git2::Error> {
    let repo = Repository::discover(repo_path)?;
    let oids = collect_commit_ids(&repo, max_commits)?;

    let now_secs = Utc::now().timestamp();
    let paths_arc: Option<Arc<HashSet<PathBuf>>> = paths.map(Arc::new);

    let scores = compute_scores_parallel(&oids, repo_path, &paths_arc, now_secs);

    Ok(scores.into_iter().collect())
}

fn collect_commit_ids(
    repo: &Repository,
    max_commits: Option<usize>,
) -> Result<Vec<Oid>, git2::Error> {
    let mut revwalk = repo.revwalk()?;
    revwalk.push_head()?;
    revwalk.set_sorting(Sort::TIME)?; // newest → oldest
    revwalk.simplify_first_parent()?;

    let iter = revwalk.take(max_commits.unwrap_or(usize::MAX));
    iter.collect()
}

fn compute_scores_parallel(
    oids: &[Oid],
    repo_path: &Path,
    paths: &Option<Arc<HashSet<PathBuf>>>,
    now_secs: i64,
) -> HashMap<PathBuf, f64> {
    const COMMITS_PER_WORKER: usize = 250;

    oids.par_chunks(COMMITS_PER_WORKER)
        .map(|chunk| process_chunk(chunk, repo_path, paths, now_secs))
        .reduce(HashMap::default, |mut acc, local| {
            for (k, v) in local {
                *acc.entry(k).or_default() += v;
            }
            acc
        })
}

fn process_chunk(
    chunk: &[Oid],
    repo_path: &Path,
    paths: &Option<Arc<HashSet<PathBuf>>>,
    now_secs: i64,
) -> HashMap<PathBuf, f64> {
    let repo = Repository::open(repo_path).expect("re-open repo inside worker");
    let mut size_cache: HashMap<Oid, u64> = HashMap::default();
    let mut local_scores: HashMap<PathBuf, f64> = HashMap::default();

    for oid in chunk {
        let commit = match repo.find_commit(*oid) {
            Ok(c) if c.parent_count() <= 1 => c, // skip merge commits
            _ => continue,
        };

        let tree = match commit.tree() {
            Ok(t) => t,
            Err(_) => continue,
        };

        let age_days = ((now_secs - commit.time().seconds()) / 86_400).max(0) as f64;
        let weight = 1.0 / (age_days + 1.0).powi(2);

        let mut diff_opts = DiffOptions::new();
        diff_opts
            .context_lines(0)
            .interhunk_lines(0)
            .skip_binary_check(true)
            .include_typechange(false);

        if let Some(filter) = paths {
            for p in filter.iter() {
                diff_opts.pathspec(p);
            }
        }

        let parent_tree = if commit.parent_count() == 1 {
            commit.parent(0).ok().and_then(|p| p.tree().ok())
        } else {
            None
        };

        let diff =
            match repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut diff_opts)) {
                Ok(d) => d,
                Err(_) => continue,
            };

        for delta in diff.deltas() {
            if let Some(path) = delta.new_file().path() {
                if paths.as_ref().map_or(true, |set| set.contains(path)) {
                    let blob_oid = delta.new_file().id();
                    if blob_oid.is_zero() {
                        // deletion or missing
                        continue;
                    }
                    let size_bytes = *size_cache.entry(blob_oid).or_insert_with(|| {
                        repo.find_blob(blob_oid)
                            .map(|b| b.size() as u64)
                            .unwrap_or(0)
                    });
                    let penalty = size_penalty(size_bytes);

                    *local_scores.entry(path.to_path_buf()).or_default() += weight * penalty;
                }
            }
        }
    }

    local_scores
}
