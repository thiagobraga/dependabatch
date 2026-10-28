use anyhow::{Context, Result, anyhow};
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// True when the checked-out repo contains `package-lock.json`.
pub fn has_npm_lock(dir: &Path) -> bool {
    dir.join("package-lock.json").is_file()
}

/// Rebuild `package-lock.json` from the edited manifests with npm.
///
/// Runs inside an ephemeral docker container (`--rm`), mounted read-write only
/// while it runs. Leaves the host system untouched.
pub fn regenerate_npm_lock(dir: &Path, image: &str) -> Result<()> {
    docker_available()
        .context("docker is required for npm lockfile regeneration (or run with --no-regen)")?;
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
            image,
            "npm",
            "install",
            "--package-lock-only",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
        ])
        .output()
        .context("failed to launch docker")?;
    if !output.status.success() {
        return Err(anyhow!(
            "npm lockfile regen failed inside docker:\n{}",
            trim(&output.stderr)
        ));
    }
    if !has_npm_lock(dir) {
        return Err(anyhow!(
            "npm lockfile regen finished but package-lock.json is missing"
        ));
    }
    Ok(())
}

fn docker_available() -> Result<()> {
    let output = Command::new("docker")
        .args(["version", "--format", "{{.ServerVersion}}"])
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
