use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// A single GitHub code-scanning alert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodeScanningAlert {
    pub number: u64,
    pub state: String,
    pub severity: String,
    pub rule_severity: Option<String>,
    pub rule_id: String,
    pub description: String,
    pub tool: String,
    pub path: String,
    pub start_line: Option<u64>,
    pub end_line: Option<u64>,
    pub git_ref: Option<String>,
    pub category: Option<String>,
    pub package: Option<String>,
    pub installed_version: Option<String>,
    pub fixed_version: Option<String>,
    pub html_url: String,
    pub message: Option<String>,
    pub help: Option<String>,
    pub tags: Vec<String>,
    pub created_at: Option<String>,
    pub dismissed_reason: Option<String>,
    pub dismissed_comment: Option<String>,
}

impl CodeScanningAlert {
    /// Parse a code-scanning alert JSON object returned by the GitHub REST API.
    pub fn from_json(v: &Value) -> Option<Self> {
        let number = v.pointer("/number")?.as_u64()?;
        let state = v
            .pointer("/state")
            .and_then(Value::as_str)
            .unwrap_or("open")
            .to_string();

        let security_sev = v
            .pointer("/rule/security_severity_level")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let rule_sev = v
            .pointer("/rule/severity")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let severity = security_sev
            .map(str::to_string)
            .or_else(|| rule_sev.clone())
            .unwrap_or_else(|| "unknown".to_string());

        let rule_id = v
            .pointer("/rule/id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let description = v
            .pointer("/rule/description")
            .and_then(Value::as_str)
            .or_else(|| {
                v.pointer("/most_recent_instance/message/text")
                    .and_then(Value::as_str)
            })
            .unwrap_or_default()
            .to_string();

        let tool_name = v
            .pointer("/tool/name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let tool_ver = v
            .pointer("/tool/version")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let tool = match tool_ver {
            Some(ver) => format!("{tool_name} {ver}"),
            None => tool_name.to_string(),
        };

        let path = v
            .pointer("/most_recent_instance/location/path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let start_line = v
            .pointer("/most_recent_instance/location/start_line")
            .and_then(Value::as_u64)
            .filter(|&n| n > 0);
        let end_line = v
            .pointer("/most_recent_instance/location/end_line")
            .and_then(Value::as_u64)
            .filter(|&n| n > 0);
        let git_ref = v
            .pointer("/most_recent_instance/ref")
            .and_then(Value::as_str)
            .map(str::to_string);
        let category = v
            .pointer("/most_recent_instance/category")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let html_url = v
            .pointer("/html_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let message = v
            .pointer("/most_recent_instance/message/text")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let (package, installed_version, fixed_version) =
            parse_pkg_fields(message.as_deref().unwrap_or_default());
        let help = v
            .pointer("/rule/help")
            .and_then(Value::as_str)
            .or_else(|| v.pointer("/rule/full_description").and_then(Value::as_str))
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let tags = v
            .pointer("/rule/tags")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        let created_at = v
            .pointer("/created_at")
            .and_then(Value::as_str)
            .map(str::to_string);
        let dismissed_reason = v
            .pointer("/dismissed_reason")
            .and_then(Value::as_str)
            .map(str::to_string);
        let dismissed_comment = v
            .pointer("/dismissed_comment")
            .and_then(Value::as_str)
            .map(str::to_string);

        Some(Self {
            number,
            state,
            severity,
            rule_severity: rule_sev,
            rule_id,
            description,
            tool,
            path,
            start_line,
            end_line,
            git_ref,
            category,
            package,
            installed_version,
            fixed_version,
            html_url,
            message,
            help,
            tags,
            created_at,
            dismissed_reason,
            dismissed_comment,
        })
    }

    /// Location formatted with optional line number (`path` or `path:line` when line > 1).
    pub fn location_display(&self) -> String {
        match self.start_line {
            Some(line) if line > 1 && !self.path.is_empty() => format!("{}:{line}", self.path),
            _ if !self.path.is_empty() => self.path.clone(),
            _ => "-".to_string(),
        }
    }

    /// Detailed multi-line human-readable view for a single alert (`--alert <number>`).
    pub fn render_detail(&self, repo: &str) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "alert #{} in {} ({})\n",
            self.number, repo, self.state
        ));
        let colored_sev = colorize_severity(&self.severity, &self.severity);
        let sev_line = match &self.rule_severity {
            Some(rs) if !rs.eq_ignore_ascii_case(&self.severity) => {
                format!("{colored_sev} (rule: {rs})")
            }
            _ => colored_sev,
        };
        out.push_str(&format!("severity    : {sev_line}\n"));
        if !self.rule_id.is_empty() {
            out.push_str(&format!("rule        : {}\n", self.rule_id));
        }
        if !self.description.is_empty() {
            out.push_str(&format!("description : {}\n", self.description));
        }
        if !self.tool.is_empty() {
            out.push_str(&format!("tool        : {}\n", self.tool));
        }
        out.push_str(&format!("location    : {}\n", self.location_display()));
        if let Some(cat) = &self.category {
            out.push_str(&format!("category    : {cat}\n"));
        }
        if let Some(pkg) = &self.package {
            let inst = self.installed_version.as_deref().unwrap_or("?");
            let fix = self.fixed_version.as_deref().unwrap_or("none");
            out.push_str(&format!("package     : {pkg} ({inst} -> {fix})\n"));
        }
        if let Some(r) = &self.git_ref {
            out.push_str(&format!("ref         : {r}\n"));
        }
        if let Some(created) = &self.created_at {
            out.push_str(&format!("created     : {created}\n"));
        }
        if !self.tags.is_empty() {
            out.push_str(&format!("tags        : {}\n", self.tags.join(", ")));
        }
        if let Some(reason) = &self.dismissed_reason {
            out.push_str(&format!("dismissed   : {reason}\n"));
        }
        if let Some(comment) = &self.dismissed_comment {
            out.push_str(&format!("comment     : {comment}\n"));
        }
        if !self.html_url.is_empty() {
            out.push_str(&format!("url         : {}\n", self.html_url));
        }
        if let Some(msg) = &self.message {
            if msg != &self.description {
                out.push_str(&format!("\nmessage:\n  {}\n", msg.replace('\n', "\n  ")));
            }
        }
        if let Some(help) = &self.help {
            out.push_str(&format!("\nhelp:\n  {}\n", help.replace('\n', "\n  ")));
        }
        out.trim_end().to_string()
    }
}

/// Summary of alerts grouped by file/location path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PathAlertGroup {
    pub path: String,
    pub count: usize,
    pub critical: usize,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
    pub other: usize,
    pub alerts: Vec<u64>,
}

