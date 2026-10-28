use serde::Serialize;

/// Author login dependabot uses on pull requests.
pub const DEPENDABOT_LOGIN: &str = "dependabot[bot]";

/// A single dependency bump parsed from a PR title such as
/// `Bump @types/node from 22.10.2 to 22.10.3 in /packages/site`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DepBump {
    pub name: String,
    pub from: String,
    pub to: String,
    /// Manifest path reported by dependabot, when present after `in /...`.
    pub path: Option<String>,
}

/// What a dependabot pull request is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Update {
    /// A single dependency bump we understand well enough to batch.
    Bump(DepBump),
    /// Grouped updates (e.g. multiple dependencies bumped together in one PR).
    Group { title: String, bumps: Vec<DepBump> },
    /// Anything else (grouped updates without parsed bumps, unusual titles).
    Other { title: String },
}

/// One changed file inside a dependabot PR, with its unified diff hunks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrFile {
    pub filename: String,
    pub hunks: Vec<Hunk>,
    /// Raw unified diff of the file as returned by the API; `None` for
    /// binary files, deletions and renames.
    pub patch: Option<String>,
}

/// Location and length of one hunk on the base and head side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Hunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
}

/// A parsed dependabot pull request.
#[derive(Debug, Clone, Serialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub update: Update,
    pub files: Vec<PrFile>,
    /// Base commit the PR was created against, when the API provided it.
    pub base_sha: Option<String>,
}

impl PullRequest {
    pub fn new(number: u64, title: &str, author: &str, files: Vec<PrFile>) -> Self {
        let update = match parse_update(title) {
            Update::Bump(bump) => Update::Bump(bump),
            _ => {
                // Inspect files for package.json updates
                let mut manifest_bumps = Vec::new();
                for f in &files {
                    if let Some(patch) = &f.patch {
                        if f.filename.ends_with("package.json") {
                            manifest_bumps.extend(crate::manifest::extract_package_json_bumps(
                                &f.filename,
                                patch,
                            ));
                        }
                    }
                }
                if manifest_bumps.len() == 1 {
                    Update::Bump(manifest_bumps.remove(0))
                } else if !manifest_bumps.is_empty() {
                    Update::Group {
                        title: title.to_string(),
                        bumps: manifest_bumps,
                    }
                } else if is_group_title(title) {
                    Update::Group {
                        title: title.to_string(),
                        bumps: Vec::new(),
                    }
                } else {
                    Update::Other {
                        title: title.to_string(),
                    }
                }
            }
        };

        Self {
            number,
            title: title.to_string(),
            author: author.to_string(),
            update,
            files,
            base_sha: None,
        }
    }

    /// All dependency bumps inside this pull request.
    pub fn bumps(&self) -> Vec<&DepBump> {
        match &self.update {
            Update::Bump(b) => vec![b],
            Update::Group { bumps, .. } => bumps.iter().collect(),
            Update::Other { .. } => Vec::new(),
        }
    }

    /// Short rendering for reports: `name 1.2.3 -> 1.2.4` or a trimmed title.
    pub fn describe(&self) -> String {
        match &self.update {
            Update::Bump(b) => format!("{} {} -> {}", b.name, b.from, b.to),
            Update::Group { title, bumps } => {
                if bumps.is_empty() {
                    truncate_title(title, 40)
                } else if bumps.len() == 1 {
                    format!("{} {} -> {}", bumps[0].name, bumps[0].from, bumps[0].to)
                } else {
                    let names: Vec<&str> = bumps.iter().map(|b| b.name.as_str()).collect();
                    let sample = if names.len() <= 2 {
                        names.join(", ")
                    } else {
                        format!("{}, {} +{}", names[0], names[1], names.len() - 2)
                    };
                    format!("{} updates ({sample})", bumps.len())
                }
            }
            Update::Other { title } => truncate_title(title, 40),
        }
    }
}

fn truncate_title(title: &str, max_len: usize) -> String {
    let chars: Vec<char> = title.chars().collect();
    if chars.len() > max_len {
        let head: String = chars.iter().take(max_len - 3).collect();
        format!("{head}...")
    } else {
        title.to_string()
    }
}

