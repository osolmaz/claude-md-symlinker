use std::{
    fs,
    fs::Metadata,
    io::{ErrorKind, Write},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tempfile::tempdir;

use crate::{
    adapters::Adapter,
    config::{MaterializationConfig, MaterializationStrategy},
    git::GitRepo,
};

const MANAGED_MARKER: &str = "claudemdeez managed";

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
    let lexical_source = repo.root.join(&adapter.source);
    let source = validated_source_path(repo, adapter)?;
    let target = repo.root.join(&adapter.target);

    let metadata = match fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(TargetState::Missing),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect target {}", target.display()));
        }
    };

    if target_parent_contains_symlink(repo, &target)? {
        return Ok(if metadata.file_type().is_symlink() {
            TargetState::UnknownSymlink
        } else if metadata.is_file() {
            TargetState::UnknownRegularFile
        } else {
            TargetState::Other
        });
    }

    if metadata.file_type().is_symlink() {
        return if symlink_points_to(
            repo,
            &target,
            source.as_deref().unwrap_or(&lexical_source),
            &desired_symlink_target(repo, adapter),
        )? {
            Ok(TargetState::ManagedSymlink {
                repair_needed: fs::read_link(&target)? != desired_symlink_target(repo, adapter),
            })
        } else {
            Ok(TargetState::UnknownSymlink)
        };
    }

    if metadata.is_file() {
        if source
            .as_deref()
            .is_some_and(|source| same_file::is_same_file(source, &target).unwrap_or(false))
        {
            return Ok(TargetState::ManagedHardlink);
        }

        let bytes = fs::read(&target)
            .with_context(|| format!("failed to read target {}", target.display()))?;
        if is_managed_copy(&bytes, adapter) {
            let refresh_needed = if source.is_some() {
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
            let desired = desired_existing_kind(config, MaterializationKind::Copy);
            if desired != MaterializationKind::Copy {
                return replace_with_kind(repo, adapter, desired, dry_run);
            }
            let permissions_refresh_needed = !target_permissions_match_source(repo, adapter)?;
            if refresh_needed {
                validate_existing_target_for_write(repo, adapter)?;
            }
            if refresh_needed && !dry_run {
                write_managed_copy(repo, adapter)?;
            } else if permissions_refresh_needed && !dry_run {
                apply_source_permissions(repo, adapter)?;
            }
            Ok(MaterializeOutcome {
                kind: MaterializationKind::Copy,
                changed: refresh_needed || permissions_refresh_needed,
            })
        }
        TargetState::ManagedSymlink { repair_needed } => {
            let desired = desired_existing_kind(config, MaterializationKind::Symlink);
            if desired != MaterializationKind::Symlink {
                return replace_with_kind(repo, adapter, desired, dry_run);
            }
            if repair_needed {
                let target = repo.root.join(&adapter.target);
                create_parent_dir(repo, &target)?;
            }
            if repair_needed && !dry_run {
                let target = repo.root.join(&adapter.target);
                remove_file(&target)?;
                create_symlink(repo, adapter)?;
            }
            Ok(MaterializeOutcome {
                kind: MaterializationKind::Symlink,
                changed: repair_needed,
            })
        }
        TargetState::ManagedHardlink => {
            let desired = desired_existing_kind(config, MaterializationKind::Hardlink);
            if desired != MaterializationKind::Hardlink {
                return replace_with_kind(repo, adapter, desired, dry_run);
            }
            Ok(MaterializeOutcome {
                kind: MaterializationKind::Hardlink,
                changed: false,
            })
        }
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

    create_parent_dir(repo, &target)?;
    if !dry_run {
        remove_file(&target)?;
    }
    Ok(true)
}

pub fn source_hash(repo: &GitRepo, adapter: &Adapter) -> Result<Option<String>> {
    let Some(source) = validated_source_path(repo, adapter)? else {
        return Ok(None);
    };
    file_hash(&source)
}

pub fn source_exists(repo: &GitRepo, adapter: &Adapter) -> Result<bool> {
    Ok(validated_source_path(repo, adapter)?.is_some())
}

fn file_hash(path: &Path) -> Result<Option<String>> {
    let Some(_metadata) = source_metadata_path(path)? else {
        return Ok(None);
    };

    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(Some(format!("{:x}", hasher.finalize())))
}

fn required_source_path(repo: &GitRepo, adapter: &Adapter) -> Result<PathBuf> {
    let path = repo.root.join(&adapter.source);
    validated_source_path(repo, adapter)?
        .with_context(|| format!("source file does not exist: {}", path.display()))
}

fn validated_source_path(repo: &GitRepo, adapter: &Adapter) -> Result<Option<PathBuf>> {
    let path = repo.root.join(&adapter.source);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }

    let repo_root = repo
        .root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize repo root {}", repo.root.display()))?;
    let resolved = path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize source file {}", path.display()))?;
    if !resolved.starts_with(&repo_root) {
        bail!(
            "source file {} resolves outside repository root {}",
            path.display(),
            repo_root.display()
        );
    }

    Ok(Some(resolved))
}

