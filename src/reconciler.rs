use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{Context, Result};

use crate::{
    adapters::{self, Adapter},
    config::{AppConfig, SourceMissingBehavior},
    discovery, exclude, git,
    git::GitRepo,
    materializer::{self, MaterializationKind, TargetState},
    reporting::{RepoResult, Report, Status},
    state::{ShimRecord, State},
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

fn reconcile_adapter(
    config: &AppConfig,
    repo: &GitRepo,
    adapter: &Adapter,
    state: &State,
    options: ReconcileOptions,
    shared_exclude: bool,
) -> Result<(RepoResult, bool)> {
    let source = repo.root.join(&adapter.source);
    let target = repo.root.join(&adapter.target);

    if !source.exists() {
        let target_state = materializer::classify(repo, adapter)?;
        let stored_kind = stored_managed_kind(repo, adapter, state)?;
        let stored_hardlink_matches = if matches!(target_state, TargetState::UnknownRegularFile) {
            stored_hardlink_matches(repo, adapter, state)?
        } else {
            false
        };
        let stored_missing_kind = if matches!(target_state, TargetState::Missing) {
            stored_kind
        } else {
            None
        };
        let stale_missing_managed_target = stored_missing_kind.is_some() && !shared_exclude;
        let target_can_be_removed =
            target_managed_kind(&target_state).is_some() || stored_hardlink_matches;
        let managed_kind = target_managed_kind(&target_state)
            .or_else(|| stored_hardlink_matches.then_some(MaterializationKind::Hardlink))
            .or(stored_missing_kind);

        if adapter.on_source_missing == SourceMissingBehavior::RemoveIfManaged {
            if git::is_tracked(repo, &adapter.target)
                .with_context(|| format!("failed to check tracked target {}", target.display()))?
            {
                let result = result_for(
                    repo,
                    adapter,
                    Status::TrackedConflict,
                    "target is tracked by Git; leaving it untouched and not excluding it",
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
                return Ok((result, false));
            }

            if target_can_be_removed || stale_missing_managed_target {
                let exclude_updated = exclude::remove(
                    repo,
                    &adapter.target,
                    config.git.exclude_mode,
                    options.dry_run,
                )?;
                let removed = if target_can_be_removed {
                    materializer::remove_target(repo, adapter, options.dry_run)?
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
                        adapter,
                        materialization,
                        result.status,
                        &result.message,
                    )?;
                }
                return Ok((result, exclude_updated));
            }
        }

        let unmanaged_target_exists = matches!(
            target_state,
            TargetState::UnknownRegularFile | TargetState::UnknownSymlink | TargetState::Other
        );
        let should_remove_exclude = unmanaged_target_exists || stale_missing_managed_target;
        let exclude_updated = if target_can_be_removed || !should_remove_exclude {
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
                "source file does not exist; Git exclude removed"
            } else {
                "source file does not exist"
            },
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
        return Ok((result, exclude_updated));
    }

    if git::is_tracked(repo, &adapter.target)
        .with_context(|| format!("failed to check tracked target {}", target.display()))?
    {
        let result = result_for(
            repo,
            adapter,
            Status::TrackedConflict,
            "target is tracked by Git; leaving it untouched and not excluding it",
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
        return Ok((result, false));
    }

    match materializer::classify(repo, adapter)? {
        TargetState::UnknownRegularFile => {
            if let Some(recovered) = recover_stale_hardlink(config, repo, adapter, state, options)?
            {
                return Ok(recovered);
            }

            unmanaged_conflict(config, repo, adapter, state, options)
        }
        TargetState::UnknownSymlink | TargetState::Other => {
            unmanaged_conflict(config, repo, adapter, state, options)
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
                if exclude_updated && !options.dry_run {
                    let _ = exclude::remove(
                        repo,
                        &adapter.target,
                        config.git.exclude_mode,
                        options.dry_run,
                    );
                }
            })?;

            let status = status_for(previous_state, outcome.changed);
            let mut message = message_for(status, outcome.kind, options.dry_run);
            if exclude_updated {
                message.push_str("; Git exclude updated");
            }

            let result = result_for(repo, adapter, status, message);
            if !options.dry_run {
                record(
                    state,
                    repo,
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

fn unmanaged_conflict(
    config: &AppConfig,
    repo: &GitRepo,
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
            adapter,
            None,
            Status::Conflict,
            &result.message,
        )?;
    }
    Ok((result, exclude_updated))
}

fn recover_stale_hardlink(
    config: &AppConfig,
    repo: &GitRepo,
    adapter: &Adapter,
    state: &State,
    options: ReconcileOptions,
) -> Result<Option<(RepoResult, bool)>> {
    let Some(stored) = state.get_shim(repo, &adapter.name, &adapter.target.to_string_lossy())?
    else {
        return Ok(None);
    };

    if stored.materialization.as_deref() != Some("hardlink") {
        return Ok(None);
    }

    if materializer::target_hash(repo, adapter)? != stored.content_hash {
        return Ok(None);
    }

    let exclude_updated = exclude::ensure(
        repo,
        &adapter.target,
        config.git.exclude_mode,
        options.dry_run,
    )?;
    let outcome =
        materializer::recreate_hardlink(repo, adapter, options.dry_run).inspect_err(|_| {
            if exclude_updated && !options.dry_run {
                let _ = exclude::remove(
                    repo,
                    &adapter.target,
                    config.git.exclude_mode,
                    options.dry_run,
                );
            }
        })?;
    let mut message = message_for(Status::Repaired, outcome.kind, options.dry_run);
    if exclude_updated {
        message.push_str("; Git exclude updated");
    }

    let result = result_for(repo, adapter, Status::Repaired, message);
    if !options.dry_run {
        record(
            state,
            repo,
            adapter,
            Some(outcome.kind),
            result.status,
            &result.message,
        )?;
    }

    Ok(Some((result, exclude_updated)))
}

fn stored_hardlink_matches(repo: &GitRepo, adapter: &Adapter, state: &State) -> Result<bool> {
    let Some(stored) = state.get_shim(repo, &adapter.name, &adapter.target.to_string_lossy())?
    else {
        return Ok(false);
    };

    if stored.materialization.as_deref() != Some("hardlink") {
        return Ok(false);
    }

    Ok(materializer::target_hash(repo, adapter)? == stored.content_hash)
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

fn status_for(previous_state: TargetState, changed: bool) -> Status {
    match previous_state {
        TargetState::Missing => Status::Created,
        TargetState::ManagedSymlink { .. } if changed => Status::Repaired,
        TargetState::ManagedSymlink { .. } | TargetState::ManagedHardlink => Status::Kept,
        TargetState::ManagedCopy { .. } if changed => Status::Refreshed,
        TargetState::ManagedCopy { .. } => Status::Kept,
        TargetState::UnknownRegularFile | TargetState::UnknownSymlink | TargetState::Other => {
            unreachable!("unmanaged targets are handled before materialization")
        }
    }
}

fn message_for(status: Status, kind: MaterializationKind, dry_run: bool) -> String {
    let prefix = if dry_run { "would " } else { "" };
    match status {
        Status::Created => format!("{prefix}created {kind:?} shim"),
        Status::Refreshed => format!("{prefix}refreshed managed copy"),
        Status::Kept => format!("managed {kind:?} already correct"),
        Status::Repaired => format!("{prefix}repaired {kind:?} shim"),
        _ => format!("{status:?}"),
    }
}

fn record(
    state: &State,
    repo: &GitRepo,
    adapter: &Adapter,
    materialization: Option<MaterializationKind>,
    status: Status,
    message: &str,
) -> Result<()> {
    let content_hash = match materialization {
        Some(MaterializationKind::Hardlink) if !repo.root.join(&adapter.source).exists() => {
            materializer::target_hash(repo, adapter)?
        }
        _ => materializer::source_hash(repo, adapter)?,
    };

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
