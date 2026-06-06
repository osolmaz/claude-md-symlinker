use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    adapters::Adapter,
    config::{MaterializationConfig, MaterializationStrategy},
    git::GitRepo,
};

const MANAGED_MARKER: &str = "claudectomy managed";

#[derive(Debug, Clone, Copy, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MaterializationKind {
    Symlink,
    Copy,
    Hardlink,
}

impl MaterializationKind {
    pub fn from_state_value(value: &str) -> Option<Self> {
        Some(match value {
            "symlink" => Self::Symlink,
            "copy" => Self::Copy,
            "hardlink" => Self::Hardlink,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetState {
    Missing,
    ManagedSymlink { repair_needed: bool },
    ManagedCopy { refresh_needed: bool },
    ManagedHardlink,
    UnknownRegularFile,
    UnknownSymlink,
    Other,
}

#[derive(Debug, Clone, Serialize)]
pub struct MaterializeOutcome {
    pub kind: MaterializationKind,
    pub changed: bool,
}

pub fn classify(repo: &GitRepo, adapter: &Adapter) -> Result<TargetState> {
    let source = repo.root.join(&adapter.source);
    let target = repo.root.join(&adapter.target);

    let Ok(metadata) = fs::symlink_metadata(&target) else {
        return Ok(TargetState::Missing);
    };

    if metadata.file_type().is_symlink() {
        return if symlink_points_to(&target, &source)? {
            Ok(TargetState::ManagedSymlink {
                repair_needed: fs::read_link(&target)? != desired_symlink_target(repo, adapter),
            })
        } else {
            Ok(TargetState::UnknownSymlink)
        };
    }

    if metadata.is_file() {
        if source.exists() && same_file::is_same_file(&source, &target).unwrap_or(false) {
            return Ok(TargetState::ManagedHardlink);
        }

        let bytes = fs::read(&target)
            .with_context(|| format!("failed to read target {}", target.display()))?;
        if is_managed_copy(&bytes, adapter) {
            let refresh_needed = if source.exists() {
                let desired = managed_copy_bytes(repo, adapter)?;
                bytes != desired
            } else {
                false
            };
            return Ok(TargetState::ManagedCopy { refresh_needed });
        }

        return Ok(TargetState::UnknownRegularFile);
    }

    Ok(TargetState::Other)
}

pub fn create_or_refresh(
    repo: &GitRepo,
    adapter: &Adapter,
    config: &MaterializationConfig,
    dry_run: bool,
) -> Result<MaterializeOutcome> {
    let target_state = classify(repo, adapter)?;
    match target_state {
        TargetState::ManagedCopy { refresh_needed } => {
            if refresh_needed && !dry_run {
                write_managed_copy(repo, adapter)?;
            }
            Ok(MaterializeOutcome {
                kind: MaterializationKind::Copy,
                changed: refresh_needed,
            })
        }
        TargetState::ManagedSymlink { repair_needed } => {
            if repair_needed && !dry_run {
                let target = repo.root.join(&adapter.target);
                fs::remove_file(&target)
                    .with_context(|| format!("failed to remove {}", target.display()))?;
                create_symlink(repo, adapter)?;
            }
            Ok(MaterializeOutcome {
                kind: MaterializationKind::Symlink,
                changed: repair_needed,
            })
        }
        TargetState::ManagedHardlink => Ok(MaterializeOutcome {
            kind: MaterializationKind::Hardlink,
            changed: false,
        }),
        TargetState::Missing => create_missing(repo, adapter, config, dry_run),
        TargetState::UnknownRegularFile | TargetState::UnknownSymlink | TargetState::Other => {
            unreachable!("create_or_refresh called on unmanaged target")
        }
    }
}

pub fn remove_target(repo: &GitRepo, adapter: &Adapter, dry_run: bool) -> Result<bool> {
    let target = repo.root.join(&adapter.target);
    if !target.exists() && fs::symlink_metadata(&target).is_err() {
        return Ok(false);
    }

    if !dry_run {
        fs::remove_file(&target)
            .with_context(|| format!("failed to remove {}", target.display()))?;
    }
    Ok(true)
}

pub fn recreate_hardlink(
    repo: &GitRepo,
    adapter: &Adapter,
    dry_run: bool,
) -> Result<MaterializeOutcome> {
    let target = repo.root.join(&adapter.target);
    if !dry_run {
        if fs::symlink_metadata(&target).is_ok() {
            fs::remove_file(&target)
                .with_context(|| format!("failed to remove {}", target.display()))?;
        }
        create_hardlink(repo, adapter)?;
    }

    Ok(MaterializeOutcome {
        kind: MaterializationKind::Hardlink,
        changed: true,
    })
}

pub fn source_hash(repo: &GitRepo, adapter: &Adapter) -> Result<Option<String>> {
    file_hash(&repo.root.join(&adapter.source))
}

pub fn target_hash(repo: &GitRepo, adapter: &Adapter) -> Result<Option<String>> {
    file_hash(&repo.root.join(&adapter.target))
}

fn file_hash(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }

    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(Some(format!("{:x}", hasher.finalize())))
}

pub fn managed_copy_header(adapter: &Adapter) -> String {
    format!(
        "<!-- claudectomy managed: source={}; adapter={}; do not edit this file directly. -->",
        adapter.source.display(),
        adapter.name
    )
}

fn create_missing(
    repo: &GitRepo,
    adapter: &Adapter,
    config: &MaterializationConfig,
    dry_run: bool,
) -> Result<MaterializeOutcome> {
    match config.strategy {
        MaterializationStrategy::Symlink => {
            if !dry_run {
                create_symlink(repo, adapter)?;
            }
            Ok(MaterializeOutcome {
                kind: MaterializationKind::Symlink,
                changed: true,
            })
        }
        MaterializationStrategy::Copy => {
            if !dry_run {
                write_managed_copy(repo, adapter)?;
            }
            Ok(MaterializeOutcome {
                kind: MaterializationKind::Copy,
                changed: true,
            })
        }
        MaterializationStrategy::Hardlink => {
            if !dry_run {
                create_hardlink(repo, adapter)?;
            }
            Ok(MaterializeOutcome {
                kind: MaterializationKind::Hardlink,
                changed: true,
            })
        }
        MaterializationStrategy::Auto => {
            if dry_run {
                return Ok(MaterializeOutcome {
                    kind: MaterializationKind::Symlink,
                    changed: true,
                });
            }

            match create_symlink(repo, adapter) {
                Ok(()) => Ok(MaterializeOutcome {
                    kind: MaterializationKind::Symlink,
                    changed: true,
                }),
                Err(symlink_error) => {
                    if config.allow_hardlink && create_hardlink(repo, adapter).is_ok() {
                        return Ok(MaterializeOutcome {
                            kind: MaterializationKind::Hardlink,
                            changed: true,
                        });
                    }
                    tracing::debug!("symlink failed, falling back to copy: {symlink_error:#}");
                    write_managed_copy(repo, adapter)?;
                    Ok(MaterializeOutcome {
                        kind: MaterializationKind::Copy,
                        changed: true,
                    })
                }
            }
        }
    }
}

fn create_symlink(repo: &GitRepo, adapter: &Adapter) -> Result<()> {
    let target = repo.root.join(&adapter.target);
    create_parent_dir(&target)?;
    let relative_source = desired_symlink_target(repo, adapter);
    symlink_file(&relative_source, &target)
        .with_context(|| format!("failed to symlink {}", target.display()))?;
    Ok(())
}

fn desired_symlink_target(repo: &GitRepo, adapter: &Adapter) -> std::path::PathBuf {
    let source = repo.root.join(&adapter.source);
    let target = repo.root.join(&adapter.target);
    let target_parent = target.parent().unwrap_or(repo.root.as_path());
    pathdiff::diff_paths(&source, target_parent).unwrap_or(source)
}

fn create_hardlink(repo: &GitRepo, adapter: &Adapter) -> Result<()> {
    let source = repo.root.join(&adapter.source);
    let target = repo.root.join(&adapter.target);
    create_parent_dir(&target)?;
    fs::hard_link(&source, &target)
        .with_context(|| format!("failed to hardlink {}", target.display()))?;
    Ok(())
}

fn write_managed_copy(repo: &GitRepo, adapter: &Adapter) -> Result<()> {
    let target = repo.root.join(&adapter.target);
    create_parent_dir(&target)?;
    let bytes = managed_copy_bytes(repo, adapter)?;
    fs::write(&target, bytes).with_context(|| format!("failed to write {}", target.display()))?;
    Ok(())
}

fn managed_copy_bytes(repo: &GitRepo, adapter: &Adapter) -> Result<Vec<u8>> {
    let source = repo.root.join(&adapter.source);
    let mut bytes = managed_copy_header(adapter).into_bytes();
    bytes.push(b'\n');
    bytes
        .extend(fs::read(&source).with_context(|| format!("failed to read {}", source.display()))?);
    Ok(bytes)
}

fn is_managed_copy(bytes: &[u8], adapter: &Adapter) -> bool {
    let header = managed_copy_header(adapter);
    bytes
        .split(|byte| *byte == b'\n')
        .next()
        .and_then(|line| std::str::from_utf8(line).ok())
        .map(|line| line.contains(MANAGED_MARKER) && line == header)
        .unwrap_or(false)
}

fn symlink_points_to(target: &Path, source: &Path) -> Result<bool> {
    let link = fs::read_link(target)
        .with_context(|| format!("failed to read symlink {}", target.display()))?;
    let resolved = if link.is_absolute() {
        link
    } else {
        target.parent().unwrap_or(Path::new(".")).join(link)
    };

    if paths_match_lexically(&resolved, source) {
        return Ok(true);
    }

    match (resolved.canonicalize(), source.canonicalize()) {
        (Ok(resolved), Ok(source)) => Ok(resolved == source),
        _ => Ok(false),
    }
}

fn paths_match_lexically(left: &Path, right: &Path) -> bool {
    lexical_normalize(left) == lexical_normalize(right)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    normalized
}

fn create_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn symlink_file(source: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, target)
}

#[cfg(windows)]
fn symlink_file(source: &Path, target: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(source, target)
}
