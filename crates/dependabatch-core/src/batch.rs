use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::client::{GithubClient, RepoSlug};
use crate::plan::{FileKind, Plan, RegenPlan, classify_file};
use crate::pr::{PrFile, PullRequest};

/// Branch name every batch PR comes from.
pub const BATCH_BRANCH: &str = "dependabatch/batch";

/// Outcome of a successful batch run.
#[derive(Debug, Serialize)]
pub struct BatchResult {
    pub branch: String,
    pub pr_url: String,
    pub applied_files: usize,
    pub bumps: usize,
}

/// Batch PR title, conventional-commits style.
pub fn batch_title(plan: &Plan) -> String {
    let bumps = plan.included.len();
    let noun = if bumps == 1 {
        "dependency"
    } else {
        "dependencies"
    };
    format!("chore(deps): batch bump {bumps} {noun} via dependabatch")
}

/// Batch PR body: table of every bump plus the superseded originals.
pub fn batch_body(plan: &Plan) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Consolidates {} dependabot update PR(s) into one batch.\n\n",
        plan.included.len()
    ));
    out.push_str("| PR | Update |\n| --- | --- |\n");
    for pr in &plan.included {
        out.push_str(&format!("| #{} | `{}` |\n", pr.number, pr.describe()));
    }
    let list = plan
        .included
        .iter()
        .map(|pr| format!("#{}", pr.number))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!(
        "\n**Supersedes:** {list}. The originals close as superseded when this PR merges.\n"
    ));
    out.push_str(&format!(
        "\n---\n<sub>Batched by [dependabatch](https://github.com/thiagobraga/dependabatch) v{}.</sub>\n",
        env!("CARGO_PKG_VERSION")
    ));
    out
}

/// Full pipeline: clone base, apply manifest/workflow patches, regenerate the
/// lockfile, commit, push `dependabatch/batch` and open the PR.
pub async fn run(
    plan: &Plan,
    repo: &RepoSlug,
    base_override: Option<&str>,
    token: &str,
    regen_image: &str,
) -> Result<BatchResult> {
    ensure_git()?;
    if plan.included.is_empty() {
        bail!("nothing to batch");
    }
    let step = |msg: &str| eprintln!("[dependabatch] {msg}");

    let client = GithubClient::new(token)?;
    let default_base = match base_override {
        Some(b) => b.to_string(),
        None => client.default_branch(repo).await?,
    };
    let pinned = common_base_sha(plan);
    match &pinned {
        Some(sha) => step(&format!(
            "building batch branch from base {default_base}, pinned to PR base {:.7}",
            sha
        )),
        None => step(&format!(
            "building batch branch from base {default_base} (branch tip)"
        )),
    }

    let parent = temp_parent()?;
    let work = match Workspace::new(&parent, repo, token, pinned.as_deref(), &default_base) {
        Ok(w) => w,
        Err(err) => return Err(err.context(scratch_note(&parent))),
    };

    let mut applied = 0usize;
    for pr in &plan.included {
        for file in applicable_files(pr) {
            let diff = file_diff(file);
            let piece = path_name(&parent.join("piece.diff"));
            fs::write(&piece, diff).context("writing diff piece")?;
            if let Err(err) = work.git(&["apply", "--whitespace=nowarn", &piece]) {
                return Err(err
                    .context(format!(
                        "applying #{} `{}` failed",
                        pr.number, file.filename
                    ))
                    .context(scratch_note(&parent)));
            }
            fs::remove_file(&piece).ok();
            applied += 1;
        }
    }

    if plan.regen == RegenPlan::Npm {
        step("regenerating npm lockfile inside an ephemeral docker container");
        if let Err(err) = crate::regen::regenerate_npm_lock(&work.root, regen_image) {
            return Err(err.context(scratch_note(&parent)));
        }
    }

    let title = batch_title(plan);
    step(&format!("committing `{title}`"));
    work.git(&["add", "-A"])?;
    work.git(&["commit", "--quiet", "-m", &title])?;
    let push_arg = format!("HEAD:refs/heads/{BATCH_BRANCH}");
    step("pushing branch");
    if let Err(err) = work.git(&["push", "--force", "origin", &push_arg]) {
        return Err(err.context(scratch_note(&parent)));
    }

    step("opening the batch PR");
    let body = batch_body(plan);
    let pr_url = match client
        .create_pull(repo, &title, BATCH_BRANCH, &default_base, &body)
        .await
    {
        Ok(url) => url,
        Err(err) => return Err(err.context(scratch_note(&parent))),
    };

    let _ = fs::remove_dir_all(&parent);
    Ok(BatchResult {
        branch: BATCH_BRANCH.to_string(),
        pr_url,
        applied_files: applied,
        bumps: plan.included.len(),
    })
}

fn applicable_files(pr: &PullRequest) -> Vec<&PrFile> {
    pr.files
        .iter()
        .filter(|f| {
            matches!(
                classify_file(&f.filename),
                FileKind::NpmManifest | FileKind::Workflow
            )
        })
        .filter(|f| f.patch.is_some())
        .collect()
}

/// Patches carry their own file header via the wrap diff; positions come from
/// the API patch body.
fn file_diff(f: &PrFile) -> String {
    format!(
        "diff --git a/{name} a/{name}\n--- a/{name}\n+++ b/{name}\n{patch}",
        name = f.filename,
        patch = f.patch.as_deref().unwrap_or_default()
    )
}

