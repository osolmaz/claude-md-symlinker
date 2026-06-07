use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use claudemdeez::{
    cleaner::{self, CleanOptions},
    config::{AppConfig, MaterializationStrategy, SourceMissingBehavior},
    reconciler::{self, ReconcileOptions},
    state::State,
};
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    data: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("temp root"),
            data: tempfile::tempdir().expect("temp data"),
        }
    }

    fn config(&self) -> AppConfig {
        let mut config = AppConfig::default();
        config.scan.roots = vec![self.root.path().to_path_buf()];
        config
    }

    fn state(&self) -> State {
        State::open(self.data.path().to_path_buf()).expect("state opens")
    }

    fn repo(&self, name: &str) -> PathBuf {
        let path = self.root.path().join(name);
        fs::create_dir_all(&path).expect("repo dir");
        git_init(&path);
        path
    }
}

#[test]
fn apply_creates_symlink_and_keeps_it_out_of_git_status() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.repos_scanned, 1);
    assert_eq!(report.summary.created, 1);
    assert_eq!(report.summary.exclude_updates, 1);
    assert!(repo.join("CLAUDE.md").exists());
    assert!(
        fs::symlink_metadata(repo.join("CLAUDE.md"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_link(repo.join("CLAUDE.md")).unwrap(),
        PathBuf::from("AGENTS.md")
    );
    assert!(git_status(&repo, "CLAUDE.md").is_empty());

    let exclude = git_stdout(&repo, &["rev-parse", "--git-path", "info/exclude"]);
    let exclude_path = path_from_git_output(&repo, &exclude);
    assert!(
        fs::read_to_string(exclude_path)
            .unwrap()
            .contains("/CLAUDE.md")
    );
}

#[test]
fn apply_is_idempotent() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();

    let config = fixture.config();
    let state = fixture.state();
    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    let second = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(second.summary.created, 0);
    assert_eq!(second.summary.kept, 1);
    assert_eq!(second.summary.exclude_updates, 0);
}

#[test]
fn repo_without_agents_md_is_left_alone() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.no_source, 1);
    assert!(!repo.join("CLAUDE.md").exists());
}

#[test]
fn excluded_paths_are_not_scanned_or_modified() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let mut config = fixture.config();
    config.scan.exclude_paths = vec![repo.clone()];

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.repos_scanned, 0);
    assert!(!repo.join("CLAUDE.md").exists());
}

#[test]
fn discovery_does_not_obey_ignore_files_inside_scan_roots() {
    let fixture = Fixture::new();
    fs::write(fixture.root.path().join(".ignore"), "repo/\n").unwrap();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.repos_scanned, 1);
    assert_eq!(report.summary.created, 1);
    assert!(repo.join("CLAUDE.md").exists());
}

#[test]
fn discovery_preserves_significant_whitespace_in_git_paths() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo ");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.repos_scanned, 1);
    assert_eq!(report.summary.created, 1);
    assert!(repo.join("CLAUDE.md").exists());
}

#[cfg(unix)]
#[test]
fn absolute_symlink_to_agents_md_is_repaired_to_relative_symlink() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let source = repo.join("AGENTS.md");
    fs::write(&source, "canonical instructions\n").unwrap();
    std::os::unix::fs::symlink(&source, repo.join("CLAUDE.md")).unwrap();

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.repaired, 1);
    assert_eq!(
        fs::read_link(repo.join("CLAUDE.md")).unwrap(),
        PathBuf::from("AGENTS.md")
    );
}

#[test]
fn existing_unknown_claude_md_is_not_changed_or_ignored() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    fs::write(repo.join("CLAUDE.md"), "user-owned claude file\n").unwrap();

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.conflicts, 1);
    assert_eq!(
        fs::read_to_string(repo.join("CLAUDE.md")).unwrap(),
        "user-owned claude file\n"
    );
    assert_eq!(git_status(&repo, "CLAUDE.md"), "?? CLAUDE.md\n");

    let exclude = git_stdout(&repo, &["rev-parse", "--git-path", "info/exclude"]);
    let exclude_path = path_from_git_output(&repo, &exclude);
    let exclude_text = fs::read_to_string(exclude_path).unwrap_or_default();
    assert!(!exclude_text.contains("/CLAUDE.md"));
}

#[test]
fn replacing_managed_shim_with_user_file_removes_old_exclude() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("CLAUDE.md")).unwrap();
    fs::write(repo.join("CLAUDE.md"), "user-owned replacement\n").unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.conflicts, 1);
    assert_eq!(report.summary.exclude_updates, 1);
    assert_eq!(git_status(&repo, "CLAUDE.md"), "?? CLAUDE.md\n");
}

#[test]
fn tracked_claude_md_is_not_changed_or_ignored() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    fs::write(repo.join("CLAUDE.md"), "tracked claude file\n").unwrap();
    git(&repo, &["add", "CLAUDE.md"]);

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.tracked_conflicts, 1);
    assert_eq!(
        fs::read_to_string(repo.join("CLAUDE.md")).unwrap(),
        "tracked claude file\n"
    );

    let exclude = git_stdout(&repo, &["rev-parse", "--git-path", "info/exclude"]);
    let exclude_path = path_from_git_output(&repo, &exclude);
    let exclude_text = fs::read_to_string(exclude_path).unwrap_or_default();
    assert!(!exclude_text.contains("/CLAUDE.md"));
}

#[test]
fn tracked_managed_shim_removes_old_exclude() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    assert!(git_check_ignore(&repo, "CLAUDE.md"));
    git(&repo, &["add", "-f", "CLAUDE.md"]);

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.tracked_conflicts, 1);
    assert_eq!(report.summary.exclude_updates, 1);
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap_or_default();
    assert!(!exclude_text.lines().any(|line| line == "/CLAUDE.md"));
    git(&repo, &["rm", "--cached", "CLAUDE.md"]);
    assert_eq!(git_status(&repo, "CLAUDE.md"), "?? CLAUDE.md\n");
}

#[test]
fn source_missing_tracked_managed_shim_removes_old_exclude() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    git(&repo, &["add", "-f", "CLAUDE.md"]);
    fs::remove_file(repo.join("AGENTS.md")).unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.tracked_conflicts, 1);
    assert_eq!(report.summary.no_source, 0);
    assert_eq!(report.summary.exclude_updates, 1);
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap_or_default();
    assert!(!exclude_text.lines().any(|line| line == "/CLAUDE.md"));
}

#[test]
fn source_missing_tracked_managed_shim_does_not_recreate_exclude() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::write(git_exclude_path(&repo), "").unwrap();
    git(&repo, &["add", "-f", "CLAUDE.md"]);
    fs::remove_file(repo.join("AGENTS.md")).unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.tracked_conflicts, 1);
    assert_eq!(report.summary.no_source, 0);
    assert_eq!(report.summary.exclude_updates, 0);
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap_or_default();
    assert!(!exclude_text.lines().any(|line| line == "/CLAUDE.md"));
}

#[test]
fn tracked_deleted_claude_md_is_not_recreated() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    fs::write(repo.join("CLAUDE.md"), "tracked claude file\n").unwrap();
    git(&repo, &["add", "CLAUDE.md"]);
    fs::remove_file(repo.join("CLAUDE.md")).unwrap();

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.tracked_conflicts, 1);
    assert!(!repo.join("CLAUDE.md").exists());
    assert_eq!(git_status(&repo, "CLAUDE.md"), "AD CLAUDE.md\n");
}

#[cfg(unix)]
#[test]
fn git_tracked_check_failure_is_reported_as_error() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    fs::write(repo.join("CLAUDE.md"), "tracked claude file\n").unwrap();
    git(&repo, &["add", "CLAUDE.md"]);
    fs::remove_file(repo.join("CLAUDE.md")).unwrap();
    let index_path = repo.join(".git/index");
    let original_permissions = fs::metadata(&index_path).unwrap().permissions();
    fs::set_permissions(&index_path, fs::Permissions::from_mode(0o000)).unwrap();

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    );

    fs::set_permissions(&index_path, original_permissions).unwrap();
    let report = report.unwrap();
    assert_eq!(report.summary.errors, 1);
    assert!(!repo.join("CLAUDE.md").exists());
}

#[cfg(unix)]
#[test]
fn unknown_broken_symlink_is_not_removed_when_source_is_missing() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    std::os::unix::fs::symlink("DOES_NOT_EXIST", repo.join("CLAUDE.md")).unwrap();
    let mut config = fixture.config();
    config.adapters.claude.on_source_missing = SourceMissingBehavior::RemoveIfManaged;

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.no_source, 1);
    assert_eq!(
        fs::read_link(repo.join("CLAUDE.md")).unwrap(),
        PathBuf::from("DOES_NOT_EXIST")
    );
}

#[test]
fn adapter_target_path_cannot_escape_repo_root() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let mut config = fixture.config();
    config.adapters.claude.target = PathBuf::from("../OUTSIDE.md");

    let error = reconciler::apply(
        &config,
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap_err();

    assert!(error.to_string().contains("must stay inside"));
    assert!(!fixture.root.path().join("OUTSIDE.md").exists());
}

#[test]
fn adapter_source_and_target_must_be_distinct() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let mut config = fixture.config();
    config.adapters.claude.target = PathBuf::from("AGENTS.md");

    let error = reconciler::apply(
        &config,
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap_err();

    assert!(error.to_string().contains("must be different"));
    assert_eq!(git_status(&repo, "AGENTS.md"), "?? AGENTS.md\n");
}

#[test]
fn adapter_paths_cannot_point_inside_git_internals() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let mut config = fixture.config();
    config.adapters.claude.target = PathBuf::from(".git/hooks/pre-commit");

    let error = reconciler::apply(
        &config,
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap_err();

    assert!(error.to_string().contains("Git internals"));
    assert!(!repo.join(".git/hooks/pre-commit").exists());
}

#[test]
fn adapter_paths_cannot_point_inside_git_internals_with_mixed_case() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let mut config = fixture.config();
    config.adapters.claude.target = PathBuf::from(".GIT/hooks/pre-commit");

    let error = reconciler::apply(
        &config,
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap_err();

    assert!(error.to_string().contains("Git internals"));
    assert!(!repo.join(".git/hooks/pre-commit").exists());
}

#[test]
fn adapter_paths_cannot_contain_control_characters() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let mut config = fixture.config();
    config.adapters.claude.target = PathBuf::from("CLAUDE.md\nfoo");

    let error = reconciler::apply(
        &config,
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap_err();

    assert!(error.to_string().contains("control characters"));
    assert!(!repo.join("CLAUDE.md\nfoo").exists());
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap_or_default();
    assert!(!exclude_text.contains("foo"));
}

#[cfg(unix)]
#[test]
fn symlinked_target_parent_outside_repo_is_rejected() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let outside = tempfile::tempdir().unwrap();
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), repo.join(".claude")).unwrap();
    let mut config = fixture.config();
    config.adapters.claude.target = PathBuf::from(".claude/CLAUDE.md");

    let dry_run = reconciler::apply(
        &config,
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: true },
    )
    .unwrap();

    assert_eq!(dry_run.summary.errors, 1);
    assert!(!outside.path().join("CLAUDE.md").exists());
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap_or_default();
    assert!(
        !exclude_text
            .lines()
            .any(|line| line == "/.claude/CLAUDE.md")
    );

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.errors, 1);
    assert!(!outside.path().join("CLAUDE.md").exists());
    assert!(
        fs::symlink_metadata(repo.join(".claude"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap_or_default();
    assert!(
        !exclude_text
            .lines()
            .any(|line| line == "/.claude/CLAUDE.md")
    );
}

#[cfg(unix)]
#[test]
fn symlinked_target_parent_inside_repo_is_rejected() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    fs::create_dir_all(repo.join("docs/subdir")).unwrap();
    std::os::unix::fs::symlink("docs/subdir", repo.join(".claude")).unwrap();
    let mut config = fixture.config();
    config.adapters.claude.target = PathBuf::from(".claude/CLAUDE.md");

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.errors, 1);
    assert!(!repo.join("docs/subdir/CLAUDE.md").exists());
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap_or_default();
    assert!(
        !exclude_text
            .lines()
            .any(|line| line == "/.claude/CLAUDE.md")
    );
}

