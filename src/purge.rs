use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::{
    adapters::Adapter,
    cli::PurgeArgs,
    config::{AdapterConfig, ExcludeMode, SourceMissingBehavior},
    exclude, git,
    materializer::{self, TargetState},
    state::{ManagedShim, State},
};

#[derive(Debug, Default, Serialize)]
pub struct PurgeReport {
    pub removed: Vec<PathBuf>,
    pub skipped: Vec<PurgeItem>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PurgeItem {
    pub path: PathBuf,
    pub reason: String,
}

pub fn run(args: &PurgeArgs, state: &State, dry_run: bool, json: bool) -> Result<u8> {
    let _ = args.yes;
    let report = purge(state, dry_run)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_plain(&report);
    }
    Ok(if report.skipped.is_empty() { 0 } else { 2 })
}

pub fn purge(state: &State, dry_run: bool) -> Result<PurgeReport> {
    let shims = state.managed_shims()?;
    let mut report = PurgeReport {
        dry_run,
        ..PurgeReport::default()
    };

    for shim in shims {
        match purge_one(state, &shim, dry_run) {
            Ok(Some(path)) => report.removed.push(path),
            Ok(None) => {}
            Err(error) => report.skipped.push(PurgeItem {
                path: shim.repo_root.join(&shim.target_rel_path),
                reason: error.to_string(),
            }),
        }
    }
    Ok(report)
}

fn purge_one(state: &State, shim: &ManagedShim, dry_run: bool) -> Result<Option<PathBuf>> {
    let Some(repo) = git::discover_repo(&shim.repo_root)? else {
        anyhow::bail!("repository is no longer a Git worktree");
    };
    let adapter = adapter_for_shim(shim)?;
    let target = repo.root.join(&adapter.target);
    if git::is_tracked(&repo, &adapter.target)? {
        anyhow::bail!("target is tracked by Git");
    }
    let target_state = materializer::classify(&repo, &adapter)?;
    if !matches!(
        target_state,
        TargetState::ManagedSymlink { .. }
            | TargetState::ManagedCopy { .. }
            | TargetState::ManagedHardlink
    ) {
        anyhow::bail!("target is not a filesystem-proven managed shim");
    }

    if !dry_run {
        materializer::remove_target(&repo, &adapter, false)?;
        exclude::remove(&repo, &adapter.target, ExcludeMode::PerRepo, false)?;
        state.mark_shim_removed(shim.id)?;
    }
    Ok(Some(target))
}

fn adapter_for_shim(shim: &ManagedShim) -> Result<Adapter> {
    Adapter::from_config(
        "claude",
        &AdapterConfig {
            enabled: true,
            source: shim.source_rel_path.clone(),
            target: shim.target_rel_path.clone(),
            on_source_missing: SourceMissingBehavior::Leave,
        },
    )?
    .context("claude adapter unexpectedly disabled")
}

fn print_plain(report: &PurgeReport) {
    if report.dry_run {
        println!("Dry run. No filesystem changes were made.");
    }
    println!("Removed {} managed shims.", report.removed.len());
    for path in &report.removed {
        println!("  {}", path.display());
    }
    println!("Skipped {} files.", report.skipped.len());
    for item in &report.skipped {
        println!("  {}  {}", item.path.display(), item.reason);
    }
}
