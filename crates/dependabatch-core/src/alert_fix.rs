use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::alerts::CodeScanningAlert;
use crate::client::{GithubClient, RepoSlug};
use crate::pr::DepBump;

/// Branch prefix used for code-scanning alert fix PRs.
pub const ALERTS_BATCH_BRANCH: &str = "dependabatch/alerts";

/// How a group of one or more code-scanning alerts on the same target will be fixed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AlertFixStrategy {
    /// Bump an npm dependency in `package.json` / `package-lock.json`.
    NpmPackage {
        manifest_dir: Option<String>,
        bumps: Vec<DepBump>,
    },
    /// Update a Docker base image or OS/binary package in Dockerfiles / Compose files.
    ContainerImage {
        target: String,
        category: Option<String>,
        packages: Vec<String>,
    },
    /// Fix a static-analysis / CodeQL finding in a repository source file.
    SourceCode {
        path: String,
        line: Option<u64>,
        rule_id: String,
        description: String,
        message: Option<String>,
        help: Option<String>,
    },
}

/// A planned fix unit covering one or more alerts on the same path/target.
#[derive(Debug, Clone, Serialize)]
pub struct AlertFixItem {
    pub alerts: Vec<u64>,
    pub severity: String,
    pub path: String,
    pub category: Option<String>,
    pub strategy: AlertFixStrategy,
}

impl AlertFixItem {
    pub fn describe(&self) -> String {
        match &self.strategy {
            AlertFixStrategy::NpmPackage { bumps, .. } => {
                let list: Vec<String> = bumps
                    .iter()
                    .map(|b| format!("{} {} -> {}", b.name, b.from, b.to))
                    .collect();
                format!("npm bump {} ({})", list.join(", "), self.path)
            }
            AlertFixStrategy::ContainerImage {
                target,
                category,
                packages,
            } => {
                let cat = category
                    .as_deref()
                    .map(|c| format!(" [{c}]"))
                    .unwrap_or_default();
                let pkgs = if packages.is_empty() {
                    String::new()
                } else {
                    let shown: Vec<&str> = packages.iter().take(3).map(String::as_str).collect();
                    if packages.len() > 3 {
                        format!(": {} +{} more", shown.join(", "), packages.len() - 3)
                    } else {
                        format!(": {}", shown.join(", "))
                    }
                };
                format!("container/image {target}{cat}{pkgs}")
            }
            AlertFixStrategy::SourceCode {
                path,
                line,
                rule_id,
                description,
                ..
            } => {
                let loc = match line {
                    Some(l) if *l > 1 => format!("{path}:{l}"),
                    _ => path.clone(),
                };
                if rule_id.is_empty() {
                    format!("{description} ({loc})")
                } else {
                    format!("{rule_id}: {description} ({loc})")
                }
            }
        }
    }
}

/// Why an alert was excluded from the automated fix plan.
#[derive(Debug, Clone, Serialize)]
pub struct ExcludedAlert {
    pub number: u64,
    pub reason: String,
}

/// Complete plan for fixing code-scanning alerts.
#[derive(Debug, Clone, Serialize)]
pub struct AlertFixPlan {
    pub items: Vec<AlertFixItem>,
    pub excluded: Vec<ExcludedAlert>,
    pub single_alert: Option<u64>,
}

/// Outcome of a successful `dbatch fix -a` / `dbatch fix --alert <N>` run.
#[derive(Debug, Serialize)]
pub struct AlertFixResult {
    pub branch: String,
    pub pr_url: String,
    pub fixed_items: usize,
    pub alert_numbers: Vec<u64>,
}

impl AlertFixPlan {
    pub fn all_alert_numbers(&self) -> Vec<u64> {
        let mut nums: Vec<u64> = self
            .items
            .iter()
            .flat_map(|i| i.alerts.iter().copied())
            .collect();
        nums.sort_unstable();
        nums.dedup();
        nums
    }

    pub fn branch_name(&self) -> String {
        if let Some(n) = self.single_alert {
            format!("dependabatch/alert-{n}")
        } else {
            let nums = self.all_alert_numbers();
            if nums.len() == 1 {
                format!("dependabatch/alert-{}", nums[0])
            } else {
                ALERTS_BATCH_BRANCH.to_string()
            }
        }
    }

