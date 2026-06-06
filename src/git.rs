use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct GitRepo {
    pub root: PathBuf,
    pub git_dir: PathBuf,
    pub exclude_path: PathBuf,
}

pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn discover_repo(path: &Path) -> Result<Option<GitRepo>> {
    let Some(root_text) = git_stdout(path, &["rev-parse", "--show-toplevel"])? else {
        return Ok(None);
    };

    let root = PathBuf::from(root_text.trim())
        .canonicalize()
        .with_context(|| format!("failed to canonicalize Git root from {}", path.display()))?;

    let Some(is_bare) = git_stdout(&root, &["rev-parse", "--is-bare-repository"])? else {
        return Ok(None);
    };
    if is_bare.trim() == "true" {
        return Ok(None);
    }

    let git_dir = git_path(&root, &["rev-parse", "--git-dir"])?;
    let exclude_path = git_path(&root, &["rev-parse", "--git-path", "info/exclude"])?;

    Ok(Some(GitRepo {
        root,
        git_dir,
        exclude_path,
    }))
}

pub fn is_tracked(repo: &GitRepo, rel_path: &Path) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .arg("ls-files")
        .arg("--error-unmatch")
        .arg("--")
        .arg(rel_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to run git ls-files in {}", repo.root.display()))?;
    Ok(status.success())
}

pub fn ensure_global_excludes_file(path: &Path, dry_run: bool) -> Result<bool> {
    if let Some(existing) = global_config_value("core.excludesFile")? {
        if Path::new(existing.trim()) == path {
            return Ok(false);
        }

        bail!(
            "global core.excludesFile is already set to {}; refusing to overwrite it",
            existing.trim()
        );
    }

    if dry_run {
        return Ok(true);
    }

    let status = Command::new("git")
        .arg("config")
        .arg("--global")
        .arg("core.excludesFile")
        .arg(path)
        .status()
        .context("failed to run git config --global core.excludesFile")?;

    if !status.success() {
        bail!("git config --global core.excludesFile failed");
    }
    Ok(true)
}

fn global_config_value(key: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("config")
        .arg("--global")
        .arg("--get")
        .arg(key)
        .output()
        .with_context(|| format!("failed to read global git config {key}"))?;

    if !output.status.success() {
        return Ok(None);
    }

    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

fn git_path(repo: &Path, args: &[&str]) -> Result<PathBuf> {
    let output = git_stdout(repo, args)?.context("git command did not return a path")?;
    let path = PathBuf::from(output.trim());
    let absolute = if path.is_absolute() {
        path
    } else {
        repo.join(path)
    };
    Ok(absolute)
}

fn git_stdout(path: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git in {}", path.display()))?;

    if !output.status.success() {
        return Ok(None);
    }

    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}
