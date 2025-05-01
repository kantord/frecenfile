use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{NaiveDateTime, TimeZone, Utc};
use git2::{DiffOptions, Repository};

pub fn analyze_repo(
    repo_path: &Path,
    paths: Option<HashSet<PathBuf>>,
) -> Result<Vec<(PathBuf, f64)>, git2::Error> {
    let repo = Repository::discover(repo_path)?;
    let mut scores: HashMap<PathBuf, f64> = HashMap::new();

    let mut revwalk = repo.revwalk()?;
    revwalk.push_head()?;
    // Skip merges: only follow the first parent (reduces diff count)
    revwalk.simplify_first_parent()?;
    let now = Utc::now();

    let mut diff_opts = DiffOptions::new();
    diff_opts.context_lines(0).interhunk_lines(0);

    for oid in revwalk {
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        let tree = commit.tree()?;

        // Compute weight
        let commit_time = commit.time();
        let naive = NaiveDateTime::from_timestamp(commit_time.seconds(), 0);
        let commit_dt = Utc.from_utc_datetime(&naive);
        let age_days = (now - commit_dt).num_days().max(0);
        let weight = 1.0 / ((age_days as f64 + 1.0).powi(2));

        // Diff against first parent
        let parent_tree = if commit.parent_count() > 0 {
            Some(commit.parent(0)?.tree()?)
        } else {
            None
        };
        let diff =
            repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut diff_opts))?;

        // Iterate deltas directly
        for delta in diff.deltas() {
            if let Some(new_path) = delta.new_file().path() {
                let rel = new_path.to_path_buf();
                if let Some(ref filter) = paths {
                    if !filter.contains(&rel) {
                        continue;
                    }
                }
                *scores.entry(rel).or_default() += weight;
            }
        }
    }

    Ok(scores.into_iter().collect())
}

