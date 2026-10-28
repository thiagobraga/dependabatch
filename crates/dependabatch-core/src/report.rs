use serde::Serialize;
use std::collections::BTreeSet;

use crate::conflict;
use crate::pr::{PullRequest, Update};

/// One row of the scan report.
#[derive(Debug, Clone, Serialize)]
pub struct ScanRow {
    pub number: u64,
    pub update: String,
    pub files: usize,
    /// `ready`, `conflict`, `duplicate` or `other`.
    pub status: String,
}

/// Dry-run output of the scan stage: what would be batched and what stays out.
#[derive(Debug, Serialize)]
pub struct ScanReport {
    pub repo: String,
    pub rows: Vec<ScanRow>,
    pub conflicts: Vec<(u64, u64)>,
    pub duplicates: Vec<Vec<u64>>,
}

impl ScanReport {
    pub fn build(repo: &str, prs: &[PullRequest]) -> Self {
        let conflicts = conflict::conflicts_within(prs);
        let duplicates = conflict::duplicate_groups(prs);
        let conflicted = conflict::conflicted_numbers(&conflicts);
        let duplicated: BTreeSet<u64> = duplicates.iter().flatten().copied().collect();

        let rows = prs
            .iter()
            .map(|pr| ScanRow {
                number: pr.number,
                update: pr.describe(),
                files: pr.files.len(),
                status: match &pr.update {
                    Update::Bump(_) if conflicted.contains(&pr.number) => "conflict".into(),
                    Update::Bump(_) if duplicated.contains(&pr.number) => "duplicate".into(),
                    Update::Bump(_) => "ready".into(),
                    Update::Other { .. } => "other".into(),
                },
            })
            .collect();

        Self {
            repo: repo.to_string(),
            rows,
            conflicts,
            duplicates,
        }
    }

    /// Human readable report table.
    pub fn render(&self) -> String {
        if self.rows.is_empty() {
            return format!("no open dependabot PRs in {}", self.repo);
        }
        let mut out = String::new();
        out.push_str(&format!(
            "{} open dependabot PRs in {}\n\n",
            self.rows.len(),
            self.repo
        ));
        let mut table = vec![
            row(&["PR", "UPDATE", "FILES", "STATUS"]),
            row(&["--", "------", "-----", "------"]),
        ];
        for r in &self.rows {
            table.push(row(&[
                &r.number.to_string(),
                &r.update,
                &r.files.to_string(),
                &r.status.to_uppercase(),
            ]));
        }
        out.push_str(&table.join("\n"));

        if !self.duplicates.is_empty() {
            out.push_str("\n\nduplicate dep (newest wins in batch):\n");
            for group in &self.duplicates {
                out.push_str(&format!(
                    "  {} {}\n",
                    group
                        .iter()
                        .map(|n| format!("#{n}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    self.row_update(group[0]).unwrap_or_default(),
                ));
            }
        }
        if !self.conflicts.is_empty() {
            out.push_str("\n\nconflicting pairs (kept out of Batch):\n");
            for (a, b) in &self.conflicts {
                out.push_str(&format!("  #{a} <-> #{b}\n"));
            }
        }
        out
    }

    fn row_update(&self, number: u64) -> Option<&str> {
        self.rows
            .iter()
            .find(|r| r.number == number)
            .map(|r| r.update.as_str())
    }
}

fn row(cells: &[&str]) -> String {
    let widths = [6, 44, 5, 10];
    cells
        .iter()
        .zip(widths)
        .map(|(c, w)| fit(c, w))
        .collect::<Vec<_>>()
        .join("  ")
        .trim_end()
        .to_string()
}

fn fit(text: &str, max: usize) -> String {
    let take = if text.chars().count() > max {
        max.saturating_sub(1)
    } else {
        text.chars().count()
    };
    let left: String = text.chars().take(take).collect();
    let mut out = left;
    while out.chars().count() < max {
        out.push(' ');
    }
    out
}
