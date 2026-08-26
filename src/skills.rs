use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use pathdiff::diff_paths;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    config::ExcludeMode,
    exclude,
    git::{self, GitRepo},
    reporting::{RepoResult, Report, Status},
    state::{ManagedSkillLink, ObservedSkillRoot, SkillLinkRecord, SkillRootRecord, State},
};

pub const ADAPTER_NAME: &str = "claude-skills";
const SOURCE_ROOT: &str = ".agents/skills";
const TARGET_ROOT: &str = ".claude/skills";
const MAX_SKILL_NAME_CHARS: usize = 64;
const MAX_SKILL_DESCRIPTION_CHARS: usize = 1024;
const EXTERNAL_STATE_FILES: &[&str] =
    &[".onurpi-agents-sync.json", ".tools-agents-skill-sync.json"];

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillScope {
    Project,
    Global,
}

impl SkillScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkillRoot {
    pub scope: SkillScope,
    pub repo: Option<GitRepo>,
    pub base_dir: PathBuf,
    pub source_root: PathBuf,
    pub target_root: PathBuf,
}

impl SkillRoot {
    pub fn project(repo: &GitRepo, base_dir: &Path) -> Result<Self> {
        let base_dir = canonical_or_absolute(base_dir)?;
        if !base_dir.starts_with(&repo.root) {
            bail!(
                "skill base {} is outside repository {}",
                base_dir.display(),
                repo.root.display()
            );
        }
        Ok(Self {
            scope: SkillScope::Project,
            repo: Some(repo.clone()),
            source_root: base_dir.join(SOURCE_ROOT),
            target_root: base_dir.join(TARGET_ROOT),
            base_dir,
        })
    }

    pub fn global(home: &Path) -> Result<Self> {
        let base_dir = canonical_or_absolute(home)?;
        Ok(Self {
            scope: SkillScope::Global,
            repo: None,
            source_root: base_dir.join(SOURCE_ROOT),
            target_root: base_dir.join(TARGET_ROOT),
            base_dir,
        })
    }

    pub fn from_observed(root: &ObservedSkillRoot) -> Result<Self> {
        let scope = match root.scope.as_str() {
            "project" => SkillScope::Project,
            "global" => SkillScope::Global,
            other => bail!("unknown skill scope `{other}`"),
        };
        let repo = root
            .repo_root
            .as_deref()
            .map(git::discover_repo)
            .transpose()?
            .flatten();
        if scope == SkillScope::Project && repo.is_none() {
            bail!("recorded project repository is no longer a Git worktree");
        }
        Ok(Self {
            scope,
            repo,
            base_dir: root.base_dir.clone(),
            source_root: root.source_root.clone(),
            target_root: root.target_root.clone(),
        })
    }

    fn report_repo(&self) -> String {
        self.repo
            .as_ref()
            .map(|repo| repo.root.display().to_string())
            .unwrap_or_else(|| self.base_dir.display().to_string())
    }

    fn target_rel(&self, target: &Path) -> Result<Option<PathBuf>> {
        self.repo
            .as_ref()
            .map(|repo| {
                target
                    .strip_prefix(&repo.root)
                    .map(PathBuf::from)
                    .with_context(|| {
                        format!(
                            "skill target {} is outside repository {}",
                            target.display(),
                            repo.root.display()
                        )
                    })
            })
            .transpose()
    }
}

#[derive(Debug, Clone)]
struct ValidSkill {
    name: String,
    source: PathBuf,
}

type InvalidSkill = (String, PathBuf, String);

#[derive(Debug, Default)]
struct ExternalOwnership {
    managed: BTreeSet<String>,
    blocks_all: bool,
    markers: Vec<PathBuf>,
}