#[test]
fn dry_run_rejects_target_parent_that_is_file() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    fs::write(repo.join(".claude"), "not a directory\n").unwrap();
    let mut config = fixture.config();
    config.adapters.claude.target = PathBuf::from(".claude/CLAUDE.md");

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: true },
    )
    .unwrap();

    assert_eq!(report.summary.errors, 1);
    assert_eq!(report.summary.created, 0);
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap_or_default();
    assert!(
        !exclude_text
            .lines()
            .any(|line| line == "/.claude/CLAUDE.md")
    );
}

#[cfg(unix)]
#[test]
fn existing_target_under_symlinked_parent_is_conflict() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let source = repo.join("AGENTS.md");
    fs::write(&source, "canonical instructions\n").unwrap();
    fs::create_dir_all(repo.join("docs/subdir")).unwrap();
    std::os::unix::fs::symlink("docs/subdir", repo.join(".claude")).unwrap();
    std::os::unix::fs::symlink(&source, repo.join("docs/subdir/CLAUDE.md")).unwrap();
    let mut config = fixture.config();
    config.adapters.claude.target = PathBuf::from(".claude/CLAUDE.md");

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.conflicts, 1);
    assert_eq!(
        fs::read_link(repo.join("docs/subdir/CLAUDE.md")).unwrap(),
        source
    );
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap_or_default();
    assert!(
        !exclude_text
            .lines()
            .any(|line| line == "/.claude/CLAUDE.md")
    );
}

#[cfg(unix)]
#[test]
fn symlink_with_lexical_match_through_symlinked_component_is_conflict() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir_all(outside.path().join("inner")).unwrap();
    fs::write(outside.path().join("AGENTS.md"), "outside\n").unwrap();
    fs::write(repo.join("AGENTS.md"), "canonical\n").unwrap();
    std::os::unix::fs::symlink(outside.path().join("inner"), repo.join("out")).unwrap();
    std::os::unix::fs::symlink("out/../AGENTS.md", repo.join("CLAUDE.md")).unwrap();

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.conflicts, 1);
    assert_eq!(
        fs::read_link(repo.join("CLAUDE.md")).unwrap(),
        PathBuf::from("out/../AGENTS.md")
    );
    assert_eq!(
        fs::read_to_string(repo.join("CLAUDE.md")).unwrap(),
        "outside\n"
    );
    assert_eq!(git_status(&repo, "CLAUDE.md"), "?? CLAUDE.md\n");
}

#[test]
fn target_with_gitignore_metacharacters_is_excluded_literally() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let mut config = fixture.config();
    config.adapters.claude.target = PathBuf::from("CLAUDE [1]?.md ");

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.created, 1);
    fs::write(repo.join("CLAUDE 1x.md"), "unrelated user file\n").unwrap();
    assert!(git_check_ignore(&repo, "CLAUDE [1]?.md "));
    assert!(!git_check_ignore(&repo, "CLAUDE 1x.md"));
    let status = git_status(&repo, "CLAUDE 1x.md");
    assert!(status.starts_with("?? "));
    assert!(status.contains("CLAUDE 1x.md"));
}

#[test]
fn tracked_check_treats_target_metacharacters_literally() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    fs::write(repo.join("CLAUDEA.md"), "tracked but different\n").unwrap();
    git(&repo, &["add", "CLAUDEA.md"]);
    let mut config = fixture.config();
    config.adapters.claude.target = PathBuf::from("CLAUDE?.md");

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.created, 1);
    assert_eq!(report.summary.tracked_conflicts, 0);
    assert!(repo.join("CLAUDE?.md").exists());
    assert_eq!(
        fs::read_to_string(repo.join("CLAUDEA.md")).unwrap(),
        "tracked but different\n"
    );
}

#[test]
fn invalid_utf8_exclude_file_is_not_overwritten() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let exclude = git_stdout(&repo, &["rev-parse", "--git-path", "info/exclude"]);
    let exclude_path = path_from_git_output(&repo, &exclude);
    let original = b"keep-this-rule\n\xff\n";
    fs::write(&exclude_path, original).unwrap();

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.errors, 1);
    assert!(!repo.join("CLAUDE.md").exists());
    assert_eq!(fs::read(exclude_path).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn fifo_source_is_rejected_without_hanging() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let mkfifo = Command::new("mkfifo")
        .arg(repo.join("AGENTS.md"))
        .output()
        .expect("mkfifo runs");
    assert!(
        mkfifo.status.success(),
        "mkfifo failed: {}",
        String::from_utf8_lossy(&mkfifo.stderr)
    );
    let config_path = fixture.root.path().join("config.toml");
    fs::write(
        &config_path,
        format!("[scan]\nroots = [\"{}\"]\n", fixture.root.path().display()),
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_claudemdeez");
    let child = Command::new(bin)
        .env("CLAUDEMDEEZ_DATA_DIR", fixture.data.path())
        .args(["--config", config_path.to_str().unwrap(), "apply"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("apply starts");

    let output = wait_with_timeout(child, Duration::from_secs(2))
        .expect("apply should reject the FIFO source without hanging");

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("is not a regular file"));
    assert!(!repo.join("CLAUDE.md").exists());
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap_or_default();
    assert!(!exclude_text.lines().any(|line| line == "/CLAUDE.md"));
}

#[test]
fn non_file_source_is_rejected_before_creating_shim() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::create_dir(repo.join("AGENTS.md")).unwrap();

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.errors, 1);
    assert!(!repo.join("CLAUDE.md").exists());
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap_or_default();
    assert!(!exclude_text.lines().any(|line| line == "/CLAUDE.md"));
}

#[cfg(unix)]
#[test]
fn symlinked_source_is_rejected_before_creating_shim() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), "outside secret\n").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), repo.join("AGENTS.md")).unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Copy;

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.errors, 1);
    assert!(!repo.join("CLAUDE.md").exists());
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap_or_default();
    assert!(!exclude_text.lines().any(|line| line == "/CLAUDE.md"));
}

#[cfg(unix)]
#[test]
fn symlinked_source_parent_outside_repo_is_rejected_before_copying() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("AGENTS.md"), "outside secret\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), repo.join("link")).unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Copy;
    config.adapters.claude.source = PathBuf::from("link/AGENTS.md");

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.errors, 1);
    assert!(!repo.join("CLAUDE.md").exists());
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap_or_default();
    assert!(!exclude_text.lines().any(|line| line == "/CLAUDE.md"));
}

#[test]
fn dry_run_reports_without_mutating_repo_or_state() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let state = State::disabled();

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: true },
    )
    .unwrap();

    assert_eq!(report.summary.created, 1);
    assert!(!repo.join("CLAUDE.md").exists());
    assert!(fs::read_dir(fixture.data.path()).unwrap().next().is_none());
}

#[test]
fn copy_materialization_refreshes_managed_copy() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "v1\n").unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Copy;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::write(repo.join("AGENTS.md"), "v2\n").unwrap();
    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.refreshed, 1);
    let text = fs::read_to_string(repo.join("CLAUDE.md")).unwrap();
    assert!(text.contains("claudemdeez managed"));
    assert!(text.ends_with("v2\n"));
}

#[cfg(unix)]
#[test]
fn copy_materialization_preserves_source_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "private instructions\n").unwrap();
    fs::set_permissions(repo.join("AGENTS.md"), fs::Permissions::from_mode(0o600)).unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Copy;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    let target_mode = fs::metadata(repo.join("CLAUDE.md"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(target_mode, 0o600);

    fs::set_permissions(repo.join("AGENTS.md"), fs::Permissions::from_mode(0o640)).unwrap();
    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.refreshed, 1);
    let target_mode = fs::metadata(repo.join("CLAUDE.md"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(target_mode, 0o640);

    fs::write(repo.join("AGENTS.md"), "read-only instructions\n").unwrap();
    fs::set_permissions(repo.join("AGENTS.md"), fs::Permissions::from_mode(0o444)).unwrap();
    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.refreshed, 1);
    let target_mode = fs::metadata(repo.join("CLAUDE.md"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(target_mode, 0o444);
    assert!(
        fs::read_to_string(repo.join("CLAUDE.md"))
            .unwrap()
            .ends_with("read-only instructions\n")
    );
}

#[test]
fn clean_removes_readonly_managed_copy() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let source = repo.join("AGENTS.md");
    fs::write(&source, "read-only instructions\n").unwrap();
    let mut permissions = fs::metadata(&source).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&source, permissions).unwrap();

    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Copy;
    let state = fixture.state();
    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
    }
    #[cfg(windows)]
    {
        let mut source_permissions = fs::metadata(&source).unwrap().permissions();
        source_permissions.set_readonly(false);
        fs::set_permissions(&source, source_permissions).unwrap();
    }
    fs::remove_file(&source).unwrap();

    let report = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: false,
            remove_if_source_missing: true,
        },
    )
    .unwrap();

    assert_eq!(report.summary.cleaned, 1);
    assert!(!repo.join("CLAUDE.md").exists());
}

#[cfg(unix)]
#[test]
fn dry_run_copy_refresh_reports_readonly_target_refresh() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "v1\n").unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Copy;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::write(repo.join("AGENTS.md"), "v2\n").unwrap();
    fs::set_permissions(repo.join("CLAUDE.md"), fs::Permissions::from_mode(0o444)).unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: true },
    );

    fs::set_permissions(repo.join("CLAUDE.md"), fs::Permissions::from_mode(0o644)).unwrap();
    let report = report.unwrap();
    assert_eq!(report.summary.errors, 0);
    assert_eq!(report.summary.refreshed, 1);
    assert!(
        fs::read_to_string(repo.join("CLAUDE.md"))
            .unwrap()
            .ends_with("v1\n")
    );
}

#[cfg(unix)]
#[test]
fn dry_run_symlink_repair_rejects_unwritable_parent() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let source = repo.join("AGENTS.md");
    fs::write(&source, "canonical instructions\n").unwrap();
    std::os::unix::fs::symlink(&source, repo.join("CLAUDE.md")).unwrap();
    let original_permissions = fs::metadata(&repo).unwrap().permissions();
    fs::set_permissions(&repo, fs::Permissions::from_mode(0o555)).unwrap();

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: true },
    );

    fs::set_permissions(&repo, original_permissions).unwrap();
    let report = report.unwrap();
    assert_eq!(report.summary.errors, 1);
    assert_eq!(report.summary.repaired, 0);
    assert_eq!(fs::read_link(repo.join("CLAUDE.md")).unwrap(), source);
}