/// Classify a dependabot PR by its title.
///
/// Handles shapes dependabot produces, including conventional commit prefixes:
/// - `Bump vue from 3.4.0 to 3.5.0`
/// - `chore(deps): bump qs from 6.16.0 to 6.17.0 in /api in the security group...`
/// - `Bump @types/node from 22.10.2 to 22.10.3 in /packages/site`
/// - everything else falls back to [`Update::Other`]
pub fn parse_update(title: &str) -> Update {
    let stripped = strip_conventional_prefix(title);
    match strip_bump_keyword(stripped).and_then(parse_bump) {
        Some(bump) => Update::Bump(bump),
        None => Update::Other {
            title: title.to_string(),
        },
    }
}

fn strip_conventional_prefix(title: &str) -> &str {
    let Some((prefix, rest)) = title.split_once(": ") else {
        return title;
    };
    let prefix = prefix.trim();
    let kind = prefix.split('(').next().unwrap_or(prefix);
    if matches!(
        kind.to_ascii_lowercase().as_str(),
        "chore" | "ci" | "fix" | "build" | "perf" | "refactor" | "style" | "test" | "deps"
    ) {
        rest.trim()
    } else {
        title
    }
}

fn strip_bump_keyword(s: &str) -> Option<&str> {
    if let Some(rest) = s.strip_prefix("Bump ") {
        Some(rest)
    } else if let Some(rest) = s.strip_prefix("bump ") {
        Some(rest)
    } else {
        None
    }
}

fn is_group_title(title: &str) -> bool {
    let s = strip_conventional_prefix(title);
    let s = strip_bump_keyword(s).unwrap_or(s);
    s.starts_with("the ") && s.contains(" group")
}

fn parse_bump(rest: &str) -> Option<DepBump> {
    let (name, tail) = rest.split_once(" from ")?;
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let (from, to_and_path) = tail.split_once(" to ")?;
    if from.is_empty() || from.contains(char::is_whitespace) {
        return None;
    }
    let (to, path) = parse_to_and_path(to_and_path);
    if to.is_empty() || to.contains(char::is_whitespace) {
        return None;
    }
    Some(DepBump {
        name: name.to_string(),
        from: from.to_string(),
        to,
        path,
    })
}

fn parse_to_and_path(s: &str) -> (String, Option<String>) {
    let s = s.trim();
    if let Some((to_part, after_in)) = s.split_once(" in /") {
        let to = to_part.trim();
        let dir = after_in.split_whitespace().next().unwrap_or(after_in);
        let dir = dir.trim_end_matches('/');
        (to.to_string(), Some(format!("/{dir}")))
    } else {
        let to = s
            .split_once(" in the ")
            .map(|(t, _)| t)
            .or_else(|| s.split_once(" across ").map(|(t, _)| t))
            .unwrap_or(s)
            .trim();
        (to.to_string(), None)
    }
}

/// Extract hunk positions from one unified diff patch body.
pub fn parse_hunks_from_patch(patch: &str) -> Vec<Hunk> {
    let mut out = Vec::new();
    for line in patch.lines() {
        let Some(rest) = line.strip_prefix("@@ ") else {
            continue;
        };
        let Some((lhs, _)) = rest.split_once(" @@") else {
            continue;
        };
        let mut sides = lhs.split_whitespace();
        let (Some(old), Some(new)) = (sides.next(), sides.next()) else {
            continue;
        };
        let (old_start, old_len) = parse_range(old);
        let (new_start, new_len) = parse_range(new);
        out.push(Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
        });
    }
    out
}

