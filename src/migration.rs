use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::{
    git,
    reconciler::{self, ReconcileOptions},
    state::{DetectedClaudeFile, State},
};

#[derive(Debug, Clone, Copy)]
pub struct MigrateOptions {
    pub dry_run: bool,
    pub auto_safe_only: bool,
    pub replace_existing: bool,
    pub git_add: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct MigrationReport {
    pub safe: Vec<MigrationItem>,
    pub migrated: Vec<MigrationItem>,
    pub needs_review: Vec<MigrationItem>,
    pub skipped: Vec<MigrationItem>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MigrationItem {
    pub id: i64,
    pub claude_path: PathBuf,
    pub agents_path: PathBuf,
    pub status: String,
    pub reason: Option<String>,
    pub diff: Option<String>,
}

pub fn migrate(state: &State, options: MigrateOptions) -> Result<MigrationReport> {
    let candidates = state.migration_candidates()?;
    let mut report = MigrationReport {
        dry_run: options.dry_run,
        ..MigrationReport::default()
    };

    for candidate in candidates {
        let preflight = preflight(&candidate, options.replace_existing)?;
        match preflight.kind {
            PreflightKind::Skipped => {
                let item = item(&candidate, "skipped", Some(preflight.reason), None);
                if !options.dry_run {
                    state.mark_detected_migration_result(
                        candidate.id,
                        "skipped",
                        item.reason.as_deref(),
                        false,
                    )?;
                }
                report.skipped.push(item);
            }
            PreflightKind::NeedsReview => {
                let item = item(&candidate, "needs_review", Some(preflight.reason), None);
                if !options.dry_run {
                    state.mark_detected_migration_result(
                        candidate.id,
                        "needs_review",
                        item.reason.as_deref(),
                        false,
                    )?;
                }
                report.needs_review.push(item);
            }
            PreflightKind::Safe { rewritten, diff } => {
                let safe_item = item(&candidate, "safe", None, diff);
                if options.dry_run {
                    report.safe.push(safe_item);
                    continue;
                }
                apply_migration(&candidate, &rewritten, state, options)?;
                let migrated = item(&candidate, "migrated", None, None);
                state.mark_detected_migration_result(candidate.id, "migrated", None, true)?;
                report.migrated.push(migrated);
            }
        }
    }

    Ok(report)
}

pub fn print_plain(report: &MigrationReport) {
    if report.dry_run {
        println!("Dry run. No filesystem changes were made.");
    }
    print_group("Safe migrations", &report.safe);
    print_group("Migrated", &report.migrated);
    print_group("Needs review", &report.needs_review);
    print_group("Skipped", &report.skipped);
}

fn print_group(name: &str, items: &[MigrationItem]) {
    println!("{name}: {}", items.len());
    for item in items {
        match &item.reason {
            Some(reason) => println!(
                "  {} -> {}  {}",
                item.claude_path.display(),
                item.agents_path.display(),
                reason
            ),
            None => println!(
                "  {} -> {}",
                item.claude_path.display(),
                item.agents_path.display()
            ),
        }
        if let Some(diff) = &item.diff
            && !diff.is_empty()
        {
            for line in diff.lines() {
                println!("    {line}");
            }
        }
    }
}

enum PreflightKind {
    Safe {
        rewritten: String,
        diff: Option<String>,
    },
    NeedsReview,
    Skipped,
}

struct Preflight {
    kind: PreflightKind,
    reason: String,
}

fn preflight(candidate: &DetectedClaudeFile, replace_existing: bool) -> Result<Preflight> {
    let metadata = match fs::symlink_metadata(&candidate.claude_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(skip("CLAUDE.md no longer exists"));
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {}", candidate.claude_path.display()));
        }
    };

    if metadata.file_type().is_symlink() {
        return Ok(skip("CLAUDE.md is a symlink"));
    }
    if !metadata.is_file() {
        return Ok(skip("CLAUDE.md is not a regular file"));
    }
    if candidate.agents_path.exists() && !replace_existing {
        return Ok(skip("sibling AGENTS.md already exists"));
    }

    let bytes = fs::read(&candidate.claude_path)
        .with_context(|| format!("failed to read {}", candidate.claude_path.display()))?;
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(_) => return Ok(needs_review("CLAUDE.md is not valid UTF-8 Markdown")),
    };
    let (rewritten, warnings) = rewrite_content(&content);
    if let Some(warning) = warnings.first() {
        return Ok(needs_review(warning));
    }

    Ok(Preflight {
        kind: PreflightKind::Safe {
            diff: content_diff(&content, &rewritten),
            rewritten,
        },
        reason: String::new(),
    })
}

fn skip(reason: &str) -> Preflight {
    Preflight {
        kind: PreflightKind::Skipped,
        reason: reason.to_string(),
    }
}

