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
    skills,
    state::State,
};

const DEFAULT_REPAIR_INTERVAL_SECONDS: u64 = 10 * 60;
const DEFAULT_JITTER_SECONDS: i64 = 30;
const DEFAULT_MAX_INSTRUCTION_DIRS: usize = 500;

#[derive(Debug, Default, Serialize)]
pub struct DaemonReport {
    pub instruction_dirs_checked: usize,
    pub skill_roots_checked: usize,
    pub created: usize,
    pub repaired: usize,
    pub refreshed: usize,
    pub kept: usize,
    pub cleaned: usize,
    pub exclude_updates: usize,
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
    let home = config::home_dir();
    let mut report = repair_once_with_home(state, dry_run, max, home.as_deref())?;
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
        report = repair_once_with_home(state, false, max, home.as_deref())?;
        tracing::info!(
            "repair tick instruction_dirs={} skill_roots={} created={} repaired={} errors={}",
            report.instruction_dirs_checked,
            report.skill_roots_checked,
            report.created,
            report.repaired,
            report.errors
        );
    }
}

pub fn repair_once(state: &State, dry_run: bool, max: usize) -> Result<DaemonReport> {
    repair_once_with_home(state, dry_run, max, None)
}

fn repair_once_with_home(
    state: &State,
    dry_run: bool,
    max: usize,
    home: Option<&Path>,
) -> Result<DaemonReport> {
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

    let skill_roots = state.active_skill_roots(max)?;
    let global_is_observed = home.is_some_and(|home| {
        skill_roots
            .iter()
            .any(|root| root.scope == "global" && same_path(&root.base_dir, home))
    });
    for skill_root in skill_roots {
        match skills::reconcile_observed(&skill_root, state, dry_run) {
            Ok(reconcile) => {
                report.skill_roots_checked += 1;
                report.created += reconcile.summary.created;
                report.repaired += reconcile.summary.repaired;
                report.kept += reconcile.summary.kept;
                report.cleaned += reconcile.summary.cleaned;
                report.exclude_updates += reconcile.summary.exclude_updates;
                report.conflicts += reconcile.summary.conflicts;
                report.tracked_conflicts += reconcile.summary.tracked_conflicts;
                report.errors += reconcile.summary.errors;
            }
            Err(error) => {
                report.errors += 1;
                if !dry_run {
                    state.mark_skill_root_result(
                        skill_root.id,
                        "error",
                        Some(&error.to_string()),
                    )?;
                    state.record_event(
                        "error",
                        skill_root.repo_root.as_deref(),
                        Some(skills::ADAPTER_NAME),
                        "daemon",
                        &error.to_string(),
                    )?;
                }
            }
        }
    }

    if let Some(home) = home.filter(|_| !global_is_observed) {
        match skills::reconcile_global(home, state, dry_run) {
            Ok(reconcile) => {
                report.skill_roots_checked += 1;
                report.created += reconcile.summary.created;
                report.repaired += reconcile.summary.repaired;
                report.kept += reconcile.summary.kept;
                report.cleaned += reconcile.summary.cleaned;
                report.exclude_updates += reconcile.summary.exclude_updates;
                report.conflicts += reconcile.summary.conflicts;
                report.tracked_conflicts += reconcile.summary.tracked_conflicts;
                report.errors += reconcile.summary.errors;
            }
            Err(error) => {
                report.errors += 1;
                if !dry_run {
                    state.record_event(
                        "error",
                        None,
                        Some(skills::ADAPTER_NAME),
                        "daemon",
                        &error.to_string(),
                    )?;
                }
            }
        }
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
    println!("Checked {} skill roots.", report.skill_roots_checked);
    println!("Created {} shims and skill links.", report.created);
    println!("Repaired {} shims.", report.repaired);
    println!("Refreshed {} copies.", report.refreshed);
    println!("Kept {} managed shims and skill links.", report.kept);
    println!("Cleaned {} managed skill links.", report.cleaned);
    println!("Updated {} exclude files.", report.exclude_updates);
    println!("Skipped {} conflicts.", report.conflicts);
    println!("Skipped {} tracked targets.", report.tracked_conflicts);
    println!("Errors {}.", report.errors);
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;

    use super::repair_once_with_home;
    use crate::state::State;

    #[test]
    fn repair_checks_the_fixed_global_root_before_it_is_observed() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let source = home.path().join(".agents/skills/deploy");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("SKILL.md"),
            "---\nname: deploy\ndescription: Test skill\n---\n",
        )
        .unwrap();
        let state = State::open(data.path().to_path_buf()).unwrap();

        let report = repair_once_with_home(&state, false, 500, Some(home.path())).unwrap();

        assert_eq!(report.skill_roots_checked, 1);
        assert_eq!(report.created, 1);
        assert_eq!(
            fs::read_link(home.path().join(".claude/skills/deploy")).unwrap(),
            std::path::PathBuf::from("../../.agents/skills/deploy")
        );
    }
}
