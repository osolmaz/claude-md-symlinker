use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use claudectomy::{
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
    assert!(text.contains("claudectomy managed"));
    assert!(text.ends_with("v2\n"));
}

#[test]
fn hardlink_materialization_recovers_after_source_replacement() {
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

    assert_eq!(report.summary.repaired, 1);
    assert_eq!(fs::read_to_string(repo.join("CLAUDE.md")).unwrap(), "v2\n");
    assert!(same_file::is_same_file(repo.join("AGENTS.md"), repo.join("CLAUDE.md")).unwrap());
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
fn remove_if_managed_cleans_stale_hardlink_after_source_deletion() {
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

    assert_eq!(report.summary.cleaned, 1);
    assert_eq!(report.summary.exclude_updates, 1);
    assert!(!repo.join("CLAUDE.md").exists());
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
fn clean_removes_stale_hardlink_after_source_deletion() {
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

    assert_eq!(report.summary.cleaned, 1);
    assert_eq!(report.summary.exclude_updates, 1);
    assert!(!repo.join("CLAUDE.md").exists());
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
fn apply_no_source_does_not_forget_stale_hardlink_ownership() {
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

    assert_eq!(report.summary.cleaned, 1);
    assert_eq!(report.summary.exclude_updates, 1);
    assert!(!repo.join("CLAUDE.md").exists());
}

#[test]
fn clean_preview_does_not_forget_stale_hardlink_ownership() {
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

    assert_eq!(cleaned.summary.cleaned, 1);
    assert!(!repo.join("CLAUDE.md").exists());
}

#[test]
fn clean_does_not_forget_stale_hardlink_after_source_replacement() {
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
    assert_eq!(preview.summary.kept, 1);

    let repaired = reconciler::apply(
        &config,
        false,
        &[],
        &state,
        ReconcileOptions { dry_run: false },
    )
    .unwrap();

    assert_eq!(repaired.summary.repaired, 1);
    assert_eq!(fs::read_to_string(repo.join("CLAUDE.md")).unwrap(), "v2\n");
    assert!(same_file::is_same_file(repo.join("AGENTS.md"), repo.join("CLAUDE.md")).unwrap());
}

#[test]
fn cli_dry_run_clean_reads_existing_state_for_hardlinks() {
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
    let bin = env!("CARGO_BIN_EXE_claudectomy");

    let apply = Command::new(bin)
        .env("CLAUDECTOMY_DATA_DIR", fixture.data.path())
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
        .env("CLAUDECTOMY_DATA_DIR", fixture.data.path())
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
    assert_eq!(dry_run_json["summary"]["cleaned"], 1);
    assert!(repo.join("CLAUDE.md").exists());

    let clean = Command::new(bin)
        .env("CLAUDECTOMY_DATA_DIR", fixture.data.path())
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
    assert!(!repo.join("CLAUDE.md").exists());
}

#[test]
fn cli_dry_run_init_does_not_write_config() {
    let fixture = Fixture::new();
    let config_path = fixture.root.path().join("dry-run-config.toml");
    let bin = env!("CARGO_BIN_EXE_claudectomy");

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
fn global_mode_configures_preseeded_global_exclude_file() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config_path = fixture.root.path().join("config.toml");
    let data_dir = fixture.data.path();
    fs::write(
        data_dir.join("git-excludes"),
        "# claudectomy managed begin\n/CLAUDE.md\n# claudectomy managed end\n",
    )
    .unwrap();
    fs::write(
        &config_path,
        format!(
            "[scan]\nroots = [\"{}\"]\n\n[git]\nexclude_mode = \"global\"\n",
            fixture.root.path().display()
        ),
    )
    .unwrap();
    let global_config = fixture.root.path().join("global-gitconfig");
    let bin = env!("CARGO_BIN_EXE_claudectomy");

    let apply = Command::new(bin)
        .env("CLAUDECTOMY_DATA_DIR", data_dir)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .args(["--config", config_path.to_str().unwrap(), "apply"])
        .output()
        .expect("apply runs");
    assert!(
        apply.status.success(),
        "apply failed: {}\n{}",
        String::from_utf8_lossy(&apply.stdout),
        String::from_utf8_lossy(&apply.stderr)
    );

    let configured = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .args(["config", "--global", "--get", "core.excludesFile"])
        .output()
        .expect("git config reads");
    assert!(configured.status.success());
    assert_eq!(
        String::from_utf8_lossy(&configured.stdout).trim(),
        data_dir.join("git-excludes").to_string_lossy()
    );

    let status = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .arg("-C")
        .arg(&repo)
        .args(["status", "--short", "--", "CLAUDE.md"])
        .output()
        .expect("git status runs");
    assert!(status.status.success());
    assert_eq!(String::from_utf8_lossy(&status.stdout), "");
}

#[test]
fn global_mode_conflict_removes_stale_per_repo_ignore() {
    let fixture = Fixture::new();
    let repo = fixture.repo("repo");
    fs::write(repo.join("AGENTS.md"), "canonical instructions\n").unwrap();
    let config_path = fixture.root.path().join("config.toml");
    let data_dir = fixture.data.path();
    let global_config = fixture.root.path().join("global-gitconfig");
    let bin = env!("CARGO_BIN_EXE_claudectomy");

    fs::write(
        &config_path,
        format!("[scan]\nroots = [\"{}\"]\n", fixture.root.path().display()),
    )
    .unwrap();
    let apply = Command::new(bin)
        .env("CLAUDECTOMY_DATA_DIR", data_dir)
        .args(["--config", config_path.to_str().unwrap(), "apply"])
        .output()
        .expect("apply runs");
    assert!(
        apply.status.success(),
        "apply failed: {}",
        String::from_utf8_lossy(&apply.stderr)
    );

    fs::remove_file(repo.join("CLAUDE.md")).unwrap();
    fs::write(repo.join("CLAUDE.md"), "user-owned replacement\n").unwrap();
    fs::write(
        data_dir.join("git-excludes"),
        "# claudectomy managed begin\n/CLAUDE.md\n# claudectomy managed end\n",
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
        format!(
            "[scan]\nroots = [\"{}\"]\n\n[git]\nexclude_mode = \"global\"\n",
            fixture.root.path().display()
        ),
    )
    .unwrap();
    let conflict = Command::new(bin)
        .env("CLAUDECTOMY_DATA_DIR", data_dir)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .args(["--config", config_path.to_str().unwrap(), "apply"])
        .output()
        .expect("global apply runs");
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
        b"# claudectomy managed begin\n/CLAUDE.md\n# claudectomy managed end\n\xff".to_vec();
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
        b"# claudectomy managed begin\n/CLAUDE.md\n# claudectomy managed end\n\xff".to_vec();
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

fn git_exclude_path(repo: &Path) -> PathBuf {
    let exclude = git_stdout(repo, &["rev-parse", "--git-path", "info/exclude"]);
    path_from_git_output(repo, &exclude)
}

fn path_from_git_output(repo: &Path, text: &str) -> PathBuf {
    let path = PathBuf::from(text);
    if path.is_absolute() {
        path
    } else {
        repo.join(path)
    }
}