#[test]
fn explicit_copy_strategy_replaces_existing_managed_symlink() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let mut config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    assert!(
        fs::symlink_metadata(repo.join("CLAUDE.md"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    config.materialization.strategy = MaterializationStrategy::Copy;
    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.repaired, 1);
    assert!(
        !fs::symlink_metadata(repo.join("CLAUDE.md"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let text = fs::read_to_string(repo.join("CLAUDE.md")).unwrap();
    assert!(text.contains("claudemdeez managed"));
    assert!(text.ends_with("canonical instructions\n"));
}

#[test]
fn hardlink_materialization_conflicts_after_source_replacement() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "v1\n").unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Hardlink;
    config.materialization.allow_hardlink = true;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    fs::rename(repo.join("AGENTS.md"), repo.join("old-agents")).unwrap();
    fs::write(repo.join("AGENTS.md"), "v2\n").unwrap();
    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.conflicts, 1);
    assert_eq!(report.summary.exclude_updates, 1);
    assert_eq!(fs::read_to_string(repo.join("CLAUDE.md")).unwrap(), "v1\n");
    assert!(!same_file::is_same_file(repo.join("AGENTS.md"), repo.join("CLAUDE.md")).unwrap());
}

#[test]
fn preexisting_untracked_hardlink_is_conflict_without_state() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "v1\n").unwrap();
    fs::hard_link(repo.join("AGENTS.md"), repo.join("CLAUDE.md")).unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Copy;

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.conflicts, 1);
    assert_eq!(report.summary.repaired, 0);
    assert_eq!(fs::read_to_string(repo.join("CLAUDE.md")).unwrap(), "v1\n");
    assert!(same_file::is_same_file(repo.join("AGENTS.md"), repo.join("CLAUDE.md")).unwrap());
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap_or_default();
    assert!(!exclude_text.lines().any(|line| line == "/CLAUDE.md"));
}

#[test]
fn managed_hardlink_is_idempotent_with_state() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "v1\n").unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Hardlink;
    config.materialization.allow_hardlink = true;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.kept, 1);
    assert_eq!(report.summary.conflicts, 0);
    assert!(same_file::is_same_file(repo.join("AGENTS.md"), repo.join("CLAUDE.md")).unwrap());
}

#[test]
fn hardlink_replacement_with_same_contents_is_not_reclaimed() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "v1\n").unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Hardlink;
    config.materialization.allow_hardlink = true;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("CLAUDE.md")).unwrap();
    fs::write(repo.join("CLAUDE.md"), "v1\n").unwrap();
    fs::write(repo.join("AGENTS.md"), "v2\n").unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.conflicts, 1);
    assert_eq!(report.summary.exclude_updates, 1);
    assert_eq!(fs::read_to_string(repo.join("CLAUDE.md")).unwrap(), "v1\n");
    assert!(!same_file::is_same_file(repo.join("AGENTS.md"), repo.join("CLAUDE.md")).unwrap());
}

#[test]
fn remove_if_managed_does_not_delete_same_content_hardlink_replacement() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "v1\n").unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Hardlink;
    config.materialization.allow_hardlink = true;
    config.adapters.claude.on_source_missing = SourceMissingBehavior::RemoveIfManaged;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("CLAUDE.md")).unwrap();
    fs::write(repo.join("CLAUDE.md"), "v1\n").unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.no_source, 1);
    assert_eq!(report.summary.cleaned, 0);
    assert_eq!(report.summary.exclude_updates, 1);
    assert_eq!(fs::read_to_string(repo.join("CLAUDE.md")).unwrap(), "v1\n");
}

#[test]
fn dry_run_reports_strategy_replacement_as_repair() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "v1\n").unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Hardlink;
    config.materialization.allow_hardlink = true;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    config.materialization.strategy = MaterializationStrategy::Copy;
    let preview = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: true },
    )
    .unwrap();

    assert_eq!(preview.summary.repaired, 1);
    assert_eq!(preview.summary.kept, 0);
    assert!(same_file::is_same_file(repo.join("AGENTS.md"), repo.join("CLAUDE.md")).unwrap());
}

#[test]
fn dry_run_validates_forced_strategy_replacement() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let source_rel = deep_relative_path("source", 1300, "AGENTS.md");
    let target_rel = deep_relative_path("target", 1300, "CLAUDE.md");
    let Some(source_parent) = source_rel.parent() else {
        panic!("source has parent");
    };
    let Some(target_parent) = target_rel.parent() else {
        panic!("target has parent");
    };
    if fs::create_dir_all(repo.join(source_parent)).is_err()
        || fs::create_dir_all(repo.join(target_parent)).is_err()
    {
        return;
    }
    fs::write(repo.join(&source_rel), "canonical instructions\n").unwrap();
    let mut config = fixture.config();
    config.adapters.claude.source = source_rel;
    config.adapters.claude.target = target_rel.clone();
    config.materialization.strategy = MaterializationStrategy::Copy;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    let target = repo.join(&target_rel);
    let original = fs::read(&target).unwrap();

    config.materialization.strategy = MaterializationStrategy::Symlink;
    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: true },
    )
    .unwrap();

    assert_eq!(report.summary.errors, 1);
    assert_eq!(fs::read(&target).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn dry_run_reports_unwritable_target_parent_error() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let original_permissions = fs::metadata(&repo).unwrap().permissions();
    fs::set_permissions(&repo, fs::Permissions::from_mode(0o555)).unwrap();

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: true },
    );

    fs::set_permissions(&repo, original_permissions).unwrap();
    let report = report.unwrap();
    assert_eq!(report.summary.errors, 1);
    assert!(!repo.join("CLAUDE.md").exists());
}

#[cfg(unix)]
#[test]
fn failed_recreate_of_missing_target_removes_stale_exclude() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("CLAUDE.md")).unwrap();

    let original_permissions = fs::metadata(&repo).unwrap().permissions();
    fs::set_permissions(&repo, fs::Permissions::from_mode(0o555)).unwrap();
    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    );
    fs::set_permissions(&repo, original_permissions).unwrap();

    let report = report.unwrap();
    assert_eq!(report.summary.errors, 1);
    assert!(!repo.join("CLAUDE.md").exists());
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap();
    assert!(!exclude_text.lines().any(|line| line == "/CLAUDE.md"));

    fs::write(repo.join("CLAUDE.md"), "user-owned replacement\n").unwrap();
    assert_eq!(git_status(&repo, "CLAUDE.md"), "?? CLAUDE.md\n");
}

#[cfg(unix)]
#[test]
fn failed_refresh_of_existing_managed_target_keeps_exclude() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "v1\n").unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Copy;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    let exclude_path = git_exclude_path(&repo);
    fs::write(&exclude_path, "").unwrap();
    fs::write(repo.join("AGENTS.md"), "v2\n").unwrap();
    let original_permissions = fs::metadata(&repo).unwrap().permissions();
    fs::set_permissions(&repo, fs::Permissions::from_mode(0o555)).unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    );

    fs::set_permissions(&repo, original_permissions).unwrap();
    let report = report.unwrap();
    assert_eq!(report.summary.errors, 1);
    assert!(repo.join("CLAUDE.md").exists());
    let exclude_text = fs::read_to_string(exclude_path).unwrap();
    assert!(exclude_text.lines().any(|line| line == "/CLAUDE.md"));
}

#[cfg(unix)]
#[test]
fn failed_strategy_replacement_removes_stale_exclude_when_target_was_removed() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let source_rel = deep_relative_path("source", 1300, "AGENTS.md");
    let target_rel = deep_relative_path("target", 1300, "CLAUDE.md");
    let Some(source_parent) = source_rel.parent() else {
        panic!("source has parent");
    };
    let Some(target_parent) = target_rel.parent() else {
        panic!("target has parent");
    };
    if fs::create_dir_all(repo.join(source_parent)).is_err()
        || fs::create_dir_all(repo.join(target_parent)).is_err()
    {
        return;
    }
    fs::write(repo.join(&source_rel), "canonical instructions\n").unwrap();
    let mut config = fixture.config();
    config.adapters.claude.source = source_rel;
    config.adapters.claude.target = target_rel.clone();
    config.materialization.strategy = MaterializationStrategy::Copy;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    config.materialization.strategy = MaterializationStrategy::Symlink;
    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.errors, 1);
    assert!(!repo.join(&target_rel).exists());
    let target_entry = format!("/{}", target_rel.display());
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap_or_default();
    assert!(!exclude_text.lines().any(|line| line == target_entry));
    assert!(!git_check_ignore(&repo, target_rel.to_str().unwrap()));
}

#[test]
fn clean_removes_only_stale_managed_shims_when_requested() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();
    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();

    let preview = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: true,
            remove_if_source_missing: true,
        },
    )
    .unwrap();
    assert_eq!(preview.summary.cleaned, 1);
    assert!(fs::symlink_metadata(repo.join("CLAUDE.md")).is_ok());

    let cleaned = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: false,
            remove_if_source_missing: true,
        },
    )
    .unwrap();
    assert_eq!(cleaned.summary.cleaned, 1);
    assert_eq!(cleaned.summary.exclude_updates, 1);
    assert!(!repo.join("CLAUDE.md").exists());

    fs::write(repo.join("CLAUDE.md"), "new user claude\n").unwrap();
    assert_eq!(git_status(&repo, "CLAUDE.md"), "?? CLAUDE.md\n");
}

#[test]
fn apply_remove_if_managed_removes_stale_shim_and_exclude() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let mut config = fixture.config();
    config.adapters.claude.on_source_missing = SourceMissingBehavior::RemoveIfManaged;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.cleaned, 1);
    assert_eq!(report.summary.exclude_updates, 1);
    assert!(!repo.join("CLAUDE.md").exists());
    fs::write(repo.join("CLAUDE.md"), "new user claude\n").unwrap();
    assert_eq!(git_status(&repo, "CLAUDE.md"), "?? CLAUDE.md\n");
}

#[test]
fn remove_if_managed_cleans_nested_relative_symlink_after_source_deletion() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let mut config = fixture.config();
    config.adapters.claude.target = PathBuf::from(".claude/CLAUDE.md");
    config.adapters.claude.on_source_missing = SourceMissingBehavior::RemoveIfManaged;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    assert_eq!(
        fs::read_link(repo.join(".claude/CLAUDE.md")).unwrap(),
        PathBuf::from("../AGENTS.md")
    );
    fs::remove_file(repo.join("AGENTS.md")).unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.cleaned, 1);
    assert_eq!(report.summary.exclude_updates, 1);
    assert!(fs::symlink_metadata(repo.join(".claude/CLAUDE.md")).is_err());
}

#[test]
fn source_missing_with_user_file_removes_stale_exclude_even_when_leaving_managed_shims() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("CLAUDE.md")).unwrap();
    fs::write(repo.join("CLAUDE.md"), "user-owned replacement\n").unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.no_source, 1);
    assert_eq!(report.summary.exclude_updates, 1);
    assert_eq!(git_status(&repo, "CLAUDE.md"), "?? CLAUDE.md\n");
}

#[test]
fn clean_with_user_file_removes_stale_exclude_when_source_is_missing() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("CLAUDE.md")).unwrap();
    fs::write(repo.join("CLAUDE.md"), "user-owned replacement\n").unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();

    let report = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: false,
            remove_if_source_missing: true,
        },
    )
    .unwrap();

    assert_eq!(report.summary.no_source, 1);
    assert_eq!(report.summary.exclude_updates, 1);
    assert_eq!(
        fs::read_to_string(repo.join("CLAUDE.md")).unwrap(),
        "user-owned replacement\n"
    );
    assert_eq!(git_status(&repo, "CLAUDE.md"), "?? CLAUDE.md\n");
}

