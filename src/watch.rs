use std::{
    path::{Component, Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{
    adapters::{self, Adapter},
    config::{AppConfig, ScanScope},
    reconciler::{self, ReconcileOptions},
    reporting::print_plain,
    state::State,
};

pub fn run(
    config: &AppConfig,
    config_existed: bool,
    cli_roots: &[PathBuf],
    state: &State,
    dry_run: bool,
    config_path: &Path,
) -> Result<()> {
    let scope = config.scan_scope(config_existed, cli_roots)?;
    let adapters = adapters::enabled_adapters(config)?;
    let initial = reconciler::apply(
        config,
        config_existed,
        cli_roots,
        state,
        ReconcileOptions { dry_run },
    )?;
    print_plain(&initial, dry_run);

    let (tx, rx) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        move |result| {
            let _ = tx.send(result);
        },
        NotifyConfig::default(),
    )
    .context("failed to create filesystem watcher")?;

    for root in &scope.roots {
        watcher
            .watch(root, RecursiveMode::Recursive)
            .with_context(|| format!("failed to watch {}", root.display()))?;
    }

    let event_poll_interval =
        Duration::from_secs(config.watch.reconcile_interval_minutes.max(1) * 60);
    let full_rescan_interval =
        Duration::from_secs(config.watch.full_rescan_interval_hours.max(1) * 60 * 60);
    let debounce = Duration::from_millis(500);
    let mut last_run = Instant::now();

    loop {
        let until_full_rescan = full_rescan_interval
            .checked_sub(last_run.elapsed())
            .unwrap_or(Duration::ZERO);
        let timeout = event_poll_interval.min(until_full_rescan.max(Duration::from_millis(1)));

        match rx.recv_timeout(timeout) {
            Ok(Ok(event)) => {
                let mut should_run = event_is_relevant(&event, &scope, &adapters, config_path);
                thread::sleep(debounce);
                while let Ok(result) = rx.try_recv() {
                    match result {
                        Ok(event) => {
                            should_run |= event_is_relevant(&event, &scope, &adapters, config_path);
                        }
                        Err(error) => tracing::warn!("watch error: {error}"),
                    }
                }
                if should_run {
                    run_once(config, config_existed, cli_roots, state, dry_run)?;
                    last_run = Instant::now();
                }
            }
            Ok(Err(error)) => {
                tracing::warn!("watch error: {error}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if last_run.elapsed() >= full_rescan_interval {
                    run_once(config, config_existed, cli_roots, state, dry_run)?;
                    last_run = Instant::now();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("filesystem watcher disconnected");
            }
        }
    }
}

fn run_once(
    config: &AppConfig,
    config_existed: bool,
    cli_roots: &[PathBuf],
    state: &State,
    dry_run: bool,
) -> Result<()> {
    let report = reconciler::apply(
        config,
        config_existed,
        cli_roots,
        state,
        ReconcileOptions { dry_run },
    )?;
    print_plain(&report, dry_run);
    Ok(())
}

fn event_is_relevant(
    event: &Event,
    scope: &ScanScope,
    adapters: &[Adapter],
    config_path: &Path,
) -> bool {
    event
        .paths
        .iter()
        .any(|path| path_is_relevant(path, scope, adapters, config_path))
}

fn path_is_relevant(
    path: &Path,
    scope: &ScanScope,
    adapters: &[Adapter],
    config_path: &Path,
) -> bool {
    if path == config_path {
        return true;
    }

    if !scope.roots.iter().any(|root| path.starts_with(root)) {
        return false;
    }

    if !scope.include_paths.is_empty()
        && !scope
            .include_paths
            .iter()
            .any(|included| path.starts_with(included))
    {
        return false;
    }

    if scope.path_is_excluded(path) || has_excluded_dir_component(path, scope) {
        return false;
    }

    if path
        .components()
        .any(|component| matches!(component, Component::Normal(name) if name == ".git"))
    {
        return true;
    }

    adapters
        .iter()
        .any(|adapter| path.ends_with(&adapter.source) || path.ends_with(&adapter.target))
}

fn has_excluded_dir_component(path: &Path, scope: &ScanScope) -> bool {
    let Some(relative) = scope
        .roots
        .iter()
        .find_map(|root| path.strip_prefix(root).ok())
    else {
        return false;
    };

    relative.components().any(|component| {
        matches!(
            component,
            Component::Normal(name)
                if name.to_str().is_some_and(|name| scope.exclude_dir_names.contains(name))
        )
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, path::PathBuf};

    use notify::{Event, EventKind, event::EventAttributes};

    use super::event_is_relevant;
    use crate::{
        adapters::Adapter,
        config::{ScanScope, SourceMissingBehavior},
    };

    #[test]
    fn source_target_git_and_config_events_are_relevant() {
        let root = PathBuf::from("/workspace");
        let scope = scope(root.clone());
        let adapters = adapters();
        let config_path = PathBuf::from("/home/user/.config/claudectomy/config.toml");

        assert!(event_is_relevant(
            &event(root.join("repo/AGENTS.md")),
            &scope,
            &adapters,
            &config_path
        ));
        assert!(event_is_relevant(
            &event(root.join("repo/CLAUDE.md")),
            &scope,
            &adapters,
            &config_path
        ));
        assert!(event_is_relevant(
            &event(root.join("repo/.git/index")),
            &scope,
            &adapters,
            &config_path
        ));
        assert!(event_is_relevant(
            &event(config_path.clone()),
            &scope,
            &adapters,
            &config_path
        ));
    }

    #[test]
    fn unrelated_and_excluded_events_are_ignored() {
        let root = PathBuf::from("/workspace");
        let mut scope = scope(root.clone());
        scope.exclude_paths = vec![root.join("archive")];
        let adapters = adapters();
        let config_path = PathBuf::from("/home/user/.config/claudectomy/config.toml");

        assert!(!event_is_relevant(
            &event(root.join("repo/src/main.rs")),
            &scope,
            &adapters,
            &config_path
        ));
        assert!(!event_is_relevant(
            &event(root.join("repo/target/AGENTS.md")),
            &scope,
            &adapters,
            &config_path
        ));
        assert!(!event_is_relevant(
            &event(root.join("archive/repo/AGENTS.md")),
            &scope,
            &adapters,
            &config_path
        ));
        assert!(!event_is_relevant(
            &event(PathBuf::from("/elsewhere/repo/AGENTS.md")),
            &scope,
            &adapters,
            &config_path
        ));

        scope.include_paths = vec![root.join("included")];
        assert!(!event_is_relevant(
            &event(root.join("repo/AGENTS.md")),
            &scope,
            &adapters,
            &config_path
        ));
        assert!(event_is_relevant(
            &event(root.join("included/repo/AGENTS.md")),
            &scope,
            &adapters,
            &config_path
        ));
    }

    fn scope(root: PathBuf) -> ScanScope {
        ScanScope {
            roots: vec![root],
            include_paths: Vec::new(),
            exclude_paths: Vec::new(),
            exclude_dir_names: ["node_modules", "target", "dist", "build"]
                .into_iter()
                .map(String::from)
                .collect::<BTreeSet<_>>(),
        }
    }

    fn adapters() -> Vec<Adapter> {
        vec![Adapter {
            name: "claude".to_string(),
            source: PathBuf::from("AGENTS.md"),
            target: PathBuf::from("CLAUDE.md"),
            on_source_missing: SourceMissingBehavior::Leave,
        }]
    }

    fn event(path: PathBuf) -> Event {
        Event {
            kind: EventKind::Any,
            paths: vec![path],
            attrs: EventAttributes::default(),
        }
    }
}
