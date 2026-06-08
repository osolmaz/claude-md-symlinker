use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    adapters::Adapter,
    cli::ObserveArgs,
    config::{AdapterConfig, SourceMissingBehavior},
    git::{self, GitRepo},
    materializer::{self, TargetState},
    migration,
    reconciler::{self, ReconcileOptions},
    state::{DetectedClaudeRecord, State},
};

#[derive(Debug, Default, Serialize)]
pub struct ObserveReport {
    pub cwd: Option<PathBuf>,
    pub repo: Option<PathBuf>,
    pub instruction_dirs: Vec<PathBuf>,
    pub detected_claude_files: Vec<DetectedFileReport>,
    pub created: usize,
    pub repaired: usize,
    pub refreshed: usize,
    pub kept: usize,
    pub conflicts: usize,
    pub tracked_conflicts: usize,
    pub errors: Vec<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DetectedFileReport {
    pub path: PathBuf,
    pub classification: String,
}

pub fn run(args: &ObserveArgs, state: &State, dry_run: bool, json: bool) -> Result<u8> {
    let result = observe(args, state, dry_run);
    let mut exit_code = 0;
    let report = match result {
        Ok(report) => report,
        Err(error) if args.strict => return Err(error),
        Err(error) => {
            tracing::warn!("observe failed: {error:#}");
            exit_code = 0;
            ObserveReport {
                errors: vec![error.to_string()],
                dry_run,
                ..ObserveReport::default()
            }
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(exit_code)
}

pub fn observe(args: &ObserveArgs, state: &State, dry_run: bool) -> Result<ObserveReport> {
    let cwd = resolve_cwd(args)?;
    let mut report = ObserveReport {
        cwd: Some(cwd.clone()),
        dry_run,
        ..ObserveReport::default()
    };

    let Some(repo) = git::discover_repo(&cwd)? else {
        return Ok(report);
    };
    report.repo = Some(repo.root.clone());

    let dirs = cwd_parent_dirs(&cwd, &repo.root)?;
    let auto_migrate = state.setting_bool("auto_migrate", false)?;

    for dir in dirs {
        let agents_path = dir.join("AGENTS.md");
        let claude_path = dir.join("CLAUDE.md");
        let source_rel = rel_path(&repo, &agents_path)?;
        let target_rel = rel_path(&repo, &claude_path)?;

        if fs::symlink_metadata(&claude_path).is_ok() {
            let classification = classify_claude_file(&repo, &dir)?;
            let hash = if matches!(classification.as_str(), "user_file" | "tracked_user_file") {
                file_hash(&claude_path)?
            } else {
                None
            };
            if !dry_run {
                state.record_detected_claude_file(DetectedClaudeRecord {
                    repo: &repo,
                    instruction_dir: &dir,
                    claude_path: &claude_path,
                    agents_path: &agents_path,
                    classification: &classification,
                    content_hash: hash.as_deref(),
                    status: None,
                    error: None,
                })?;
            }
            report.detected_claude_files.push(DetectedFileReport {
                path: claude_path.clone(),
                classification,
            });
        }

        if !agents_path.is_file() {
            continue;
        }

        report.instruction_dirs.push(dir.clone());
        if !dry_run {
            state.record_instruction_dir(
                &repo,
                &dir,
                &source_rel.to_string_lossy(),
                &target_rel.to_string_lossy(),
                "claude-hook",
                Some(&cwd),
            )?;
        }

        if !args.no_apply {
            let reconcile = reconciler::apply_instruction_dir(
                &repo,
                &dir,
                state,
                ReconcileOptions { dry_run },
            )?;
            report.created += reconcile.summary.created;
            report.repaired += reconcile.summary.repaired;
            report.refreshed += reconcile.summary.refreshed;
            report.kept += reconcile.summary.kept;
            report.conflicts += reconcile.summary.conflicts;
            report.tracked_conflicts += reconcile.summary.tracked_conflicts;
            if reconcile.summary.errors > 0 {
                report.errors.extend(
                    reconcile
                        .results
                        .iter()
                        .filter(|result| result.status == crate::reporting::Status::Error)
                        .map(|result| result.message.clone()),
                );
            }
        }
    }

    if auto_migrate && !dry_run {
        let migrate_report = migration::migrate(
            state,
            migration::MigrateOptions {
                dry_run: false,
                auto_safe_only: true,
                replace_existing: false,
                git_add: true,
            },
        )?;
        report.errors.extend(
            migrate_report
                .needs_review
                .into_iter()
                .chain(migrate_report.skipped)
                .filter_map(|item| {
                    item.reason
                        .map(|reason| format!("{}: {reason}", item.claude_path.display()))
                }),
        );
    }

    Ok(report)
}

fn resolve_cwd(args: &ObserveArgs) -> Result<PathBuf> {
    if let Some(cwd) = &args.cwd {
        return absolute_dir(cwd);
    }

    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .context("failed to read hook stdin")?;
    if let Some(cwd) = cwd_from_hook_json(&input, args.strict)? {
        return absolute_dir(&cwd);
    }

    std::env::current_dir().context("failed to read current directory")
}

fn cwd_from_hook_json(input: &str, strict: bool) -> Result<Option<PathBuf>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(error) if strict => return Err(error).context("failed to parse hook JSON"),
        Err(error) => {
            tracing::warn!("ignoring malformed hook JSON: {error}");
            return Ok(None);
        }
    };
    let event_name = value
        .get("hook_event_name")
        .or_else(|| value.get("event"))
        .or_else(|| value.get("type"))
        .and_then(|value| value.as_str());
    let cwd = if event_name == Some("CwdChanged") {
        value
            .get("new_cwd")
            .and_then(|value| value.as_str())
            .or_else(|| value.get("cwd").and_then(|value| value.as_str()))
    } else {
        value.get("cwd").and_then(|value| value.as_str())
    };
    Ok(cwd.map(PathBuf::from))
}

fn absolute_dir(path: &Path) -> Result<PathBuf> {
    let path = crate::config::expand_tilde(path);
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(absolute.canonicalize().unwrap_or(absolute))
}

fn cwd_parent_dirs(cwd: &Path, repo_root: &Path) -> Result<Vec<PathBuf>> {
    let mut current = cwd.to_path_buf();
    if !current.starts_with(repo_root) {
        current = current
            .canonicalize()
            .with_context(|| format!("failed to canonicalize cwd {}", cwd.display()))?;
    }
    if !current.starts_with(repo_root) {
        bail!(
            "cwd {} is outside discovered repo root {}",
            current.display(),
            repo_root.display()
        );
    }

    let mut dirs = Vec::new();
    loop {
        dirs.push(current.clone());
        if current == repo_root {
            break;
        }
        if !current.pop() {
            bail!("failed to walk cwd parents from {}", cwd.display());
        }
    }
    Ok(dirs)
}

fn classify_claude_file(repo: &GitRepo, instruction_dir: &Path) -> Result<String> {
    let adapter = claude_adapter(repo, instruction_dir)?;
    let target = repo.root.join(&adapter.target);
    Ok(match materializer::classify(repo, &adapter)? {
        TargetState::ManagedSymlink { .. }
        | TargetState::ManagedCopy { .. }
        | TargetState::ManagedHardlink => "managed_shim".to_string(),
        TargetState::UnknownRegularFile => {
            if git::is_tracked(repo, &adapter.target)
                .with_context(|| format!("failed to check tracked target {}", target.display()))?
            {
                "tracked_user_file".to_string()
            } else {
                "user_file".to_string()
            }
        }
        TargetState::Missing => return Err(anyhow!("CLAUDE.md disappeared during classification")),
        TargetState::UnknownSymlink | TargetState::Other => "unknown".to_string(),
    })
}

fn claude_adapter(repo: &GitRepo, instruction_dir: &Path) -> Result<Adapter> {
    let rel_dir = instruction_dir.strip_prefix(&repo.root).with_context(|| {
        format!(
            "instruction directory {} is outside repository root {}",
            instruction_dir.display(),
            repo.root.display()
        )
    })?;
    Adapter::from_config(
        "claude",
        &AdapterConfig {
            enabled: true,
            source: rel_dir.join("AGENTS.md"),
            target: rel_dir.join("CLAUDE.md"),
            on_source_missing: SourceMissingBehavior::Leave,
        },
    )?
    .context("claude adapter unexpectedly disabled")
}

fn rel_path(repo: &GitRepo, path: &Path) -> Result<PathBuf> {
    path.strip_prefix(&repo.root)
        .map(PathBuf::from)
        .with_context(|| {
            format!(
                "path {} is outside repository root {}",
                path.display(),
                repo.root.display()
            )
        })
}

fn file_hash(path: &Path) -> Result<Option<String>> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(Some(format!("{:x}", hasher.finalize())))
}