#[derive(Debug, Default, Serialize)]
pub struct SkillMigrationReport {
    pub migrated: Vec<SkillMigrationItem>,
    pub skipped: Vec<SkillMigrationItem>,
    pub errors: Vec<SkillMigrationItem>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillMigrationItem {
    pub skill: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub reason: String,
}

pub fn reconcile_project_dir(
    repo: &GitRepo,
    base_dir: &Path,
    state: &State,
    dry_run: bool,
) -> Result<Report> {
    reconcile_root(&SkillRoot::project(repo, base_dir)?, state, dry_run)
}

pub fn reconcile_global(home: &Path, state: &State, dry_run: bool) -> Result<Report> {
    reconcile_root(&SkillRoot::global(home)?, state, dry_run)
}

pub fn reconcile_observed(
    observed: &ObservedSkillRoot,
    state: &State,
    dry_run: bool,
) -> Result<Report> {
    reconcile_root(&SkillRoot::from_observed(observed)?, state, dry_run)
}

pub fn reconcile_root(root: &SkillRoot, state: &State, dry_run: bool) -> Result<Report> {
    let existing_root = state.skill_root_for_base(root.scope.as_str(), &root.base_dir)?;
    let source_metadata = match fs::symlink_metadata(&root.source_root) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect skill root {}",
                    root.source_root.display()
                )
            });
        }
    };

    if source_metadata.is_none() && existing_root.is_none() {
        return Ok(Report::new(usize::from(root.scope == SkillScope::Project)));
    }

    let mut report = Report::new(usize::from(root.scope == SkillScope::Project));
    let root_id = if dry_run {
        existing_root.as_ref().map(|record| record.id).unwrap_or(0)
    } else {
        state.record_skill_root(SkillRootRecord {
            scope: root.scope.as_str(),
            repo: root.repo.as_ref(),
            base_dir: &root.base_dir,
            source_root: &root.source_root,
            target_root: &root.target_root,
            status: "reconciling",
            error: None,
        })?
    };

    if let Some(metadata) = source_metadata {
        let validation = if metadata.file_type().is_symlink() || !metadata.is_dir() {
            Err(anyhow::anyhow!(
                "canonical skill root {} must be a real directory",
                root.source_root.display()
            ))
        } else {
            validate_source_root(root)
        };
        if let Err(error) = validation {
            let message = error.to_string();
            report.push(root_result(root, "*", Status::Error, &message));
            if !dry_run && root_id != 0 {
                state.mark_skill_root_result(root_id, "error", Some(&message))?;
            }
            return Ok(report);
        }
    }

    let stored = if root_id == 0 {
        Vec::new()
    } else {
        state.skill_links_for_root(root_id)?
    };
    let stored_by_name = stored
        .iter()
        .map(|link| (link.skill_name.clone(), link))
        .collect::<BTreeMap<_, _>>();

    if !root.source_root.exists() {
        for link in stored {
            cleanup_missing_source(root, &link, state, dry_run, &mut report)?;
        }
        if !dry_run && root_id != 0 {
            state.mark_skill_root_result(root_id, "cleaned", None)?;
        }
        return Ok(report);
    }

    validate_target_parent(root)?;
    let external = external_ownership(&root.target_root)?;
    let (valid, invalid) = discover_skills(&root.source_root)?;
    let valid_names = valid
        .iter()
        .map(|skill| skill.name.clone())
        .collect::<BTreeSet<_>>();

    for (name, source, reason) in invalid {
        let target = root.target_root.join(&name);
        let expected = expected_link(&source, &target).unwrap_or_default();
        report.push(root_result(root, &name, Status::Conflict, &reason));
        if !dry_run && root_id != 0 {
            state.record_skill_link(SkillLinkRecord {
                skill_root_id: root_id,
                skill_name: &name,
                source_path: &source,
                target_path: &target,
                expected_link: &expected,
                owned: false,
                status: "invalid",
                error: Some(&reason),
            })?;
        }
    }

    for skill in valid {
        reconcile_skill(
            root,
            root_id,
            &skill,
            stored_by_name.get(&skill.name).copied(),
            &external,
            state,
            dry_run,
            &mut report,
        )?;
    }

    for link in stored {
        if !valid_names.contains(&link.skill_name) {
            cleanup_missing_source(root, &link, state, dry_run, &mut report)?;
        }
    }

    if !dry_run && root_id != 0 {
        let status = if report.summary.errors > 0 {
            "error"
        } else if report.summary.conflicts > 0 || report.summary.tracked_conflicts > 0 {
            "conflict"
        } else {
            "ok"
        };
        state.mark_skill_root_result(root_id, status, None)?;
    }
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn reconcile_skill(
    root: &SkillRoot,
    root_id: i64,
    skill: &ValidSkill,
    stored: Option<&ManagedSkillLink>,
    external: &ExternalOwnership,
    state: &State,
    dry_run: bool,
    report: &mut Report,
) -> Result<()> {
    let target = root.target_root.join(&skill.name);
    let expected = expected_link(&skill.source, &target)?;

    if external.blocks_all || external.managed.contains(&skill.name) {
        let marker = external
            .markers
            .first()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "external synchronizer state".to_string());
        let message = format!("target is owned by another synchronizer ({marker})");
        record_skill_result(
            root,
            root_id,
            skill,
            &target,
            &expected,
            false,
            "external_conflict",
            Some(&message),
            state,
            dry_run,
        )?;
        report.push(root_result(root, &skill.name, Status::Conflict, message));
        return Ok(());
    }

    if let Some(target_rel) = root.target_rel(&target)?
        && git::is_tracked(
            root.repo.as_ref().expect("project target has repo"),
            &target_rel,
        )?
    {
        let exclude_updated = exclude::remove(
            root.repo.as_ref().expect("project target has repo"),
            &target_rel,
            ExcludeMode::PerRepo,
            dry_run,
        )?;
        if exclude_updated {
            report.summary.exclude_updates += 1;
        }
        let message = if exclude_updated {
            "target is tracked by Git; removed stale managed exclude"
        } else {
            "target is tracked by Git"
        };
        record_skill_result(
            root,
            root_id,
            skill,
            &target,
            &expected,
            false,
            "tracked_conflict",
            Some(message),
            state,
            dry_run,
        )?;
        report.push(root_result(
            root,
            &skill.name,
            Status::TrackedConflict,
            message,
        ));
        return Ok(());
    }

    match fs::symlink_metadata(&target) {
        Err(error) if error.kind() == ErrorKind::NotFound => {
            validate_target_parent(root)?;
            let status = if stored.is_some() {
                Status::Repaired
            } else {
                Status::Created
            };
            if !dry_run {
                create_target_parent(root)?;
                create_directory_symlink(&expected, &target)?;
                match ensure_project_exclude(root, &target, false) {
                    Ok(true) => report.summary.exclude_updates += 1,
                    Ok(false) => {}
                    Err(error) => {
                        let _ = fs::remove_file(&target);
                        return Err(error);
                    }
                }
            }
            let state_status = if status == Status::Created {
                "created"
            } else {
                "repaired"
            };
            record_skill_result(
                root,
                root_id,
                skill,
                &target,
                &expected,
                true,
                state_status,
                None,
                state,
                dry_run,
            )?;
            report.push(root_result(
                root,
                &skill.name,
                status,
                if dry_run {
                    "would create relative skill link"
                } else if status == Status::Created {
                    "created relative skill link"
                } else {
                    "repaired missing managed skill link"
                },
            ));
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect skill target {}", target.display()));
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let actual = fs::read_link(&target)
                .with_context(|| format!("failed to read skill link {}", target.display()))?;
            if actual != expected {
                let message = format!(
                    "unmanaged symlink points to {} instead of {}",
                    actual.display(),
                    expected.display()
                );
                record_skill_result(
                    root,
                    root_id,
                    skill,
                    &target,
                    &expected,
                    false,
                    "conflict",
                    Some(&message),
                    state,
                    dry_run,
                )?;
                report.push(root_result(root, &skill.name, Status::Conflict, message));
                return Ok(());
            }
            if ensure_project_exclude(root, &target, dry_run)? {
                report.summary.exclude_updates += 1;
            }
            let adopted = stored.is_none();
            record_skill_result(
                root,
                root_id,
                skill,
                &target,
                &expected,
                true,
                if adopted { "adopted" } else { "kept" },
                None,
                state,
                dry_run,
            )?;
            report.push(root_result(
                root,
                &skill.name,
                Status::Kept,
                if adopted {
                    "adopted exact existing skill link"
                } else {
                    "kept managed skill link"
                },
            ));
        }
        Ok(metadata) => {
            let kind = if metadata.is_dir() {
                "directory"
            } else if metadata.is_file() {
                "file"
            } else {
                "filesystem entry"
            };
            let message = format!("unmanaged target {kind} already exists");
            record_skill_result(
                root,
                root_id,
                skill,
                &target,
                &expected,
                false,
                "conflict",
                Some(&message),
                state,
                dry_run,
            )?;
            report.push(root_result(root, &skill.name, Status::Conflict, message));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record_skill_result(
    _root: &SkillRoot,
    root_id: i64,
    skill: &ValidSkill,
    target: &Path,
    expected: &Path,
    owned: bool,
    status: &str,
    error: Option<&str>,
    state: &State,
    dry_run: bool,
) -> Result<()> {
    if dry_run || root_id == 0 {
        return Ok(());
    }
    state.record_skill_link(SkillLinkRecord {
        skill_root_id: root_id,
        skill_name: &skill.name,
        source_path: &skill.source,
        target_path: target,
        expected_link: expected,
        owned,
        status,
        error,
    })
}

fn cleanup_missing_source(
    root: &SkillRoot,
    link: &ManagedSkillLink,
    state: &State,
    dry_run: bool,
    report: &mut Report,
) -> Result<()> {
    if let Err(error) = validate_target_parent(root) {
        report.push(root_result(
            root,
            &link.skill_name,
            Status::Conflict,
            format!("unsafe target parent; preserved link: {error}"),
        ));
        return Ok(());
    }
    let external = external_ownership(&root.target_root)?;
    if external.blocks_all || external.managed.contains(&link.skill_name) {
        report.push(root_result(
            root,
            &link.skill_name,
            Status::Conflict,
            "external synchronizer now claims the target; preserved link",
        ));
        return Ok(());
    }
    match fs::symlink_metadata(&link.target_path) {
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if remove_project_exclude(root, &link.target_path, dry_run)? {
                report.summary.exclude_updates += 1;
            }
            if !dry_run {
                state.mark_skill_link_removed(link.id)?;
            }
            report.push(root_result(
                root,
                &link.skill_name,
                Status::Cleaned,
                if dry_run {
                    "would forget missing managed skill link"
                } else {
                    "forgot missing managed skill link"
                },
            ));
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect skill target {}",
                    link.target_path.display()
                )
            });
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let actual = fs::read_link(&link.target_path)?;
            if actual != link.expected_link {
                report.push(root_result(
                    root,
                    &link.skill_name,
                    Status::Conflict,
                    "managed record no longer matches the on-disk link; preserved target",
                ));
                return Ok(());
            }
            if !dry_run {
                fs::remove_file(&link.target_path).with_context(|| {
                    format!("failed to remove skill link {}", link.target_path.display())
                })?;
                if remove_project_exclude(root, &link.target_path, false)? {
                    report.summary.exclude_updates += 1;
                }
                state.mark_skill_link_removed(link.id)?;
            }
            report.push(root_result(
                root,
                &link.skill_name,
                Status::Cleaned,
                if dry_run {
                    "would remove managed skill link with missing source"
                } else {
                    "removed managed skill link with missing source"
                },
            ));
        }
        Ok(_) => report.push(root_result(
            root,
            &link.skill_name,
            Status::Conflict,
            "managed record no longer matches a directory link; preserved target",
        )),
    }
    Ok(())
}