/// Report of code-scanning alerts for a repository.
#[derive(Debug, Clone, Serialize)]
pub struct AlertsReport {
    pub repo: String,
    pub state: String,
    pub git_ref: Option<String>,
    pub severity_filter: Option<String>,
    pub total: usize,
    pub alerts: Vec<CodeScanningAlert>,
    pub by_path: Vec<PathAlertGroup>,
}

impl AlertsReport {
    pub fn build(
        repo: &str,
        state: &str,
        git_ref: Option<String>,
        severity_filter: Option<String>,
        mut alerts: Vec<CodeScanningAlert>,
    ) -> Self {
        if let Some(sev) = &severity_filter {
            let target = sev.to_ascii_lowercase();
            alerts.retain(|a| {
                a.severity.to_ascii_lowercase() == target
                    || a.rule_severity
                        .as_deref()
                        .map(|r| r.to_ascii_lowercase() == target)
                        .unwrap_or(false)
            });
        }

        // Group by path
        let mut groups_map: BTreeMap<String, PathAlertGroup> = BTreeMap::new();
        for a in &alerts {
            let key = if a.path.is_empty() {
                "(no path)".to_string()
            } else {
                a.path.clone()
            };
            let entry = groups_map
                .entry(key.clone())
                .or_insert_with(|| PathAlertGroup {
                    path: key,
                    count: 0,
                    critical: 0,
                    high: 0,
                    medium: 0,
                    low: 0,
                    other: 0,
                    alerts: Vec::new(),
                });
            entry.count += 1;
            match a.severity.to_ascii_lowercase().as_str() {
                "critical" => entry.critical += 1,
                "high" => entry.high += 1,
                "medium" => entry.medium += 1,
                "low" => entry.low += 1,
                _ => entry.other += 1,
            }
            entry.alerts.push(a.number);
        }

        let mut by_path: Vec<PathAlertGroup> = groups_map.into_values().collect();
        by_path.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| b.critical.cmp(&a.critical))
                .then_with(|| b.high.cmp(&a.high))
                .then_with(|| a.path.cmp(&b.path))
        });

        let total = alerts.len();
        Self {
            repo: repo.to_string(),
            state: state.to_string(),
            git_ref,
            severity_filter,
            total,
            alerts,
            by_path,
        }
    }

    /// Human-readable table of alerts plus breakdown grouped by path.
    pub fn render(&self) -> String {
        let ref_suffix = self
            .git_ref
            .as_deref()
            .map(|r| format!(" on {r}"))
            .unwrap_or_default();
        let sev_suffix = self
            .severity_filter
            .as_deref()
            .map(|s| format!(" [severity={s}]"))
            .unwrap_or_default();

        if self.alerts.is_empty() {
            return format!(
                "no {} code-scanning alerts in {}{}{}",
                self.state, self.repo, ref_suffix, sev_suffix
            );
        }

        let mut out = String::new();
        out.push_str(&format!(
            "{} {} code-scanning alert(s) in {}{}{}\n\n",
            self.alerts.len(),
            self.state,
            self.repo,
            ref_suffix,
            sev_suffix
        ));

        let mut table = vec![
            row(&["ALERT", "SEVERITY", "DESCRIPTION", "PATH"], None),
            row(&["-----", "--------", "-----------", "----"], None),
        ];
        for a in &self.alerts {
            let num = format!("#{}", a.number);
            let sev = a.severity.to_uppercase();
            let loc = a.location_display();
            table.push(row(&[&num, &sev, &a.description, &loc], Some(&a.severity)));
        }
        out.push_str(&table.join("\n"));

        if !self.by_path.is_empty() {
            out.push_str("\n\nby path:\n");
            for g in &self.by_path {
                let mut counts = Vec::new();
                if g.critical > 0 {
                    counts.push(colorize_severity(
                        &format!("{} critical", g.critical),
                        "critical",
                    ));
                }
                if g.high > 0 {
                    counts.push(colorize_severity(&format!("{} high", g.high), "high"));
                }
                if g.medium > 0 {
                    counts.push(colorize_severity(&format!("{} medium", g.medium), "medium"));
                }
                if g.low > 0 {
                    counts.push(colorize_severity(&format!("{} low", g.low), "low"));
                }
                if g.other > 0 {
                    counts.push(colorize_severity(&format!("{} other", g.other), "other"));
                }
                let nums: Vec<String> = g.alerts.iter().take(6).map(|n| format!("#{n}")).collect();
                let nums_str = if g.alerts.len() > 6 {
                    format!("{} +{} more", nums.join(", "), g.alerts.len() - 6)
                } else {
                    nums.join(", ")
                };
                out.push_str(&format!(
                    "  {} ({}: {}) — {}\n",
                    g.path,
                    g.count,
                    counts.join(", "),
                    nums_str
                ));
            }
        }

        out.trim_end().to_string()
    }
}

