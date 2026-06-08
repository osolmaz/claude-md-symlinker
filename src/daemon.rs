use std::{
    fs::{self, File, OpenOptions},
    path::Path,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::Serialize;

use crate::{
    cli::DaemonArgs,
    config, git,
    reconciler::{self, ReconcileOptions},
    state::State,
};

const DEFAULT_REPAIR_INTERVAL_SECONDS: u64 = 10 * 60;
const DEFAULT_JITTER_SECONDS: i64 = 30;
const DEFAULT_MAX_INSTRUCTION_DIRS: usize = 500;

#[derive(Debug, Default, Serialize)]
pub struct DaemonReport {
    pub instruction_dirs_checked: usize,
    pub created: usize,
    pub repaired: usize,
    pub refreshed: usize,
    pub kept: usize,
    pub conflicts: usize,
    pub tracked_conflicts: usize,
    pub errors: usize,
    pub dry_run: bool,
}

pub fn run(args: &DaemonArgs, state: &State, dry_run: bool, json: bool) -> Result<u8> {
    let _lock = if dry_run { None } else { Some(acquire_lock()?) };
    let max = args
        .max_instruction_dirs
        .unwrap_or(DEFAULT_MAX_INSTRUCTION_DIRS);
    let mut report = repair_once(state, dry_run, max)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_plain(&report);
    }
    if args.once || dry_run {
        return Ok(report_exit_code(&report));
    }

    loop {
        let interval = Duration::from_secs(
            args.interval_seconds
                .unwrap_or(DEFAULT_REPAIR_INTERVAL_SECONDS),
        );
        thread::sleep(with_jitter(interval));
        report = repair_once(state, false, max)?;
        tracing::info!(
            "repair tick checked={} created={} repaired={} errors={}",
            report.instruction_dirs_checked,
            report.created,
            report.repaired,
            report.errors
        );
    }
}

pub fn repair_once(state: &State, dry_run: bool, max: usize) -> Result<DaemonReport> {
    let dirs = state.active_instruction_dirs(max)?;
    let mut report = DaemonReport {
        dry_run,
        ..DaemonReport::default()
    };

    for dir in dirs {
        if !dir.repo_root.exists() {
            report.errors += 1;
            if !dry_run {
                state.mark_instruction_result(
                    &dir.instruction_dir,
                    Some("error"),
                    Some("repository root no longer exists"),
                )?;
                state.record_event(
                    "error",
                    Some(&dir.repo_root),
                    Some("claude"),
                    "daemon",
                    "repository root no longer exists",
                )?;
            }
            continue;
        }
        if !dir.instruction_dir.exists() {
            report.errors += 1;
            if !dry_run {
                state.mark_instruction_result(
                    &dir.instruction_dir,
                    Some("error"),
                    Some("instruction directory no longer exists"),
                )?;
            }
            continue;
        }

        let Some(repo) = git::discover_repo(&dir.repo_root)? else {
            report.errors += 1;
            if !dry_run {
                state.mark_instruction_result(
                    &dir.instruction_dir,
                    Some("error"),
                    Some("repository is no longer a Git worktree"),
                )?;
            }
            continue;
        };
        if !same_path(&repo.root, &dir.repo_root) {
            report.errors += 1;
            if !dry_run {
                state.mark_instruction_result(
                    &dir.instruction_dir,
                    Some("error"),
                    Some("repository root changed"),
                )?;
            }
            continue;
        }

        let reconcile = reconciler::apply_instruction_dir(
            &repo,
            &dir.instruction_dir,
            state,
            ReconcileOptions { dry_run },
        )?;
        report.instruction_dirs_checked += 1;
        report.created += reconcile.summary.created;
        report.repaired += reconcile.summary.repaired;
        report.refreshed += reconcile.summary.refreshed;
        report.kept += reconcile.summary.kept;
        report.conflicts += reconcile.summary.conflicts;
        report.tracked_conflicts += reconcile.summary.tracked_conflicts;
        report.errors += reconcile.summary.errors;
    }

    Ok(report)
}

fn acquire_lock() -> Result<File> {
    let data_dir = config::data_dir()?;
    fs::create_dir_all(&data_dir)
        .with_context(|| format!("failed to create data dir {}", data_dir.display()))?;
    let lock_path = data_dir.join("daemon.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open lock {}", lock_path.display()))?;
    file.try_lock_exclusive()
        .with_context(|| "another claude-md-symlinker daemon is already running")?;
    Ok(file)
}

fn with_jitter(base: Duration) -> Duration {
    let jitter = DEFAULT_JITTER_SECONDS;
    if jitter <= 0 {
        return base;
    }
    let span = (jitter * 2 + 1) as u64;
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    let offset = (seed % span) as i64 - jitter;
    if offset.is_negative() {
        base.saturating_sub(Duration::from_secs(offset.unsigned_abs()))
    } else {
        base + Duration::from_secs(offset as u64)
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn report_exit_code(report: &DaemonReport) -> u8 {
    if report.errors > 0 {
        1
    } else if report.conflicts > 0 || report.tracked_conflicts > 0 {
        2
    } else {
        0
    }
}

fn print_plain(report: &DaemonReport) {
    if report.dry_run {
        println!("Dry run. No filesystem changes were made.");
    }
    println!(
        "Checked {} instruction directories.",
        report.instruction_dirs_checked
    );
    println!("Created {} shims.", report.created);
    println!("Repaired {} shims.", report.repaired);
    println!("Refreshed {} copies.", report.refreshed);
    println!("Kept {} managed shims.", report.kept);
    println!("Skipped {} conflicts.", report.conflicts);
    println!("Skipped {} tracked targets.", report.tracked_conflicts);
    println!("Errors {}.", report.errors);
}