pub fn clean_stale(state: &State, dry_run: bool) -> Result<Report> {
    let mut report = Report::new(0);
    for link in state.managed_skill_links()? {
        let source_valid = link.source_path.exists()
            && validate_skill(
                &link.skill_name,
                &link.source_path,
                link.source_path.parent().unwrap_or(&link.source_path),
            )
            .is_ok();
        if source_valid {
            continue;
        }
        match purge_link(&link, state, dry_run) {
            Ok(_) => report.push(RepoResult {
                repo: link
                    .repo_root
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "global".to_string()),
                adapter: ADAPTER_NAME.to_string(),
                source: link.source_path.display().to_string(),
                target: link.target_path.display().to_string(),
                status: Status::Cleaned,
                message: if dry_run {
                    "would remove stale managed skill link".to_string()
                } else {
                    "removed stale managed skill link".to_string()
                },
            }),
            Err(error) => report.push(RepoResult {
                repo: link
                    .repo_root
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "global".to_string()),
                adapter: ADAPTER_NAME.to_string(),
                source: link.source_path.display().to_string(),
                target: link.target_path.display().to_string(),
                status: Status::Conflict,
                message: error.to_string(),
            }),
        }
    }
    Ok(report)
}

pub fn check_managed_link(link: &ManagedSkillLink) -> Result<()> {
    let target_root = link
        .target_path
        .parent()
        .with_context(|| format!("skill target {} has no parent", link.target_path.display()))?;
    validate_recorded_target_parent(&link.target_path)?;
    let external = external_ownership(target_root)?;
    if external.blocks_all || external.managed.contains(&link.skill_name) {
        bail!("target is claimed by another skill synchronizer");
    }
    let metadata = fs::symlink_metadata(&link.target_path).with_context(|| {
        format!(
            "managed skill link {} is missing",
            link.target_path.display()
        )
    })?;
    if !metadata.file_type().is_symlink() {
        bail!("target is not a directory symlink");
    }
    let actual = fs::read_link(&link.target_path)?;
    if actual != link.expected_link {
        bail!("target no longer points to the recorded source");
    }
    Ok(())
}