    pub fn summary(&self) -> String {
        let mut out = String::new();
        let nums = self.all_alert_numbers();
        if self.items.is_empty() {
            out.push_str("nothing to fix: no actionable code-scanning alerts matched\n");
        } else {
            out.push_str(&format!(
                "alert fix plan: {} target(s) covering {} alert(s)\n",
                self.items.len(),
                nums.len()
            ));
            for item in &self.items {
                let ids = format_alert_ids(&item.alerts, 4);
                let sev =
                    crate::alerts::colorize_severity(&item.severity.to_uppercase(), &item.severity);
                out.push_str(&format!(
                    "  try {:<16} [{}] {}\n",
                    ids,
                    sev,
                    item.describe()
                ));
            }
        }
        for ex in &self.excluded {
            out.push_str(&format!("  skip #{:<14} {}\n", ex.number, ex.reason));
        }
        out.trim_end().to_string()
    }
}

/// Build an [`AlertFixPlan`] from open code-scanning alerts.
pub fn select_alert_fixes(
    alerts: &[CodeScanningAlert],
    single_alert: Option<u64>,
    path_filter: Option<&str>,
) -> AlertFixPlan {
    let mut items = Vec::new();
    let mut excluded = Vec::new();

    // Group alerts by (path, category)
    let mut groups: BTreeMap<(String, Option<String>), Vec<&CodeScanningAlert>> = BTreeMap::new();
    for a in alerts {
        if let Some(target_num) = single_alert {
            if a.number != target_num {
                continue;
            }
        }
        if let Some(pf) = path_filter {
            if !a.path.contains(pf)
                && !a
                    .category
                    .as_deref()
                    .map(|c| c.contains(pf))
                    .unwrap_or(false)
            {
                continue;
            }
        }
        if a.path.is_empty() {
            excluded.push(ExcludedAlert {
                number: a.number,
                reason: "alert has no file/target path".into(),
            });
            continue;
        }
        groups
            .entry((a.path.clone(), a.category.clone()))
            .or_default()
            .push(a);
    }

    for ((path, category), group) in groups {
        let alert_nums: Vec<u64> = group.iter().map(|a| a.number).collect();
        let highest_sev = highest_severity(&group);

        // 1. Check if it's an npm manifest/lockfile alert
        let base = path.rsplit('/').next().unwrap_or(&path);
        if base == "package.json" || base == "package-lock.json" {
            let mut bumps = Vec::new();
            let dir = path
                .rsplit_once('/')
                .map(|(d, _)| format!("/{}", d.trim_start_matches('/')));
            for a in &group {
                if let (Some(pkg), Some(fixed)) = (&a.package, &a.fixed_version) {
                    if let Some(clean_ver) = first_fixed_version(fixed) {
                        bumps.push(DepBump {
                            name: pkg.clone(),
                            from: a.installed_version.clone().unwrap_or_else(|| "*".into()),
                            to: clean_ver,
                            path: dir.clone(),
                        });
                    }
                }
            }
            if !bumps.is_empty() {
                items.push(AlertFixItem {
                    alerts: alert_nums,
                    severity: highest_sev,
                    path,
                    category,
                    strategy: AlertFixStrategy::NpmPackage {
                        manifest_dir: dir,
                        bumps,
                    },
                });
                continue;
            }
        }

        // 2. Check if it's a container / image / binary alert (e.g. library/*, usr/local/bin/*, infra-*, container-scan-*)
        let is_container = path.starts_with("library/")
            || path.starts_with("usr/")
            || path.starts_with("bin/")
            || path.starts_with("sbin/")
            || path.starts_with("etc/")
            || category
                .as_deref()
                .map(|c| c.starts_with("infra-") || c.starts_with("container-scan"))
                .unwrap_or(false)
            || group.iter().any(|a| a.package.is_some());

        if is_container {
            let mut pkgs = Vec::new();
            let mut any_fixable = false;
            for a in &group {
                if let Some(pkg) = &a.package {
                    let fix_str = a.fixed_version.as_deref().unwrap_or("unfixed");
                    if !a.fixed_version.as_deref().unwrap_or_default().is_empty() {
                        any_fixable = true;
                    }
                    let label = format!("{pkg} ({fix_str})");
                    if !pkgs.contains(&label) {
                        pkgs.push(label);
                    }
                } else {
                    any_fixable = true;
                }
            }
            if !any_fixable {
                for a in &group {
                    excluded.push(ExcludedAlert {
                        number: a.number,
                        reason: format!("no upstream fixed version available for `{path}`"),
                    });
                }
                continue;
            }
            items.push(AlertFixItem {
                alerts: alert_nums,
                severity: highest_sev,
                path: path.clone(),
                category: category.clone(),
                strategy: AlertFixStrategy::ContainerImage {
                    target: path,
                    category,
                    packages: pkgs,
                },
            });
            continue;
        }

        // 3. Source code alert (CodeQL / static analysis on repository files)
        for a in group {
            items.push(AlertFixItem {
                alerts: vec![a.number],
                severity: a.severity.clone(),
                path: a.path.clone(),
                category: a.category.clone(),
                strategy: AlertFixStrategy::SourceCode {
                    path: a.path.clone(),
                    line: a.start_line,
                    rule_id: a.rule_id.clone(),
                    description: a.description.clone(),
                    message: a.message.clone(),
                    help: a.help.clone(),
                },
            });
        }
    }

    AlertFixPlan {
        items,
        excluded,
        single_alert,
    }
}

