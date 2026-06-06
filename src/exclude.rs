use std::{collections::BTreeSet, fs, path::Path};

use anyhow::{Context, Result};

use crate::{
    config::{ExcludeMode, data_dir},
    git::{self, GitRepo},
};

const BEGIN: &str = "# claudectomy managed begin";
const END: &str = "# claudectomy managed end";

pub fn ensure(repo: &GitRepo, target_rel: &Path, mode: ExcludeMode, dry_run: bool) -> Result<bool> {
    match mode {
        ExcludeMode::PerRepo => ensure_file(&repo.exclude_path, target_rel, dry_run),
        ExcludeMode::Global => {
            let path = data_dir()?.join("git-excludes");
            let updated = ensure_entry_file(&path, &ignore_entry(target_rel), dry_run)?;
            let removed_unignore =
                remove_entry_file(&repo.exclude_path, &unignore_entry(target_rel), dry_run)?;
            let config_updated = git::ensure_global_excludes_file(&path, dry_run)?;
            Ok(updated || removed_unignore || config_updated)
        }
    }
}

pub fn remove(repo: &GitRepo, target_rel: &Path, mode: ExcludeMode, dry_run: bool) -> Result<bool> {
    match mode {
        ExcludeMode::PerRepo => remove_from_file(&repo.exclude_path, target_rel, dry_run),
        ExcludeMode::Global => {
            let removed_ignore =
                remove_entry_file(&repo.exclude_path, &ignore_entry(target_rel), dry_run)?;
            let ensured_unignore =
                ensure_entry_file(&repo.exclude_path, &unignore_entry(target_rel), dry_run)?;
            Ok(removed_ignore || ensured_unignore)
        }
    }
}

fn ensure_file(path: &Path, target_rel: &Path, dry_run: bool) -> Result<bool> {
    ensure_entry_file(path, &ignore_entry(target_rel), dry_run)
}

fn ensure_entry_file(path: &Path, entry: &str, dry_run: bool) -> Result<bool> {
    let current = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read exclude file {}", path.display()));
        }
    };
    let next = upsert_managed_entry(&current, entry);
    if next == current {
        return Ok(false);
    }

    if !dry_run {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create exclude dir {}", parent.display()))?;
        }
        fs::write(path, next)
            .with_context(|| format!("failed to write exclude file {}", path.display()))?;
    }
    Ok(true)
}

fn remove_from_file(path: &Path, target_rel: &Path, dry_run: bool) -> Result<bool> {
    remove_entry_file(path, &ignore_entry(target_rel), dry_run)
}

fn remove_entry_file(path: &Path, entry: &str, dry_run: bool) -> Result<bool> {
    let current = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read exclude file {}", path.display()));
        }
    };

    let next = remove_managed_entry(&current, entry);
    if next == current {
        return Ok(false);
    }

    if !dry_run {
        fs::write(path, next)
            .with_context(|| format!("failed to write exclude file {}", path.display()))?;
    }
    Ok(true)
}

fn ignore_entry(target_rel: &Path) -> String {
    format!("/{}", target_rel.to_string_lossy().replace('\\', "/"))
}

fn unignore_entry(target_rel: &Path) -> String {
    format!("!/{}", target_rel.to_string_lossy().replace('\\', "/"))
}

fn upsert_managed_entry(current: &str, entry: &str) -> String {
    let mut entries = BTreeSet::new();
    entries.insert(entry.to_string());

    let lines: Vec<&str> = current.lines().collect();
    let begin = lines.iter().position(|line| *line == BEGIN);
    let end = lines.iter().position(|line| *line == END);

    if let (Some(begin), Some(end)) = (begin, end)
        && begin < end
    {
        for line in &lines[begin + 1..end] {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                entries.insert(trimmed.to_string());
            }
        }

        let mut replacement = vec![BEGIN.to_string()];
        replacement.extend(entries);
        replacement.push(END.to_string());

        let mut out = Vec::<String>::new();
        out.extend(lines[..begin].iter().map(|line| (*line).to_string()));
        out.extend(replacement);
        out.extend(lines[end + 1..].iter().map(|line| (*line).to_string()));
        return with_trailing_newline(out.join("\n"));
    }

    let mut out = current.trim_end_matches('\n').to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(BEGIN);
    out.push('\n');
    out.push_str(entry);
    out.push('\n');
    out.push_str(END);
    out.push('\n');
    out
}

fn remove_managed_entry(current: &str, entry: &str) -> String {
    let lines: Vec<&str> = current.lines().collect();
    let begin = lines.iter().position(|line| *line == BEGIN);
    let end = lines.iter().position(|line| *line == END);

    let (Some(begin), Some(end)) = (begin, end) else {
        return current.to_string();
    };
    if begin >= end {
        return current.to_string();
    }

    let mut entries = BTreeSet::new();
    for line in &lines[begin + 1..end] {
        let trimmed = line.trim();
        if !trimmed.is_empty() && trimmed != entry {
            entries.insert(trimmed.to_string());
        }
    }

    let mut out = Vec::<String>::new();
    out.extend(lines[..begin].iter().map(|line| (*line).to_string()));
    if !entries.is_empty() {
        out.push(BEGIN.to_string());
        out.extend(entries);
        out.push(END.to_string());
    }
    out.extend(lines[end + 1..].iter().map(|line| (*line).to_string()));

    with_trailing_newline(out.join("\n").trim_matches('\n').to_string())
}

fn with_trailing_newline(mut text: String) -> String {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{remove_managed_entry, upsert_managed_entry};

    #[test]
    fn upsert_is_idempotent() {
        let once = upsert_managed_entry("existing\n", "/CLAUDE.md");
        let twice = upsert_managed_entry(&once, "/CLAUDE.md");
        assert_eq!(once, twice);
    }

    #[test]
    fn remove_deletes_empty_managed_block() {
        let current = upsert_managed_entry("existing\n", "/CLAUDE.md");
        let next = remove_managed_entry(&current, "/CLAUDE.md");
        assert!(!next.contains("claudectomy managed"));
        assert!(!next.contains("/CLAUDE.md"));
        assert!(next.contains("existing"));
    }
}