pub fn purge_link(
    link: &ManagedSkillLink,
    state: &State,
    dry_run: bool,
) -> Result<Option<PathBuf>> {
    match fs::symlink_metadata(&link.target_path) {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            remove_recorded_project_exclude(link, dry_run)?;
            if !dry_run {
                state.mark_skill_link_removed(link.id)?;
            }
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    }
    check_managed_link(link)?;
    if let Some(repo_root) = &link.repo_root {
        let repo = git::discover_repo(repo_root)?
            .with_context(|| format!("repository no longer exists: {}", repo_root.display()))?;
        let target_rel = link.target_path.strip_prefix(&repo.root)?;
        if git::is_tracked(&repo, target_rel)? {
            bail!("target is tracked by Git");
        }
        if !dry_run {
            exclude::remove(&repo, target_rel, ExcludeMode::PerRepo, false)?;
        }
    }
    if !dry_run {
        fs::remove_file(&link.target_path)?;
        state.mark_skill_link_removed(link.id)?;
    }
    Ok(Some(link.target_path.clone()))
}

fn remove_recorded_project_exclude(link: &ManagedSkillLink, dry_run: bool) -> Result<()> {
    let Some(repo_root) = &link.repo_root else {
        return Ok(());
    };
    let repo = git::discover_repo(repo_root)?
        .with_context(|| format!("repository no longer exists: {}", repo_root.display()))?;
    let target_rel = link.target_path.strip_prefix(&repo.root)?;
    exclude::remove(&repo, target_rel, ExcludeMode::PerRepo, dry_run)?;
    Ok(())
}

pub fn print_migration_plain(report: &SkillMigrationReport) {
    if report.dry_run {
        println!("Dry run. No filesystem changes were made.");
    }
    println!("Migrated {} copied skills.", report.migrated.len());
    for item in &report.migrated {
        println!("  {}  {}", item.target.display(), item.reason);
    }
    println!("Skipped {} copied skills.", report.skipped.len());
    for item in &report.skipped {
        println!("  {}  {}", item.target.display(), item.reason);
    }
    println!("Errors {}.", report.errors.len());
    for item in &report.errors {
        println!("  {}  {}", item.target.display(), item.reason);
    }
}

pub fn migrate_recorded(state: &State, dry_run: bool) -> Result<SkillMigrationReport> {
    let mut report = SkillMigrationReport {
        dry_run,
        ..SkillMigrationReport::default()
    };
    for observed in state.active_skill_roots(usize::MAX)? {
        let root = match SkillRoot::from_observed(&observed) {
            Ok(root) => root,
            Err(error) => {
                report.errors.push(SkillMigrationItem {
                    skill: "*".to_string(),
                    source: observed.source_root,
                    target: observed.target_root,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        if let Err(error) = validate_target_parent(&root) {
            report.errors.push(SkillMigrationItem {
                skill: "*".to_string(),
                source: root.source_root,
                target: root.target_root,
                reason: error.to_string(),
            });
            continue;
        }
        let external = external_ownership(&root.target_root)?;
        let (skills, invalid) = discover_skills(&root.source_root)?;
        for (name, source, reason) in invalid {
            report.skipped.push(SkillMigrationItem {
                target: root.target_root.join(&name),
                skill: name,
                source,
                reason,
            });
        }
        for skill in skills {
            let target = root.target_root.join(&skill.name);
            let item = SkillMigrationItem {
                skill: skill.name.clone(),
                source: skill.source.clone(),
                target: target.clone(),
                reason: String::new(),
            };
            if external.blocks_all || external.managed.contains(&skill.name) {
                report.skipped.push(SkillMigrationItem {
                    reason: "target is owned by another synchronizer".to_string(),
                    ..item
                });
                continue;
            }
            let metadata = match fs::symlink_metadata(&target) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    report.errors.push(SkillMigrationItem {
                        reason: error.to_string(),
                        ..item
                    });
                    continue;
                }
            };
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                continue;
            }
            if let Some(target_rel) = root.target_rel(&target)?
                && git::is_tracked(
                    root.repo.as_ref().expect("project target has repo"),
                    &target_rel,
                )?
            {
                report.skipped.push(SkillMigrationItem {
                    reason: "target is tracked by Git".to_string(),
                    ..item
                });
                continue;
            }
            if let Err(error) = trees_match(&skill.source, &target) {
                report.skipped.push(SkillMigrationItem {
                    reason: error.to_string(),
                    ..item
                });
                continue;
            }
            if dry_run {
                report.migrated.push(SkillMigrationItem {
                    reason: "would replace identical copy with relative link".to_string(),
                    ..item
                });
                continue;
            }
            match migrate_copy(&root, &skill, &target) {
                Ok(()) => {
                    state.record_skill_link(SkillLinkRecord {
                        skill_root_id: observed.id,
                        skill_name: &skill.name,
                        source_path: &skill.source,
                        target_path: &target,
                        expected_link: &expected_link(&skill.source, &target)?,
                        owned: true,
                        status: "migrated",
                        error: None,
                    })?;
                    report.migrated.push(SkillMigrationItem {
                        reason: "replaced identical copy with relative link".to_string(),
                        ..item
                    });
                }
                Err(error) => report.errors.push(SkillMigrationItem {
                    reason: error.to_string(),
                    ..item
                }),
            }
        }
    }
    Ok(report)
}

fn migrate_copy(root: &SkillRoot, skill: &ValidSkill, target: &Path) -> Result<()> {
    let expected = expected_link(&skill.source, target)?;
    let parent = target
        .parent()
        .with_context(|| format!("target {} has no parent", target.display()))?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let backup = parent.join(format!(
        ".claude-md-symlinker-{}-{suffix}.backup",
        skill.name
    ));
    fs::rename(target, &backup).with_context(|| {
        format!(
            "failed to move copied skill {} to temporary backup {}",
            target.display(),
            backup.display()
        )
    })?;

    let mut exclude_added = false;
    let operation = (|| -> Result<()> {
        create_directory_symlink(&expected, target)?;
        if fs::read_link(target)? != expected {
            bail!("created skill link failed verification");
        }
        trees_match(&skill.source, &backup)?;
        exclude_added = ensure_project_exclude(root, target, false)?;
        fs::remove_dir_all(&backup)
            .with_context(|| format!("failed to remove temporary backup {}", backup.display()))?;
        Ok(())
    })();

    if let Err(error) = operation {
        if exclude_added {
            let _ = remove_project_exclude(root, target, false);
        }
        let _ = fs::remove_file(target);
        if backup.exists() {
            fs::rename(&backup, target).with_context(|| {
                format!(
                    "migration failed and rollback could not restore {}",
                    target.display()
                )
            })?;
        }
        return Err(error);
    }
    Ok(())
}

pub fn external_writer_message(target_root: &Path) -> Result<Option<String>> {
    let ownership = external_ownership(target_root)?;
    if ownership.markers.is_empty() {
        Ok(None)
    } else {
        Ok(Some(format!(
            "external skill synchronizer state found at {}",
            ownership.markers[0].display()
        )))
    }
}

fn external_ownership(target_root: &Path) -> Result<ExternalOwnership> {
    let mut ownership = ExternalOwnership::default();
    for name in EXTERNAL_STATE_FILES {
        let marker = target_root.join(name);
        let text = match fs::read_to_string(&marker) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", marker.display()));
            }
        };
        ownership.markers.push(marker.clone());
        let value: serde_json::Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => {
                ownership.blocks_all = true;
                continue;
            }
        };
        let mut found = false;
        for key in [
            "managed_skill_ids",
            "managed",
            "pending_skill_ids",
            "pending",
        ] {
            if let Some(items) = value.get(key).and_then(|value| value.as_array()) {
                found = true;
                for item in items.iter().filter_map(|item| item.as_str()) {
                    ownership.managed.insert(item.to_string());
                }
            }
        }
        if !found {
            ownership.blocks_all = true;
        }
    }
    Ok(ownership)
}

