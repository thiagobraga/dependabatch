use anyhow::{Context, Result, anyhow, ensure};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// A package manager, the lockfile it owns and the script that rebuilds it.
struct Tool {
    name: &'static str,
    lock: &'static str,
    script: &'static str,
}

const NPM: Tool = Tool {
    name: "npm",
    lock: "package-lock.json",
    script: "npm install --package-lock-only --ignore-scripts --no-audit --no-fund",
};

/// Berry updates only the lockfile.
const YARN_BERRY: Tool = Tool {
    name: "yarn",
    lock: "yarn.lock",
    script: "corepack enable && yarn install --mode update-lockfile",
};

/// Classic has no lockfile-only mode, so `node_modules` is removed afterwards
/// to keep it out of the batch commit.
const YARN_CLASSIC: Tool = Tool {
    name: "yarn",
    lock: "yarn.lock",
    script: "corepack enable && yarn install --ignore-scripts --ignore-engines \
             --non-interactive && rm -rf node_modules",
};

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

/// Find `root` and its direct subdirectories (minus hidden, node_modules and
/// target) that hold `package.json` and an npm or yarn lockfile.
pub fn find_node_lock_dirs(root: &Path) -> Vec<PathBuf> {
    let subdirs = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            !(name.starts_with('.') || name == "node_modules" || name == "target")
        })
        .map(|e| e.path())
        .filter(|p| p.is_dir());
    std::iter::once(root.to_path_buf())
        .chain(subdirs)
        .filter(|dir| is_node_lock_dir(dir))
        .collect()
}

/// Rebuild every npm / yarn lockfile found in the repo inside docker.
/// Falls back to the root when no lock directory is found.
pub fn regenerate_all_node_locks(root: &Path, image: &str) -> Result<()> {
    Some(find_node_lock_dirs(root))
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| vec![root.to_path_buf()])
        .iter()
        .try_for_each(|dir| regenerate_node_lock(dir, image))
}

/// Yarn only when `yarn.lock` is the sole lockfile; npm otherwise.
fn tool_for(dir: &Path) -> &'static Tool {
    match (has_npm_lock(dir), has_yarn_lock(dir)) {
        (false, true) if is_yarn_berry(dir) => &YARN_BERRY,
        (false, true) => &YARN_CLASSIC,
        _ => &NPM,
    }
}

/// Rebuild the lockfile of one directory from its edited manifests.
///
/// Runs inside an ephemeral docker container (`--rm`), mounted read-write only
/// while it runs. Leaves the host system untouched.
pub fn regenerate_node_lock(dir: &Path, image: &str) -> Result<()> {
    let tool = tool_for(dir);
    run_in_docker(dir, image, tool)?;
    ensure!(
        dir.join(tool.lock).is_file(),
        "{} lockfile regen finished but {} is missing",
        tool.name,
        tool.lock
    );
    Ok(())
}

/// Berry lockfiles start with a `__metadata:` block; classic ones do not.
fn is_yarn_berry(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("yarn.lock"))
        .is_ok_and(|s| s.starts_with("__metadata:") || s.contains("\n__metadata:"))
}

fn run_in_docker(dir: &Path, image: &str, tool: &Tool) -> Result<()> {
    docker_available().context(format!(
        "docker is required for {} lockfile regeneration (or run with --no-regen)",
        tool.name
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
            tool.script,
        ])
        .output()
        .context("failed to launch docker")?;
    ensure!(
        output.status.success(),
        "{} lockfile regen failed inside docker:\n{}",
        tool.name,
        trim(&output.stderr)
    );
    Ok(())
}

fn docker_available() -> Result<()> {
    let output = Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output()
        .context("docker binary not found on PATH")?;
    output
        .status
        .success()
        .then_some(())
        .ok_or_else(|| anyhow!("docker daemon not reachable; start it or run with --no-regen"))
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
