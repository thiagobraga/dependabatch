use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use dependabatch_core::alert_fix;
use dependabatch_core::alerts::{AlertsReport, normalize_git_ref};
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

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn alert_fixes_default_to_no_ai() {
        match Cli::try_parse_from(["dbatch", "fix", "-a"])
            .unwrap()
            .command
        {
            Command::Run { ai, .. } => assert_eq!(ai, "none"),
            _ => panic!("expected fix command"),
        }
        match Cli::try_parse_from(["dbatch", "alerts", "--fix"])
            .unwrap()
            .command
        {
            Command::Alerts { ai, .. } => assert_eq!(ai, "none"),
            _ => panic!("expected alerts command"),
        }
        assert!(Cli::try_parse_from(["dbatch", "fix", "-a", "--ai", "invalid"]).is_err());
    }
}

/// Shared options for querying GitHub code-scanning alerts.
#[derive(Args, Debug, Clone)]
struct AlertFilterArgs {
    /// Alert state: open, fixed, dismissed, or closed.
    #[arg(long, default_value = "open")]
    state: String,
    /// Filter alerts by severity (critical, high, medium, low, error, warning, note).
    #[arg(long)]
    severity: Option<String>,
    /// Git ref or branch for alerts (defaults to the repo default branch; pass `all` to omit ref filter).
    #[arg(long = "ref")]
    git_ref: Option<String>,
    /// Show full details for a single code-scanning alert number.
    #[arg(long)]
    alert: Option<u64>,
}

