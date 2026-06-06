use std::{
    collections::BTreeSet,
    env, fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::{
    config::{ExcludeMode, data_dir, expand_tilde},
    git::{self, GitRepo},
};

const BEGIN: &str = "# claudectomy managed begin";
const END: &str = "# claudectomy managed end";

pub fn ensure(repo: &GitRepo, target_rel: &Path, mode: ExcludeMode, dry_run: bool) -> Result<bool> {
    match mode {
        ExcludeMode::PerRepo => ensure_file(repo, target_rel, dry_run),
        ExcludeMode::Global => reject_global_mode(),
    }
}

pub fn remove(repo: &GitRepo, target_rel: &Path, mode: ExcludeMode, dry_run: bool) -> Result<bool> {
    match mode {
        ExcludeMode::PerRepo => {
            let removed_ignore = remove_from_file(repo, target_rel, dry_run)?;
            let ensured_unignore = if claudectomy_global_ignore_is_active(target_rel)? {
                ensure_entry_file(repo, &unignore_entry(target_rel), dry_run)?
            } else {
                false
            };
            Ok(removed_ignore || ensured_unignore)
        }
        ExcludeMode::Global => reject_global_mode(),
    }
}

pub fn validate_mode(mode: ExcludeMode) -> Result<()> {
    match mode {
        ExcludeMode::PerRepo => Ok(()),
        ExcludeMode::Global => reject_global_mode(),
    }
}

fn reject_global_mode<T>() -> Result<T> {
    bail!(
        "global exclude mode is disabled because Git global excludes cannot be scoped to configured roots; use per_repo"
    )
}

fn claudectomy_global_ignore_is_active(target_rel: &Path) -> Result<bool> {
    let claudectomy_global_path = data_dir()?.join("git-excludes");
    let Some(configured_path) = git::configured_global_excludes_file()? else {
        return Ok(false);
    };
    if normalized_path(&configured_path)? != normalized_path(&claudectomy_global_path)? {
        return Ok(false);
    }

    let current = match fs::read_to_string(&claudectomy_global_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to read global exclude file {}",
                    claudectomy_global_path.display()
                )
            });
        }
    };
    let entry = ignore_entry(target_rel);
    Ok(current.lines().any(|line| line == entry))
}

fn normalized_path(path: &Path) -> Result<PathBuf> {
    let expanded = expand_tilde(path);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        env::current_dir()?.join(expanded)
    };
    Ok(absolute.canonicalize().unwrap_or(absolute))
}

fn ensure_file(repo: &GitRepo, target_rel: &Path, dry_run: bool) -> Result<bool> {
    ensure_entry_file(repo, &ignore_entry(target_rel), dry_run)
}

fn ensure_entry_file(repo: &GitRepo, entry: &str, dry_run: bool) -> Result<bool> {
    let path = &repo.exclude_path;
    validate_exclude_location(repo)?;
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

    if dry_run {
        validate_exclude_writable(repo)?;
    } else {
        validate_exclude_writable(repo)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create exclude dir {}", parent.display()))?;
        }
        fs::write(path, next)
            .with_context(|| format!("failed to write exclude file {}", path.display()))?;
    }
    Ok(true)
}

fn remove_from_file(repo: &GitRepo, target_rel: &Path, dry_run: bool) -> Result<bool> {
    remove_entry_file(repo, &ignore_entry(target_rel), dry_run)
}

fn remove_entry_file(repo: &GitRepo, entry: &str, dry_run: bool) -> Result<bool> {
    let path = &repo.exclude_path;
    validate_exclude_location(repo)?;
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

    if dry_run {
        validate_exclude_writable(repo)?;
    } else {
        validate_exclude_writable(repo)?;
        fs::write(path, next)
            .with_context(|| format!("failed to write exclude file {}", path.display()))?;
    }
    Ok(true)
}

