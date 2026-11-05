use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// True when the checked-out repo contains `package-lock.json`.
pub fn has_npm_lock(dir: &Path) -> bool {
    dir.join("package-lock.json").is_file()
}

/// True when the directory contains `yarn.lock`.
pub fn has_yarn_lock(dir: &Path) -> bool {
    dir.join("yarn.lock").is_file()
}

/// True when the directory holds a package.json plus an npm or yarn lockfile.
fn is_node_lock_dir(dir: &Path) -> bool {
    dir.join("package.json").is_file() && (has_npm_lock(dir) || has_yarn_lock(dir))
}

/// Find all directories within `root` that contain `package.json` and an npm or yarn lockfile.
pub fn find_node_lock_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if is_node_lock_dir(root) {
        dirs.push(root.to_path_buf());
    }
    // Check 1 level of subdirectories (avoiding node_modules, .git, etc.)
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || name == "node_modules" || name == "target" {
                    continue;
                }
                if is_node_lock_dir(&path) {
                    dirs.push(path);
                }
            }
        }
    }
    dirs
}

/// Rebuild every npm / yarn lockfile found in the repo inside docker.
pub fn regenerate_all_node_locks(root: &Path, image: &str) -> Result<()> {
    let dirs = find_node_lock_dirs(root);
    if dirs.is_empty() {
        // Fall back to root if package.json exists
        return regenerate_node_lock(root, image);
    }
    for dir in dirs {
        regenerate_node_lock(&dir, image)?;
    }
    Ok(())
}

/// Pick the package manager for one directory: yarn only when no npm lock exists.
pub fn regenerate_node_lock(dir: &Path, image: &str) -> Result<()> {
    if has_yarn_lock(dir) && !has_npm_lock(dir) {
        regenerate_yarn_lock(dir, image)
    } else {
        regenerate_npm_lock(dir, image)
    }
}

/// Rebuild `package-lock.json` from the edited manifests with npm.
///
/// Runs inside an ephemeral docker container (`--rm`), mounted read-write only
/// while it runs. Leaves the host system untouched.
pub fn regenerate_npm_lock(dir: &Path, image: &str) -> Result<()> {
    run_in_docker(
        dir,
        image,
        "npm install --package-lock-only --ignore-scripts --no-audit --no-fund",
        "npm",
    )?;
    if !has_npm_lock(dir) {
        return Err(anyhow!(
            "npm lockfile regen finished but package-lock.json is missing"
        ));
    }
    Ok(())
}

/// Rebuild `yarn.lock` from the edited manifests with yarn.
///
/// Yarn berry (`__metadata:` header) updates only the lockfile. Yarn classic
/// has no lockfile-only mode, so `node_modules` is removed afterwards to keep
/// it out of the batch commit.
pub fn regenerate_yarn_lock(dir: &Path, image: &str) -> Result<()> {
    let script = if is_yarn_berry(dir) {
        "corepack enable && yarn install --mode update-lockfile"
    } else {
        "corepack enable && yarn install --ignore-scripts --ignore-engines --non-interactive \
         && rm -rf node_modules"
    };
    run_in_docker(dir, image, script, "yarn")?;
    if !has_yarn_lock(dir) {
        return Err(anyhow!(
            "yarn lockfile regen finished but yarn.lock is missing"
        ));
    }
    Ok(())
}

/// Berry lockfiles start with a `__metadata:` block; classic ones do not.
fn is_yarn_berry(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("yarn.lock"))
        .map(|s| s.contains("\n__metadata:") || s.starts_with("__metadata:"))
        .unwrap_or(false)
}

fn run_in_docker(dir: &Path, image: &str, script: &str, tool: &str) -> Result<()> {
    docker_available().context(format!(
        "docker is required for {tool} lockfile regeneration (or run with --no-regen)"
    ))?;
    let name = container_name();
    let mount = format!("{}:/work", dir.display());
    let output = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--name",
            &name,
            "-v",
            &mount,
            "-w",
            "/work",
            "-e",
            "NPM_CONFIG_UPDATE_NOTIFIER=false",
            "-e",
            "YARN_ENABLE_TELEMETRY=0",
            "-e",
            "COREPACK_ENABLE_DOWNLOAD_PROMPT=0",
            image,
            "sh",
            "-c",
            script,
        ])
        .output()
        .context("failed to launch docker")?;
    if !output.status.success() {
        return Err(anyhow!(
            "{tool} lockfile regen failed inside docker:\n{}",
            trim(&output.stderr)
        ));
    }
    Ok(())
}

fn docker_available() -> Result<()> {
    let output = Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output()
        .context("docker binary not found on PATH")?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "docker daemon not reachable; start it or run with --no-regen"
        ))
    }
}

fn container_name() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("dependabatch-regen-{secs}-{}", std::process::id())
}

fn trim(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dbatch-regen-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_yarn_and_npm_dirs() {
        let root = tmp("find");
        std::fs::write(root.join("package.json"), "{}").unwrap();
        std::fs::write(root.join("yarn.lock"), "").unwrap();
        let sub = root.join("web");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("package.json"), "{}").unwrap();
        std::fs::write(sub.join("package-lock.json"), "{}").unwrap();
        let dirs = find_node_lock_dirs(&root);
        assert!(dirs.contains(&root) && dirs.contains(&sub));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn detects_yarn_berry() {
        let root = tmp("berry");
        std::fs::write(
            root.join("yarn.lock"),
            "# comment\n__metadata:\n  version: 8\n",
        )
        .unwrap();
        assert!(is_yarn_berry(&root));
        std::fs::write(root.join("yarn.lock"), "# yarn lockfile v1\n\nfoo@^1:\n").unwrap();
        assert!(!is_yarn_berry(&root));
        std::fs::remove_dir_all(&root).ok();
    }
}