#[test]
fn clean_with_user_file_removes_stale_exclude_when_source_exists() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("CLAUDE.md")).unwrap();
    fs::write(repo.join("CLAUDE.md"), "user-owned replacement\n").unwrap();

    let report = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: false,
            remove_if_source_missing: false,
        },
    )
    .unwrap();

    assert_eq!(report.summary.conflicts, 1);
    assert_eq!(report.summary.exclude_updates, 1);
    assert_eq!(
        fs::read_to_string(repo.join("CLAUDE.md")).unwrap(),
        "user-owned replacement\n"
    );
    assert_eq!(git_status(&repo, "CLAUDE.md"), "?? CLAUDE.md\n");
}

#[test]
fn source_missing_after_deleted_managed_target_removes_stale_exclude() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("CLAUDE.md")).unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.no_source, 1);
    assert_eq!(report.summary.exclude_updates, 1);

    fs::write(repo.join("CLAUDE.md"), "new user claude\n").unwrap();
    assert_eq!(git_status(&repo, "CLAUDE.md"), "?? CLAUDE.md\n");
}

#[test]
fn source_missing_keeps_existing_managed_target_ignored() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(git_exclude_path(&repo)).unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.no_source, 1);
    assert_eq!(report.summary.exclude_updates, 1);
    assert!(fs::symlink_metadata(repo.join("CLAUDE.md")).is_ok());
    assert!(git_status(&repo, "CLAUDE.md").is_empty());
}

#[cfg(unix)]
#[test]
fn source_missing_cleanup_preserves_exclude_when_target_cannot_be_inspected() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    fs::create_dir(repo.join("blocked")).unwrap();
    let mut config = fixture.config();
    config.adapters.claude.target = PathBuf::from("blocked/CLAUDE.md");
    config.adapters.claude.on_source_missing = SourceMissingBehavior::RemoveIfManaged;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();
    let original_permissions = fs::metadata(repo.join("blocked")).unwrap().permissions();
    fs::set_permissions(repo.join("blocked"), fs::Permissions::from_mode(0o000)).unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    );

    fs::set_permissions(repo.join("blocked"), original_permissions).unwrap();
    let report = report.unwrap();
    assert_eq!(report.summary.errors, 1);
    assert_eq!(report.summary.cleaned, 0);
    assert!(fs::symlink_metadata(repo.join("blocked/CLAUDE.md")).is_ok());
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap();
    assert!(
        exclude_text
            .lines()
            .any(|line| line == "/blocked/CLAUDE.md")
    );
}

#[test]
fn remove_if_managed_does_not_clean_unprovable_stale_hardlink_after_source_deletion() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "v1\n").unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Hardlink;
    config.materialization.allow_hardlink = true;
    config.adapters.claude.on_source_missing = SourceMissingBehavior::RemoveIfManaged;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.no_source, 1);
    assert_eq!(report.summary.exclude_updates, 1);
    assert!(repo.join("CLAUDE.md").exists());
}

#[test]
fn stale_hardlink_state_does_not_claim_directory_replacement() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "v1\n").unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Hardlink;
    config.materialization.allow_hardlink = true;
    config.adapters.claude.on_source_missing = SourceMissingBehavior::RemoveIfManaged;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();
    fs::remove_file(repo.join("CLAUDE.md")).unwrap();
    fs::create_dir(repo.join("CLAUDE.md")).unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.no_source, 1);
    assert_eq!(report.summary.errors, 0);
    assert!(repo.join("CLAUDE.md").is_dir());
}

#[test]
fn clean_does_not_remove_unprovable_stale_hardlink_after_source_deletion() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "v1\n").unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Hardlink;
    config.materialization.allow_hardlink = true;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();

    let report = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: false,
            remove_if_source_missing: true,
        },
    )
    .unwrap();

    assert_eq!(report.summary.no_source, 1);
    assert_eq!(report.summary.exclude_updates, 1);
    assert!(repo.join("CLAUDE.md").exists());
}

#[test]
fn clean_does_not_claim_directory_replacement_from_stale_hardlink_state() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "v1\n").unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Hardlink;
    config.materialization.allow_hardlink = true;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();
    fs::remove_file(repo.join("CLAUDE.md")).unwrap();
    fs::create_dir(repo.join("CLAUDE.md")).unwrap();

    let report = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: false,
            remove_if_source_missing: true,
        },
    )
    .unwrap();

    assert_eq!(report.summary.no_source, 1);
    assert_eq!(report.summary.errors, 0);
    assert!(repo.join("CLAUDE.md").is_dir());
}

#[test]
fn apply_no_source_drops_unprovable_stale_hardlink_ownership() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "v1\n").unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Hardlink;
    config.materialization.allow_hardlink = true;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();
    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    let report = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: false,
            remove_if_source_missing: true,
        },
    )
    .unwrap();

    assert_eq!(report.summary.no_source, 1);
    assert_eq!(report.summary.cleaned, 0);
    assert!(repo.join("CLAUDE.md").exists());
}

#[test]
fn clean_preview_drops_unprovable_stale_hardlink_ownership() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "v1\n").unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Hardlink;
    config.materialization.allow_hardlink = true;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();

    let preview = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: false,
            remove_if_source_missing: false,
        },
    )
    .unwrap();
    assert_eq!(preview.summary.no_source, 1);

    let cleaned = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: false,
            remove_if_source_missing: true,
        },
    )
    .unwrap();

    assert_eq!(cleaned.summary.no_source, 1);
    assert_eq!(cleaned.summary.cleaned, 0);
    assert!(repo.join("CLAUDE.md").exists());
}

#[test]
fn clean_does_not_reclaim_hardlink_after_source_replacement() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "v1\n").unwrap();
    let mut config = fixture.config();
    config.materialization.strategy = MaterializationStrategy::Hardlink;
    config.materialization.allow_hardlink = true;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();
    fs::write(repo.join("AGENTS.md"), "v2\n").unwrap();

    let preview = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: false,
            remove_if_source_missing: false,
        },
    )
    .unwrap();
    assert_eq!(preview.summary.conflicts, 1);
    assert_eq!(preview.summary.exclude_updates, 1);
    assert_eq!(git_status(&repo, "CLAUDE.md"), "?? CLAUDE.md\n");

    let conflict = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(conflict.summary.conflicts, 1);
    assert_eq!(conflict.summary.exclude_updates, 0);
    assert_eq!(fs::read_to_string(repo.join("CLAUDE.md")).unwrap(), "v1\n");
    assert!(!same_file::is_same_file(repo.join("AGENTS.md"), repo.join("CLAUDE.md")).unwrap());
}

#[test]
fn cli_dry_run_clean_does_not_remove_stale_hardlinks_by_state() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "v1\n").unwrap();
    let config_path = fixture.root.path().join("config.toml");
    fs::write(
        &config_path,
        format!(
            "[scan]\nroots = [\"{}\"]\n\n[materialization]\nstrategy = \"hardlink\"\nallow_hardlink = true\n",
            fixture.root.path().display()
        ),
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let apply = Command::new(bin)
        .env("CLAUDEMDEEZ_DATA_DIR", fixture.data.path())
        .args(["--config", config_path.to_str().unwrap(), "apply"])
        .output()
        .expect("apply runs");
    assert!(
        apply.status.success(),
        "apply failed: {}",
        String::from_utf8_lossy(&apply.stderr)
    );
    fs::remove_file(repo.join("AGENTS.md")).unwrap();

    let dry_run = Command::new(bin)
        .env("CLAUDEMDEEZ_DATA_DIR", fixture.data.path())
        .args([
            "--dry-run",
            "--json",
            "--config",
            config_path.to_str().unwrap(),
            "clean",
            "--remove-if-source-missing",
        ])
        .output()
        .expect("dry-run clean runs");
    assert!(
        dry_run.status.success(),
        "dry-run failed: {}",
        String::from_utf8_lossy(&dry_run.stderr)
    );
    let dry_run_json: serde_json::Value = serde_json::from_slice(&dry_run.stdout).unwrap();
    assert_eq!(dry_run_json["summary"]["no_source"], 1);
    assert_eq!(dry_run_json["summary"]["cleaned"], 0);
    assert!(repo.join("CLAUDE.md").exists());

    let clean = Command::new(bin)
        .env("CLAUDEMDEEZ_DATA_DIR", fixture.data.path())
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "clean",
            "--remove-if-source-missing",
        ])
        .output()
        .expect("clean runs");
    assert!(
        clean.status.success(),
        "clean failed: {}",
        String::from_utf8_lossy(&clean.stderr)
    );
    assert!(repo.join("CLAUDE.md").exists());
}

#[test]
fn cli_dry_run_init_does_not_write_config() {
    let fixture = Fixture::new();
    let config_path = fixture.root.path().join("dry-run-config.toml");
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let dry_run = Command::new(bin)
        .args([
            "--dry-run",
            "--config",
            config_path.to_str().unwrap(),
            "init",
            fixture.root.path().to_str().unwrap(),
        ])
        .output()
        .expect("dry-run init runs");

    assert!(
        dry_run.status.success(),
        "dry-run init failed: {}",
        String::from_utf8_lossy(&dry_run.stderr)
    );
    assert!(!config_path.exists());

    let dry_run_json = Command::new(bin)
        .args([
            "--dry-run",
            "--json",
            "--config",
            config_path.to_str().unwrap(),
            "init",
            fixture.root.path().to_str().unwrap(),
        ])
        .output()
        .expect("dry-run json init runs");

    assert!(
        dry_run_json.status.success(),
        "dry-run json init failed: {}",
        String::from_utf8_lossy(&dry_run_json.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&dry_run_json.stdout).unwrap();
    assert_eq!(json["dry_run"], true);
    assert!(!config_path.exists());
}

#[test]
fn global_mode_is_rejected_without_mutating_repo_or_global_config() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config_path = fixture.root.path().join("config.toml");
    let data_dir = fixture.data.path();
    fs::write(
        &config_path,
        format!(
            "[scan]\nroots = [\"{}\"]\n\n[git]\nexclude_mode = \"global\"\n",
            fixture.root.path().display()
        ),
    )
    .unwrap();
    let global_config = fixture.root.path().join("global-gitconfig");
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let apply = Command::new(bin)
        .env("CLAUDEMDEEZ_DATA_DIR", data_dir)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .args(["--config", config_path.to_str().unwrap(), "apply"])
        .output()
        .expect("apply runs");
    assert_eq!(apply.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&apply.stderr);
    assert!(stderr.contains("global exclude mode is disabled"));
    assert!(!repo.join("CLAUDE.md").exists());

    let configured = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .args(["config", "--global", "--get", "core.excludesFile"])
        .output()
        .expect("git config reads");
    assert!(!configured.status.success());
}

#[test]
fn global_mode_is_rejected_even_without_sources() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let config_path = fixture.root.path().join("config.toml");
    fs::write(
        &config_path,
        format!(
            "[scan]\nroots = [\"{}\"]\n\n[git]\nexclude_mode = \"global\"\n",
            fixture.root.path().display()
        ),
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let apply = Command::new(bin)
        .env("CLAUDEMDEEZ_DATA_DIR", fixture.data.path())
        .args(["--config", config_path.to_str().unwrap(), "apply"])
        .output()
        .expect("apply runs");

    assert_eq!(apply.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&apply.stderr);
    assert!(stderr.contains("global exclude mode is disabled"));
    assert!(!repo.join("CLAUDE.md").exists());
}

