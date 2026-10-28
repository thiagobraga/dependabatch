use crate::conflict;
use crate::pr::{PrFile, PullRequest, Update};
use serde::Serialize;

/// What kind of file a dependabot PR touches, and whether we can batch it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileKind {
    /// `package.json` anywhere in the tree.
    NpmManifest,
    /// `package-lock.json`, regenerated with npm inside docker.
    NpmLock,
    /// GitHub Actions workflow under `.github/workflows/`.
    Workflow,
    /// Dockerfile or docker compose configuration.
    Docker,
    /// Lockfile kinds whose regen is not implemented yet.
    PendingLock(String),
    /// Anything else (gradle, terraform, binlockless...).
    Other,
}

pub fn classify_file(filename: &str) -> FileKind {
    let base = filename.rsplit('/').next().unwrap_or(filename);
    match base {
        "package.json" => FileKind::NpmManifest,
        "package-lock.json" => FileKind::NpmLock,
        "yarn.lock" | "pnpm-lock.yaml" | "pnpm-lock.yml" | "bun.lock" | "bun.lockb" => {
            FileKind::PendingLock(base.to_string())
        }
        _ if base == "Dockerfile"
            || base.starts_with("Dockerfile.")
            || base.ends_with(".dockerfile")
            || base == "compose.yml"
            || base == "compose.yaml"
            || base == "docker-compose.yml"
            || base == "docker-compose.yaml"
            || (base.starts_with("compose.")
                && (base.ends_with(".yml") || base.ends_with(".yaml"))) =>
        {
            FileKind::Docker
        }
        _ if filename.starts_with(".github/workflows/")
            && (base.ends_with(".yml") || base.ends_with(".yaml")) =>
        {
            FileKind::Workflow
        }
        _ => FileKind::Other,
    }
}

/// Lockfiles and semantic manifests are merged/regenerated, so overlap inside them is fine.
fn manifest_or_lockfile_skipped(f: &PrFile) -> bool {
    matches!(
        classify_file(&f.filename),
        FileKind::NpmLock | FileKind::NpmManifest
    )
}

/// Why a PR stayed out of the batch.
#[derive(Debug, Serialize)]
pub struct Excluded {
    pub number: u64,
    pub reason: String,
}

/// How the lockfiles get refreshed for the batch branch.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegenPlan {
    /// Run npm inside a docker container to rebuild `package-lock.json`.
    Npm,
    /// Manifest edits only; the repo has no npm lockfile to regenerate.
    ManifestOnly,
    /// User override: `--no-regen` skips the lockfile step.
    Skipped,
    /// Nothing to regenerate (Actions-only batches, for example).
    None,
}

/// The batch plan: what goes in, what stays out, how files get rebuilt.
#[derive(Debug, Serialize)]
pub struct Plan {
    pub included: Vec<PullRequest>,
    pub excluded: Vec<Excluded>,
    pub regen: RegenPlan,
}

