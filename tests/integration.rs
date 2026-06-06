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

fn path_from_git_output(repo: &Path, text: &str) -> PathBuf {
    let path = PathBuf::from(text);
    if path.is_absolute() {
        path
    } else {
        repo.join(path)
    }
}