fn needs_review(reason: &str) -> Preflight {
    Preflight {
        kind: PreflightKind::NeedsReview,
        reason: reason.to_string(),
    }
}

fn rewrite_content(content: &str) -> (String, Vec<String>) {
    let rewritten = content
        .replace("# CLAUDE.md", "# AGENTS.md")
        .replace(
            "This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.",
            "This file provides guidance to AI agents when working with code in this repository.",
        )
        .replace("Claude Code (claude.ai/code)", "AI agents");

    let mut warnings = Vec::new();
    if rewritten.contains("Claude")
        || rewritten.contains("CLAUDE.md")
        || rewritten.contains("claude.ai/code")
    {
        warnings.push("unknown Claude-specific text".to_string());
    }
    (rewritten, warnings)
}

fn content_diff(before: &str, after: &str) -> Option<String> {
    if before == after {
        return None;
    }
    let mut diff = String::new();
    for (before_line, after_line) in before.lines().zip(after.lines()) {
        if before_line != after_line {
            diff.push_str("- ");
            diff.push_str(before_line);
            diff.push('\n');
            diff.push_str("+ ");
            diff.push_str(after_line);
            diff.push('\n');
        }
    }
    let before_count = before.lines().count();
    let after_count = after.lines().count();
    if before_count != after_count {
        diff.push_str(&format!(
            "~ line count changed from {before_count} to {after_count}\n"
        ));
    }
    Some(diff)
}

fn apply_migration(
    candidate: &DetectedClaudeFile,
    rewritten: &str,
    state: &State,
    options: MigrateOptions,
) -> Result<()> {
    let repo = git::discover_repo(&candidate.instruction_dir)?.with_context(|| {
        format!(
            "{} is no longer inside a Git repo",
            candidate.instruction_dir.display()
        )
    })?;
    let claude_rel = candidate
        .claude_path
        .strip_prefix(&repo.root)
        .with_context(|| {
            format!(
                "{} is outside {}",
                candidate.claude_path.display(),
                repo.root.display()
            )
        })?;
    let agents_rel = candidate
        .agents_path
        .strip_prefix(&repo.root)
        .with_context(|| {
            format!(
                "{} is outside {}",
                candidate.agents_path.display(),
                repo.root.display()
            )
        })?;

    if candidate.agents_path.exists() && !options.replace_existing {
        bail!("sibling AGENTS.md already exists");
    }

    if git::is_tracked(&repo, claude_rel)? && options.git_add {
        git_mv(&repo.root, claude_rel, agents_rel, options.replace_existing)?;
        fs::write(&candidate.agents_path, rewritten)
            .with_context(|| format!("failed to write {}", candidate.agents_path.display()))?;
        git_add(&repo.root, agents_rel)?;
    } else {
        let tmp = candidate.agents_path.with_extension("md.tmp");
        fs::write(&tmp, rewritten).with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, &candidate.agents_path).with_context(|| {
            format!(
                "failed to move {} to {}",
                tmp.display(),
                candidate.agents_path.display()
            )
        })?;
        fs::remove_file(&candidate.claude_path)
            .with_context(|| format!("failed to remove {}", candidate.claude_path.display()))?;
        if options.git_add {
            git_add(&repo.root, agents_rel)?;
        }
    }

    let verified = fs::read_to_string(&candidate.agents_path)
        .with_context(|| format!("failed to verify {}", candidate.agents_path.display()))?;
    if verified != rewritten {
        bail!(
            "verification failed after writing {}",
            candidate.agents_path.display()
        );
    }

    reconciler::apply_instruction_dir(
        &repo,
        &candidate.instruction_dir,
        state,
        ReconcileOptions { dry_run: false },
    )?;
    Ok(())
}

fn git_mv(repo: &Path, from: &Path, to: &Path, replace_existing: bool) -> Result<()> {
    if replace_existing {
        git_command(repo, &["mv", "-f", "--"], &[from, to])
    } else {
        git_command(repo, &["mv", "--"], &[from, to])
    }
}

fn git_add(repo: &Path, path: &Path) -> Result<()> {
    git_command(repo, &["add", "--"], &[path])
}

fn git_command(repo: &Path, args: &[&str], paths: &[&Path]) -> Result<()> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .arg("--literal-pathspecs")
        .args(args)
        .args(paths)
        .stdout(Stdio::null());
    let output = command
        .output()
        .with_context(|| format!("failed to run git in {}", repo.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git command failed in {}: {}",
            repo.display(),
            stderr.trim()
        );
    }
    Ok(())
}

fn item(
    candidate: &DetectedClaudeFile,
    status: &str,
    reason: Option<String>,
    diff: Option<String>,
) -> MigrationItem {
    MigrationItem {
        id: candidate.id,
        claude_path: candidate.claude_path.clone(),
        agents_path: candidate.agents_path.clone(),
        status: status.to_string(),
        reason,
        diff,
    }
}