/// Build the plan from a full scan of open dependabot PRs.
///
/// Selection rules, in order:
/// 1. single-dependency bumps only, with creatable file types,
/// 2. duplicates collapse to the PR with the highest target version,
/// 3. conflicting PRs are dropped, ignoring lockfile-only overlaps,
/// 4. `--exclude` names are filtered out by the CLI layer.
pub fn select(prs: &[PullRequest], repo_has_npm_lock: bool) -> Plan {
    let mut included: Vec<PullRequest> = Vec::new();
    let mut excluded: Vec<Excluded> = Vec::new();

    let mut exclude = |pr: &PullRequest, reason: String| {
        excluded.push(Excluded {
            number: pr.number,
            reason,
        })
    };

    // 1: shape and file types. Lockfile patch data may be missing entirely
    // (the API omits huge diffs); lockfiles are regenerated, not applied.
    for pr in prs {
        match &pr.update {
            Update::Bump(_) | Update::Group { .. } => {}
            Update::Other { .. } => {
                exclude(pr, "unsupported update format".into());
                continue;
            }
        }
        let mut reject: Option<String> = None;
        for file in &pr.files {
            let kind = classify_file(&file.filename);
            match kind {
                FileKind::NpmManifest
                | FileKind::NpmLock
                | FileKind::Workflow
                | FileKind::Docker => {}
                FileKind::PendingLock(name) => {
                    reject = Some(format!("lockfile `{name}` support is pending"));
                    break;
                }
                FileKind::Other => {
                    reject = Some(format!("unsupported file `{}`", file.filename));
                    break;
                }
            }
            if kind != FileKind::NpmLock && file.patch.is_none() {
                reject = Some(format!(
                    "no patch data for `{}` (binary, deletion or rename)",
                    file.filename
                ));
                break;
            }
        }
        match reject {
            Some(reason) => exclude(pr, reason),
            None => included.push(pr.clone()),
        }
    }

    // 2: duplicates, same dep in same manifest
    let mut by_key: std::collections::BTreeMap<(&str, &Option<String>), Vec<&PullRequest>> =
        std::collections::BTreeMap::new();
    for pr in &included {
        for b in pr.bumps() {
            by_key
                .entry((b.name.as_str(), &b.path))
                .or_default()
                .push(pr);
        }
    }
    let mut keep: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut drop_to: std::collections::BTreeMap<u64, u64> = std::collections::BTreeMap::new();
    for ((dep_name, _), group) in by_key {
        if group.len() == 1 {
            keep.insert(group[0].number);
            continue;
        }
        let target_ver = |pr: &PullRequest| -> Option<String> {
            pr.bumps()
                .into_iter()
                .find(|b| b.name == dep_name)
                .map(|b| b.to.clone())
        };
        let mut ranked: Vec<&PullRequest> = group.clone();
        ranked.sort_by(|a, b| {
            version_key(target_ver(b).as_deref()).cmp(&version_key(target_ver(a).as_deref()))
        });
        let survivor = ranked[0].number;
        keep.insert(survivor);
        for loser in &ranked[1..] {
            if loser.bumps().len() <= 1 {
                drop_to.insert(loser.number, survivor);
            }
        }
    }
    included.retain(|pr| !drop_to.contains_key(&pr.number));
    for (loser, survivor) in drop_to {
        excluded.push(Excluded {
            number: loser,
            reason: format!("duplicate of #{survivor} (same dependency)"),
        });
    }

    // 3: conflicts among survivors, ignoring regenerated lockfiles and semantic manifests
    let conflicts = conflict::conflicts_while_skipping(&included, &manifest_or_lockfile_skipped);
    if !conflicts.is_empty() {
        let conflicted = conflict::conflicted_numbers(&conflicts);
        included.retain(|pr| !conflicted.contains(&pr.number));
        let pairs: std::collections::BTreeMap<u64, Vec<u64>> =
            conflicts
                .iter()
                .fold(std::collections::BTreeMap::new(), |mut m, (a, b)| {
                    m.entry(*a).or_default().push(*b);
                    m.entry(*b).or_default().push(*a);
                    m
                });
        for pr in prs {
            if let Some(others) = pairs.get(&pr.number) {
                excluded.push(Excluded {
                    number: pr.number,
                    reason: format!(
                        "conflicts with other PRs ({})",
                        others
                            .iter()
                            .map(|n| format!("#{n}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ),
                });
            }
        }
    }

    // 4: regen plan
    let touches_lock = included.iter().any(|pr| {
        pr.files
            .iter()
            .any(|f| classify_file(&f.filename) == FileKind::NpmLock)
    });
    let touches_manifest = included.iter().any(|pr| {
        pr.files
            .iter()
            .any(|f| classify_file(&f.filename) == FileKind::NpmManifest)
    });
    let regen = if touches_lock || (touches_manifest && repo_has_npm_lock) {
        RegenPlan::Npm
    } else if touches_manifest {
        RegenPlan::ManifestOnly
    } else {
        RegenPlan::None
    };

    included.sort_by_key(|pr| pr.number);
    excluded.sort_by_key(|e| e.number);

    Plan {
        included,
        excluded,
        regen,
    }
}

/// Numeric-aware sort key so `1.10.0` beats `1.9.0`.
fn version_key(version: Option<&str>) -> Vec<u64> {
    match version {
        None => Vec::new(),
        Some(v) => v
            .split('.')
            .map(|chunk| {
                chunk
                    .trim_start_matches('v')
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect(),
    }
}

impl Plan {
    /// Human readable plan summary for the CLI.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        if self.included.is_empty() {
            out.push_str("nothing to batch: every open dependabot PR is excluded\n");
        } else {
            out.push_str(&format!(
                "batch: {} PRs in {} dep(s), regen: {:?}\n",
                self.included.len(),
                count_bumps(&self.included),
                self.regen
            ));
            for pr in &self.included {
                out.push_str(&format!("  in  #{:<4} {}\n", pr.number, pr.describe()));
            }
        }
        for e in &self.excluded {
            out.push_str(&format!("  out #{:<4} {}\n", e.number, e.reason));
        }
        out
    }
}

fn count_bumps(prs: &[PullRequest]) -> usize {
    prs.iter().map(|pr| pr.bumps().len().max(1)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::DEPENDABOT_LOGIN;
    use crate::pr::{DepBump, Hunk};

    fn bump(name: &str, from: &str, to: &str) -> Update {
        Update::Bump(DepBump {
            name: name.into(),
            from: from.into(),
            to: to.into(),
            path: None,
        })
    }

    fn pr(number: u64, update: Update, files: Vec<PrFile>) -> PullRequest {
        PullRequest {
            number,
            title: match &update {
                Update::Bump(b) => format!("Bump {} from {} to {}", b.name, b.from, b.to),
                Update::Group { title, .. } => title.clone(),
                Update::Other { title } => title.clone(),
            },
            author: DEPENDABOT_LOGIN.into(),
            update,
            files,
            base_sha: None,
        }
    }

    fn file(filename: &str, old_start: usize, old_len: usize) -> PrFile {
        PrFile {
            filename: filename.into(),
            hunks: vec![Hunk {
                old_start,
                old_len,
                new_start: old_start,
                new_len: old_len,
            }],
            patch: Some("@@ -1,1 +1,1 @@\n".into()),
        }
    }

    #[test]
    fn other_updates_and_bad_files_are_out() {
        let prs = vec![
            pr(
                1,
                Update::Other {
                    title: "Bump the group with 3 updates".into(),
                },
                Vec::new(),
            ),
            pr(
                2,
                bump("gradle", "1", "2"),
                vec![file("build.gradle", 1, 2)],
            ),
            pr(3, bump("yarn", "1", "2"), vec![file("yarn.lock", 1, 2)]),
        ];
        let plan = select(&prs, false);
        assert_eq!(plan.included.len(), 0);
        let reasons: Vec<&str> = plan.excluded.iter().map(|e| e.reason.as_str()).collect();
        assert!(
            reasons
                .iter()
                .any(|r| r.contains("unsupported update format"))
        );
        assert!(reasons.iter().any(|r| r.contains("build.gradle")));
        assert!(reasons.iter().any(|r| r.contains("pending")));
    }

    #[test]
    fn docker_files_are_batched() {
        let prs = vec![
            pr(
                1,
                bump("node", "20", "22"),
                vec![file(".docker/api/Dockerfile", 1, 2)],
            ),
            pr(
                2,
                bump("postgres", "15", "16"),
                vec![file("compose.yml", 1, 2)],
            ),
        ];
        let plan = select(&prs, false);
        assert_eq!(plan.included.len(), 2);
        assert!(plan.excluded.is_empty());
    }

    #[test]
    fn duplicate_keeps_highest_version() {
        let prs = vec![
            pr(
                10,
                bump("vue", "3.4.0", "3.5.0"),
                vec![file("package.json", 1, 2)],
            ),
            pr(
                11,
                bump("vue", "3.4.0", "3.6.0"),
                vec![file("package.json", 1, 2)],
            ),
        ];
        let plan = select(&prs, false);
        assert_eq!(plan.included.len(), 1);
        assert_eq!(plan.included[0].number, 11);
        assert_eq!(plan.excluded.len(), 1);
        assert_eq!(
            plan.excluded[0].reason,
            "duplicate of #11 (same dependency)"
        );
    }

    #[test]
    fn same_dep_in_two_manifests_is_not_duplicate() {
        let mk = |num, path: &str| PullRequest {
            number: num,
            title: "Bump vite from 3.0.0 to 3.1.0".into(),
            author: DEPENDABOT_LOGIN.into(),
            update: Update::Bump(DepBump {
                name: "vite".into(),
                from: "3.0.0".into(),
                to: "3.1.0".into(),
                path: Some(path.into()),
            }),
            files: vec![file(
                format!("{path}/package.json").trim_start_matches('/'),
                1,
                2,
            )],
            base_sha: None,
        };
        let plan = select(&[mk(1, "/app"), mk(2, "/docs")], false);
        assert_eq!(plan.included.len(), 2);
        assert!(plan.excluded.is_empty());
    }

    #[test]
    fn manifest_deps_merge_cleanly_without_text_conflict() {
        let prs = vec![
            pr(
                1,
                bump("vue", "3.4.0", "3.5.0"),
                vec![file("package.json", 10, 30)],
            ),
            pr(
                2,
                bump("axios", "1.6.0", "1.7.0"),
                vec![file("package.json", 15, 5)],
            ),
        ];
        let plan = select(&prs, false);
        assert_eq!(plan.included.len(), 2);
        assert!(plan.excluded.is_empty());
    }

    #[test]
    fn workflow_conflicts_drop_both() {
        let prs = vec![
            pr(
                1,
                bump("actions/checkout", "3", "4"),
                vec![file(".github/workflows/ci.yml", 10, 30)],
            ),
            pr(
                2,
                bump("actions/setup-node", "3", "4"),
                vec![file(".github/workflows/ci.yml", 15, 5)],
            ),
        ];
        let plan = select(&prs, false);
        assert!(plan.included.is_empty());
        assert_eq!(plan.excluded.len(), 2);
    }

    #[test]
    fn lockfile_only_conflicts_are_ignored() {
        let shared_lock = file("package-lock.json", 10, 300);
        let prs = vec![
            pr(
                1,
                bump("vue", "3.4.0", "3.5.0"),
                vec![file("package.json", 10, 4), shared_lock.clone()],
            ),
            pr(
                2,
                bump("axios", "1.6.0", "1.7.0"),
                vec![file("package.json", 60, 4), shared_lock],
            ),
        ];
        let plan = select(&prs, true);
        assert_eq!(plan.included.len(), 2);
        assert_eq!(plan.regen, RegenPlan::Npm);
    }

    #[test]
    fn missing_lockfile_patch_is_fine() {
        let mut lock = file("package-lock.json", 10, 300);
        lock.patch = None;
        let prs = vec![pr(
            1,
            bump("vue", "3.4.0", "3.5.0"),
            vec![file("package.json", 10, 4), lock],
        )];
        let plan = select(&prs, true);
        assert_eq!(plan.included.len(), 1);
        assert_eq!(plan.regen, RegenPlan::Npm);
    }

    #[test]
    fn actions_only_batch_has_no_regen() {
        let prs = vec![pr(
            1,
            bump("actions/checkout", "4", "5"),
            vec![file(".github/workflows/ci.yml", 1, 2)],
        )];
        let plan = select(&prs, true);
        assert_eq!(plan.included.len(), 1);
        assert_eq!(plan.regen, RegenPlan::None);
    }

    #[test]
    fn version_key_is_numeric() {
        assert!(version_key(Some("1.10.0")) > version_key(Some("1.9.0")));
        assert!(version_key(Some("2.0.0")) > version_key(Some("1.99.0")));
        assert!(version_key(None).is_empty());
    }
}
