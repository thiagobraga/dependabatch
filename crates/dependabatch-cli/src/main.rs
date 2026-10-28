use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use dependabatch_core::batch;
use dependabatch_core::client::{GithubClient, RepoSlug};
use dependabatch_core::plan::{RegenPlan, select};
use dependabatch_core::pr::Update;
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
    /// Apply clean dependabot PRs to a batch branch and open the superseding PR.
    Batch {
        /// Repository in `owner/name` form.
        repo: String,
        /// Base branch to fork the batch from (defaults to the repo default).
        #[arg(long)]
        base: Option<String>,
        /// Dependency names to keep out of the batch.
        #[arg(long)]
        exclude: Vec<String>,
        /// Docker image used to regenerate npm lockfiles.
        #[arg(long, default_value = "node:24")]
        regen_image: String,
        /// Skip lockfile regeneration entirely (manifest edits only).
        #[arg(long)]
        no_regen: bool,
        /// Print the plan and the PR preview, then stop (also -n).
        #[arg(long, short = 'n')]
        dry_run: bool,
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
        Command::Batch {
            repo,
            base,
            exclude,
            regen_image,
            no_regen,
            dry_run,
        } => {
            let slug = RepoSlug::parse(&repo)?;
            let token = token()?;
            let client = GithubClient::new(&token)?;
            let prs = client.open_dependabot_prs(&slug).await?;
            let base = match base {
                Some(b) => b,
                None => client.default_branch(&slug).await?,
            };
            let has_lock = client
                .file_exists(&slug, "package-lock.json", &base)
                .await
                .unwrap_or(false);

            let mut plan = select(&prs, has_lock);
            for name in &exclude {
                let removed: Vec<u64> = plan
                    .included
                    .iter()
                    .filter(|pr| dep_name(pr).is_some_and(|n| n == name))
                    .map(|pr| pr.number)
                    .collect();
                if removed.is_empty() {
                    return Err(anyhow!("--exclude {name}: no such dependency in the batch"));
                }
                plan.included
                    .retain(|pr| dep_name(pr).is_none_or(|n| n != name));
                plan.excluded.extend(removed.into_iter().map(|number| {
                    dependabatch_core::plan::Excluded {
                        number,
                        reason: "excluded via --exclude".to_string(),
                    }
                }));
            }
            if no_regen {
                plan.regen = RegenPlan::Skipped;
            }

            println!("{}", plan.summary());
            if dry_run {
                if !plan.included.is_empty() {
                    println!("\n----- batch PR preview -----\n");
                    println!("{}", batch::preview(&plan));
                }
                return Ok(());
            }
            if plan.included.is_empty() {
                bail!("nothing to batch");
            }

            let result = batch::run(&plan, &slug, Some(&base), &token, &regen_image).await?;
            println!();
            println!("batch PR      : {}", result.pr_url);
            println!("branch        : {}", result.branch);
            println!("bumps         : {}", result.bumps);
            println!("applied files : {}", result.applied_files);
            Ok(())
        }
    }
}

fn dep_name(pr: &dependabatch_core::pr::PullRequest) -> Option<&str> {
    match &pr.update {
        Update::Bump(b) => Some(b.name.as_str()),
        Update::Other { .. } => None,
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