/// Normalize a branch or ref argument into a full `refs/...` string for the code-scanning API.
pub fn normalize_git_ref(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.starts_with("refs/") {
        trimmed.to_string()
    } else {
        format!("refs/heads/{trimmed}")
    }
}

fn row(cells: &[&str], severity: Option<&str>) -> String {
    let widths = [7, 10, 48];
    let mut parts: Vec<String> = cells
        .iter()
        .zip(widths)
        .enumerate()
        .map(|(idx, (c, w))| {
            let padded = fit(c, w);
            if idx == 1 {
                if let Some(sev) = severity {
                    let trimmed = padded.trim_end();
                    let trailing = &padded[trimmed.len()..];
                    return format!("{}{trailing}", colorize_severity(trimmed, sev));
                }
            }
            padded
        })
        .collect();
    if let Some(last) = cells.get(widths.len()) {
        parts.push((*last).to_string());
    }
    parts.join("  ").trim_end().to_string()
}

/// Apply ANSI color to a severity string (`critical`, `high`, `medium`/`warning`, `low`/`note`).
/// Respects `NO_COLOR` when set.
pub fn colorize_severity(text: &str, severity: &str) -> String {
    if std::env::var_os("NO_COLOR").is_some() {
        return text.to_string();
    }
    let code = match severity.to_ascii_lowercase().as_str() {
        "critical" => "\x1b[1;31m",         // bold bright red
        "high" | "error" => "\x1b[31m",     // red
        "medium" | "warning" => "\x1b[33m", // yellow
        "low" | "note" => "\x1b[36m",       // cyan
        _ => "\x1b[2m",                     // dim
    };
    format!("{code}{text}\x1b[0m")
}