fn discover_skills(source_root: &Path) -> Result<(Vec<ValidSkill>, Vec<InvalidSkill>)> {
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    let mut entries = fs::read_dir(source_root)
        .with_context(|| format!("failed to read skill root {}", source_root.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let source = entry.path();
        match validate_skill(&name, &source, source_root) {
            Ok(()) => valid.push(ValidSkill { name, source }),
            Err(error) => invalid.push((name, source, error.to_string())),
        }
    }
    Ok((valid, invalid))
}

fn validate_skill(name: &str, source: &Path, source_root: &Path) -> Result<()> {
    if !valid_skill_name(name) {
        bail!(
            "skill directory name must be at most {MAX_SKILL_NAME_CHARS} characters and use lowercase letters, digits, and single hyphens"
        );
    }
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("skill source must be a real directory");
    }
    let canonical_root = source_root.canonicalize()?;
    let canonical_source = source.canonicalize()?;
    if !canonical_source.starts_with(&canonical_root) {
        bail!("skill source resolves outside the canonical skill root");
    }

    let skill_file = source.join("SKILL.md");
    let skill_metadata = fs::symlink_metadata(&skill_file)
        .with_context(|| format!("missing regular SKILL.md in {name}"))?;
    if skill_metadata.file_type().is_symlink() || !skill_metadata.is_file() {
        bail!("SKILL.md must be a regular file");
    }
    validate_skill_frontmatter(name, &skill_file)?;
    validate_skill_tree(source)?;
    Ok(())
}

fn validate_skill_frontmatter(expected_name: &str, path: &Path) -> Result<()> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        bail!("SKILL.md must start with YAML frontmatter");
    }
    let mut name = None;
    let mut description = None;
    let mut closed = false;
    for line in lines {
        if line == "---" {
            closed = true;
            break;
        }
        if let Some(value) = line.strip_prefix("name:") {
            name = Some(unquote(value.trim()));
        }
        if let Some(value) = line.strip_prefix("description:") {
            description = Some(unquote(value.trim()));
        }
    }
    if !closed {
        bail!("SKILL.md frontmatter is not closed");
    }
    let name = name
        .filter(|value| !value.is_empty())
        .context("SKILL.md frontmatter needs name")?;
    if name != expected_name {
        bail!("SKILL.md name `{name}` does not match directory `{expected_name}`");
    }
    let description = description
        .filter(|value| !value.is_empty())
        .context("SKILL.md frontmatter needs description")?;
    if description.chars().count() > MAX_SKILL_DESCRIPTION_CHARS {
        bail!("SKILL.md description exceeds {MAX_SKILL_DESCRIPTION_CHARS} characters");
    }
    Ok(())
}

fn validate_skill_tree(root: &Path) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                bail!("skill tree contains symlink {}", path.display());
            }
            if metadata.is_dir() {
                pending.push(path);
            } else if !metadata.is_file() {
                bail!("skill tree contains unsupported entry {}", path.display());
            }
        }
    }
    Ok(())
}

fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_SKILL_NAME_CHARS
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

fn unquote(value: &str) -> String {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn validate_source_root(root: &SkillRoot) -> Result<()> {
    if !root.source_root.starts_with(&root.base_dir) {
        bail!("Agent Skills source resolves outside its allowed base");
    }
    let relative = root.source_root.strip_prefix(&root.base_dir)?;
    let mut current = root.base_dir.clone();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            bail!("Agent Skills source contains an unsafe path component");
        };
        current.push(part);
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("failed to inspect source parent {}", current.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("source parent {} is a symlink", current.display());
        }
        if !metadata.is_dir() {
            bail!("source parent {} is not a directory", current.display());
        }
    }
    let canonical = root.source_root.canonicalize()?;
    if !canonical.starts_with(&root.base_dir) {
        bail!("Agent Skills source resolves outside its allowed base");
    }
    Ok(())
}

fn validate_target_parent(root: &SkillRoot) -> Result<()> {
    validate_parent_chain(&root.base_dir, &root.target_root, "Claude skill target")
}