fn path_name(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

/// All included PRs usually share the same base sha; `None` builds from the
/// branch tip instead.
fn common_base_sha(plan: &Plan) -> Option<String> {
    let mut sha: Option<String> = None;
    for pr in &plan.included {
        match (&pr.base_sha, &sha) {
            (None, _) => return None,
            (Some(s), None) => sha = Some(s.clone()),
            (Some(s), Some(cur)) if s != cur => return None,
            _ => {}
        }
    }
    sha
}

/// Mid-run failures keep the scratch dir; this note tells the user where.
fn scratch_note(parent: &Path) -> String {
    format!("scratch dir kept for inspection: {}", parent.display())
}

fn ensure_git() -> Result<()> {
    let ok = Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        bail!("git binary is required")
    }
}

fn temp_parent() -> Result<PathBuf> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let dir = std::env::temp_dir().join(format!("dependabatch-{secs}-{}", std::process::id()));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A scratch git clone with token auth via GIT_ASKPASS, so the token never
/// lands in URLs, logs or git config.
struct Workspace {
    root: PathBuf,
    askpass: PathBuf,
    token: String,
}

impl Workspace {
    fn new(
        parent: &Path,
        repo: &RepoSlug,
        token: &str,
        pinned_sha: Option<&str>,
        base_branch: &str,
    ) -> Result<Self> {
        let askpass = parent.join("dependabatch-askpass.sh");
        fs::write(
            &askpass,
            "#!/bin/sh\ncase \"$1\" in *[Pp]assword*) echo \"$DEPENDABATCH_TOKEN\" ;; *) echo \"x-access-token\" ;; esac\n",
        )?;
        fs::set_permissions(&askpass, fs::Permissions::from_mode(0o700))?;

        let root = parent.join("work");
        let url = format!("https://github.com/{}.git", repo.as_str());
        let clone = [
            "clone",
            "--quiet",
            "--depth",
            "1",
            "--branch",
            base_branch,
            &url,
            root.to_str().context("clone path")?,
        ];
        let mut cmd = auth_cmd(&askpass, token, Some(parent));
        cmd.args(clone);
        clone_run(cmd, repo)?;

        let w = Self {
            root,
            askpass,
            token: token.to_string(),
        };
        if let Some(sha) = pinned_sha {
            w.git(&["fetch", "--quiet", "--depth", "1", "origin", sha])?;
            w.git(&["checkout", "--quiet", "--detach", sha])?;
        }
        w.git(&["switch", "--quiet", "-c", BATCH_BRANCH])?;
        Ok(w)
    }

    fn git(&self, args: &[&str]) -> Result<()> {
        let mut cmd = auth_cmd(&self.askpass, &self.token, Some(&self.root));
        cmd.args(args);
        let out = cmd.output().with_context(|| format!("git {args:?}"))?;
        if !out.status.success() {
            return Err(anyhow!(
                "git {args:?} failed:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }
}

fn auth_cmd(askpass: &Path, token: &str, cwd: Option<&Path>) -> Command {
    let mut cmd = Command::new("git");
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    cmd.env("GIT_ASKPASS", askpass)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("DEPENDABATCH_TOKEN", token);
    cmd
}

fn clone_run(cmd: Command, repo: &RepoSlug) -> Result<()> {
    let mut cmd = cmd;
    let out = cmd.output().context("git clone failed")?;
    if !out.status.success() {
        bail!(
            "git clone failed for {}:\n{}",
            repo.as_str(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::select;
    use crate::pr::{DepBump, Update};

    fn pr(number: u64, name: &str, files: Vec<PrFile>) -> PullRequest {
        PullRequest {
            number,
            title: format!("Bump {name} from 1.0.0 to 1.1.0"),
            author: crate::pr::DEPENDABOT_LOGIN.into(),
            update: Update::Bump(DepBump {
                name: name.into(),
                from: "1.0.0".into(),
                to: "1.1.0".into(),
                path: None,
            }),
            files,
            base_sha: None,
        }
    }

    fn file(filename: &str) -> PrFile {
        PrFile {
            filename: filename.into(),
            hunks: Vec::new(),
            patch: Some("@@ -1,1 +1,2 @@\n-old\n+new\n".into()),
        }
    }

    #[test]
    fn title_is_singular_and_plural() {
        let one = [pr(1, "vue", vec![file("package.json")])];
        let plan = select(&one, false);
        assert_eq!(
            batch_title(&plan),
            "chore(deps): batch bump 1 dependency via dependabatch"
        );

        let two = [
            pr(1, "vue", vec![file("package.json")]),
            pr(2, "axios", vec![file("package.json")]),
        ];
        let plan = select(&two, false);
        assert_eq!(
            batch_title(&plan),
            "chore(deps): batch bump 2 dependencies via dependabatch"
        );
    }

    #[test]
    fn body_lists_supersedes() {
        let prs = [
            pr(1, "vue", vec![file("package.json")]),
            pr(2, "axios", vec![file("package.json")]),
        ];
        let plan = select(&prs, false);
        let body = batch_body(&plan);
        assert!(body.contains("Supersedes:** #1, #2"));
        assert!(body.contains("| #1 | `vue 1.0.0 -> 1.1.0` |"));
        assert!(body.contains("close as superseded"));
    }

    #[test]
    fn file_diff_has_headers() {
        let f = file("package.json");
        let diff = file_diff(&f);
        assert!(diff.starts_with("diff --git a/package.json a/package.json"));
        assert!(diff.contains("@@ -1,1 +1,2 @@"));
    }
}
