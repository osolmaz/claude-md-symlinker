use std::path::PathBuf;

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
    let adapters = adapters::enabled_adapters(config);
    let mut report = Report::new(repos.len());

    for repo in repos {
        for adapter in &adapters {
            let (result, exclude_updated) =
                reconcile_adapter(config, &repo, adapter, state, options).unwrap_or_else(|error| {
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
) -> Result<(RepoResult, bool)> {
    let source = repo.root.join(&adapter.source);
    let target = repo.root.join(&adapter.target);

    if !source.exists() {
        let target_state = materializer::classify(repo, adapter)?;
        let managed_target =
            target_is_managed(&target_state) || stored_hardlink_matches(repo, adapter, state)?;

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

            if managed_target {
                materializer::remove_target(repo, adapter, options.dry_run)?;
                let exclude_updated = exclude::remove(
                    repo,
                    &adapter.target,
                    config.git.exclude_mode,
                    options.dry_run,
                )?;
                let mut message = if options.dry_run {
                    "would remove stale managed shim".to_string()
                } else {
                    "removed stale managed shim".to_string()
                };
                if exclude_updated {
                    message.push_str("; Git exclude updated");
                }
                let result = result_for(repo, adapter, Status::Cleaned, message);
                if !options.dry_run {
                    record(state, repo, adapter, None, Status::Cleaned, &result.message)?;
                }
                return Ok((result, exclude_updated));
            }
        }

        let unmanaged_target_exists = matches!(
            target_state,
            TargetState::UnknownRegularFile | TargetState::UnknownSymlink | TargetState::Other
        );
        let exclude_updated = if managed_target || !unmanaged_target_exists {
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
                None,
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

fn target_is_managed(target_state: &TargetState) -> bool {
    matches!(
        target_state,
        TargetState::ManagedSymlink { .. }
            | TargetState::ManagedCopy { .. }
            | TargetState::ManagedHardlink
    )
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
    state.record(ShimRecord {
        repo,
        adapter_name: &adapter.name,
        source_rel_path: &adapter.source.to_string_lossy(),
        target_rel_path: &adapter.target.to_string_lossy(),
        materialization,
        content_hash: materializer::source_hash(repo, adapter)?,
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
