use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};

/// Propose a newer tag only for the exact image identified by the scanner.
pub(crate) async fn apply(root: &Path, target: &str, category: Option<&str>) -> Result<bool> {
    let Some(image) = target.strip_prefix("library/") else {
        return Ok(false);
    };
    if image.is_empty()
        || !image
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Ok(false);
    }
    let category_tag = match category.and_then(|c| c.strip_prefix("infra-")) {
        Some(value) => match value.split_once(':') {
            Some((name, tag)) if name == image && !tag.is_empty() => Some(tag),
            _ => return Ok(false),
        },
        None => None,
    };
    let mut files = Vec::new();
    collect_files(root, &mut files)?;
    let client = reqwest::Client::builder()
        .user_agent("dependabatch")
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let mut changed = false;
    for file in files {
        let content = fs::read_to_string(&file)?;
        let mut updated = String::new();
        for line in content.split_inclusive('\n') {
            if let Some((start, end, tag)) = image_tag(line, image) {
                if category_tag.is_none_or(|expected| expected == tag) {
                    if let Some(new_tag) = fetch_tag(&client, image, tag).await {
                        updated.push_str(&line[..start]);
                        updated.push_str(&new_tag);
                        updated.push_str(&line[end..]);
                        continue;
                    }
                }
            }
            updated.push_str(line);
        }
        if updated != content {
            fs::write(&file, updated)?;
            changed = true;
        }
    }
    Ok(changed)
}

// Only simple, literal FROM and Compose image entries are safe to rewrite.
fn image_tag<'a>(line: &'a str, image: &str) -> Option<(usize, usize, &'a str)> {
    let trimmed = line.trim_start();
    let rest = trimmed
        .strip_prefix("FROM ")
        .or_else(|| trimmed.strip_prefix("image:"))?
        .trim_start();
    let rest = rest
        .strip_prefix('"')
        .or_else(|| rest.strip_prefix('\''))
        .unwrap_or(rest);
    let reference = rest.strip_prefix(image)?.strip_prefix(':')?;
    let len = reference
        .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '#')
        .unwrap_or(reference.len());
    let tag = &reference[..len];
    if tag.is_empty() || tag.contains('@') || tag.contains('$') {
        return None;
    }
    let start = line.len() - reference.len();
    Some((start, start + len, tag))
}

fn version(tag: &str) -> Option<(Vec<u64>, &str)> {
    let (numbers, suffix) = tag.split_once('-').map_or((tag, ""), |(n, s)| (n, s));
    let parts: Option<Vec<_>> = numbers
        .split('.')
        .map(|p| {
            if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
                None
            } else {
                p.parse().ok()
            }
        })
        .collect();
    Some((parts?, suffix))
}

fn choose_tag<'a>(current: &str, candidates: impl Iterator<Item = &'a str>) -> Option<String> {
    let (old, suffix) = version(current)?;
    candidates
        .filter_map(|tag| {
            let (v, s) = version(tag)?;
            (s == suffix && v.len() == old.len() && v.first() == old.first() && v > old)
                .then_some((v, tag))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, tag)| tag.to_owned())
}

async fn fetch_tag(client: &reqwest::Client, image: &str, current: &str) -> Option<String> {
    version(current)?;
    let url = format!("https://hub.docker.com/v2/repositories/library/{image}/tags");
    let response: serde_json::Value = client
        .get(url)
        .query(&[
            ("page_size", "100"),
            ("name", current.split(['.', '-']).next()?),
        ])
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    let tags = response.get("results")?.as_array()?;
    choose_tag(
        current,
        tags.iter().filter_map(|item| item.get("name")?.as_str()),
    )
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if kind.is_dir() {
            if !matches!(
                name.as_ref(),
                ".git" | "node_modules" | "target" | ".worktrees" | "dist"
            ) {
                collect_files(&entry.path(), files)?;
            }
        } else if kind.is_file()
            && (name == "Dockerfile"
                || name.starts_with("Dockerfile.")
                || name.ends_with(".dockerfile")
                || ((name.starts_with("compose") || name.starts_with("docker-compose"))
                    && (name.ends_with(".yml") || name.ends_with(".yaml"))))
        {
            files.push(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_fix_matches_only_literal_image_references() {
        assert_eq!(
            image_tag("    image: \"nginx:1.31.8-alpine\" # hi\n", "nginx").map(|v| v.2),
            Some("1.31.8-alpine")
        );
        assert_eq!(
            image_tag("FROM nginx:1.31-alpine AS build\n", "nginx").map(|v| v.2),
            Some("1.31-alpine")
        );
        for line in [
            "image: mynginx:1.2",
            "# image: nginx:1.2",
            "image: nginx:1.2@sha256:abc",
            "image: nginx:${TAG}",
        ] {
            assert!(image_tag(line, "nginx").is_none());
        }
    }

    #[test]
    fn container_fix_stays_in_major_variant_and_precision() {
        let tags = [
            "2.0.0-alpine",
            "1.32.1-alpine",
            "1.31.9-alpine",
            "1.33.0-bookworm",
            "1.34-alpine",
            "1.99.0-alpine3.22",
        ];
        assert_eq!(
            choose_tag("1.31.8-alpine", tags.into_iter()),
            Some("1.32.1-alpine".into())
        );
        assert_eq!(
            choose_tag("7-alpine", ["8-alpine", "7.2-alpine"].into_iter()),
            None
        );
        assert_eq!(choose_tag("latest", ["1.0"].into_iter()), None);
    }

    #[tokio::test]
    async fn container_fix_skips_embedded_binaries_without_network() {
        assert!(
            !apply(
                Path::new("/nonexistent"),
                "usr/local/bin/gosu",
                Some("infra-postgres:18-alpine")
            )
            .await
            .unwrap()
        );
        assert!(
            !apply(
                Path::new("/nonexistent"),
                "library/redis",
                Some("infra-postgres:18-alpine")
            )
            .await
            .unwrap()
        );
    }
}