fn validate_recorded_target_parent(target: &Path) -> Result<()> {
    let target_root = target
        .parent()
        .with_context(|| format!("skill target {} has no parent", target.display()))?;
    if target_root.file_name().and_then(|name| name.to_str()) != Some("skills") {
        bail!("recorded skill target is outside a .claude/skills root");
    }
    let claude_root = target_root
        .parent()
        .with_context(|| format!("skill root {} has no parent", target_root.display()))?;
    if claude_root.file_name().and_then(|name| name.to_str()) != Some(".claude") {
        bail!("recorded skill target is outside a .claude/skills root");
    }
    let base = claude_root
        .parent()
        .with_context(|| format!("Claude root {} has no parent", claude_root.display()))?;
    validate_parent_chain(base, target_root, "recorded Claude skill target")
}

fn validate_parent_chain(base: &Path, root: &Path, label: &str) -> Result<()> {
    if !root.starts_with(base) {
        bail!("{label} resolves outside its allowed base");
    }
    let relative = root.strip_prefix(base)?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            bail!("{label} contains an unsafe path component");
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!("target parent {} is a symlink", current.display());
                }
                if !metadata.is_dir() {
                    bail!("target parent {} is not a directory", current.display());
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn create_target_parent(root: &SkillRoot) -> Result<()> {
    validate_target_parent(root)?;
    fs::create_dir_all(&root.target_root)
        .with_context(|| format!("failed to create {}", root.target_root.display()))?;
    validate_target_parent(root)
}

fn expected_link(source: &Path, target: &Path) -> Result<PathBuf> {
    let parent = target
        .parent()
        .with_context(|| format!("target {} has no parent", target.display()))?;
    diff_paths(source, parent).with_context(|| {
        format!(
            "failed to calculate relative link from {} to {}",
            target.display(),
            source.display()
        )
    })
}

fn ensure_project_exclude(root: &SkillRoot, target: &Path, dry_run: bool) -> Result<bool> {
    let Some(repo) = &root.repo else {
        return Ok(false);
    };
    let target_rel = target.strip_prefix(&repo.root)?;
    exclude::ensure(repo, target_rel, ExcludeMode::PerRepo, dry_run)
}

fn remove_project_exclude(root: &SkillRoot, target: &Path, dry_run: bool) -> Result<bool> {
    let Some(repo) = &root.repo else {
        return Ok(false);
    };
    let target_rel = target.strip_prefix(&repo.root)?;
    exclude::remove(repo, target_rel, ExcludeMode::PerRepo, dry_run)
}

fn canonical_or_absolute(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", path.display()));
    }
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn root_result(
    root: &SkillRoot,
    skill: &str,
    status: Status,
    message: impl Into<String>,
) -> RepoResult {
    RepoResult {
        repo: root.report_repo(),
        adapter: ADAPTER_NAME.to_string(),
        source: root.source_root.join(skill).display().to_string(),
        target: root.target_root.join(skill).display().to_string(),
        status,
        message: message.into(),
    }
}

fn trees_match(source: &Path, target: &Path) -> Result<()> {
    let source_manifest = tree_manifest(source)?;
    let target_manifest = tree_manifest(target)?;
    if source_manifest != target_manifest {
        bail!("copied target differs from canonical source");
    }
    Ok(())
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
enum ManifestEntry {
    Directory,
    File { hash: String, executable: bool },
}

fn tree_manifest(root: &Path) -> Result<BTreeMap<PathBuf, ManifestEntry>> {
    let mut manifest = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(root)?.to_path_buf();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                bail!("tree contains symlink {}", path.display());
            }
            if metadata.is_dir() {
                manifest.insert(relative, ManifestEntry::Directory);
                pending.push(path);
            } else if metadata.is_file() {
                let bytes = fs::read(&path)?;
                let mut hasher = Sha256::new();
                hasher.update(bytes);
                manifest.insert(
                    relative,
                    ManifestEntry::File {
                        hash: format!("{:x}", hasher.finalize()),
                        executable: is_executable(&metadata),
                    },
                );
            } else {
                bail!("tree contains unsupported entry {}", path.display());
            }
        }
    }
    Ok(manifest)
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn create_directory_symlink(source: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, target)
}