fn required_source_metadata_path(path: &Path) -> Result<Metadata> {
    source_metadata_path(path)?
        .with_context(|| format!("source file does not exist: {}", path.display()))
}

fn source_metadata_path(path: &Path) -> Result<Option<Metadata>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    Ok(Some(metadata))
}

pub fn managed_copy_header(adapter: &Adapter) -> String {
    format!(
        "<!-- claudemdeez managed: source={}; adapter={}; do not edit this file directly. -->",
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
    let target = repo.root.join(&adapter.target);
    validate_parent_dir(repo, &target)?;

    match config.strategy {
        MaterializationStrategy::Symlink => {
            if dry_run {
                validate_kind_creation(repo, adapter, MaterializationKind::Symlink)?;
            } else {
                create_symlink(repo, adapter)?;
            }
            Ok(MaterializeOutcome {
                kind: MaterializationKind::Symlink,
                changed: true,
            })
        }
        MaterializationStrategy::Copy => {
            if dry_run {
                validate_kind_creation(repo, adapter, MaterializationKind::Copy)?;
            } else {
                write_managed_copy(repo, adapter)?;
            }
            Ok(MaterializeOutcome {
                kind: MaterializationKind::Copy,
                changed: true,
            })
        }
        MaterializationStrategy::Hardlink => {
            if dry_run {
                validate_kind_creation(repo, adapter, MaterializationKind::Hardlink)?;
            } else {
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
                    kind: planned_auto_kind(config),
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

fn desired_existing_kind(
    config: &MaterializationConfig,
    current: MaterializationKind,
) -> MaterializationKind {
    match config.strategy {
        MaterializationStrategy::Symlink => MaterializationKind::Symlink,
        MaterializationStrategy::Copy => MaterializationKind::Copy,
        MaterializationStrategy::Hardlink => MaterializationKind::Hardlink,
        MaterializationStrategy::Auto => current,
    }
}

fn replace_with_kind(
    repo: &GitRepo,
    adapter: &Adapter,
    kind: MaterializationKind,
    dry_run: bool,
) -> Result<MaterializeOutcome> {
    let target = repo.root.join(&adapter.target);
    validate_parent_dir(repo, &target)?;

    if dry_run {
        validate_kind_creation(repo, adapter, kind)?;
    } else {
        if fs::symlink_metadata(&target).is_ok() {
            remove_file(&target)?;
        }
        match kind {
            MaterializationKind::Symlink => create_symlink(repo, adapter)?,
            MaterializationKind::Copy => write_managed_copy(repo, adapter)?,
            MaterializationKind::Hardlink => create_hardlink(repo, adapter)?,
        }
    }

    Ok(MaterializeOutcome {
        kind,
        changed: true,
    })
}

fn validate_kind_creation(
    repo: &GitRepo,
    adapter: &Adapter,
    kind: MaterializationKind,
) -> Result<()> {
    let target = repo.root.join(&adapter.target);
    validate_parent_dir(repo, &target)?;

    match kind {
        MaterializationKind::Symlink => validate_symlink_creation(repo, adapter),
        MaterializationKind::Copy => validate_copy_creation(repo, adapter),
        MaterializationKind::Hardlink => validate_hardlink_creation(repo, adapter),
    }
}

fn validate_symlink_creation(repo: &GitRepo, adapter: &Adapter) -> Result<()> {
    let dir = tempdir().context("failed to create symlink probe directory")?;
    let probe = dir.path().join("target");
    let relative_source = desired_symlink_target(repo, adapter);
    symlink_file(&relative_source, &probe).with_context(|| {
        format!(
            "failed to symlink {}",
            repo.root.join(&adapter.target).display()
        )
    })?;
    Ok(())
}

fn validate_copy_creation(repo: &GitRepo, adapter: &Adapter) -> Result<()> {
    managed_copy_bytes(repo, adapter)?;
    Ok(())
}

fn validate_hardlink_creation(repo: &GitRepo, adapter: &Adapter) -> Result<()> {
    required_source_path(repo, adapter)?;
    let dir = tempdir().context("failed to create hardlink probe directory")?;
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    fs::write(&source, "probe").context("failed to write hardlink probe source")?;
    fs::hard_link(&source, &target).context("failed to hardlink probe target")?;
    Ok(())
}

fn remove_file(path: &Path) -> Result<()> {
    clear_readonly_for_removal(path)?;
    fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))?;
    Ok(())
}

#[cfg(windows)]
fn clear_readonly_for_removal(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.is_file() && !metadata.file_type().is_symlink() {
        let mut permissions = metadata.permissions();
        if permissions.readonly() {
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions)
                .with_context(|| format!("failed to set permissions on {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn clear_readonly_for_removal(_path: &Path) -> Result<()> {
    Ok(())
}

fn planned_auto_kind(config: &MaterializationConfig) -> MaterializationKind {
    if symlink_probe() {
        MaterializationKind::Symlink
    } else if config.allow_hardlink && hardlink_probe() {
        MaterializationKind::Hardlink
    } else {
        MaterializationKind::Copy
    }
}

fn symlink_probe() -> bool {
    let Ok(dir) = tempdir() else {
        return false;
    };
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    fs::write(&source, "probe").is_ok() && symlink_file(&source, &target).is_ok()
}

fn hardlink_probe() -> bool {
    let Ok(dir) = tempdir() else {
        return false;
    };
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    fs::write(&source, "probe").is_ok() && fs::hard_link(&source, &target).is_ok()
}

fn create_symlink(repo: &GitRepo, adapter: &Adapter) -> Result<()> {
    let target = repo.root.join(&adapter.target);
    create_parent_dir(repo, &target)?;
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
    let source = required_source_path(repo, adapter)?;
    let target = repo.root.join(&adapter.target);
    create_parent_dir(repo, &target)?;
    fs::hard_link(&source, &target)
        .with_context(|| format!("failed to hardlink {}", target.display()))?;
    Ok(())
}

fn write_managed_copy(repo: &GitRepo, adapter: &Adapter) -> Result<()> {
    let target = repo.root.join(&adapter.target);
    let source = required_source_path(repo, adapter)?;
    create_parent_dir(repo, &target)?;
    let bytes = managed_copy_bytes(repo, adapter)?;
    write_with_source_permissions(&source, &target, &bytes)?;
    Ok(())
}

#[cfg(unix)]
fn write_with_source_permissions(source: &Path, target: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::{fs::OpenOptionsExt, fs::PermissionsExt};

    let mode = source_mode(source)?;
    let write_mode = mode | 0o200;
    if target.exists() {
        fs::set_permissions(target, fs::Permissions::from_mode(write_mode))
            .with_context(|| format!("failed to set permissions on {}", target.display()))?;
    }

    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(write_mode)
        .open(target)
        .with_context(|| format!("failed to write {}", target.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("failed to write {}", target.display()))?;
    file.set_permissions(fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to set permissions on {}", target.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_with_source_permissions(source: &Path, target: &Path, bytes: &[u8]) -> Result<()> {
    let permissions = required_source_metadata_path(source)?.permissions();
    if target.exists() {
        let mut write_permissions = permissions.clone();
        write_permissions.set_readonly(false);
        fs::set_permissions(target, write_permissions)
            .with_context(|| format!("failed to set permissions on {}", target.display()))?;
    }
    fs::write(target, bytes).with_context(|| format!("failed to write {}", target.display()))?;
    fs::set_permissions(target, permissions)
        .with_context(|| format!("failed to set permissions on {}", target.display()))?;
    Ok(())
}

fn apply_source_permissions(repo: &GitRepo, adapter: &Adapter) -> Result<()> {
    let source = required_source_path(repo, adapter)?;
    let target = repo.root.join(&adapter.target);
    apply_permissions(&source, &target)
}

#[cfg(unix)]
fn apply_permissions(source: &Path, target: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(target, fs::Permissions::from_mode(source_mode(source)?))
        .with_context(|| format!("failed to set permissions on {}", target.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn apply_permissions(source: &Path, target: &Path) -> Result<()> {
    let permissions = required_source_metadata_path(source)?.permissions();
    fs::set_permissions(target, permissions)
        .with_context(|| format!("failed to set permissions on {}", target.display()))?;
    Ok(())
}

fn target_permissions_match_source(repo: &GitRepo, adapter: &Adapter) -> Result<bool> {
    let source = required_source_path(repo, adapter)?;
    let target = repo.root.join(&adapter.target);
    permissions_match(&source, &target)
}

#[cfg(unix)]
fn permissions_match(source: &Path, target: &Path) -> Result<bool> {
    Ok(source_mode(source)? == target_mode(target)?)
}

#[cfg(not(unix))]
fn permissions_match(source: &Path, target: &Path) -> Result<bool> {
    let source = required_source_metadata_path(source)?.permissions();
    let target = fs::metadata(target)
        .with_context(|| format!("failed to inspect {}", target.display()))?
        .permissions();
    Ok(source.readonly() == target.readonly())
}

#[cfg(unix)]
fn source_mode(source: &Path) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;

    Ok(required_source_metadata_path(source)?.permissions().mode() & 0o7777)
}

#[cfg(unix)]
fn target_mode(target: &Path) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;

    Ok(fs::metadata(target)
        .with_context(|| format!("failed to inspect {}", target.display()))?
        .permissions()
        .mode()
        & 0o7777)
}

fn validate_existing_target_for_write(repo: &GitRepo, adapter: &Adapter) -> Result<()> {
    let target = repo.root.join(&adapter.target);
    validate_parent_dir(repo, &target)?;
    let metadata =
        fs::metadata(&target).with_context(|| format!("failed to inspect {}", target.display()))?;
    if !metadata.is_file() {
        bail!("target {} is not a regular file", target.display());
    }
    Ok(())
}

fn managed_copy_bytes(repo: &GitRepo, adapter: &Adapter) -> Result<Vec<u8>> {
    let source = required_source_path(repo, adapter)?;
    required_source_metadata_path(&source)?;
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

fn symlink_points_to(
    repo: &GitRepo,
    target: &Path,
    source: &Path,
    desired_link: &Path,
) -> Result<bool> {
    let link = fs::read_link(target)
        .with_context(|| format!("failed to read symlink {}", target.display()))?;
    let resolved = if link.is_absolute() {
        link.clone()
    } else {
        target.parent().unwrap_or(Path::new(".")).join(&link)
    };

    if let (Ok(resolved), Ok(source)) = (resolved.canonicalize(), source.canonicalize()) {
        return Ok(resolved == source);
    }

    if link == desired_link && !target_parent_contains_symlink(repo, target)? {
        return Ok(true);
    }

    Ok(false)
}

fn create_parent_dir(repo: &GitRepo, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        validate_parent_dir(repo, path)?;

        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
        let repo_root = repo
            .root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize repo root {}", repo.root.display()))?;
        ensure_resolves_under_repo(&repo_root, parent)?;
    }
    Ok(())
}

fn validate_parent_dir(repo: &GitRepo, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_no_symlinked_target_parent(repo, path)?;
        let repo_root = repo
            .root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize repo root {}", repo.root.display()))?;
        if let Some(existing_parent) = nearest_existing_ancestor(parent) {
            ensure_resolves_under_repo(&repo_root, existing_parent)?;
            ensure_writable_directory(existing_parent)?;
        }
    }
    Ok(())
}

fn ensure_no_symlinked_target_parent(repo: &GitRepo, path: &Path) -> Result<()> {
    if target_parent_contains_symlink(repo, path)? {
        bail!(
            "target parent for {} contains a symlink; refusing to write because the Git exclude path would not match the real file",
            path.display()
        );
    }
    Ok(())
}

fn target_parent_contains_symlink(repo: &GitRepo, path: &Path) -> Result<bool> {
    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    let Ok(relative_parent) = parent.strip_prefix(&repo.root) else {
        return Ok(false);
    };

    let mut current = repo.root.clone();
    for component in relative_parent.components() {
        match component {
            Component::Normal(part) => current.push(part),
            Component::CurDir => continue,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return Ok(false),
        }

        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect target parent {}", current.display())
                });
            }
        };
        if metadata.file_type().is_symlink() {
            return Ok(true);
        }
    }

    Ok(false)
}

fn ensure_writable_directory(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to inspect target parent {}", path.display()))?;
    if !metadata.is_dir() {
        bail!("target parent {} is not a directory", path.display());
    }
    if metadata.permissions().readonly() {
        bail!("target parent {} is not writable", path.display());
    }
    Ok(())
}

fn nearest_existing_ancestor(mut path: &Path) -> Option<&Path> {
    loop {
        if path.exists() {
            return Some(path);
        }
        path = path.parent()?;
    }
}

fn ensure_resolves_under_repo(repo_root: &Path, path: &Path) -> Result<()> {
    let resolved = path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize target parent {}", path.display()))?;
    if !resolved.starts_with(repo_root) {
        bail!(
            "target parent {} resolves outside repository root {}",
            path.display(),
            repo_root.display()
        );
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
