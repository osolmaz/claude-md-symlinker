use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::{
    cli::{ReposArgs, ReposCommand},
    config, exclude, git,
    state::{ObservedRepoSummary, State},
};

#[derive(Debug, Serialize)]
struct RemoveReport {
    repo: PathBuf,
    removed: bool,
    exclude_updates: usize,
}

#[derive(Debug, Serialize)]
struct PruneReport {
    repos: usize,
    instruction_dirs: usize,
}

pub fn run(args: &ReposArgs, state: &State, json: bool) -> Result<u8> {
    match &args.command {
        ReposCommand::List => list(state, json),
        ReposCommand::Remove(args) => {
            let repo = absolute_existing_or_logical(&args.repo)?;
            let exclude_updates = if args.clean_exclude {
                clean_excludes_for_repo(state, &repo)?
            } else {
                0
            };
            let removed = state.deactivate_repo(&repo)?;
            let report = RemoveReport {
                repo,
                removed,
                exclude_updates,
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if report.removed {
                println!("Removed {} from active scope.", report.repo.display());
                println!("Updated {} exclude files.", report.exclude_updates);
            } else {
                println!("Repo was not active: {}", report.repo.display());
            }
            Ok(0)
        }
        ReposCommand::Prune(args) => {
            let (repos, instruction_dirs) = state.prune_missing(args.forget_history)?;
            let report = PruneReport {
                repos,
                instruction_dirs,
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("Pruned {} repos.", repos);
                println!("Pruned {} instruction directories.", instruction_dirs);
            }
            Ok(0)
        }
    }
}

fn list(state: &State, json: bool) -> Result<u8> {
    let repos = state.observed_repos()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&repos_as_json(&repos))?);
        return Ok(0);
    }
    println!(
        "{:<64} {:>16} {:<20} {:<20} {:<14} error",
        "repo", "instruction_dirs", "last_seen_at", "last_reconciled_at", "last_status"
    );
    for repo in repos {
        println!(
            "{:<64} {:>16} {:<20} {:<20} {:<14} {}",
            repo.repo.display(),
            repo.instruction_dirs,
            repo.last_seen_at.as_deref().unwrap_or(""),
            repo.last_reconciled_at.as_deref().unwrap_or(""),
            repo.last_status.as_deref().unwrap_or(""),
            repo.last_error.as_deref().unwrap_or("")
        );
    }
    Ok(0)
}

fn repos_as_json(repos: &[ObservedRepoSummary]) -> Vec<serde_json::Value> {
    repos
        .iter()
        .map(|repo| {
            serde_json::json!({
                "repo": repo.repo,
                "instruction_dirs": repo.instruction_dirs,
                "last_seen_at": repo.last_seen_at,
                "last_reconciled_at": repo.last_reconciled_at,
                "last_status": repo.last_status,
                "last_error": repo.last_error,
            })
        })
        .collect()
}

fn clean_excludes_for_repo(state: &State, repo_path: &Path) -> Result<usize> {
    let Some(repo) = git::discover_repo(repo_path)? else {
        return Ok(0);
    };
    let mut updates = 0;
    for shim in state.managed_shims()? {
        if shim.repo_root == repo.root
            && exclude::remove(
                &repo,
                &shim.target_rel_path,
                crate::config::ExcludeMode::PerRepo,
                false,
            )?
        {
            updates += 1;
        }
    }
    Ok(updates)
}

fn absolute_existing_or_logical(path: &Path) -> Result<PathBuf> {
    let expanded = config::expand_tilde(path);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()?.join(expanded)
    };
    Ok(absolute.canonicalize().unwrap_or(absolute))
}