#[test]
fn doctor_fails_when_global_mode_is_configured() {
    let fixture = Fixture::new();
    let config_path = fixture.root.path().join("config.toml");
    fs::write(
        &config_path,
        format!(
            "[scan]\nroots = [\"{}\"]\n\n[git]\nexclude_mode = \"global\"\n",
            fixture.root.path().display()
        ),
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let doctor = Command::new(bin)
        .env("CLAUDEMDEEZ_DATA_DIR", fixture.data.path())
        .args(["--config", config_path.to_str().unwrap(), "doctor"])
        .output()
        .expect("doctor runs");

    assert_eq!(doctor.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(stdout.contains("fail\tconfig"));
    assert!(stdout.contains("global exclude mode is disabled"));
}

#[test]
fn doctor_dry_run_does_not_create_state() {
    let fixture = Fixture::new();
    let config_path = fixture.root.path().join("config.toml");
    fs::write(
        &config_path,
        format!("[scan]\nroots = [\"{}\"]\n", fixture.root.path().display()),
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let doctor = Command::new(bin)
        .env("CLAUDEMDEEZ_DATA_DIR", fixture.data.path())
        .args([
            "--dry-run",
            "--config",
            config_path.to_str().unwrap(),
            "doctor",
        ])
        .output()
        .expect("doctor runs");

    assert!(
        doctor.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    assert!(fs::read_dir(fixture.data.path()).unwrap().next().is_none());
}

#[test]
fn per_repo_mode_conflict_unignores_owned_global_exclude() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config_path = fixture.root.path().join("config.toml");
    let data_dir = fixture.data.path();
    let global_config = fixture.root.path().join("global-gitconfig");
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    fs::write(repo.join("CLAUDE.md"), "user-owned replacement\n").unwrap();
    fs::write(
        data_dir.join("git-excludes"),
        "# claudemdeez managed begin\n/CLAUDE.md\n# claudemdeez managed end\n",
    )
    .unwrap();
    let configured = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .args([
            "config",
            "--global",
            "core.excludesFile",
            data_dir.join("git-excludes").to_str().unwrap(),
        ])
        .output()
        .expect("git config writes");
    assert!(
        configured.status.success(),
        "git config failed: {}",
        String::from_utf8_lossy(&configured.stderr)
    );

    fs::write(
        &config_path,
        format!("[scan]\nroots = [\"{}\"]\n", fixture.root.path().display()),
    )
    .unwrap();
    let conflict = Command::new(bin)
        .env("CLAUDEMDEEZ_DATA_DIR", data_dir)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .args(["--config", config_path.to_str().unwrap(), "apply"])
        .output()
        .expect("per-repo apply runs");
    assert_eq!(conflict.status.code(), Some(2));

    let status = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .arg("-C")
        .arg(&repo)
        .args(["status", "--short", "--", "CLAUDE.md"])
        .output()
        .expect("git status runs");
    assert!(status.status.success());
    assert_eq!(String::from_utf8_lossy(&status.stdout), "?? CLAUDE.md\n");

    let exclude = fs::read_to_string(git_exclude_path(&repo)).unwrap();
    assert!(exclude.lines().any(|line| line == "!/CLAUDE.md"));
    assert!(!exclude.lines().any(|line| line == "/CLAUDE.md"));
}

#[test]
fn per_repo_mode_unignores_owned_global_exclude_configured_with_tilde() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    fs::write(repo.join("CLAUDE.md"), "user-owned replacement\n").unwrap();

    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
    let data_dir = tempfile::Builder::new()
        .prefix(".claudemdeez-global-")
        .tempdir_in(&home)
        .expect("home temp data dir");
    let global_excludes = data_dir.path().join("git-excludes");
    fs::write(
        &global_excludes,
        "# claudemdeez managed begin\n/CLAUDE.md\n# claudemdeez managed end\n",
    )
    .unwrap();
    let tilde_excludes = format!(
        "~/{}",
        global_excludes
            .strip_prefix(&home)
            .unwrap()
            .to_string_lossy()
    );

    let config_path = fixture.root.path().join("config.toml");
    fs::write(
        &config_path,
        format!("[scan]\nroots = [\"{}\"]\n", fixture.root.path().display()),
    )
    .unwrap();
    let global_config = fixture.root.path().join("global-gitconfig");
    let configured = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .args(["config", "--global", "core.excludesFile", &tilde_excludes])
        .output()
        .expect("git config writes");
    assert!(
        configured.status.success(),
        "git config failed: {}",
        String::from_utf8_lossy(&configured.stderr)
    );

    let bin = env!("CARGO_BIN_EXE_claudemdeez");
    let conflict = Command::new(bin)
        .env("CLAUDEMDEEZ_DATA_DIR", data_dir.path())
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .args(["--config", config_path.to_str().unwrap(), "apply"])
        .output()
        .expect("per-repo apply runs");
    assert_eq!(conflict.status.code(), Some(2));

    let status = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .arg("-C")
        .arg(&repo)
        .args(["status", "--short", "--", "CLAUDE.md"])
        .output()
        .expect("git status runs");
    assert!(status.status.success());
    assert_eq!(String::from_utf8_lossy(&status.stdout), "?? CLAUDE.md\n");

    let exclude = fs::read_to_string(git_exclude_path(&repo)).unwrap();
    assert!(exclude.lines().any(|line| line == "!/CLAUDE.md"));
    assert!(!exclude.lines().any(|line| line == "/CLAUDE.md"));
}

#[test]
fn per_repo_mode_unignores_owned_global_exclude_with_escaped_trailing_space() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    fs::write(repo.join("CLAUDE.md "), "user-owned replacement\n").unwrap();
    let mut config = fixture.config();
    config.adapters.claude.target = PathBuf::from("CLAUDE.md ");
    let config_path = fixture.root.path().join("config.toml");
    fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
    let data_dir = fixture.data.path();
    fs::write(
        data_dir.join("git-excludes"),
        "# claudemdeez managed begin\n/CLAUDE.md\\ \n# claudemdeez managed end\n",
    )
    .unwrap();
    let global_config = fixture.root.path().join("global-gitconfig");
    let configured = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .args([
            "config",
            "--global",
            "core.excludesFile",
            data_dir.join("git-excludes").to_str().unwrap(),
        ])
        .output()
        .expect("git config writes");
    assert!(
        configured.status.success(),
        "git config failed: {}",
        String::from_utf8_lossy(&configured.stderr)
    );

    let bin = env!("CARGO_BIN_EXE_claudemdeez");
    let conflict = Command::new(bin)
        .env("CLAUDEMDEEZ_DATA_DIR", data_dir)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .args(["--config", config_path.to_str().unwrap(), "apply"])
        .output()
        .expect("per-repo apply runs");
    assert_eq!(conflict.status.code(), Some(2));

    let exclude = fs::read_to_string(git_exclude_path(&repo)).unwrap();
    assert!(exclude.lines().any(|line| line == "!/CLAUDE.md\\ "));
    assert!(!exclude.lines().any(|line| line == "/CLAUDE.md\\ "));
    assert!(!git_check_ignore(&repo, "CLAUDE.md "));
    assert!(git_status(&repo, "CLAUDE.md ").starts_with("?? "));
}

#[test]
fn clean_removes_stale_exclude_when_managed_target_was_already_deleted() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("CLAUDE.md")).unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();

    let report = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: false,
            remove_if_source_missing: true,
        },
    )
    .unwrap();

    assert_eq!(report.summary.cleaned, 1);
    assert_eq!(report.summary.exclude_updates, 1);

    fs::write(repo.join("CLAUDE.md"), "new user claude\n").unwrap();
    assert_eq!(git_status(&repo, "CLAUDE.md"), "?? CLAUDE.md\n");
}

#[test]
fn clean_without_remove_flag_preserves_missing_managed_shim_ownership() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("CLAUDE.md")).unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();

    let preview = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: false,
            remove_if_source_missing: false,
        },
    )
    .unwrap();
    assert_eq!(preview.summary.no_source, 1);

    let cleaned = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: false,
            remove_if_source_missing: true,
        },
    )
    .unwrap();

    assert_eq!(cleaned.summary.cleaned, 1);
    assert_eq!(cleaned.summary.exclude_updates, 1);

    fs::write(repo.join("CLAUDE.md"), "new user claude\n").unwrap();
    assert_eq!(git_status(&repo, "CLAUDE.md"), "?? CLAUDE.md\n");
}

#[test]
fn watch_disabled_exits_before_reconciling() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical\n").unwrap();
    let config_path = fixture.root.path().join("watch-disabled.toml");
    fs::write(
        &config_path,
        format!(
            "[scan]\nroots = [\"{}\"]\n\n[watch]\nenabled = false\n",
            fixture.root.path().display()
        ),
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let watch = Command::new(bin)
        .env("CLAUDEMDEEZ_DATA_DIR", fixture.data.path())
        .args(["--config", config_path.to_str().unwrap(), "watch"])
        .output()
        .expect("watch runs");

    assert_eq!(watch.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&watch.stderr);
    assert!(stderr.contains("watch is disabled in config"));
    assert!(!repo.join("CLAUDE.md").exists());
}

