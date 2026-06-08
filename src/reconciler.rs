use std::{collections::BTreeMap, fs, io::ErrorKind, path::PathBuf};

use anyhow::{Context, Result};

use crate::{
    adapters::{self, Adapter},
    config::{AppConfig, SourceMissingBehavior},
    discovery, exclude, git,
    git::GitRepo,
    materializer::{self, MaterializationKind, TargetState},
    reporting::{RepoResult, Report, Status},
    state::{ShimRecord, State, TargetedShimRecord},
};

#[derive(Debug, Clone, Copy)]
pub struct ReconcileOptions {
    pub dry_run: bool,
}

pub fn apply(
    config: &AppConfig,
    config_existed: bool,
    cli_roots: &[PathBuf],
    state: &State,
    options: ReconcileOptions,
) -> Result<Report> {
    exclude::validate_mode(config.git.exclude_mode)?;
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
            let (result, exclude_updated) =
                reconcile_adapter(config, &repo, adapter, state, options, shared_exclude)
                    .unwrap_or_else(|error| {
                        (
                            result_for(&repo, adapter, Status::Error, error.to_string()),
                            false,
                        )
                    });

            if result.status == Status::Error && !options.dry_run {
                let _ = state.record(ShimRecord {
                    repo: &repo,
                    adapter_name: &adapter.name,
                    source_rel_path: &adapter.source.to_string_lossy(),
                    target_rel_path: &adapter.target.to_string_lossy(),
                    materialization: None,
                    content_hash: None,
                    status: Status::Error,
                    message: &result.message,
                });
            }

            if exclude_updated {
                report.summary.exclude_updates += 1;
            }
            report.push(result);
        }
    }

    Ok(report)
}

pub fn apply_instruction_dir(
    repo: &GitRepo,
    instruction_dir: &std::path::Path,
    state: &State,
    options: ReconcileOptions,
) -> Result<Report> {
    let config = AppConfig::default();
    let adapter = adapter_for_instruction_dir(repo, instruction_dir)?;
    let mut report = Report::new(1);
    let (result, exclude_updated) = reconcile_adapter_in_dir(
        &config,
        repo,
        instruction_dir,
        &adapter,
        state,
        options,
        false,
    )
    .unwrap_or_else(|error| {
        let message = error.to_string();
        if !options.dry_run {
            let _ = state.mark_instruction_result(instruction_dir, Some("error"), Some(&message));
            let _ = state.record_event(
                "error",
                Some(&repo.root),
                Some(&adapter.name),
                "error",
                &message,
            );
        }
        (result_for(repo, &adapter, Status::Error, message), false)
    });
    if exclude_updated {
        report.summary.exclude_updates += 1;
    }
    report.push(result);
    Ok(report)
}

fn reconcile_adapter(
    config: &AppConfig,
    repo: &GitRepo,
    adapter: &Adapter,
    state: &State,
    options: ReconcileOptions,
    shared_exclude: bool,
) -> Result<(RepoResult, bool)> {
    reconcile_adapter_in_dir(
        config,
        repo,
        &repo.root,
        adapter,
        state,
        options,
        shared_exclude,
    )
}