#[derive(Subcommand)]
enum Command {
    /// List open dependabot PRs with conflict analysis (or code-scanning alerts with `--alerts`).
    Scan {
        /// `owner/name`, a GitHub URL, or a local git checkout path
        /// (defaults to the current directory).
        repo: Option<String>,
        /// Print machine readable JSON.
        #[arg(long)]
        json: bool,
        /// Scan GitHub code-scanning alerts instead of dependabot PRs.
        #[arg(short = 'a', long = "alerts", visible_alias = "code-scanning")]
        alerts: bool,
        #[command(flatten)]
        filter: AlertFilterArgs,
    },
    /// List or inspect GitHub code-scanning alerts for a repository.
    #[command(name = "alerts", visible_aliases = ["code-scanning", "cs"])]
    Alerts {
        /// `owner/name`, a GitHub URL, or a local git checkout path
        /// (defaults to the current directory).
        repo: Option<String>,
        /// Print machine readable JSON.
        #[arg(long)]
        json: bool,
        /// Fix the matched code-scanning alert(s) and open a PR.
        #[arg(long)]
        fix: bool,
        /// Print the fix plan and PR preview without touching anything (with `--fix`).
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Filter alerts by file/target path substring.
        #[arg(long)]
        path: Option<String>,
        /// AI CLI fallback mode when fixing alerts: auto, claude, gemini, or none.
        #[arg(long, default_value = "none", value_parser = ["auto", "claude", "gemini", "none"])]
        ai: String,
        #[command(flatten)]
        filter: AlertFilterArgs,
    },
    /// Apply clean dependabot PRs to a batch branch (or fix code-scanning alerts with `-a`) and open a PR.
    #[command(name = "run", visible_aliases = ["batch", "pr", "fix"])]
    Run {
        /// `owner/name`, a GitHub URL, or a local git checkout path
        /// (defaults to the current directory).
        repo: Option<String>,
        /// Base branch to fork from (defaults to the repo default).
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
        /// Fix GitHub code-scanning alerts instead of batching dependabot PRs (also -a).
        #[arg(short = 'a', long = "alerts", visible_alias = "code-scanning")]
        alerts: bool,
        /// Fix a single code-scanning alert by number (implies `--alerts`).
        #[arg(long)]
        alert: Option<u64>,
        /// Filter code-scanning alerts by severity (critical, high, medium, low, warning, error).
        #[arg(long)]
        severity: Option<String>,
        /// Filter code-scanning alerts by path/target substring.
        #[arg(long)]
        path: Option<String>,
        /// AI CLI fallback mode when fixing alerts: auto, claude, gemini, or none.
        #[arg(long, default_value = "none", value_parser = ["auto", "claude", "gemini", "none"])]
        ai: String,
    },
    /// Merge the open batch PR and close all superseded dependabot PRs.
    Merge {
        /// `owner/name`, a GitHub URL, or a local git checkout path
        /// (defaults to the current directory).
        repo: Option<String>,
        /// Merge method: squash, merge, or rebase.
        #[arg(long, default_value = "squash")]
        method: String,
    },
    /// Close superseded dependabot PRs referenced by the latest (or given) batch PR.
    #[command(name = "close", visible_aliases = ["cleanup"])]
    Close {
        /// `owner/name`, a GitHub URL, or a local git checkout path
        /// (defaults to the current directory).
        repo: Option<String>,
        /// Explicit batch PR number (defaults to the latest `dependabatch/batch` PR).
        #[arg(long)]
        pr: Option<u64>,
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
        Command::Scan {
            repo,
            json,
            alerts,
            filter,
        } => {
            if alerts
                || filter.alert.is_some()
                || filter.severity.is_some()
                || filter.git_ref.is_some()
                || filter.state != "open"
            {
                return run_alerts(repo.as_deref(), json, &filter).await;
            }
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
        Command::Alerts {
            repo,
            json,
            fix,
            dry_run,
            path,
            ai,
            filter,
        } => {
            if fix {
                return run_fix_alerts(
                    repo.as_deref(),
                    None,
                    filter.alert,
                    filter.severity.as_deref(),
                    path.as_deref(),
                    "node:24",
                    false,
                    dry_run,
                    &ai,
                )
                .await;
            }
            run_alerts(repo.as_deref(), json, &filter).await
        }
        Command::Run {
            repo,
            base,
            exclude,
            regen_image,
            no_regen,
            dry_run,
            alerts,
            alert,
            severity,
            path,
            ai,
        } => {
            if alerts || alert.is_some() || severity.is_some() || path.is_some() {
                return run_fix_alerts(
                    repo.as_deref(),
                    base.as_deref(),
                    alert,
                    severity.as_deref(),
                    path.as_deref(),
                    &regen_image,
                    no_regen,
                    dry_run,
                    &ai,
                )
                .await;
            }
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
        Command::Merge { repo, method } => {
            let slug = resolve_repo(repo.as_deref())?;
            let token = token()?;
            let res = batch::merge(&slug, &token, &method).await?;
            println!("merged PR     : #{} ({})", res.batch_pr, res.pr_url);
            println!("closed PRs    : {}", format_pr_list(&res.closed));
            if !res.already_closed.is_empty() {
                println!("already closed: {}", format_pr_list(&res.already_closed));
            }
            Ok(())
        }
        Command::Close { repo, pr } => {
            let slug = resolve_repo(repo.as_deref())?;
            let token = token()?;
            let res = batch::close_superseded(&slug, &token, pr).await?;
            println!("batch PR      : #{}", res.batch_pr);
            println!("closed PRs    : {}", format_pr_list(&res.closed));
            if !res.already_closed.is_empty() {
                println!("already closed: {}", format_pr_list(&res.already_closed));
            }
            Ok(())
        }
        Command::Serve { host, port } => serve(host, port).await,
    }
}

async fn run_alerts(repo: Option<&str>, json: bool, filter: &AlertFilterArgs) -> Result<()> {
    let slug = resolve_repo(repo)?;
    let client = GithubClient::new(&token()?)?;

    if let Some(number) = filter.alert {
        let alert = client.code_scanning_alert(&slug, number).await?;
        if json {
            println!("{}", serde_json::to_string_pretty(&alert)?);
        } else {
            println!("{}", alert.render_detail(slug.as_str()));
        }
        return Ok(());
    }

    let resolved_ref = match filter.git_ref.as_deref() {
        Some("all" | "*") => None,
        Some(r) => Some(normalize_git_ref(r)),
        None => {
            let branch = client.default_branch(&slug).await?;
            Some(normalize_git_ref(&branch))
        }
    };

    let alerts = client
        .code_scanning_alerts(
            &slug,
            &filter.state,
            resolved_ref.as_deref(),
            filter.severity.as_deref(),
        )
        .await?;
    let report = AlertsReport::build(
        slug.as_str(),
        &filter.state,
        resolved_ref,
        filter.severity.clone(),
        alerts,
    );
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", report.render());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_fix_alerts(
    repo: Option<&str>,
    base: Option<&str>,
    single_alert: Option<u64>,
    severity: Option<&str>,
    path_filter: Option<&str>,
    regen_image: &str,
    no_regen: bool,
    dry_run: bool,
    ai_mode: &str,
) -> Result<()> {
    let slug = resolve_repo(repo)?;
    let token = token()?;
    let client = GithubClient::new(&token)?;
    let base_branch = match base {
        Some(b) => b.to_string(),
        None => client.default_branch(&slug).await?,
    };
    let git_ref = normalize_git_ref(&base_branch);

    let alerts = if let Some(num) = single_alert {
        vec![client.code_scanning_alert(&slug, num).await?]
    } else {
        let fetched = client
            .code_scanning_alerts(&slug, "open", Some(&git_ref), severity)
            .await?;
        let report = AlertsReport::build(
            slug.as_str(),
            "open",
            Some(git_ref),
            severity.map(str::to_string),
            fetched,
        );
        report.alerts
    };

    let plan = alert_fix::select_alert_fixes(&alerts, single_alert, path_filter);
    println!("{}", plan.summary());
    if dry_run {
        if !plan.items.is_empty() {
            println!("\n----- alert fix PR preview -----\n");
            println!("{}", alert_fix::preview(&plan, &slug));
        }
        return Ok(());
    }
    if plan.items.is_empty() {
        bail!("nothing to fix");
    }

    let result = alert_fix::run(
        &plan,
        &slug,
        Some(&base_branch),
        &token,
        regen_image,
        no_regen,
        ai_mode,
    )
    .await?;
    println!();
    println!("alert fix PR  : {}", result.pr_url);
    println!("branch        : {}", result.branch);
    println!("changed targets : {}", result.fixed_items);
    println!("alerts        : {}", format_pr_list(&result.alert_numbers));
    Ok(())
}

fn format_pr_list(prs: &[u64]) -> String {
    if prs.is_empty() {
        "none".to_string()
    } else {
        prs.iter()
            .map(|n| format!("#{n}"))
            .collect::<Vec<_>>()
            .join(", ")
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