/// PR title for an alert fix run.
pub fn fix_pr_title(plan: &AlertFixPlan) -> String {
    let nums = plan.all_alert_numbers();
    if nums.len() == 1 && plan.items.len() == 1 {
        let n = nums[0];
        match &plan.items[0].strategy {
            AlertFixStrategy::SourceCode { rule_id, .. } if !rule_id.is_empty() => {
                format!("fix(security): address code-scanning alert #{n} ({rule_id})")
            }
            _ => format!(
                "fix(security): address code-scanning alert #{n} in {}",
                plan.items[0].path
            ),
        }
    } else {
        format!(
            "fix(security): address {} code-scanning alert(s) via dependabatch",
            nums.len()
        )
    }
}

/// PR body for an alert fix run.
pub fn fix_pr_body(plan: &AlertFixPlan, repo: &RepoSlug) -> String {
    let nums = plan.all_alert_numbers();
    let mut out = String::new();
    out.push_str(&format!(
        "Proposes changes for {} code-scanning alert(s) in `{}`. These alerts are not verified as fixed. Build and test the changes, then rerun the security scan.\n\n",
        nums.len(),
        repo.as_str()
    ));
    out.push_str("| Alerts | Severity | Target | Fix |\n| --- | --- | --- | --- |\n");
    for item in &plan.items {
        let links: Vec<String> = item
            .alerts
            .iter()
            .take(5)
            .map(|n| {
                format!(
                    "[#{n}](https://github.com/{}/security/code-scanning/{n})",
                    repo.as_str()
                )
            })
            .collect();
        let links_str = if item.alerts.len() > 5 {
            format!("{} +{} more", links.join(", "), item.alerts.len() - 5)
        } else {
            links.join(", ")
        };
        out.push_str(&format!(
            "| {} | `{}` | `{}` | {} |\n",
            links_str,
            item.severity.to_uppercase(),
            item.path,
            item.describe()
        ));
    }
    out.push_str(&format!(
        "\n---\n<sub>Generated by [dependabatch](https://github.com/thiagobraga/dependabatch) v{}.</sub>\n",
        env!("CARGO_PKG_VERSION")
    ));
    out
}

/// Full preview (title + body) for `--dry-run`.
pub fn preview(plan: &AlertFixPlan, repo: &RepoSlug) -> String {
    format!("{}\n\n{}", fix_pr_title(plan), fix_pr_body(plan, repo))
}

