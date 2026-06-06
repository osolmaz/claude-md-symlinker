use std::{collections::BTreeMap, path::PathBuf};

use anyhow::Result;

use crate::{
    adapters::{self, Adapter},
    config::{AppConfig, ExcludeMode},
    discovery,
    git::{self, GitRepo},
    materializer::{self, MaterializationKind, TargetState},
    reporting::{RepoResult, Report, Status},
    state::{ShimRecord, State},
};

#[derive(Debug, Clone, Copy)]
pub struct CleanOptions {
    pub dry_run: bool,
    pub remove_if_source_missing: bool,
}

pub fn clean(
    config: &AppConfig,
    config_existed: bool,
    cli_roots: &[PathBuf],
    state: &State,
    options: CleanOptions,
) -> Result<Report> {
    crate::exclude::validate_mode(config.git.exclude_mode)?;
    let scope = config.scan_scope(config_existed, cli_roots)?;
    let repos = discovery::discover(&scope)?;
    let adapters = adapters::enabled_adapters(config)?;
    let exclude_counts = exclude_path_counts(&repos);
    let mut report = Report::new(repos.len());

    for repo in repos {
        let shared_exclude = exclude_counts
            .get(&repo.exclude_path)
            .copied()
            .unwrap_or_default()
            > 1;
        for adapter in &adapters {
            let (result, exclude_updated) = clean_adapter(
                &repo,
                adapter,
                state,
                config.git.exclude_mode,
                options,
                shared_exclude,
            )
            .unwrap_or_else(|error| {
                (
                    result_for(&repo, adapter, Status::Error, error.to_string()),
                    false,
                )
            });
            if exclude_updated {
                report.summary.exclude_updates += 1;
            }
            report.push(result);
        }
    }

    Ok(report)
}

fn clean_adapter(
    repo: &GitRepo,
    adapter: &Adapter,
    state: &State,
    exclude_mode: ExcludeMode,
    options: CleanOptions,
    shared_exclude: bool,
) -> Result<(RepoResult, bool)> {
    let source_exists = materializer::source_exists(repo, adapter)?;
    let mut target_state = materializer::classify(repo, adapter)?;
    let stored_kind = stored_managed_kind(repo, adapter, state)?;
    if matches!(target_state, TargetState::ManagedHardlink)
        && stored_kind != Some(MaterializationKind::Hardlink)
    {
        target_state = TargetState::UnknownRegularFile;
    }
    let unmanaged_target_exists = matches!(
        target_state,
        TargetState::UnknownRegularFile | TargetState::UnknownSymlink | TargetState::Other
    );
    let stored_missing_kind = if matches!(target_state, TargetState::Missing) {
        stored_kind
    } else {
        None
    };
    let stale_missing_managed_target = stored_missing_kind.is_some() && !shared_exclude;
    let target_can_be_removed = target_managed_kind(&target_state).is_some();
    let managed_kind = target_managed_kind(&target_state).or(stored_missing_kind);
    let managed = managed_kind.is_some();

    if unmanaged_target_exists && managed_kind.is_none() {
        let exclude_updated =
            crate::exclude::remove(repo, &adapter.target, exclude_mode, options.dry_run)?;
        let status = if source_exists {
            Status::Conflict
        } else {
            Status::NoSource
        };
        let mut message = if source_exists {
            "target exists and is not managed; leaving it visible to Git".to_string()
        } else {
            "source file does not exist; target is not managed".to_string()
        };
        if exclude_updated {
            if options.dry_run {
                message.push_str("; would remove stale Git exclude");
            } else {
                message.push_str("; Git exclude removed");
            }
        }
        let result = result_for(repo, adapter, status, message);
        if !options.dry_run {
            record(state, repo, adapter, None, status, &result.message)?;
        }
        return Ok((result, exclude_updated));
    }

    if source_exists || !managed {
        let result = result_for(repo, adapter, Status::Kept, "nothing to clean");
        if !options.dry_run {
            record(
                state,
                repo,
                adapter,
                managed_kind,
                Status::Kept,
                &result.message,
            )?;
        }
        return Ok((result, false));
    }

    if !options.remove_if_source_missing {
        let result = result_for(
            repo,
            adapter,
            Status::NoSource,
            "managed shim is stale; pass --remove-if-source-missing to remove it",
        );
        if !options.dry_run {
            record(
                state,
                repo,
                adapter,
                managed_kind,
                Status::NoSource,
                &result.message,
            )?;
        }
        return Ok((result, false));
    }

    if git::is_tracked(repo, &adapter.target)? {
        let exclude_updated =
            crate::exclude::remove(repo, &adapter.target, exclude_mode, options.dry_run)?;
        let result = result_for(
            repo,
            adapter,
            Status::TrackedConflict,
            tracked_conflict_message(exclude_updated, options.dry_run),
        );
        if !options.dry_run {
            record(
                state,
                repo,
                adapter,
                None,
                Status::TrackedConflict,
                &result.message,
            )?;
        }
        return Ok((result, exclude_updated));
    }

    if target_can_be_removed {
        crate::exclude::remove(repo, &adapter.target, exclude_mode, true)?;
    }
    let removed = if target_can_be_removed {
        materializer::remove_target(repo, adapter, options.dry_run)?
    } else {
        false
    };
    let should_update_exclude =
        stale_missing_managed_target || removed || (target_can_be_removed && options.dry_run);
    let exclude_updated = if should_update_exclude {
        crate::exclude::remove(repo, &adapter.target, exclude_mode, options.dry_run)?
    } else {
        false
    };
    let result = if removed || exclude_updated {
        let mut message = if removed {
            if options.dry_run {
                "would remove stale managed shim".to_string()
            } else {
                "removed stale managed shim".to_string()
            }
        } else if options.dry_run {
            "would remove stale managed shim exclude".to_string()
        } else {
            "removed stale managed shim exclude".to_string()
        };
        if exclude_updated {
            message.push_str("; Git exclude updated");
        }
        result_for(repo, adapter, Status::Cleaned, message)
    } else {
        result_for(repo, adapter, Status::Kept, "nothing to clean")
    };
    if !options.dry_run {
        let materialization = if result.status == Status::Kept {
            managed_kind
        } else {
            None
        };
        record(
            state,
            repo,
            adapter,
            materialization,
            result.status,
            &result.message,
        )?;
    }
    Ok((result, exclude_updated))
}

