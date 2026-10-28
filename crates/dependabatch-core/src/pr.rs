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
    /// Anything else (grouped updates, security groups, unusual titles).
    Other { title: String },
}

/// One changed file inside a dependabot PR, with its unified diff hunks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrFile {
    pub filename: String,
    pub hunks: Vec<Hunk>,
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
}

impl PullRequest {
    pub fn new(number: u64, title: &str, author: &str, files: Vec<PrFile>) -> Self {
        Self {
            number,
            title: title.to_string(),
            author: author.to_string(),
            update: parse_update(title),
            files,
        }
    }

    /// Short rendering for reports: `name 1.2.3 -> 1.2.4` or a trimmed title.
    pub fn describe(&self) -> String {
        match &self.update {
            Update::Bump(b) => format!("{} {} -> {}", b.name, b.from, b.to),
            Update::Other { title } => {
                let chars: Vec<char> = title.chars().collect();
                if chars.len() > 40 {
                    let head: String = chars.iter().take(37).collect();
                    format!("{head}...")
                } else {
                    title.clone()
                }
            }
        }
    }
}

/// Classify a dependabot PR by its title.
///
/// Handles the shapes dependabot itself produces:
/// - `Bump vue from 3.4.0 to 3.5.0`
/// - `Bump @types/node from 22.10.2 to 22.10.3 in /packages/site`
/// - everything else falls back to [`Update::Other`]
pub fn parse_update(title: &str) -> Update {
    match title.strip_prefix("Bump ").and_then(parse_bump) {
        Some(bump) => Update::Bump(bump),
        None => Update::Other {
            title: title.to_string(),
        },
    }
}

fn parse_bump(rest: &str) -> Option<DepBump> {
    let (name, tail) = rest.split_once(" from ")?;
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let (tail, path) = match tail.split_once(" in /") {
        Some((t, p)) => (t, Some(format!("/{p}"))),
        None => (tail, None),
    };
    let (from, to) = tail.trim_end().split_once(" to ")?;
    if from.is_empty() || to.is_empty() || from.contains(char::is_whitespace) {
        return None;
    }
    Some(DepBump {
        name: name.to_string(),
        from: from.to_string(),
        to: to.to_string(),
        path,
    })
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