#[test]
fn watch_reloads_config_and_updates_watched_roots() {
    let fixture = Fixture::new();
    let first_repo = fixture.repo("first");
    let second_repo = fixture.repo("second");
    fs::write(first_repo.join("AGENTS.md"), "first\n").unwrap();
    fs::write(second_repo.join("AGENTS.md"), "second\n").unwrap();
    let config_path = fixture.root.path().join("watch-config.toml");
    write_config_roots(&config_path, &[&first_repo]);
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let mut child = Command::new(bin)
        .env("CLAUDEMDEEZ_DATA_DIR", fixture.data.path())
        .current_dir(fixture.root.path())
        .args(["--config", "watch-config.toml", "watch"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("watch starts");

    assert!(
        wait_until(Duration::from_secs(5), || first_repo
            .join("CLAUDE.md")
            .exists()),
        "initial watched repo was not reconciled"
    );

    write_config_roots(&config_path, &[&second_repo]);
    assert!(
        wait_until(Duration::from_secs(5), || second_repo
            .join("CLAUDE.md")
            .exists()),
        "updated watched repo was not reconciled"
    );

    fs::remove_file(first_repo.join("CLAUDE.md")).unwrap();
    fs::write(first_repo.join("AGENTS.md"), "first changed\n").unwrap();
    thread::sleep(Duration::from_secs(1));
    assert!(!first_repo.join("CLAUDE.md").exists());

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn watch_honors_json_output_flag() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical\n").unwrap();
    let config_path = fixture.root.path().join("watch-json.toml");
    write_config_roots(&config_path, &[&repo]);
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let mut child = Command::new(bin)
        .env("CLAUDEMDEEZ_DATA_DIR", fixture.data.path())
        .args([
            "--dry-run",
            "--json",
            "--config",
            config_path.to_str().unwrap(),
            "watch",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("watch starts");

    let stdout = child.stdout.take().expect("stdout is piped");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut text = String::new();
        for _ in 0..4 {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    text.push_str(&line);
                    if text.contains("\"summary\"") {
                        break;
                    }
                }
            }
        }
        let _ = tx.send(text);
    });

    let output = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("watch should print an initial report");
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        output.trim_start().starts_with('{'),
        "watch output was not JSON: {output}"
    );
    assert!(output.contains("\"summary\""));
    assert!(!output.contains("Scanned "));
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_requires_configured_roots() {
    let fixture = Fixture::new();
    let config_path = fixture.root.path().join("service-empty.toml");
    fs::write(&config_path, "[scan]\nroots = []\n").unwrap();
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service install runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("service install requires configured scan roots"));
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_rejects_relative_scan_paths() {
    let fixture = Fixture::new();
    let config_path = fixture.root.path().join("service-relative.toml");
    fs::write(&config_path, "[scan]\nroots = [\".\"]\n").unwrap();
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .current_dir(fixture.root.path())
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service install runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("service install requires absolute scan paths"));
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_rejects_option_like_unit_name() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let config_path = fixture.root.path().join("service.toml");
    write_config_roots(&config_path, &[&repo]);
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name=-foo",
        ])
        .output()
        .expect("service install dry-run runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("service unit name must not start with '-'"));
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_rejects_empty_service_basename() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let config_path = fixture.root.path().join("service.toml");
    write_config_roots(&config_path, &[&repo]);
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            ".service",
        ])
        .output()
        .expect("service install dry-run runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("service unit name must include a name before `.service`"));
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_treats_empty_xdg_config_home_as_unset() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let config_path = fixture.root.path().join("service.toml");
    write_config_roots(&config_path, &[&repo]);
    let home = fixture.root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let path = fake_systemctl_path_failing_show_env(&fixture);
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", "")
        .env("HOME", &home)
        .env("PATH", path)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service install dry-run runs");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            home.join(".config/systemd/user/claudemdeez-test.service")
                .to_str()
                .unwrap()
        )
    );
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_rejects_control_characters_in_unit_values() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let config_path = fixture.root.path().join("service\nbad.toml");
    write_config_roots(&config_path, &[&repo]);
    let xdg_config_home = fixture.root.path().join("xdg-config");
    let unit_path = xdg_config_home
        .join("systemd/user")
        .join("claudemdeez-test.service");
    let path = fake_systemctl_path_with_xdg(&fixture, &xdg_config_home);
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("PATH", path)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service install dry-run runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("service config path must not contain control characters"));
    assert!(!unit_path.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_uses_manager_unit_path_with_escaped_spaces() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical\n").unwrap();
    let config_path = fixture.root.path().join("service.toml");
    write_config_roots(&config_path, &[&repo]);
    let xdg_config_home = fixture.root.path().join("xdg config");
    let unit_path = xdg_config_home
        .join("systemd/user")
        .join("claudemdeez-test.service");
    let unit_path_token = systemd_show_path_token(&xdg_config_home);
    let path = fake_systemctl_path(
        &fixture,
        &format!(
            "#!/bin/sh\nif [ \"$2\" = \"show\" ] && [ \"$3\" = \"--property=UnitPath\" ]; then printf '%s\\n' \"{unit_path_token}/systemd/user.control /run/user/1000/systemd/user.control {unit_path_token}/systemd/user /etc/systemd/user\"; exit 0; fi\nif [ \"$2\" = \"show\" ]; then printf '%s\\n\\n' not-found; exit 0; fi\nexit 0\n"
        ),
    );
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("PATH", path)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service install dry-run runs");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(unit_path.to_str().unwrap()));
    assert!(!stdout.contains("\\x20"));
    assert!(!unit_path.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_rejects_systemd_unsafe_binary_path() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let config_path = fixture.root.path().join("service.toml");
    write_config_roots(&config_path, &[&repo]);
    let xdg_config_home = fixture.root.path().join("xdg-config");
    let unit_path = xdg_config_home
        .join("systemd/user")
        .join("claudemdeez-test.service");
    let bin_path = fixture.root.path().join("bin/claude'mdeez");
    let path = fake_systemctl_path_with_xdg(&fixture, &xdg_config_home);
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("PATH", path)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
            "--bin",
            bin_path.to_str().unwrap(),
        ])
        .output()
        .expect("service install dry-run runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("service binary path contains characters systemd cannot use"));
    assert!(!unit_path.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_rejects_missing_binary_path() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let config_path = fixture.root.path().join("service.toml");
    write_config_roots(&config_path, &[&repo]);
    let xdg_config_home = fixture.root.path().join("xdg-config");
    let unit_path = xdg_config_home
        .join("systemd/user")
        .join("claudemdeez-test.service");
    let bin_path = fixture.root.path().join("bin/missing-claudemdeez");
    let path = fake_systemctl_path_with_xdg(&fixture, &xdg_config_home);
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("PATH", path)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
            "--bin",
            bin_path.to_str().unwrap(),
        ])
        .output()
        .expect("service install dry-run runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("failed to inspect service binary"));
    assert!(!unit_path.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_rejects_non_executable_binary_path() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let config_path = fixture.root.path().join("service.toml");
    write_config_roots(&config_path, &[&repo]);
    let xdg_config_home = fixture.root.path().join("xdg-config");
    let unit_path = xdg_config_home
        .join("systemd/user")
        .join("claudemdeez-test.service");
    let bin_path = fixture.root.path().join("bin/claudemdeez");
    fs::create_dir_all(bin_path.parent().unwrap()).unwrap();
    fs::write(&bin_path, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&bin_path, fs::Permissions::from_mode(0o644)).unwrap();
    let path = fake_systemctl_path_with_xdg(&fixture, &xdg_config_home);
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("PATH", path)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
            "--bin",
            bin_path.to_str().unwrap(),
        ])
        .output()
        .expect("service install dry-run runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("service binary path is not executable"));
    assert!(!unit_path.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_dry_run_refuses_unit_lookup_conflict() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical\n").unwrap();
    let config_path = fixture.root.path().join("service.toml");
    write_config_roots(&config_path, &[&repo]);
    let xdg_config_home = fixture.root.path().join("xdg-config");
    let unit_path = xdg_config_home
        .join("systemd/user")
        .join("claudemdeez-test.service");
    let unit_path_token = systemd_show_path_token(&xdg_config_home);
    let existing_unit = fixture
        .root
        .path()
        .join("usr/lib/systemd/user/claudemdeez-test.service");
    fs::create_dir_all(existing_unit.parent().unwrap()).unwrap();
    fs::write(&existing_unit, "[Service]\nExecStart=/bin/true\n").unwrap();
    let path = fake_systemctl_path(
        &fixture,
        &format!(
            "#!/bin/sh\nif [ \"$2\" = \"show\" ] && [ \"$3\" = \"--property=UnitPath\" ]; then printf '%s\\n' \"{unit_path_token}/systemd/user.control {unit_path_token}/systemd/user {}\"; exit 0; fi\nif [ \"$2\" = \"show\" ] && [ \"$3\" = \"claudemdeez-test.service\" ]; then printf '%s\\n%s\\n' loaded \"{}\"; exit 0; fi\nexit 0\n",
            existing_unit.parent().unwrap().display(),
            existing_unit.display()
        ),
    );
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("PATH", path)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service install dry-run runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("refusing to shadow existing systemd user unit"));
    assert!(stderr.contains(existing_unit.to_str().unwrap()));
    assert!(!unit_path.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_rejects_disabled_watch_config() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let config_path = fixture.root.path().join("service-watch-disabled.toml");
    fs::write(
        &config_path,
        format!(
            "[scan]\nroots = [\"{}\"]\n\n[watch]\nenabled = false\n",
            repo.display()
        ),
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service install runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("service install requires watch to be enabled"));
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_dry_run_validates_git_exclude_mode() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    let config_path = fixture.root.path().join("service-global-exclude.toml");
    fs::write(
        &config_path,
        format!(
            "[scan]\nroots = [\"{}\"]\n\n[git]\nexclude_mode = \"global\"\n",
            repo.display()
        ),
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service install dry-run runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("service install requires valid Git exclude mode"));
    assert!(stderr.contains("global exclude mode is disabled"));
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_dry_run_does_not_write_unit() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical\n").unwrap();
    let config_path = fixture.root.path().join("service.toml");
    write_config_roots(&config_path, &[&repo]);
    let xdg_config_home = fixture.root.path().join("xdg-config");
    let unit_path = xdg_config_home
        .join("systemd/user")
        .join("claudemdeez-test.service");
    let bin_path = fixture.root.path().join("bin/claudemdeez");
    let data_dir = fixture.root.path().join("data");
    fs::create_dir_all(bin_path.parent().unwrap()).unwrap();
    fs::write(&bin_path, "#!/bin/sh\n").unwrap();
    make_executable(&bin_path);
    let path = fake_systemctl_path_with_xdg(&fixture, &xdg_config_home);
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("PATH", path)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
            "--bin",
            bin_path.to_str().unwrap(),
            "--data-dir",
            data_dir.to_str().unwrap(),
        ])
        .output()
        .expect("service install dry-run runs");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("would write systemd user unit"));
    assert!(stdout.contains("claudemdeez-test.service"));
    assert!(!unit_path.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_now_restarts_existing_unit() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical\n").unwrap();
    let config_path = fixture.root.path().join("service.toml");
    write_config_roots(&config_path, &[&repo]);
    let xdg_config_home = fixture.root.path().join("xdg-config");
    let unit_dir = xdg_config_home.join("systemd/user");
    fs::create_dir_all(&unit_dir).unwrap();
    let unit_path = unit_dir.join("claudemdeez-test.service");
    fs::write(
        &unit_path,
        "# claudemdeez managed systemd user unit\n[Service]\nExecStart=/bin/true\n",
    )
    .unwrap();
    let log_path = fixture.root.path().join("systemctl.log");
    let path = fake_systemctl_path(
        &fixture,
        &format!(
            "#!/bin/sh\necho \"$@\" >> {}\nif [ \"$2\" = \"show-environment\" ]; then echo XDG_CONFIG_HOME={}; exit 0; fi\nexit 0\n",
            log_path.display(),
            xdg_config_home.display()
        ),
    );
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("PATH", path)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
            "--now",
        ])
        .output()
        .expect("service install runs");

    assert_eq!(output.status.code(), Some(0));
    let log = fs::read_to_string(log_path).unwrap();
    assert!(log.contains("--user restart claudemdeez-test.service"));
    assert!(!log.contains("--user start claudemdeez-test.service"));
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_dry_run_rejects_unwritable_unit_dir() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical\n").unwrap();
    let config_path = fixture.root.path().join("service.toml");
    write_config_roots(&config_path, &[&repo]);
    let xdg_config_home = fixture.root.path().join("xdg-config");
    let unit_dir = xdg_config_home.join("systemd/user");
    fs::create_dir_all(&unit_dir).unwrap();
    let path = fake_systemctl_path_with_xdg(&fixture, &xdg_config_home);
    let original_permissions = fs::metadata(&unit_dir).unwrap().permissions();
    fs::set_permissions(&unit_dir, fs::Permissions::from_mode(0o555)).unwrap();
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("PATH", path)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service install dry-run runs");

    fs::set_permissions(&unit_dir, original_permissions).unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("systemd user unit parent"));
    assert!(stderr.contains("is not writable"));
    assert!(!unit_dir.join("claudemdeez-test.service").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_dry_run_rejects_readonly_managed_unit() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical\n").unwrap();
    let config_path = fixture.root.path().join("service.toml");
    write_config_roots(&config_path, &[&repo]);
    let xdg_config_home = fixture.root.path().join("xdg-config");
    let unit_dir = xdg_config_home.join("systemd/user");
    fs::create_dir_all(&unit_dir).unwrap();
    let unit_path = unit_dir.join("claudemdeez-test.service");
    let path = fake_systemctl_path_with_xdg(&fixture, &xdg_config_home);
    fs::write(
        &unit_path,
        "# claudemdeez managed systemd user unit\n[Service]\nExecStart=/bin/true\n",
    )
    .unwrap();
    let original_permissions = fs::metadata(&unit_path).unwrap().permissions();
    fs::set_permissions(&unit_path, fs::Permissions::from_mode(0o444)).unwrap();
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("PATH", path)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service install dry-run runs");

    fs::set_permissions(&unit_path, original_permissions).unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("systemd user unit"));
    assert!(stderr.contains("is not writable"));
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_dry_run_refuses_unmanaged_unit_conflict() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical\n").unwrap();
    let config_path = fixture.root.path().join("service.toml");
    write_config_roots(&config_path, &[&repo]);
    let xdg_config_home = fixture.root.path().join("xdg-config");
    let unit_dir = xdg_config_home.join("systemd/user");
    fs::create_dir_all(&unit_dir).unwrap();
    let unit_path = unit_dir.join("claudemdeez-test.service");
    fs::write(&unit_path, "[Service]\nExecStart=/bin/true\n").unwrap();
    let path = fake_systemctl_path_with_xdg(&fixture, &xdg_config_home);
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("PATH", path)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service install dry-run runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("refusing to use unmanaged systemd user unit"));
    assert_eq!(
        fs::read_to_string(&unit_path).unwrap(),
        "[Service]\nExecStart=/bin/true\n"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn service_start_dry_run_requires_managed_unit() {
    let fixture = Fixture::new();
    let xdg_config_home = fixture.root.path().join("xdg-config");
    let path = fake_systemctl_path_with_xdg(&fixture, &xdg_config_home);
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("PATH", path)
        .args([
            "--dry-run",
            "service",
            "start",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service start dry-run runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("managed systemd user unit is not installed"));
}

#[cfg(target_os = "linux")]
#[test]
fn service_start_dry_run_refuses_unmanaged_unit_conflict() {
    let fixture = Fixture::new();
    let xdg_config_home = fixture.root.path().join("xdg-config");
    let unit_dir = xdg_config_home.join("systemd/user");
    fs::create_dir_all(&unit_dir).unwrap();
    let unit_path = unit_dir.join("claudemdeez-test.service");
    fs::write(&unit_path, "[Service]\nExecStart=/bin/true\n").unwrap();
    let path = fake_systemctl_path_with_xdg(&fixture, &xdg_config_home);
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("PATH", path)
        .args([
            "--dry-run",
            "service",
            "start",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service start dry-run runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("refusing to use unmanaged systemd user unit"));
    assert_eq!(
        fs::read_to_string(&unit_path).unwrap(),
        "[Service]\nExecStart=/bin/true\n"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn service_start_dry_run_refuses_unit_lookup_conflict() {
    let fixture = Fixture::new();
    let xdg_config_home = fixture.root.path().join("xdg-config");
    let unit_dir = xdg_config_home.join("systemd/user");
    fs::create_dir_all(&unit_dir).unwrap();
    let unit_path = unit_dir.join("claudemdeez-test.service");
    fs::write(
        &unit_path,
        "# claudemdeez managed systemd user unit\n[Service]\nExecStart=/bin/true\n",
    )
    .unwrap();
    let unit_path_token = systemd_show_path_token(&xdg_config_home);
    let transient_unit = fixture
        .root
        .path()
        .join("run/user/1000/systemd/transient/claudemdeez-test.service");
    let path = fake_systemctl_path(
        &fixture,
        &format!(
            "#!/bin/sh\nif [ \"$2\" = \"show\" ] && [ \"$3\" = \"--property=UnitPath\" ]; then printf '%s\\n' \"{unit_path_token}/systemd/user.control {unit_path_token}/systemd/user {}\"; exit 0; fi\nif [ \"$2\" = \"show\" ] && [ \"$3\" = \"claudemdeez-test.service\" ]; then printf '%s\\n%s\\n' loaded \"{}\"; exit 0; fi\nexit 0\n",
            transient_unit.parent().unwrap().display(),
            transient_unit.display()
        ),
    );
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("PATH", path)
        .args([
            "--dry-run",
            "service",
            "start",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service start dry-run runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("refusing to control systemd user unit"));
    assert!(stderr.contains(transient_unit.to_str().unwrap()));
}

#[cfg(target_os = "linux")]
#[test]
fn service_uninstall_dry_run_refuses_unmanaged_unit_conflict() {
    let fixture = Fixture::new();
    let xdg_config_home = fixture.root.path().join("xdg-config");
    let unit_dir = xdg_config_home.join("systemd/user");
    fs::create_dir_all(&unit_dir).unwrap();
    let unit_path = unit_dir.join("claudemdeez-test.service");
    fs::write(&unit_path, "[Service]\nExecStart=/bin/true\n").unwrap();
    let path = fake_systemctl_path_with_xdg(&fixture, &xdg_config_home);
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("PATH", path)
        .args([
            "--dry-run",
            "service",
            "uninstall",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service uninstall dry-run runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("refusing to use unmanaged systemd user unit"));
    assert_eq!(
        fs::read_to_string(&unit_path).unwrap(),
        "[Service]\nExecStart=/bin/true\n"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn service_uninstall_dry_run_refuses_unit_lookup_conflict() {
    let fixture = Fixture::new();
    let xdg_config_home = fixture.root.path().join("xdg-config");
    let unit_dir = xdg_config_home.join("systemd/user");
    fs::create_dir_all(&unit_dir).unwrap();
    let unit_path = unit_dir.join("claudemdeez-test.service");
    fs::write(
        &unit_path,
        "# claudemdeez managed systemd user unit\n[Service]\nExecStart=/bin/true\n",
    )
    .unwrap();
    let unit_path_token = systemd_show_path_token(&xdg_config_home);
    let transient_unit = fixture
        .root
        .path()
        .join("run/user/1000/systemd/transient/claudemdeez-test.service");
    let path = fake_systemctl_path(
        &fixture,
        &format!(
            "#!/bin/sh\nif [ \"$2\" = \"show\" ] && [ \"$3\" = \"--property=UnitPath\" ]; then printf '%s\\n' \"{unit_path_token}/systemd/user.control {unit_path_token}/systemd/user {}\"; exit 0; fi\nif [ \"$2\" = \"show\" ] && [ \"$3\" = \"claudemdeez-test.service\" ]; then printf '%s\\n%s\\n' loaded \"{}\"; exit 0; fi\nexit 0\n",
            transient_unit.parent().unwrap().display(),
            transient_unit.display()
        ),
    );
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("PATH", path)
        .args([
            "--dry-run",
            "service",
            "uninstall",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service uninstall dry-run runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("refusing to control systemd user unit"));
    assert!(stderr.contains(transient_unit.to_str().unwrap()));
}

#[cfg(target_os = "linux")]
#[test]
fn service_uninstall_surfaces_disable_failure() {
    let fixture = Fixture::new();
    let xdg_config_home = fixture.root.path().join("xdg-config");
    let unit_dir = xdg_config_home.join("systemd/user");
    fs::create_dir_all(&unit_dir).unwrap();
    let unit_path = unit_dir.join("claudemdeez-test.service");
    fs::write(
        &unit_path,
        "# claudemdeez managed systemd user unit\n[Service]\nExecStart=/bin/true\n",
    )
    .unwrap();
    let fake_bin_dir = fixture.root.path().join("bin");
    fs::create_dir_all(&fake_bin_dir).unwrap();
    let fake_systemctl = fake_bin_dir.join("systemctl");
    fs::write(
        &fake_systemctl,
        format!(
            "#!/bin/sh\nif [ \"$2\" = \"show\" ] && [ \"$3\" = \"--property=UnitPath\" ]; then printf '%s\\n' \"{}/systemd/user.control {}/systemd/user\"; exit 0; fi\nif [ \"$2\" = \"show\" ] && [ \"$3\" = \"claudemdeez-test.service\" ]; then printf '%s\\n%s\\n' loaded \"{}\"; exit 0; fi\nif [ \"$2\" = \"show-environment\" ]; then echo XDG_CONFIG_HOME={}; exit 0; fi\nif [ \"$2\" = \"disable\" ]; then echo disable failed >&2; exit 42; fi\nexit 0\n",
            xdg_config_home.display(),
            xdg_config_home.display(),
            unit_path.display(),
            xdg_config_home.display()
        ),
    )
    .unwrap();
    make_executable(&fake_systemctl);
    let path = format!(
        "{}:{}",
        fake_bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("PATH", path)
        .args(["service", "uninstall", "--unit-name", "claudemdeez-test"])
        .output()
        .expect("service uninstall runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("systemctl --user disable --now claudemdeez-test.service failed"));
    assert!(stderr.contains("disable failed"));
    assert!(unit_path.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn service_install_dry_run_validates_adapter_config() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical\n").unwrap();
    let config_path = fixture.root.path().join("service-invalid-adapter.toml");
    fs::write(
        &config_path,
        format!(
            "[scan]\nroots = [\"{}\"]\n\n[adapters.claude]\ntarget = \"../CLAUDE.md\"\n",
            repo.display()
        ),
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let output = Command::new(bin)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service install dry-run runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("service install requires valid adapters"));
    assert!(stderr.contains("must stay inside the repository root"));
}

#[cfg(target_os = "linux")]
#[test]
fn service_control_commands_do_not_require_valid_config() {
    let fixture = Fixture::new();
    let config_path = fixture.root.path().join("bad-service-config.toml");
    fs::write(&config_path, "not valid toml =").unwrap();
    let bin = env!("CARGO_BIN_EXE_claudemdeez");

    let uninstall = Command::new(bin)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "uninstall",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service uninstall dry-run runs");

    assert_eq!(uninstall.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&uninstall.stdout);
    assert!(stdout.contains("systemd user unit is not installed"));

    let install = Command::new(bin)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "service",
            "install",
            "--unit-name",
            "claudemdeez-test",
        ])
        .output()
        .expect("service install dry-run runs");

    assert_eq!(install.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&install.stderr);
    assert!(stderr.contains("failed to parse config"));
}

#[test]
fn clean_invalid_exclude_leaves_managed_target_in_place() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();
    let exclude_path = git_exclude_path(&repo);
    let invalid_exclude =
        b"# claudemdeez managed begin\n/CLAUDE.md\n# claudemdeez managed end\n\xff".to_vec();
    fs::write(&exclude_path, &invalid_exclude).unwrap();

    let report = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: false,
            remove_if_source_missing: true,
        },
    )
    .unwrap();

    assert_eq!(report.summary.errors, 1);
    assert!(fs::symlink_metadata(repo.join("CLAUDE.md")).is_ok());
    assert_eq!(fs::read(&exclude_path).unwrap(), invalid_exclude);
}

#[cfg(unix)]
#[test]
fn symlinked_git_exclude_file_is_rejected_before_writing() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let exclude_path = git_exclude_path(&repo);
    fs::remove_file(&exclude_path).unwrap();
    let victim = fixture.root.path().join("victim-exclude");
    fs::write(&victim, "KEEP\n").unwrap();
    std::os::unix::fs::symlink(&victim, &exclude_path).unwrap();

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.errors, 1);
    assert_eq!(report.summary.created, 0);
    assert_eq!(fs::read_to_string(&victim).unwrap(), "KEEP\n");
    assert!(!repo.join("CLAUDE.md").exists());
}

#[cfg(unix)]
#[test]
fn symlinked_git_exclude_file_is_rejected_before_removing() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("CLAUDE.md")).unwrap();
    fs::write(repo.join("CLAUDE.md"), "user-owned replacement\n").unwrap();
    let exclude_path = git_exclude_path(&repo);
    fs::remove_file(&exclude_path).unwrap();
    let victim = fixture.root.path().join("victim-exclude");
    let victim_text = "# claudemdeez managed begin\n/CLAUDE.md\n# claudemdeez managed end\n";
    fs::write(&victim, victim_text).unwrap();
    std::os::unix::fs::symlink(&victim, &exclude_path).unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.errors, 1);
    assert_eq!(report.summary.conflicts, 0);
    assert_eq!(fs::read_to_string(&victim).unwrap(), victim_text);
    assert_eq!(
        fs::read_to_string(repo.join("CLAUDE.md")).unwrap(),
        "user-owned replacement\n"
    );
}

#[cfg(unix)]
#[test]
fn symlinked_git_exclude_parent_is_rejected_before_writing() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let exclude_path = git_exclude_path(&repo);
    let info_dir = exclude_path.parent().unwrap();
    fs::remove_dir_all(info_dir).unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("exclude"), "KEEP\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), info_dir).unwrap();

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.errors, 1);
    assert_eq!(report.summary.created, 0);
    assert_eq!(
        fs::read_to_string(outside.path().join("exclude")).unwrap(),
        "KEEP\n"
    );
    assert!(!repo.join("CLAUDE.md").exists());
}

#[test]
fn remove_if_managed_invalid_exclude_leaves_managed_target_in_place() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let mut config = fixture.config();
    config.adapters.claude.on_source_missing = SourceMissingBehavior::RemoveIfManaged;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();
    let exclude_path = git_exclude_path(&repo);
    let invalid_exclude =
        b"# claudemdeez managed begin\n/CLAUDE.md\n# claudemdeez managed end\n\xff".to_vec();
    fs::write(&exclude_path, &invalid_exclude).unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.errors, 1);
    assert!(fs::symlink_metadata(repo.join("CLAUDE.md")).is_ok());
    assert_eq!(fs::read(&exclude_path).unwrap(), invalid_exclude);
}

