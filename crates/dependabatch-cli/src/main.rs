use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use dependabatch_core::client::{GithubClient, RepoSlug};
use dependabatch_core::report::ScanReport;

#[derive(Parser)]
#[command(
    name = "dependabatch",
    version,
    about = "Collect open dependabot PRs into one superseding batch PR"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List open dependabot PRs with conflict analysis (dry run).
    Scan {
        /// Repository in `owner/name` form.
        repo: String,
        /// Print machine readable JSON.
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan { repo, json } => {
            let slug = RepoSlug::parse(&repo)?;
            let client = GithubClient::new(&token()?)?;
            let prs = client.open_dependabot_prs(&slug).await?;
            let report = ScanReport::build(slug.as_str(), &prs);
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", report.render());
            }
            Ok(())
        }
    }
}

/// Auth resolution order: GITHUB_TOKEN, GH_TOKEN, then `gh auth token`.
fn token() -> Result<String> {
    for key in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(token) = std::env::var(key) {
            if !token.is_empty() {
                return Ok(token);
            }
        }
    }
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .context("no GITHUB_TOKEN set and failed to run `gh auth token`")?;
    if output.status.success() {
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    Err(anyhow!(
        "no GitHub token: set GITHUB_TOKEN or run `gh auth login`"
    ))
}
