use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use ignore::{DirEntry, WalkBuilder};

use crate::{config::ScanScope, git, git::GitRepo};

pub fn discover(scope: &ScanScope) -> Result<Vec<GitRepo>> {
    let mut repos = BTreeMap::<PathBuf, GitRepo>::new();

    for root in &scope.roots {
        let mut builder = WalkBuilder::new(root);
        builder.hidden(false);
        builder
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .parents(false);
        let exclude_paths = scope.exclude_paths.clone();
        let exclude_dir_names = scope.exclude_dir_names.clone();
        builder.filter_entry(move |entry| should_visit(&exclude_paths, &exclude_dir_names, entry));

        for entry in builder.build() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::warn!("skipping unreadable discovery entry: {error}");
                    continue;
                }
            };
            let path = entry.path();
            if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }

            if !looks_like_git_worktree(path) {
                continue;
            }

            if let Some(repo) = git::discover_repo(path)?
                && scope.repo_is_allowed(&repo.root)
            {
                repos.entry(repo.root.clone()).or_insert(repo);
            }
        }
    }

    Ok(repos.into_values().collect())
}

fn should_visit(
    exclude_paths: &[PathBuf],
    exclude_dir_names: &BTreeSet<String>,
    entry: &DirEntry,
) -> bool {
    let path = entry.path();
    if path.file_name().and_then(|name| name.to_str()) == Some(".git") {
        return false;
    }

    if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
        if exclude_paths
            .iter()
            .any(|excluded| path.starts_with(excluded))
        {
            return false;
        }

        return path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| !exclude_dir_names.contains(name))
            .unwrap_or(true);
    }

    true
}

fn looks_like_git_worktree(path: &Path) -> bool {
    fs::metadata(path.join(".git")).is_ok()
}