#[cfg(unix)]
#[test]
fn remove_if_managed_unwritable_exclude_leaves_managed_target_in_place() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let mut config = fixture.config();
    config.adapters.claude.on_source_missing = SourceMissingBehavior::RemoveIfManaged;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();
    let exclude_path = git_exclude_path(&repo);
    let original_permissions = fs::metadata(&exclude_path).unwrap().permissions();
    fs::set_permissions(&exclude_path, fs::Permissions::from_mode(0o444)).unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    );

    fs::set_permissions(&exclude_path, original_permissions).unwrap();
    let report = report.unwrap();
    assert_eq!(report.summary.errors, 1);
    assert_eq!(report.summary.cleaned, 0);
    assert!(fs::symlink_metadata(repo.join("CLAUDE.md")).is_ok());
    let exclude_text = fs::read_to_string(&exclude_path).unwrap();
    assert!(exclude_text.lines().any(|line| line == "/CLAUDE.md"));
}

#[cfg(unix)]
#[test]
fn clean_unwritable_exclude_leaves_managed_target_in_place() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();
    let exclude_path = git_exclude_path(&repo);
    let original_permissions = fs::metadata(&exclude_path).unwrap().permissions();
    fs::set_permissions(&exclude_path, fs::Permissions::from_mode(0o444)).unwrap();

    let report = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: false,
            remove_if_source_missing: true,
        },
    );

    fs::set_permissions(&exclude_path, original_permissions).unwrap();
    let report = report.unwrap();
    assert_eq!(report.summary.errors, 1);
    assert_eq!(report.summary.cleaned, 0);
    assert!(fs::symlink_metadata(repo.join("CLAUDE.md")).is_ok());
    let exclude_text = fs::read_to_string(&exclude_path).unwrap();
    assert!(exclude_text.lines().any(|line| line == "/CLAUDE.md"));
}