fn validate_exclude_location(repo: &GitRepo) -> Result<()> {
    let path = &repo.exclude_path;
    let git_dir = repo
        .git_dir
        .canonicalize()
        .with_context(|| format!("failed to canonicalize Git dir {}", repo.git_dir.display()))?;
    let git_common_dir = repo.git_common_dir.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize Git common dir {}",
            repo.git_common_dir.display()
        )
    })?;
    let parent = path
        .parent()
        .with_context(|| format!("exclude path {} has no parent", path.display()))?;
    let existing_parent = nearest_existing_ancestor(parent).with_context(|| {
        format!(
            "exclude parent {} has no existing ancestor",
            parent.display()
        )
    })?;
    let resolved_parent = existing_parent.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize exclude parent {}",
            existing_parent.display()
        )
    })?;
    if !resolved_parent.starts_with(&git_dir) && !resolved_parent.starts_with(&git_common_dir) {
        bail!(
            "exclude path {} resolves outside Git dirs {} and {}",
            path.display(),
            git_dir.display(),
            git_common_dir.display()
        );
    }

    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!("exclude file {} is a symlink", path.display());
            }
            if !metadata.is_file() {
                bail!("exclude file {} is not a regular file", path.display());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect exclude file {}", path.display()));
        }
    }

    Ok(())
}

fn validate_exclude_writable(repo: &GitRepo) -> Result<()> {
    validate_exclude_location(repo)?;
    let path = &repo.exclude_path;
    if path.exists() {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("failed to inspect exclude file {}", path.display()))?;
        if metadata.permissions().readonly() {
            bail!("exclude file {} is not writable", path.display());
        }
        return Ok(());
    }

    if let Some(parent) = path.parent()
        && parent.exists()
    {
        let metadata = fs::metadata(parent)
            .with_context(|| format!("failed to inspect exclude dir {}", parent.display()))?;
        if !metadata.is_dir() {
            bail!("exclude dir {} is not a directory", parent.display());
        }
        if metadata.permissions().readonly() {
            bail!("exclude dir {} is not writable", parent.display());
        }
    }
    Ok(())
}

fn nearest_existing_ancestor(mut path: &Path) -> Option<&Path> {
    loop {
        if path.exists() {
            return Some(path);
        }
        path = path.parent()?;
    }
}

fn ignore_entry(target_rel: &Path) -> String {
    format!("/{}", gitignore_path(target_rel))
}

fn unignore_entry(target_rel: &Path) -> String {
    format!("!/{}", gitignore_path(target_rel))
}

fn gitignore_path(target_rel: &Path) -> String {
    target_rel
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(escape_gitignore_segment(&part.to_string_lossy())),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn escape_gitignore_segment(segment: &str) -> String {
    let mut escaped = String::with_capacity(segment.len());
    for ch in segment.chars() {
        if matches!(ch, '\\' | '*' | '?' | '[' | ']' | ' ') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
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
            if !line.trim().is_empty() {
                entries.insert((*line).to_string());
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
        if !line.trim().is_empty() && *line != entry {
            entries.insert((*line).to_string());
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
    use std::path::Path;

    use super::{ignore_entry, remove_managed_entry, unignore_entry, upsert_managed_entry};

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

    #[test]
    fn entries_escape_gitignore_metacharacters() {
        let path = Path::new("nested/CLAUDE [1]?.md");
        assert_eq!(ignore_entry(path), "/nested/CLAUDE\\ \\[1\\]\\?.md");
        assert_eq!(unignore_entry(path), "!/nested/CLAUDE\\ \\[1\\]\\?.md");
    }

    #[test]
    fn remove_preserves_escaped_trailing_space_entries() {
        let entry = ignore_entry(Path::new("CLAUDE.md "));
        assert_eq!(entry, "/CLAUDE.md\\ ");
        let current = upsert_managed_entry("existing\n", &entry);
        assert!(current.contains("/CLAUDE.md\\ \n"));
        let next = remove_managed_entry(&current, &entry);
        assert!(!next.contains("claudectomy managed"));
        assert!(!next.contains("/CLAUDE.md\\ "));
    }
}
