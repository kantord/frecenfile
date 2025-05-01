use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::Utc;
use git2::{DiffOptions, Repository, Sort};
use rustc_hash::FxHashMap as HashMap;

/// Compute a frecency-style score per file inside `repo_path`.
///
/// * `paths` – optional set of paths to restrict scoring to.
/// * Returns a vector of `(PathBuf, score)` pairs.
pub fn analyze_repo(
    repo_path: &Path,
    paths: Option<HashSet<PathBuf>>,
) -> Result<Vec<(PathBuf, f64)>, git2::Error> {
    let repo = Repository::discover(repo_path)?;

    let mut revwalk = repo.revwalk()?;
    revwalk.push_head()?;
    revwalk.set_sorting(Sort::TIME)?;
    revwalk.simplify_first_parent()?;

    let now_secs = Utc::now().timestamp();
    let mut scores: HashMap<PathBuf, f64> = HashMap::default();

    for oid in revwalk {
        let commit = repo.find_commit(oid?)?;

        // skip merge commits
        if commit.parent_count() > 1 {
            continue;
        }

        let tree = commit.tree()?;

        let age_days = ((now_secs - commit.time().seconds()) / 86_400).max(0) as f64;
        let weight = 1.0 / (age_days + 1.0).powi(2);

        let mut diff_opts = DiffOptions::new();
        diff_opts
            .context_lines(0)
            .interhunk_lines(0)
            .skip_binary_check(true)
            .include_typechange(false);

        if let Some(ref filter) = paths {
            for p in filter {
                diff_opts.pathspec(p);
            }
        }

        let parent_tree = if commit.parent_count() == 1 {
            Some(commit.parent(0)?.tree()?)
        } else {
            None
        };

        let diff =
            repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut diff_opts))?;

        for delta in diff.deltas() {
            if let Some(path) = delta.new_file().path() {
                if paths.as_ref().map_or(true, |f| f.contains(path)) {
                    *scores.entry(path.to_path_buf()).or_default() += weight;
                }
            }
        }
    }

    Ok(scores.into_iter().collect())
}