#[cfg(unix)]
#[test]
fn remove_if_managed_target_removal_failure_preserves_exclude() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let mut config = fixture.config();
    config.adapters.claude.on_source_missing = SourceMissingBehavior::RemoveIfManaged;
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();
    let original_permissions = fs::metadata(&repo).unwrap().permissions();
    fs::set_permissions(&repo, fs::Permissions::from_mode(0o555)).unwrap();

    let report = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    );

    fs::set_permissions(&repo, original_permissions).unwrap();
    let report = report.unwrap();
    assert_eq!(report.summary.errors, 1);
    assert!(fs::symlink_metadata(repo.join("CLAUDE.md")).is_ok());
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap();
    assert!(exclude_text.lines().any(|line| line == "/CLAUDE.md"));
}

#[cfg(unix)]
#[test]
fn clean_target_removal_failure_preserves_exclude() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    fs::remove_file(repo.join("AGENTS.md")).unwrap();
    let original_permissions = fs::metadata(&repo).unwrap().permissions();
    fs::set_permissions(&repo, fs::Permissions::from_mode(0o555)).unwrap();

    let report = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: false,
            remove_if_source_missing: true,
        },
    );

    fs::set_permissions(&repo, original_permissions).unwrap();
    let report = report.unwrap();
    assert_eq!(report.summary.errors, 1);
    assert!(fs::symlink_metadata(repo.join("CLAUDE.md")).is_ok());
    let exclude_text = fs::read_to_string(git_exclude_path(&repo)).unwrap();
    assert!(exclude_text.lines().any(|line| line == "/CLAUDE.md"));
}

#[test]
fn clean_does_not_remove_tracked_managed_target() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config = fixture.config();
    let state = fixture.state();

    reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();
    git(&repo, &["add", "-f", "CLAUDE.md"]);
    fs::remove_file(repo.join("AGENTS.md")).unwrap();

    let report = cleaner::clean(
        &config,
        false,
        &[],
        &state,
        CleanOptions {
            dry_run: false,
            remove_if_source_missing: true,
        },
    )
    .unwrap();

    assert_eq!(report.summary.tracked_conflicts, 1);
    assert!(fs::symlink_metadata(repo.join("CLAUDE.md")).is_ok());
    assert_eq!(git_status(&repo, "CLAUDE.md"), "A  CLAUDE.md\n");
}

#[test]
fn linked_worktree_uses_worktree_exclude_file() {
    let fixture = Fixture::new();
    let main_repo = fixture.repo("main");
    fs::write(main_repo.join("README.md"), "main\n").unwrap();
    git(&main_repo, &["add", "README.md"]);
    git(&main_repo, &["commit", "-m", "initial"]);

    let worktree = fixture.root.path().join("linked");
    git(&main_repo, &["worktree", "add", worktree.to_str().unwrap()]);
    fs::write(worktree.join("AGENTS.md"), "worktree instructions\n").unwrap();

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(report.summary.created, 1);
    assert!(worktree.join("CLAUDE.md").exists());

    let exclude = git_stdout(&worktree, &["rev-parse", "--git-path", "info/exclude"]);
    let exclude_path = path_from_git_output(&worktree, &exclude);
    assert!(
        fs::read_to_string(exclude_path)
            .unwrap()
            .contains("/CLAUDE.md")
    );
    assert!(git_status(&worktree, "CLAUDE.md").is_empty());
}

#[cfg(unix)]
#[test]
fn unreadable_directories_are_skipped_during_discovery() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let blocked = fixture.root.path().join("blocked");
    fs::create_dir(&blocked).unwrap();
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();

    let report = reconciler::apply(
        &fixture.config(),
        false,
        &[],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    );

    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
    let report = report.unwrap();
    assert_eq!(report.summary.created, 1);
    assert!(repo.join("CLAUDE.md").exists());
}

#[test]
fn cli_roots_cannot_escape_configured_scope() {
    let fixture = Fixture::new();
    let allowed = fixture.repo("allowed");
    fs::write(allowed.join("AGENTS.md"), "allowed\n").unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_repo = outside.path().join("outside");
    fs::create_dir_all(&outside_repo).unwrap();
    git_init(&outside_repo);
    fs::write(outside_repo.join("AGENTS.md"), "outside\n").unwrap();

    let config = fixture.config();
    let error = reconciler::apply(
        &config,
        true,
        &[outside.path().to_path_buf()],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap_err();

    assert!(error.to_string().contains("outside configured scan scope"));
    assert!(!outside_repo.join("CLAUDE.md").exists());
}

#[test]
fn cli_roots_require_configured_scope_when_config_exists() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical\n").unwrap();
    let mut config = fixture.config();
    config.scan.roots = vec![];

    let error = reconciler::apply(
        &config,
        true,
        &[fixture.root.path().to_path_buf()],
        &fixture.state(),
        ReconcileOptions { dry_run: false },
    )
    .unwrap_err();

    assert!(error.to_string().contains("no scan roots are configured"));
    assert!(!repo.join("CLAUDE.md").exists());
}

fn git_init(repo: &Path) {
    let output = Command::new("git")
        .arg("init")
        .arg("-b")
        .arg("main")
        .arg(repo)
        .output()
        .expect("git init runs");
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    git(repo, &["config", "user.email", "test@example.com"]);
    git(repo, &["config", "user.name", "Test User"]);
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout:\n{}\nstderr:\n{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout:\n{}\nstderr:\n{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn git_status(repo: &Path, path: &str) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--short", "--"])
        .arg(path)
        .output()
        .expect("git status runs");
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn git_check_ignore(repo: &Path, path: &str) -> bool {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "--quiet", "--"])
        .arg(path)
        .output()
        .expect("git check-ignore runs");
    output.status.success()
}

fn git_exclude_path(repo: &Path) -> PathBuf {
    let exclude = git_stdout(repo, &["rev-parse", "--git-path", "info/exclude"]);
    path_from_git_output(repo, &exclude)
}

fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> Option<std::process::Output> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if child.try_wait().ok().flatten().is_some() {
            return child.wait_with_output().ok();
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

fn write_config_roots(path: &Path, roots: &[&Path]) {
    let roots = roots
        .iter()
        .map(|root| format!("\"{}\"", root.display()))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(path, format!("[scan]\nroots = [{roots}]\n")).unwrap();
}

#[cfg(target_os = "linux")]
fn fake_systemctl_path_with_xdg(fixture: &Fixture, xdg_config_home: &Path) -> String {
    let unit_path_token = systemd_show_path_token(xdg_config_home);
    fake_systemctl_path(
        fixture,
        &format!(
            "#!/bin/sh\nif [ \"$2\" = \"show\" ] && [ \"$3\" = \"--property=UnitPath\" ]; then printf '%s\\n' \"{unit_path_token}/systemd/user.control /run/user/1000/systemd/user.control {unit_path_token}/systemd/user /etc/systemd/user\"; exit 0; fi\nif [ \"$2\" = \"show\" ]; then printf '%s\\n\\n' not-found; exit 0; fi\nif [ \"$2\" = \"show-environment\" ]; then echo XDG_CONFIG_HOME={}; exit 0; fi\nexit 0\n",
            xdg_config_home.display()
        ),
    )
}

#[cfg(target_os = "linux")]
fn fake_systemctl_path_failing_show_env(fixture: &Fixture) -> String {
    fake_systemctl_path(
        fixture,
        "#!/bin/sh\nif [ \"$2\" = \"show\" ]; then exit 1; fi\nif [ \"$2\" = \"show-environment\" ]; then exit 1; fi\nexit 0\n",
    )
}

#[cfg(target_os = "linux")]
fn systemd_show_path_token(path: &Path) -> String {
    path.display().to_string().replace(' ', "\\x20")
}

#[cfg(target_os = "linux")]
fn fake_systemctl_path(fixture: &Fixture, script: &str) -> String {
    let fake_bin_dir = fixture.root.path().join("fake-systemctl-bin");
    fs::create_dir_all(&fake_bin_dir).unwrap();
    let fake_systemctl = fake_bin_dir.join("systemctl");
    fs::write(&fake_systemctl, script).unwrap();
    make_executable(&fake_systemctl);
    format!(
        "{}:{}",
        fake_bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn deep_relative_path(component: &str, depth: usize, file_name: &str) -> PathBuf {
    let mut path = PathBuf::new();
    for _ in 0..depth {
        path.push(component);
    }
    path.push(file_name);
    path
}

fn path_from_git_output(repo: &Path, text: &str) -> PathBuf {
    let path = PathBuf::from(text);
    if path.is_absolute() {
        path
    } else {
        repo.join(path)
    }
}
