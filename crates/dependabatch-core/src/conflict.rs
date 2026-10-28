use std::collections::{BTreeMap, BTreeSet};

use crate::pr::{Hunk, PrFile, PullRequest};

/// Pairs of open dependabot PRs whose patches cannot be applied together.
///
/// Two PRs conflict when they touch the same file with overlapping base-side
/// hunk ranges. Lockfile hunks normally target distinct regions, so plain
/// pairwise range checks are enough for a scan report; the real merge strategy
/// lives in the batch stage.
pub fn conflicts_within(prs: &[PullRequest]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for (i, a) in prs.iter().enumerate() {
        for b in prs.iter().skip(i + 1) {
            if files_conflict(
                &a.files.iter().collect::<Vec<_>>(),
                &b.files.iter().collect::<Vec<_>>(),
            ) {
                out.push((a.number, b.number));
            }
        }
    }
    out
}

/// Same as [`conflicts_within`], but files rejected by `skip` do not
/// participate. Used in batch planning to ignore overlap inside lockfiles that
/// dependabatch regenerates from manifests anyway.
pub fn conflicts_while_skipping(
    prs: &[PullRequest],
    skip: &dyn Fn(&PrFile) -> bool,
) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for (i, a) in prs.iter().enumerate() {
        for b in prs.iter().skip(i + 1) {
            let fa: Vec<_> = a.files.iter().filter(|f| !skip(f)).collect();
            let fb: Vec<_> = b.files.iter().filter(|f| !skip(f)).collect();
            if files_conflict(&fa, &fb) {
                out.push((a.number, b.number));
            }
        }
    }
    out
}

/// Groups of PRs bumping the same dependency in the same manifest; the
/// survivor keeps the highest target version.
pub fn duplicate_groups(prs: &[PullRequest]) -> Vec<Vec<u64>> {
    let mut by_key: BTreeMap<(&str, &Option<String>), BTreeSet<u64>> = BTreeMap::new();
    for pr in prs {
        if let crate::pr::Update::Bump(bump) = &pr.update {
            by_key
                .entry((bump.name.as_str(), &bump.path))
                .or_default()
                .insert(pr.number);
        }
    }
    by_key
        .into_values()
        .filter(|g| g.len() > 1)
        .map(|g| g.into_iter().collect())
        .collect()
}

/// Numbers that conflict with at least one other PR.
pub fn conflicted_numbers(conflicts: &[(u64, u64)]) -> BTreeSet<u64> {
    let mut set = BTreeSet::new();
    for (a, b) in conflicts {
        set.insert(*a);
        set.insert(*b);
    }
    set
}

fn files_conflict(a: &[&PrFile], b: &[&PrFile]) -> bool {
    a.iter().any(|fa| {
        b.iter()
            .any(|fb| fa.filename == fb.filename && same_file_conflict(fa, fb))
    })
}

fn same_file_conflict(a: &PrFile, b: &PrFile) -> bool {
    a.hunks
        .iter()
        .any(|&x| b.hunks.iter().any(|&y| hunks_conflict(x, y)))
}

fn hunks_conflict(x: Hunk, y: Hunk) -> bool {
    ranges_overlap(x.old_start, x.old_len, y.old_start, y.old_len)
}

fn ranges_overlap(s1: usize, l1: usize, s2: usize, l2: usize) -> bool {
    let e1 = s1 + l1.max(1);
    let e2 = s2 + l2.max(1);
    s1 < e2 && s2 < e1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::DEPENDABOT_LOGIN;

    fn file(filename: &str, hunks: &[(usize, usize)]) -> PrFile {
        PrFile {
            filename: filename.to_string(),
            hunks: hunks
                .iter()
                .map(|&(s, l)| Hunk {
                    old_start: s,
                    old_len: l,
                    new_start: s,
                    new_len: l,
                })
                .collect(),
            patch: None,
        }
    }

    fn pr(number: u64, title: &str, files: Vec<PrFile>) -> PullRequest {
        PullRequest::new(number, title, DEPENDABOT_LOGIN, files)
    }

    #[test]
    fn same_region_conflicts() {
        let prs = vec![
            pr(
                1,
                "Bump vue from 3.4.0 to 3.5.0",
                vec![file("package.json", &[(10, 5)])],
            ),
            pr(
                2,
                "Bump axios from 1.6.0 to 1.7.0",
                vec![file("package.json", &[(12, 5)])],
            ),
        ];
        assert_eq!(conflicts_within(&prs), vec![(1, 2)]);
    }

    #[test]
    fn distinct_regions_merge_cleanly() {
        let prs = vec![
            pr(
                1,
                "Bump vue from 3.4.0 to 3.5.0",
                vec![file("package.json", &[(10, 5)])],
            ),
            pr(
                2,
                "Bump axios from 1.6.0 to 1.7.0",
                vec![file("package.json", &[(100, 5)])],
            ),
        ];
        assert!(conflicts_within(&prs).is_empty());
    }

    #[test]
    fn different_files_never_conflict() {
        let prs = vec![
            pr(
                1,
                "Bump vue from 3.4.0 to 3.5.0",
                vec![file("a/package.json", &[(10, 5)])],
            ),
            pr(
                2,
                "Bump axios from 1.6.0 to 1.7.0",
                vec![file("b/package.json", &[(10, 5)])],
            ),
        ];
        assert!(conflicts_within(&prs).is_empty());
    }

    #[test]
    fn three_way_conflicts_report_all_pairs() {
        let prs = vec![
            pr(
                1,
                "Bump vue from 3.4.0 to 3.5.0",
                vec![file("package.json", &[(10, 30)])],
            ),
            pr(
                2,
                "Bump axios from 1.6.0 to 1.7.0",
                vec![file("package.json", &[(15, 5)])],
            ),
            pr(
                3,
                "Bump zod from 3.0.0 to 4.0.0",
                vec![file("package.json", &[(20, 5)])],
            ),
        ];
        assert_eq!(conflicts_within(&prs), vec![(1, 2), (1, 3)]);
    }

    #[test]
    fn same_dep_groups_as_duplicate() {
        let prs = vec![
            pr(11, "Bump vue from 3.4.0 to 3.5.0", Vec::new()),
            pr(12, "Bump vue from 3.4.0 to 3.6.0", Vec::new()),
            pr(13, "Bump axios from 1.6.0 to 1.7.0", Vec::new()),
        ];
        assert_eq!(duplicate_groups(&prs), vec![vec![11, 12]]);
    }

    #[test]
    fn lockfile_overlap_can_be_skipped() {
        let prs = vec![
            pr(
                1,
                "Bump vue from 3.4.0 to 3.5.0",
                vec![file("package-lock.json", &[(10, 5)])],
            ),
            pr(
                2,
                "Bump axios from 1.6.0 to 1.7.0",
                vec![file("package-lock.json", &[(10, 5)])],
            ),
        ];
        assert_eq!(conflicts_within(&prs), vec![(1, 2)]);
        assert!(conflicts_while_skipping(&prs, &|f| f.filename == "package-lock.json").is_empty());
    }
}