fn parse_range(token: &str) -> (usize, usize) {
    let token = &token[1..]; // drop leading - or +
    match token.split_once(',') {
        Some((start, len)) => (start.parse().unwrap_or(0), len.parse().unwrap_or(0)),
        None => (token.parse().unwrap_or(0), 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_bump() {
        let u = parse_update("Bump vue from 3.4.0 to 3.5.0");
        assert_eq!(
            u,
            Update::Bump(DepBump {
                name: "vue".into(),
                from: "3.4.0".into(),
                to: "3.5.0".into(),
                path: None,
            })
        );
    }

    #[test]
    fn parses_bump_with_path() {
        let u = parse_update("Bump @types/node from 22.10.2 to 22.10.3 in /packages/site");
        assert_eq!(
            u,
            Update::Bump(DepBump {
                name: "@types/node".into(),
                from: "22.10.2".into(),
                to: "22.10.3".into(),
                path: Some("/packages/site".into()),
            })
        );
    }

    #[test]
    fn parses_actions_bump() {
        let u = parse_update("Bump actions/checkout from 4 to 5");
        assert_eq!(
            u,
            Update::Bump(DepBump {
                name: "actions/checkout".into(),
                from: "4".into(),
                to: "5".into(),
                path: None,
            })
        );
    }

    #[test]
    fn grouped_title_is_other() {
        let u = parse_update("Bump the npm_and_yarn group across 1 directory with 3 updates");
        assert_eq!(
            u,
            Update::Other {
                title: "Bump the npm_and_yarn group across 1 directory with 3 updates".into()
            }
        );
    }

    #[test]
    fn non_bump_title_is_other() {
        assert_eq!(
            parse_update("Fix typo"),
            Update::Other {
                title: "Fix typo".into()
            }
        );
    }

    #[test]
    fn parses_conventional_commits_bump() {
        let u = parse_update(
            "chore(deps): bump qs from 6.16.0 to 6.17.0 in /api in the security group across 1 directory",
        );
        assert_eq!(
            u,
            Update::Bump(DepBump {
                name: "qs".into(),
                from: "6.16.0".into(),
                to: "6.17.0".into(),
                path: Some("/api".into()),
            })
        );

        let u2 = parse_update(
            "chore(deps-dev): bump happy-dom from 20.14.6 to 21.0.3 in /site in the minor-and-patch group across 1 directory",
        );
        assert_eq!(
            u2,
            Update::Bump(DepBump {
                name: "happy-dom".into(),
                from: "20.14.6".into(),
                to: "21.0.3".into(),
                path: Some("/site".into()),
            })
        );
    }

    #[test]
    fn pull_request_extracts_manifest_bumps_when_grouped() {
        let diff = r#"
--- a/app/package.json
+++ b/app/package.json
@@ -10,2 +10,2 @@
-    "react": "19.3.0",
+    "react": "19.3.1",
-    "vite": "8.3.4",
+    "vite": "8.3.5",
"#;
        let files = vec![PrFile {
            filename: "app/package.json".to_string(),
            hunks: vec![],
            patch: Some(diff.to_string()),
        }];
        let pr = PullRequest::new(
            368,
            "chore(deps): bump the minor-and-patch group across 1 directory with 2 updates",
            DEPENDABOT_LOGIN,
            files,
        );
        assert_eq!(pr.bumps().len(), 2);
        assert_eq!(pr.bumps()[0].name, "react");
        assert_eq!(pr.bumps()[1].name, "vite");
        assert_eq!(pr.describe(), "2 updates (react, vite)");
    }

    #[test]
    fn describe_trims_long_titles() {
        let pr = PullRequest::new(
            1,
            "Bump the production-dependencies group across 1 directory with 7 updates",
            DEPENDABOT_LOGIN,
            Vec::new(),
        );
        assert_eq!(pr.describe().chars().count(), 40);
    }

    #[test]
    fn parses_hunks() {
        let patch = "diff --git a/package.json b/package.json\n@@ -12,7 +12,9 @@\n context\n+added\n context\n@@ -80,3 +82,1 @@\n-context\n";
        let hunks = parse_hunks_from_patch(patch);
        assert_eq!(
            hunks,
            vec![
                Hunk {
                    old_start: 12,
                    old_len: 7,
                    new_start: 12,
                    new_len: 9
                },
                Hunk {
                    old_start: 80,
                    old_len: 3,
                    new_start: 82,
                    new_len: 1
                },
            ]
        );
    }

    #[test]
    fn parses_hunks_without_lengths() {
        let hunk = parse_hunks_from_patch("@@ -33 +33 @@\n").pop().unwrap();
        assert_eq!(
            hunk,
            Hunk {
                old_start: 33,
                old_len: 1,
                new_start: 33,
                new_len: 1
            }
        );
    }
}