fn reconcile_adapter_in_dir(
    config: &AppConfig,
    repo: &GitRepo,
    instruction_dir: &std::path::Path,
    adapter: &Adapter,
    state: &State,
    options: ReconcileOptions,
    shared_exclude: bool,
) -> Result<(RepoResult, bool)> {
    let target = repo.root.join(&adapter.target);

    if !materializer::source_exists(repo, adapter)? {
        let target_state = materializer::classify(repo, adapter)?;
        let stored_kind = stored_managed_kind(repo, adapter, state)?;
        let stored_missing_kind = if matches!(target_state, TargetState::Missing) {
            stored_kind
        } else {
            None
        };
        let stale_missing_managed_target = stored_missing_kind.is_some() && !shared_exclude;
        let target_can_be_removed = target_managed_kind(&target_state).is_some();
        let managed_kind = target_managed_kind(&target_state).or(stored_missing_kind);

        if git::is_tracked(repo, &adapter.target)
            .with_context(|| format!("failed to check tracked target {}", target.display()))?
        {
            let exclude_updated = exclude::remove(
                repo,
                &adapter.target,
                config.git.exclude_mode,
                options.dry_run,
            )?;
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
                    instruction_dir,
                    adapter,
                    None,
                    Status::TrackedConflict,
                    &result.message,
                )?;
            }
            return Ok((result, exclude_updated));
        }

        if adapter.on_source_missing == SourceMissingBehavior::RemoveIfManaged
            && (target_can_be_removed || stale_missing_managed_target)
        {
            if target_can_be_removed {
                exclude::remove(repo, &adapter.target, config.git.exclude_mode, true)?;
            }
            let removed = if target_can_be_removed {
                materializer::remove_target(repo, adapter, options.dry_run)?
            } else {
                false
            };
            let should_remove_exclude = stale_missing_managed_target
                || removed
                || (target_can_be_removed && options.dry_run);
            let exclude_updated = if should_remove_exclude {
                exclude::remove(
                    repo,
                    &adapter.target,
                    config.git.exclude_mode,
                    options.dry_run,
                )?
            } else {
                false
            };
            let mut message = if removed {
                if options.dry_run {
                    "would remove stale managed shim".to_string()
                } else {
                    "removed stale managed shim".to_string()
                }
            } else if exclude_updated {
                if options.dry_run {
                    "would remove stale managed shim exclude".to_string()
                } else {
                    "removed stale managed shim exclude".to_string()
                }
            } else {
                "stale managed shim already absent".to_string()
            };
            if exclude_updated {
                message.push_str("; Git exclude updated");
            }
            let status = if removed || exclude_updated {
                Status::Cleaned
            } else {
                Status::Kept
            };
            let result = result_for(repo, adapter, status, message);
            if !options.dry_run {
                let materialization = if result.status == Status::Kept {
                    managed_kind
                } else {
                    None
                };
                record(
                    state,
                    repo,
                    instruction_dir,
                    adapter,
                    materialization,
                    result.status,
                    &result.message,
                )?;
            }
            return Ok((result, exclude_updated));
        }

        let unmanaged_target_exists = matches!(
            target_state,
            TargetState::UnknownRegularFile | TargetState::UnknownSymlink | TargetState::Other
        );
        let should_remove_exclude = unmanaged_target_exists || stale_missing_managed_target;
        let exclude_updated = if target_can_be_removed {
            exclude::ensure(
                repo,
                &adapter.target,
                config.git.exclude_mode,
                options.dry_run,
            )?
        } else if !should_remove_exclude {
            false
        } else {
            exclude::remove(
                repo,
                &adapter.target,
                config.git.exclude_mode,
                options.dry_run,
            )?
        };

        let result = result_for(
            repo,
            adapter,
            Status::NoSource,
            if exclude_updated {
                if target_can_be_removed {
                    "source file does not exist; managed shim kept and Git exclude repaired"
                } else {
                    "source file does not exist; Git exclude removed"
                }
            } else {
                "source file does not exist"
            },
        );
        if !options.dry_run {
            record(
                state,
                repo,
                instruction_dir,
                adapter,
                managed_kind,
                Status::NoSource,
                &result.message,
            )?;
        }
        return Ok((result, exclude_updated));
    }

    let _source_hash = materializer::source_hash(repo, adapter)?;

    if git::is_tracked(repo, &adapter.target)
        .with_context(|| format!("failed to check tracked target {}", target.display()))?
    {
        let exclude_updated = exclude::remove(
            repo,
            &adapter.target,
            config.git.exclude_mode,
            options.dry_run,
        )?;
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
                instruction_dir,
                adapter,
                None,
                Status::TrackedConflict,
                &result.message,
            )?;
        }
        return Ok((result, exclude_updated));
    }

    let target_state = materializer::classify(repo, adapter)?;
    if matches!(target_state, TargetState::ManagedHardlink)
        && stored_managed_kind(repo, adapter, state)? != Some(MaterializationKind::Hardlink)
    {
        return unmanaged_conflict(config, repo, instruction_dir, adapter, state, options);
    }

    match target_state {
        TargetState::UnknownRegularFile => {
            unmanaged_conflict(config, repo, instruction_dir, adapter, state, options)
        }
        TargetState::UnknownSymlink | TargetState::Other => {
            unmanaged_conflict(config, repo, instruction_dir, adapter, state, options)
        }
        target_state => {
            let previous_state = target_state.clone();
            let exclude_updated = exclude::ensure(
                repo,
                &adapter.target,
                config.git.exclude_mode,
                options.dry_run,
            )?;
            let outcome = materializer::create_or_refresh(
                repo,
                adapter,
                &config.materialization,
                options.dry_run,
            )
            .inspect_err(|_| {
                if !options.dry_run && target_is_missing(repo, adapter) {
                    let _ = exclude::remove(
                        repo,
                        &adapter.target,
                        config.git.exclude_mode,
                        options.dry_run,
                    );
                }
            })?;

            let status = status_for(previous_state, outcome.kind, outcome.changed);
            let mut message = message_for(status, outcome.kind, options.dry_run);
            if exclude_updated {
                message.push_str("; Git exclude updated");
            }

            let result = result_for(repo, adapter, status, message);
            if !options.dry_run {
                record(
                    state,
                    repo,
                    instruction_dir,
                    adapter,
                    Some(outcome.kind),
                    result.status,
                    &result.message,
                )?;
            }

            Ok((result, exclude_updated))
        }
    }
}