/// Execute the alert fix plan: clone repo, apply deterministic + AI hybrid fixes,
/// commit, push branch, and open PR.
pub async fn run(
    plan: &AlertFixPlan,
    repo: &RepoSlug,
    base_override: Option<&str>,
    token: &str,
    regen_image: &str,
    no_regen: bool,
    ai_mode: &str,
) -> Result<AlertFixResult> {
    if plan.items.is_empty() {
        bail!("nothing to fix");
    }
    let step = |msg: &str| eprintln!("[dependabatch] {msg}");
    let client = GithubClient::new(token)?;
    let base_branch = match base_override {
        Some(b) => b.to_string(),
        None => client.default_branch(repo).await?,
    };
    let branch_name = plan.branch_name();

    step(&format!(
        "cloning {} (base: {base_branch}) to branch {branch_name}",
        repo.as_str()
    ));

    let parent = temp_parent()?;
    let work = clone_workspace(&parent, repo, token, &base_branch, &branch_name)?;

    let mut fixed_items = 0usize;
    let mut touched_npm = false;
    let mut changed_items = Vec::new();

    for item in &plan.items {
        step(&format!("fixing {}", item.describe()));
        let changed = match &item.strategy {
            AlertFixStrategy::NpmPackage {
                manifest_dir,
                bumps,
            } => {
                let rel_manifest = match manifest_dir {
                    Some(d) => format!("{}/package.json", d.trim_start_matches('/')),
                    None => "package.json".to_string(),
                };
                let full_path = work.root.join(&rel_manifest);
                if full_path.is_file() {
                    let refs: Vec<&DepBump> = bumps.iter().collect();
                    crate::manifest::apply_bumps_to_package_json(&full_path, &refs)?;
                    touched_npm = true;
                    true
                } else {
                    false
                }
            }
            AlertFixStrategy::SourceCode {
                path,
                line,
                rule_id,
                description,
                message,
                help,
            } => {
                let mut applied = apply_deterministic_source_fix(
                    &work.root,
                    path,
                    *line,
                    rule_id,
                    message.as_deref(),
                )?;
                if !applied && ai_mode != "none" {
                    applied = apply_ai_fix(
                        &work.root,
                        ai_mode,
                        &format!(
                            "Fix GitHub code-scanning alert in `{path}` (line: {:?}).\nRule: {rule_id} ({description})\nMessage: {}\nHelp: {}\nMake only the minimal edit to `{path}` needed to fix this alert.",
                            line,
                            message.as_deref().unwrap_or_default(),
                            help.as_deref().unwrap_or_default()
                        ),
                    )?;
                }
                applied
            }
            AlertFixStrategy::ContainerImage {
                target,
                category,
                packages,
            } => {
                let mut applied =
                    crate::container_fix::apply(&work.root, target, category.as_deref()).await?;
                if !applied && ai_mode != "none" {
                    applied = apply_ai_fix(
                        &work.root,
                        ai_mode,
                        &format!(
                            "Fix container/image code-scanning alert(s) for target `{target}` (SARIF category: {:?}, vulnerable packages: {}).\nInspect the Dockerfiles and compose*.yml files in this repository and update the base image tag, gosu binary version, or package upgrade step to resolve the vulnerability with minimal changes.",
                            category,
                            packages.join(", ")
                        ),
                    )?;
                }
                applied
            }
        };
        if changed {
            fixed_items += 1;
            changed_items.push(item.clone());
        } else {
            step(&format!(
                "skipped {}: no known applicable fix; manual review required",
                item.describe()
            ));
        }
    }

    if touched_npm && !no_regen {
        step("regenerating npm lockfile(s) inside ephemeral docker container");
        crate::regen::regenerate_all_npm_locks(&work.root, regen_image)?;
    }

    let status = work.git_stdout(&["status", "--porcelain"])?;
    if status.trim().is_empty() {
        let _ = fs::remove_dir_all(&parent);
        bail!(
            "no known applicable fix produced file changes; inspect the skipped alerts and review them manually"
        );
    }

    let applied_plan = AlertFixPlan {
        items: changed_items,
        excluded: Vec::new(),
        single_alert: plan.single_alert,
    };
    step(
        "changes are unverified: build and test the project and rerun code scanning before merging",
    );
    let title = fix_pr_title(&applied_plan);
    step(&format!("committing `{title}`"));
    work.git(&["add", "-A"])?;
    work.git(&["commit", "--quiet", "-m", &title])?;

    let push_arg = format!("HEAD:refs/heads/{branch_name}");
    step(&format!("pushing {branch_name}"));
    work.git(&["push", "--force", "origin", &push_arg])?;

    step("opening PR on GitHub");
    let body = fix_pr_body(&applied_plan, repo);
    let pr_url = client
        .create_pull(repo, &title, &branch_name, &base_branch, &body)
        .await?;

    let _ = fs::remove_dir_all(&parent);
    Ok(AlertFixResult {
        branch: branch_name,
        pr_url,
        fixed_items,
        alert_numbers: applied_plan.all_alert_numbers(),
    })
}