fn record(
    state: &State,
    repo: &GitRepo,
    adapter: &Adapter,
    materialization: Option<MaterializationKind>,
    status: Status,
    message: &str,
) -> Result<()> {
    let content_hash = materializer::source_hash(repo, adapter)?;

    state.record(ShimRecord {
        repo,
        adapter_name: &adapter.name,
        source_rel_path: &adapter.source.to_string_lossy(),
        target_rel_path: &adapter.target.to_string_lossy(),
        materialization,
        content_hash,
        status,
        message,
    })
}

fn target_managed_kind(target_state: &TargetState) -> Option<MaterializationKind> {
    match target_state {
        TargetState::ManagedSymlink { .. } => Some(MaterializationKind::Symlink),
        TargetState::ManagedCopy { .. } => Some(MaterializationKind::Copy),
        TargetState::ManagedHardlink => Some(MaterializationKind::Hardlink),
        _ => None,
    }
}

fn stored_managed_kind(
    repo: &GitRepo,
    adapter: &Adapter,
    state: &State,
) -> Result<Option<MaterializationKind>> {
    Ok(state
        .get_shim(repo, &adapter.name, &adapter.target.to_string_lossy())?
        .and_then(|stored| stored.materialization)
        .and_then(|kind| MaterializationKind::from_state_value(&kind)))
}

fn exclude_path_counts(repos: &[GitRepo]) -> BTreeMap<PathBuf, usize> {
    let mut counts = BTreeMap::new();
    for repo in repos {
        *counts.entry(repo.exclude_path.clone()).or_insert(0) += 1;
    }
    counts
}

fn result_for(
    repo: &GitRepo,
    adapter: &Adapter,
    status: Status,
    message: impl Into<String>,
) -> RepoResult {
    RepoResult {
        repo: repo.root.display().to_string(),
        adapter: adapter.name.clone(),
        source: adapter.source.display().to_string(),
        target: adapter.target.display().to_string(),
        status,
        message: message.into(),
    }
}

fn tracked_conflict_message(exclude_updated: bool, dry_run: bool) -> String {
    let mut message = "target is tracked by Git; leaving it untouched".to_string();
    if exclude_updated {
        if dry_run {
            message.push_str("; would remove stale Git exclude");
        } else {
            message.push_str("; removed stale Git exclude");
        }
    } else {
        message.push_str(" and not excluding it");
    }
    message
}