fn target_is_missing(repo: &GitRepo, adapter: &Adapter) -> bool {
    matches!(
        fs::symlink_metadata(repo.root.join(&adapter.target)),
        Err(error) if error.kind() == ErrorKind::NotFound
    )
}

fn unmanaged_conflict(
    config: &AppConfig,
    repo: &GitRepo,
    instruction_dir: &std::path::Path,
    adapter: &Adapter,
    state: &State,
    options: ReconcileOptions,
) -> Result<(RepoResult, bool)> {
    let exclude_updated = exclude::remove(
        repo,
        &adapter.target,
        config.git.exclude_mode,
        options.dry_run,
    )?;
    let mut message = "target exists and is not managed; leaving it visible to Git".to_string();
    if exclude_updated {
        message.push_str("; Git exclude removed");
    }
    let result = result_for(repo, adapter, Status::Conflict, message);
    if !options.dry_run {
        record(
            state,
            repo,
            instruction_dir,
            adapter,
            None,
            Status::Conflict,
            &result.message,
        )?;
    }
    Ok((result, exclude_updated))
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

fn target_managed_kind(target_state: &TargetState) -> Option<MaterializationKind> {
    Some(match target_state {
        TargetState::ManagedSymlink { .. } => MaterializationKind::Symlink,
        TargetState::ManagedCopy { .. } => MaterializationKind::Copy,
        TargetState::ManagedHardlink => MaterializationKind::Hardlink,
        _ => return None,
    })
}

fn status_for(
    previous_state: TargetState,
    outcome_kind: MaterializationKind,
    changed: bool,
) -> Status {
    if changed
        && target_managed_kind(&previous_state)
            .map(|previous_kind| previous_kind != outcome_kind)
            .unwrap_or(false)
    {
        return Status::Repaired;
    }

    match previous_state {
        TargetState::Missing => Status::Created,
        TargetState::ManagedSymlink { .. } if changed => Status::Repaired,
        TargetState::ManagedHardlink if changed => Status::Repaired,
        TargetState::ManagedSymlink { .. } | TargetState::ManagedHardlink => Status::Kept,
        TargetState::ManagedCopy { .. } if changed => Status::Refreshed,
        TargetState::ManagedCopy { .. } => Status::Kept,
        TargetState::UnknownRegularFile | TargetState::UnknownSymlink | TargetState::Other => {
            unreachable!("unmanaged targets are handled before materialization")
        }
    }
}

fn message_for(status: Status, kind: MaterializationKind, dry_run: bool) -> String {
    match status {
        Status::Created if dry_run => format!("would create {kind:?} shim"),
        Status::Created => format!("created {kind:?} shim"),
        Status::Refreshed if dry_run => "would refresh managed copy".to_string(),
        Status::Refreshed => "refreshed managed copy".to_string(),
        Status::Kept => format!("managed {kind:?} already correct"),
        Status::Repaired if dry_run => format!("would repair {kind:?} shim"),
        Status::Repaired => format!("repaired {kind:?} shim"),
        _ => format!("{status:?}"),
    }
}

fn record(
    state: &State,
    repo: &GitRepo,
    instruction_dir: &std::path::Path,
    adapter: &Adapter,
    materialization: Option<MaterializationKind>,
    status: Status,
    message: &str,
) -> Result<()> {
    let content_hash = materializer::source_hash(repo, adapter)?;

    if instruction_dir == repo.root {
        return state.record(ShimRecord {
            repo,
            adapter_name: &adapter.name,
            source_rel_path: &adapter.source.to_string_lossy(),
            target_rel_path: &adapter.target.to_string_lossy(),
            materialization,
            content_hash,
            status,
            message,
        });
    }

    state.record_targeted(TargetedShimRecord {
        repo,
        instruction_dir,
        adapter_name: &adapter.name,
        source_rel_path: &adapter.source.to_string_lossy(),
        target_rel_path: &adapter.target.to_string_lossy(),
        materialization,
        content_hash,
        status,
        message,
    })
}

fn adapter_for_instruction_dir(
    repo: &GitRepo,
    instruction_dir: &std::path::Path,
) -> Result<Adapter> {
    let rel_dir = instruction_dir.strip_prefix(&repo.root).with_context(|| {
        format!(
            "instruction directory {} is outside repository root {}",
            instruction_dir.display(),
            repo.root.display()
        )
    })?;
    let source = rel_dir.join("AGENTS.md");
    let target = rel_dir.join("CLAUDE.md");
    Adapter::from_config(
        "claude",
        &crate::config::AdapterConfig {
            enabled: true,
            source,
            target,
            on_source_missing: SourceMissingBehavior::Leave,
        },
    )?
    .context("claude adapter unexpectedly disabled")
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
