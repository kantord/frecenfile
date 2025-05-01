use clap::Parser;
use std::path::PathBuf;

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
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let filter = if args.paths.is_empty() {
        None
    } else {
        Some(args.paths.into_iter().collect())
    };

    let results = frecenfile::analyze_repo(&args.repo, filter)?;
    println!("{:<10}  {}", "score", "path");
    for (path, score) in results {
        println!("{:<10.4}  {}", score, path.display());
    }
    Ok(())
}