/// Deterministic fixes for known static-analysis / CodeQL rules.
pub fn apply_deterministic_source_fix(
    root: &Path,
    rel_path: &str,
    line: Option<u64>,
    rule_id: &str,
    message: Option<&str>,
) -> Result<bool> {
    let full = root.join(rel_path);
    if !full.is_file() {
        return Ok(false);
    }
    let content = fs::read_to_string(&full)?;
    let lines: Vec<&str> = content.lines().collect();
    let Some(line_num) = line.filter(|&l| l > 0 && (l as usize) <= lines.len()) else {
        return Ok(false);
    };
    let idx = (line_num as usize) - 1;

    if rule_id == "js/superfluous-trailing-arguments"
        || message
            .map(|m| m.contains("Superfluous argument"))
            .unwrap_or(false)
    {
        // Case A: "Superfluous argument passed to default constructor of class <Name>"
        // Often caused by a test stub `class <Name> { ... }` missing a constructor.
        if let Some(msg) = message {
            if let Some(class_part) = msg.split("default constructor of class ").nth(1) {
                let class_name: String = class_part
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                    .collect();
                if !class_name.is_empty() && add_constructor_to_mock_class(root, &class_name)? {
                    return Ok(true);
                }
            }
        }

        // Case B: Call site has superfluous trailing argument(s)
        if let Some(updated) = remove_superfluous_trailing_arg(&content, idx) {
            if updated != content {
                fs::write(&full, updated)?;
                return Ok(true);
            }
        }
    }

    Ok(false)
}

/// Search JS/TS source files for `class <class_name> {` without a `constructor(` and add one.
fn add_constructor_to_mock_class(root: &Path, class_name: &str) -> Result<bool> {
    let mut files = Vec::new();
    collect_js_ts_files(root, &mut files)?;
    let needle = format!("class {class_name}");
    for file in files {
        let Ok(content) = fs::read_to_string(&file) else {
            continue;
        };
        let Some(pos) = content.find(&needle) else {
            continue;
        };
        let after = &content[pos..];
        let Some(brace_rel) = after.find('{') else {
            continue;
        };
        let body_after = &after[brace_rel + 1..];
        // Find matching closing brace of the class
        let mut depth = 1i32;
        let mut end_rel = None;
        for (i, ch) in body_after.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end_rel = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(class_end) = end_rel else {
            continue;
        };
        let class_body = &body_after[..class_end];
        if class_body.contains("constructor") {
            continue;
        }
        // Determine indentation of first member inside class_body
        let indent = class_body
            .lines()
            .skip(1)
            .find(|l| !l.trim().is_empty())
            .map(|l| &l[..l.len() - l.trim_start().len()])
            .unwrap_or("    ");
        let is_ts = file
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e == "ts" || e == "tsx")
            .unwrap_or(false);
        let ctor_line = if is_ts {
            format!("\n{indent}constructor(..._args: unknown[]) {{}}")
        } else {
            format!("\n{indent}constructor() {{}}")
        };
        let insert_at = pos + brace_rel + 1;
        let mut updated = String::with_capacity(content.len() + ctor_line.len());
        updated.push_str(&content[..insert_at]);
        updated.push_str(&ctor_line);
        updated.push_str(&content[insert_at..]);
        fs::write(&file, updated)?;
        return Ok(true);
    }
    Ok(false)
}

fn collect_js_ts_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if path.is_dir() {
            if matches!(
                name_str.as_ref(),
                ".git" | "node_modules" | "target" | ".worktrees" | "dist" | "build" | "coverage"
            ) {
                continue;
            }
            collect_js_ts_files(&path, out)?;
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("ts" | "tsx" | "js" | "jsx" | "mjs")
        ) {
            out.push(path);
        }
    }
    Ok(())
}

