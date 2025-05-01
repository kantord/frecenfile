use clap::Parser;
use frecenfile::analyze_repo;
use std::path::PathBuf;
use std::process;

#[derive(Parser, Debug)]
#[command(
    name = "frecenfile",
    version,
    about = "Compute frecency scores for files in a Git repository"
)]
struct Args {
    /// Path to the Git repository (defaults to current directory)
    #[arg(short = 'D', long = "repo", value_name = "REPO", default_value = ".")]
    repo: PathBuf,

    /// Relative paths to include; omit to include all files.
    #[arg(short, long = "paths", value_name = "PATH", num_args = 1..)]
    paths: Vec<PathBuf>,

    /// Maximum number of commits to inspect (newest first). \
    /// Use 0 for “no limit”.
    #[arg(
        short = 'n',
        long = "max-commits",
        value_name = "N",
        default_value_t = 3000
    )]
    max_commits: usize,

    /// Sort ascending (lowest score first)
    #[arg(
        short = 'a',
        long = "ascending",
        help = "Sort ascending (lowest score first)"
    )]
    ascending: bool,

    /// Sort descending (highest score first)
    #[arg(
        short = 'd',
        long = "descending",
        help = "Sort descending (highest score first)"
    )]
    descending: bool,

    /// Print only file paths, without scores
    #[arg(
        short = 'P',
        long = "path-only",
        help = "Print only paths, omit scores"
    )]
    path_only: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.ascending && args.descending {
        eprintln!("Error: --ascending and --descending cannot be used together");
        process::exit(1);
    }

    let filter = if args.paths.is_empty() {
        None
    } else {
        Some(args.paths.into_iter().collect())
    };

    // When max_commits == 0 we process the entire commit history
    let max_commits_opt = if args.max_commits == 0 {
        None
    } else {
        Some(args.max_commits)
    };

    let mut results = analyze_repo(&args.repo, filter, max_commits_opt)?;

    // Default sort: descending, unless --ascending passed.
    if args.ascending {
        results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    } else {
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    }

    for (path, score) in results {
        if args.path_only {
            println!("{}", path.display());
        } else {
            println!("{:<10.4}  {}", score, path.display());
        }
    }
    Ok(())
}