#[cfg(windows)]
fn create_directory_symlink(_source: &Path, _target: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "directory skill links are supported only on Linux and macOS",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use std::{fs, path::Path, process::Command};

    use tempfile::TempDir;

    use super::{
        SkillRoot, check_managed_link, migrate_recorded, reconcile_global, reconcile_project_dir,
    };
    use crate::{git, state::State};

    struct Fixture {
        home: TempDir,
        data: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                home: tempfile::tempdir().unwrap(),
                data: tempfile::tempdir().unwrap(),
            }
        }

        fn state(&self) -> State {
            State::open(self.data.path().to_path_buf()).unwrap()
        }

        fn add_skill(&self, name: &str) -> std::path::PathBuf {
            add_skill(self.home.path(), name)
        }

        fn target(&self, name: &str) -> std::path::PathBuf {
            self.home.path().join(".claude/skills").join(name)
        }
    }

    fn add_skill(base: &Path, name: &str) -> std::path::PathBuf {
        let skill = base.join(".agents/skills").join(name);
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Test skill\n---\n\nUse it.\n"),
        )
        .unwrap();
        skill
    }

    fn init_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg("-q")
                .arg(path)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["-C", path.to_str().unwrap(), "config", "user.name", "Test"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args([
                    "-C",
                    path.to_str().unwrap(),
                    "config",
                    "user.email",
                    "test@example.com"
                ])
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn global_skill_is_one_relative_link_and_source_edits_are_immediate() {
        let fixture = Fixture::new();
        let source = fixture.add_skill("deploy");
        fs::create_dir_all(source.join("scripts")).unwrap();
        fs::write(source.join("scripts/run.sh"), "first\n").unwrap();
        let state = fixture.state();

        let report = reconcile_global(fixture.home.path(), &state, false).unwrap();
        assert_eq!(report.summary.created, 1);
        let target = fixture.target("deploy");
        assert!(
            fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_link(&target).unwrap(),
            Path::new("../../.agents/skills/deploy")
        );
        assert!(
            !fs::symlink_metadata(fixture.home.path().join(".claude/skills"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(target.join("scripts/run.sh")).unwrap(),
            "first\n"
        );

        fs::write(source.join("scripts/run.sh"), "second\n").unwrap();
        assert_eq!(
            fs::read_to_string(target.join("scripts/run.sh")).unwrap(),
            "second\n"
        );
    }

    #[test]
    fn unmanaged_skills_coexist_and_are_not_replaced() {
        let fixture = Fixture::new();
        fixture.add_skill("deploy");
        let target = fixture.target("deploy");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("SKILL.md"), "user copy\n").unwrap();

        let report = reconcile_global(fixture.home.path(), &fixture.state(), false).unwrap();
        assert_eq!(report.summary.conflicts, 1);
        assert!(target.is_dir());
        assert_eq!(
            fs::read_to_string(target.join("SKILL.md")).unwrap(),
            "user copy\n"
        );
    }

    #[test]
    fn exact_existing_link_is_adopted() {
        let fixture = Fixture::new();
        fixture.add_skill("deploy");
        let target = fixture.target("deploy");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("../../.agents/skills/deploy", &target).unwrap();

        let state = fixture.state();
        let report = reconcile_global(fixture.home.path(), &state, false).unwrap();
        assert_eq!(report.summary.kept, 1);
        assert_eq!(state.counts().unwrap().managed_skills, 1);
    }

    #[test]
    fn malformed_skill_is_reported_without_target() {
        let fixture = Fixture::new();
        let source = fixture.home.path().join(".agents/skills/bad");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), "no frontmatter\n").unwrap();

        let report = reconcile_global(fixture.home.path(), &fixture.state(), false).unwrap();
        assert_eq!(report.summary.conflicts, 1);
        assert!(!fixture.target("bad").exists());
    }

    #[test]
    fn skill_tree_with_symlink_is_rejected() {
        let fixture = Fixture::new();
        let source = fixture.add_skill("unsafe");
        fs::write(fixture.home.path().join("outside"), "secret").unwrap();
        std::os::unix::fs::symlink(
            fixture.home.path().join("outside"),
            source.join("reference"),
        )
        .unwrap();

        let report = reconcile_global(fixture.home.path(), &fixture.state(), false).unwrap();
        assert_eq!(report.summary.conflicts, 1);
        assert!(!fixture.target("unsafe").exists());
    }

    #[test]
    fn source_deletion_removes_only_the_managed_link() {
        let fixture = Fixture::new();
        let source = fixture.add_skill("deploy");
        let state = fixture.state();
        reconcile_global(fixture.home.path(), &state, false).unwrap();
        fs::remove_dir_all(source).unwrap();

        let report = reconcile_global(fixture.home.path(), &state, false).unwrap();
        assert_eq!(report.summary.cleaned, 1);
        assert!(fs::symlink_metadata(fixture.target("deploy")).is_err());
        assert_eq!(state.counts().unwrap().managed_skills, 0);
    }

    #[test]
    fn changed_managed_link_is_preserved() {
        let fixture = Fixture::new();
        let source = fixture.add_skill("deploy");
        let state = fixture.state();
        reconcile_global(fixture.home.path(), &state, false).unwrap();
        let target = fixture.target("deploy");
        fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink("elsewhere", &target).unwrap();
        fs::remove_dir_all(source).unwrap();

        let report = reconcile_global(fixture.home.path(), &state, false).unwrap();
        assert_eq!(report.summary.conflicts, 1);
        assert_eq!(fs::read_link(target).unwrap(), Path::new("elsewhere"));
        let link = state.managed_skill_links().unwrap().pop().unwrap();
        assert!(check_managed_link(&link).is_err());
    }

    #[test]
    fn identical_copy_requires_explicit_migration() {
        let fixture = Fixture::new();
        let source = fixture.add_skill("deploy");
        fs::create_dir_all(source.join("scripts")).unwrap();
        let script = source.join("scripts/run.sh");
        fs::write(&script, "run\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let target = fixture.target("deploy");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::create_dir_all(target.join("scripts")).unwrap();
        fs::copy(source.join("SKILL.md"), target.join("SKILL.md")).unwrap();
        fs::copy(&script, target.join("scripts/run.sh")).unwrap();
        fs::set_permissions(
            target.join("scripts/run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let state = fixture.state();

        let normal = reconcile_global(fixture.home.path(), &state, false).unwrap();
        assert_eq!(normal.summary.conflicts, 1);
        assert!(target.is_dir());

        let migrated = migrate_recorded(&state, false).unwrap();
        assert_eq!(migrated.migrated.len(), 1);
        assert!(
            fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::write(source.join("SKILL.md"), "changed\n").unwrap();
        assert_eq!(
            fs::read_to_string(target.join("SKILL.md")).unwrap(),
            "changed\n"
        );
    }

    #[test]
    fn different_copy_is_not_migrated() {
        let fixture = Fixture::new();
        fixture.add_skill("deploy");
        let target = fixture.target("deploy");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("SKILL.md"), "different\n").unwrap();
        let state = fixture.state();
        reconcile_global(fixture.home.path(), &state, false).unwrap();

        let report = migrate_recorded(&state, false).unwrap();
        assert_eq!(report.skipped.len(), 1);
        assert!(target.is_dir());
    }

    #[test]
    fn external_writer_blocks_automatic_ownership_and_migration() {
        let fixture = Fixture::new();
        let source = fixture.add_skill("deploy");
        let target = fixture.target("deploy");
        fs::create_dir_all(&target).unwrap();
        fs::copy(source.join("SKILL.md"), target.join("SKILL.md")).unwrap();
        fs::write(
            target.parent().unwrap().join(".onurpi-agents-sync.json"),
            r#"{"managed_skill_ids":["deploy"],"pending_skill_ids":[]}"#,
        )
        .unwrap();
        let state = fixture.state();

        let normal = reconcile_global(fixture.home.path(), &state, false).unwrap();
        assert_eq!(normal.summary.conflicts, 1);
        assert_eq!(state.counts().unwrap().external_skill_conflicts, 1);
        let migration = migrate_recorded(&state, false).unwrap();
        assert_eq!(migration.skipped.len(), 1);
        assert!(target.is_dir());
    }

    #[test]
    fn project_link_is_relative_ignored_and_survives_repo_move() {
        let parent = tempfile::tempdir().unwrap();
        let repo_path = parent.path().join("repo");
        init_repo(&repo_path);
        add_skill(&repo_path, "deploy");
        let repo = git::discover_repo(&repo_path).unwrap().unwrap();
        let data = tempfile::tempdir().unwrap();
        let state = State::open(data.path().to_path_buf()).unwrap();

        let report = reconcile_project_dir(&repo, &repo.root, &state, false).unwrap();
        assert_eq!(report.summary.created, 1);
        let target = repo.root.join(".claude/skills/deploy");
        assert_eq!(
            fs::read_link(&target).unwrap(),
            Path::new("../../.agents/skills/deploy")
        );
        let exclude = fs::read_to_string(&repo.exclude_path).unwrap();
        assert!(exclude.contains("/.claude/skills/deploy"));
        assert!(!exclude.lines().any(|line| line == "/.claude/skills"));

        let moved = parent.path().join("moved");
        fs::rename(&repo_path, &moved).unwrap();
        assert!(moved.join(".claude/skills/deploy/SKILL.md").is_file());
    }

    #[test]
    fn copied_skill_migration_rolls_back_when_exclude_update_fails() {
        let parent = tempfile::tempdir().unwrap();
        let repo_path = parent.path().join("repo");
        init_repo(&repo_path);
        let source = add_skill(&repo_path, "deploy");
        let target = repo_path.join(".claude/skills/deploy");
        fs::create_dir_all(&target).unwrap();
        fs::copy(source.join("SKILL.md"), target.join("SKILL.md")).unwrap();
        let repo = git::discover_repo(&repo_path).unwrap().unwrap();
        let data = tempfile::tempdir().unwrap();
        let state = State::open(data.path().to_path_buf()).unwrap();
        reconcile_project_dir(&repo, &repo.root, &state, false).unwrap();

        let outside = parent.path().join("outside-exclude");
        fs::write(&outside, "keep\n").unwrap();
        fs::remove_file(&repo.exclude_path).unwrap();
        std::os::unix::fs::symlink(&outside, &repo.exclude_path).unwrap();

        let report = migrate_recorded(&state, false).unwrap();
        assert_eq!(report.errors.len(), 1);
        assert!(target.is_dir());
        assert!(
            !fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(target.join("SKILL.md")).unwrap(),
            fs::read_to_string(source.join("SKILL.md")).unwrap()
        );
        assert_eq!(fs::read_to_string(outside).unwrap(), "keep\n");
    }

    #[test]
    fn source_parent_symlink_is_rejected() {
        let fixture = Fixture::new();
        let elsewhere = fixture.home.path().join("elsewhere/skills");
        let source = elsewhere.join("deploy");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("SKILL.md"),
            "---\nname: deploy\ndescription: Test skill\n---\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            fixture.home.path().join("elsewhere"),
            fixture.home.path().join(".agents"),
        )
        .unwrap();

        let report = reconcile_global(fixture.home.path(), &fixture.state(), false).unwrap();
        assert_eq!(report.summary.errors, 1);
        assert!(!fixture.target("deploy").exists());
    }

    #[test]
    fn purge_preserves_a_recorded_link_after_target_parent_is_replaced() {
        let fixture = Fixture::new();
        fixture.add_skill("deploy");
        let state = fixture.state();
        reconcile_global(fixture.home.path(), &state, false).unwrap();
        let target = fixture.target("deploy");
        let target_root = target.parent().unwrap().to_path_buf();
        fs::remove_file(&target).unwrap();
        fs::remove_dir(&target_root).unwrap();
        let outside = fixture.home.path().join("outside");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink("../../.agents/skills/deploy", outside.join("deploy")).unwrap();
        std::os::unix::fs::symlink(&outside, &target_root).unwrap();

        let report = crate::purge::purge(&state, false).unwrap();

        assert_eq!(report.skipped.len(), 1);
        assert!(fs::symlink_metadata(outside.join("deploy")).is_ok());
    }

    #[test]
    fn target_parent_symlink_is_rejected() {
        let fixture = Fixture::new();
        fixture.add_skill("deploy");
        fs::create_dir_all(fixture.home.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(
            fixture.home.path().join("elsewhere"),
            fixture.home.path().join(".claude"),
        )
        .unwrap();

        let error = reconcile_global(fixture.home.path(), &fixture.state(), false).unwrap_err();
        assert!(error.to_string().contains("target parent"));
        assert!(!fixture.home.path().join("elsewhere/skills/deploy").exists());
    }

    #[test]
    fn dry_run_does_not_create_state_or_links() {
        let fixture = Fixture::new();
        fixture.add_skill("deploy");
        let state = fixture.state();
        let report = reconcile_global(fixture.home.path(), &state, true).unwrap();
        assert_eq!(report.summary.created, 1);
        assert!(!fixture.target("deploy").exists());
        assert_eq!(state.counts().unwrap().skill_roots, 0);
    }

    #[test]
    fn observed_root_round_trips_through_state() {
        let fixture = Fixture::new();
        fixture.add_skill("deploy");
        let state = fixture.state();
        reconcile_global(fixture.home.path(), &state, false).unwrap();
        let roots = state.active_skill_roots(10).unwrap();
        assert_eq!(roots.len(), 1);
        let reconstructed = SkillRoot::from_observed(&roots[0]).unwrap();
        assert_eq!(reconstructed.base_dir, fixture.home.path());
    }
}