/// Remove superfluous trailing argument at or starting on 0-based line `target_line_idx`.
/// Handles both single-line calls (`new ResizeObserver(cb, extra)` or `new Foo(arg)`)
/// and multi-line constructor/function calls (`new ResizeObserver(\n  () => ...,\n  extra\n)`).
fn remove_superfluous_trailing_arg(content: &str, target_line_idx: usize) -> Option<String> {
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(content.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let start_byte = *line_starts.get(target_line_idx)?;
    let window_end_line = (target_line_idx + 12).min(line_starts.len());
    let end_byte = line_starts
        .get(window_end_line)
        .copied()
        .unwrap_or(content.len());
    let slice = &content[start_byte..end_byte];

    // Find the first `(` on the target line
    let first_line_end = slice.find('\n').unwrap_or(slice.len());
    let first_line = &slice[..first_line_end];

    // Find the last `(` on the target line that starts a call/constructor argument list
    let open_paren_rel = first_line.rfind('(')?;
    let after_open = &slice[open_paren_rel + 1..];

    // Walk balanced parentheses/braces/brackets/strings to find top-level commas and matching `)`
    let mut depth_paren = 0i32;
    let mut depth_brace = 0i32;
    let mut depth_bracket = 0i32;
    let mut top_commas = Vec::new();
    let mut close_paren_rel = None;
    let mut in_str: Option<char> = None;
    let mut escaped = false;

    for (i, ch) in after_open.char_indices() {
        if let Some(q) = in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                in_str = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' | '`' => in_str = Some(ch),
            '(' => depth_paren += 1,
            ')' => {
                if depth_paren == 0 && depth_brace == 0 && depth_bracket == 0 {
                    close_paren_rel = Some(i);
                    break;
                }
                depth_paren -= 1;
            }
            '{' => depth_brace += 1,
            '}' => depth_brace -= 1,
            '[' => depth_bracket += 1,
            ']' => depth_bracket -= 1,
            ',' if depth_paren == 0 && depth_brace == 0 && depth_bracket == 0 => {
                top_commas.push(i);
            }
            _ => {}
        }
    }

    let close_rel = close_paren_rel?;
    let args_str = &after_open[..close_rel];
    if args_str.trim().is_empty() {
        return None;
    }

    let new_args = if let Some(&last_comma) = top_commas.last() {
        // Drop the last argument after `last_comma`, preserving trailing newline/indent if multiline
        let tail = &after_open[last_comma + 1..close_rel];
        let trailing_ws = match tail.rfind('\n') {
            Some(nl_pos) => &tail[nl_pos..],
            None => "",
        };
        format!("{}{trailing_ws}", after_open[..last_comma].trim_end())
    } else {
        // Only 1 argument passed to a 0-arg constructor/function: drop it
        String::new()
    };

    let abs_open = start_byte + open_paren_rel;
    let abs_close = abs_open + 1 + close_rel;
    let mut out = String::with_capacity(content.len());
    out.push_str(&content[..=abs_open]);
    out.push_str(&new_args);
    out.push_str(&content[abs_close..]);
    Some(out)
}

/// Invoke an installed AI CLI (`claude` or `gemini`) in `root` as a fallback fixer.
fn apply_ai_fix(root: &Path, ai_mode: &str, prompt: &str) -> Result<bool> {
    let candidates: Vec<&str> = match ai_mode {
        "claude" => vec!["claude"],
        "gemini" => vec!["gemini"],
        "none" => return Ok(false),
        _ => vec!["claude", "gemini"],
    };

    for bin in candidates {
        let which = Command::new("which").arg(bin).output();
        if !which.map(|o| o.status.success()).unwrap_or(false) {
            continue;
        }
        eprintln!("[dependabatch] invoking `{bin}` to generate fix...");
        let status = if bin == "claude" {
            Command::new("claude")
                .current_dir(root)
                .args(["-p", prompt, "--allowedTools", "Edit,Read,Write,Glob,Grep"])
                .status()
        } else {
            Command::new("gemini")
                .current_dir(root)
                .args(["-p", prompt, "--yolo"])
                .status()
        };
        if status.map(|s| s.success()).unwrap_or(false) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn first_fixed_version(raw: &str) -> Option<String> {
    raw.split(',')
        .map(str::trim)
        .find(|s| {
            !s.is_empty()
                && s.chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
        })
        .map(str::to_string)
}

fn highest_severity(alerts: &[&CodeScanningAlert]) -> String {
    let rank = |s: &str| match s.to_ascii_lowercase().as_str() {
        "critical" => 5,
        "high" | "error" => 4,
        "medium" | "warning" => 3,
        "low" | "note" => 2,
        _ => 1,
    };
    alerts
        .iter()
        .max_by_key(|a| rank(&a.severity))
        .map(|a| a.severity.clone())
        .unwrap_or_else(|| "unknown".into())
}

fn format_alert_ids(nums: &[u64], limit: usize) -> String {
    let head: Vec<String> = nums.iter().take(limit).map(|n| format!("#{n}")).collect();
    if nums.len() > limit {
        format!("{} +{}", head.join(","), nums.len() - limit)
    } else {
        head.join(",")
    }
}

fn temp_parent() -> Result<PathBuf> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let dir =
        std::env::temp_dir().join(format!("dependabatch-alerts-{secs}-{}", std::process::id()));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

struct FixWorkspace {
    root: PathBuf,
    askpass: PathBuf,
    token: String,
}

fn clone_workspace(
    parent: &Path,
    repo: &RepoSlug,
    token: &str,
    base_branch: &str,
    target_branch: &str,
) -> Result<FixWorkspace> {
    let askpass = parent.join("dependabatch-askpass.sh");
    fs::write(
        &askpass,
        "#!/bin/sh\ncase \"$1\" in *[Pp]assword*) echo \"$DEPENDABATCH_TOKEN\" ;; *) echo \"x-access-token\" ;; esac\n",
    )?;
    fs::set_permissions(&askpass, fs::Permissions::from_mode(0o700))?;

    let root = parent.join("work");
    let url = format!("https://github.com/{}.git", repo.as_str());
    let mut cmd = Command::new("git");
    cmd.current_dir(parent)
        .env("GIT_ASKPASS", &askpass)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("DEPENDABATCH_TOKEN", token)
        .args([
            "clone",
            "--quiet",
            "--depth",
            "1",
            "--branch",
            base_branch,
            &url,
            root.to_str().context("clone path")?,
        ]);
    let out = cmd.output().context("git clone failed")?;
    if !out.status.success() {
        bail!(
            "git clone failed for {}:\n{}",
            repo.as_str(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let w = FixWorkspace {
        root,
        askpass,
        token: token.to_string(),
    };
    w.git(&["switch", "--quiet", "-c", target_branch])?;
    Ok(w)
}

impl FixWorkspace {
    fn git(&self, args: &[&str]) -> Result<()> {
        let out = Command::new("git")
            .current_dir(&self.root)
            .env("GIT_ASKPASS", &self.askpass)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("DEPENDABATCH_TOKEN", &self.token)
            .args(args)
            .output()
            .with_context(|| format!("git {args:?}"))?;
        if !out.status.success() {
            return Err(anyhow!(
                "git {args:?} failed:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    fn git_stdout(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("git")
            .current_dir(&self.root)
            .args(args)
            .output()
            .with_context(|| format!("git {args:?}"))?;
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn removes_superfluous_trailing_arg_single_and_multiline() {
        let src = "const ro = new ResizeObserver(\n  () => update(),\n  extraArg\n);\n";
        let fixed = remove_superfluous_trailing_arg(src, 0).expect("should fix");
        assert_eq!(
            fixed,
            "const ro = new ResizeObserver(\n  () => update()\n);\n"
        );

        let single = "const x = new Foo(bar, baz);\n";
        let fixed_single = remove_superfluous_trailing_arg(single, 0).expect("should fix");
        assert_eq!(fixed_single, "const x = new Foo(bar);\n");
    }

    #[test]
    fn builds_alert_fix_plan_and_preview() {
        let a1 = CodeScanningAlert::from_json(&json!({
            "number": 216,
            "state": "open",
            "html_url": "https://github.com/thiagobraga/planner/security/code-scanning/216",
            "rule": {
                "id": "js/superfluous-trailing-arguments",
                "severity": "warning",
                "description": "Superfluous trailing arguments"
            },
            "tool": { "name": "CodeQL" },
            "most_recent_instance": {
                "message": { "text": "Superfluous argument passed to default constructor" },
                "location": {
                    "path": "app/src/components/ui/ContextMenu.tsx",
                    "start_line": 429,
                    "end_line": 429
                }
            }
        }))
        .unwrap();

        let plan = select_alert_fixes(&[a1], Some(216), None);
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.branch_name(), "dependabatch/alert-216");
        let slug = RepoSlug::parse("thiagobraga/planner").unwrap();
        let prev = preview(&plan, &slug);
        assert!(prev.contains(
            "fix(security): address code-scanning alert #216 (js/superfluous-trailing-arguments)"
        ));
        assert!(prev.contains("app/src/components/ui/ContextMenu.tsx"));
    }
}