/// Extract `Package`, `Installed Version`, and `Fixed Version` from Trivy-style alert messages.
fn parse_pkg_fields(msg: &str) -> (Option<String>, Option<String>, Option<String>) {
    let mut pkg = None;
    let mut installed = None;
    let mut fixed = None;
    for line in msg.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Package:") {
            let v = rest.trim();
            if !v.is_empty() {
                pkg = Some(v.to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("Installed Version:") {
            let v = rest.trim();
            if !v.is_empty() {
                installed = Some(v.to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("Fixed Version:") {
            let v = rest.trim();
            if !v.is_empty() {
                fixed = Some(v.to_string());
            }
        }
    }
    (pkg, installed, fixed)
}

fn fit(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count > max {
        let head: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    } else {
        let mut out = text.to_string();
        while out.chars().count() < max {
            out.push(' ');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_alert_json(
        number: u64,
        sev: &str,
        desc: &str,
        path: &str,
        line: Option<u64>,
    ) -> Value {
        json!({
            "number": number,
            "state": "open",
            "html_url": format!("https://github.com/thiagobraga/planner/security/code-scanning/{number}"),
            "created_at": "2026-10-01T00:00:00Z",
            "rule": {
                "id": format!("CVE-2026-{number}"),
                "severity": "error",
                "security_severity_level": sev,
                "description": desc,
                "help": "Upgrade the affected package.",
                "tags": ["security", "cve"]
            },
            "tool": {
                "name": "Trivy",
                "version": "0.56.0"
            },
            "most_recent_instance": {
                "ref": "refs/heads/main",
                "message": {
                    "text": desc
                },
                "location": {
                    "path": path,
                    "start_line": line.unwrap_or(0),
                    "end_line": line.unwrap_or(0)
                }
            }
        })
    }

    #[test]
    fn parses_alert_and_renders_detail() {
        let raw = sample_alert_json(
            191,
            "critical",
            "stdlib: crypto/tls vulnerability",
            "usr/local/bin/gosu",
            None,
        );
        let alert = CodeScanningAlert::from_json(&raw).expect("valid alert");
        assert_eq!(alert.number, 191);
        assert_eq!(alert.severity, "critical");
        assert_eq!(alert.path, "usr/local/bin/gosu");
        assert_eq!(alert.location_display(), "usr/local/bin/gosu");

        let detail = alert.render_detail("thiagobraga/planner");
        assert!(detail.contains("alert #191 in thiagobraga/planner (open)"));
        assert!(detail.contains(&format!(
            "severity    : {} (rule: error)",
            colorize_severity("critical", "critical")
        )));
        assert!(detail.contains("location    : usr/local/bin/gosu"));
        assert!(detail.contains("Upgrade the affected package."));
    }

    #[test]
    fn builds_report_groups_by_path_and_filters_severity() {
        let alerts = vec![
            CodeScanningAlert::from_json(&sample_alert_json(
                191,
                "critical",
                "crypto/tls CVE",
                "usr/local/bin/gosu",
                None,
            ))
            .unwrap(),
            CodeScanningAlert::from_json(&sample_alert_json(
                192,
                "high",
                "net/http CVE",
                "usr/local/bin/gosu",
                None,
            ))
            .unwrap(),
            CodeScanningAlert::from_json(&sample_alert_json(
                216,
                "low",
                "Superfluous trailing arguments",
                "app/src/components/ui/ContextMenu.tsx",
                Some(42),
            ))
            .unwrap(),
        ];

        let report = AlertsReport::build(
            "thiagobraga/planner",
            "open",
            Some("refs/heads/main".into()),
            None,
            alerts.clone(),
        );
        assert_eq!(report.total, 3);
        assert_eq!(report.by_path.len(), 2);
        assert_eq!(report.by_path[0].path, "usr/local/bin/gosu");
        assert_eq!(report.by_path[0].count, 2);
        assert_eq!(report.by_path[0].critical, 1);
        assert_eq!(report.by_path[0].high, 1);

        let rendered = report.render();
        assert!(
            rendered.contains(
                "3 open code-scanning alert(s) in thiagobraga/planner on refs/heads/main"
            )
        );
        assert!(rendered.contains("#191"));
        assert!(rendered.contains(&colorize_severity("CRITICAL", "critical")));
        assert!(rendered.contains("app/src/components/ui/ContextMenu.tsx:42"));
        assert!(rendered.contains(&format!(
            "usr/local/bin/gosu (2: {}, {}) — #191, #192",
            colorize_severity("1 critical", "critical"),
            colorize_severity("1 high", "high")
        )));

        let crit_only = AlertsReport::build(
            "thiagobraga/planner",
            "open",
            Some("refs/heads/main".into()),
            Some("critical".into()),
            alerts,
        );
        assert_eq!(crit_only.total, 1);
        assert_eq!(crit_only.alerts[0].number, 191);
    }

    #[test]
    fn normalizes_git_ref() {
        assert_eq!(normalize_git_ref("main"), "refs/heads/main");
        assert_eq!(normalize_git_ref("refs/heads/main"), "refs/heads/main");
        assert_eq!(normalize_git_ref("refs/tags/v1.0.0"), "refs/tags/v1.0.0");
    }
}
