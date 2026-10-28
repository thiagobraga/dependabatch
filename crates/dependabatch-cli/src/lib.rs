use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use dependabatch_core::batch;
use dependabatch_core::client::{GithubClient, RepoSlug};
use dependabatch_core::plan::{RegenPlan, select};
use dependabatch_core::report::ScanReport;
use std::path::Path;

#[derive(Parser)]
#[command(
    name = "dependabatch",
    version,
    disable_version_flag = true,
    about = "Collect open dependabot PRs into one superseding batch PR"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Print version.
    #[arg(short = 'v', short_alias = 'V', long, action = clap::ArgAction::Version)]
    version: Option<bool>,
}

#[derive(Subcommand)]
enum Command {
    /// List open dependabot PRs with conflict analysis (dry run).
    Scan {
        /// `owner/name`, a GitHub URL, or a local git checkout path
        /// (defaults to the current directory).
        repo: Option<String>,
        /// Print machine readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Apply clean dependabot PRs to a batch branch and open the superseding PR.
    #[command(name = "run", visible_aliases = ["batch", "pr", "fix"])]
    Run {
        /// `owner/name`, a GitHub URL, or a local git checkout path
        /// (defaults to the current directory).
        repo: Option<String>,
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
    /// Run a tiny local dashboard: scan repos from the browser.
    Serve {
        /// Address to bind.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port to bind.
        #[arg(long, default_value_t = 8280)]
        port: u16,
    },
}

#[tokio::main]
pub async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan { repo, json } => {
            let slug = resolve_repo(repo.as_deref())?;
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
        Command::Run {
            repo,
            base,
            exclude,
            regen_image,
            no_regen,
            dry_run,
        } => {
            let slug = resolve_repo(repo.as_deref())?;
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
                    .filter(|pr| pr_has_dep(pr, name))
                    .map(|pr| pr.number)
                    .collect();
                if removed.is_empty() {
                    return Err(anyhow!("--exclude {name}: no such dependency in the batch"));
                }
                plan.included.retain(|pr| !pr_has_dep(pr, name));
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
        Command::Serve { host, port } => serve(host, port).await,
    }
}

/// Tiny local dashboard: static page plus a scan API backed by the core lib.
async fn serve(host: String, port: u16) -> Result<()> {
    use axum::{Json, Router, extract::Query, http::StatusCode, response::Html, routing::get};

    async fn index() -> Html<&'static str> {
        Html(include_str!("web/index.html"))
    }

    #[derive(serde::Deserialize)]
    struct ScanQuery {
        repo: String,
    }

    async fn api_scan(
        Query(q): Query<ScanQuery>,
    ) -> Result<Json<ScanReport>, (StatusCode, String)> {
        let slug = RepoSlug::parse(&q.repo).map_err(api_err(StatusCode::BAD_REQUEST))?;
        let token = token().map_err(api_err(StatusCode::INTERNAL_SERVER_ERROR))?;
        let client =
            GithubClient::new(&token).map_err(api_err(StatusCode::INTERNAL_SERVER_ERROR))?;
        let prs = client
            .open_dependabot_prs(&slug)
            .await
            .map_err(api_err(StatusCode::BAD_GATEWAY))?;
        Ok(Json(ScanReport::build(slug.as_str(), &prs)))
    }

    #[derive(serde::Serialize)]
    struct SimulateResponse {
        repo: String,
        summary: String,
        preview: String,
        included: Vec<PullPreview>,
        excluded: Vec<ExcludedPreview>,
        regen: String,
    }

    #[derive(serde::Serialize)]
    struct PullPreview {
        number: u64,
        update: String,
    }

    #[derive(serde::Serialize)]
    struct ExcludedPreview {
        number: u64,
        reason: String,
    }

    async fn api_simulate(
        Query(q): Query<ScanQuery>,
    ) -> Result<Json<SimulateResponse>, (StatusCode, String)> {
        let slug = RepoSlug::parse(&q.repo).map_err(api_err(StatusCode::BAD_REQUEST))?;
        let token = token().map_err(api_err(StatusCode::INTERNAL_SERVER_ERROR))?;
        let client =
            GithubClient::new(&token).map_err(api_err(StatusCode::INTERNAL_SERVER_ERROR))?;
        let prs = client
            .open_dependabot_prs(&slug)
            .await
            .map_err(api_err(StatusCode::BAD_GATEWAY))?;
        let base = client
            .default_branch(&slug)
            .await
            .map_err(api_err(StatusCode::BAD_GATEWAY))?;
        let has_lock = client
            .file_exists(&slug, "package-lock.json", &base)
            .await
            .unwrap_or(false);
        let plan = select(&prs, has_lock);
        let summary = plan.summary();
        let preview = batch::preview(&plan);
        let included = plan
            .included
            .iter()
            .map(|pr| PullPreview {
                number: pr.number,
                update: pr.describe(),
            })
            .collect();
        let excluded = plan
            .excluded
            .iter()
            .map(|e| ExcludedPreview {
                number: e.number,
                reason: e.reason.clone(),
            })
            .collect();
        let regen = format!("{:?}", plan.regen);

        Ok(Json(SimulateResponse {
            repo: slug.as_str().to_string(),
            summary,
            preview,
            included,
            excluded,
            regen,
        }))
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/api/scan", get(api_scan))
        .route("/api/simulate", get(api_simulate));
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    println!("dependabatch dashboard: http://{addr} (ctrl-c to stop)");
    axum::serve(listener, app)
        .await
        .with_context(|| "serving")?;
    Ok(())
}

/// Map an `anyhow` failure onto an HTTP status with the error chain as text.
fn api_err(
    status: axum::http::StatusCode,
) -> impl Fn(anyhow::Error) -> (axum::http::StatusCode, String) {
    move |e: anyhow::Error| (status, format!("{e:#}"))
}

fn pr_has_dep(pr: &dependabatch_core::pr::PullRequest, name: &str) -> bool {
    pr.bumps().iter().any(|b| b.name == name)
}

/// Resolve the target repository from the CLI argument:
/// - none: the git checkout in the current directory
/// - an existing directory: the git checkout at that path
/// - a GitHub URL (https/ssh/scp-like): parsed directly
/// - otherwise: `owner/name`
fn resolve_repo(arg: Option<&str>) -> Result<RepoSlug> {
    let Some(arg) = arg else {
        return slug_from_checkout(Path::new("."));
    };
    let path = Path::new(arg);
    if path.is_dir() {
        return slug_from_checkout(path);
    }
    if arg.contains("://") || arg.contains('@') || arg.starts_with("github.com") {
        let url = if arg.starts_with("github.com") {
            format!("https://{arg}")
        } else {
            arg.to_string()
        };
        return RepoSlug::from_remote_url(&url);
    }
    RepoSlug::parse(arg).with_context(|| {
        format!("`{arg}` is neither a local directory nor an `owner/name` repository")
    })
}

/// Read the GitHub remote of a local git checkout (`origin`, else the first remote).
fn slug_from_checkout(dir: &Path) -> Result<RepoSlug> {
    let git = |args: &[&str]| -> Result<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .context("failed to run `git`")?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let shown = dir.display();
    git(&["rev-parse", "--git-dir"]).with_context(|| {
        format!("`{shown}` is not a git repository; pass `owner/name` or a path")
    })?;
    let remotes = git(&["remote"])?;
    let remote = remotes
        .lines()
        .find(|r| *r == "origin")
        .or_else(|| remotes.lines().next())
        .ok_or_else(|| anyhow!("`{shown}` has no git remotes; pass `owner/name` explicitly"))?;
    let url = git(&["remote", "get-url", remote])?;
    RepoSlug::from_remote_url(&url)
        .with_context(|| format!("remote `{remote}` of `{shown}` is not a GitHub repository"))
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
