use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{
    adapters::{self, Adapter},
    config::{self, AppConfig, LoadedConfig, ScanScope, WatchConfig},
    reconciler::{self, ReconcileOptions},
    reporting::{print_json, print_plain},
    state::State,
};

type WatchTargets = BTreeMap<PathBuf, RecursiveMode>;

struct ActiveConfig {
    config: AppConfig,
    config_existed: bool,
    scope: ScanScope,
    adapters: Vec<Adapter>,
}

#[derive(Debug, Clone, Copy)]
struct RunOptions {
    dry_run: bool,
    json: bool,
}

pub fn run(
    loaded: LoadedConfig,
    cli_roots: &[PathBuf],
    state: &State,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let config_path = loaded.path.clone();
    let mut active = active_config(loaded, cli_roots)?;
    let options = RunOptions { dry_run, json };

    let (tx, rx) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        move |result| {
            let _ = tx.send(result);
        },
        NotifyConfig::default(),
    )
    .context("failed to create filesystem watcher")?;

    let mut watched = WatchTargets::new();
    sync_watches(
        &mut watcher,
        &mut watched,
        desired_watch_targets(&active.scope, &config_path),
    )?;

    run_once(&active, cli_roots, state, options)?;

    let debounce = Duration::from_millis(500);
    let mut last_run = Instant::now();

    loop {
        let interval = periodic_interval(&active.config.watch);
        let until_periodic_reconcile = interval
            .checked_sub(last_run.elapsed())
            .unwrap_or(Duration::ZERO);
        let timeout = until_periodic_reconcile.max(Duration::from_millis(1));

        match rx.recv_timeout(timeout) {
            Ok(Ok(event)) => {
                let mut should_run =
                    event_is_relevant(&event, &active.scope, &active.adapters, &config_path);
                thread::sleep(debounce);
                while let Ok(result) = rx.try_recv() {
                    match result {
                        Ok(event) => {
                            should_run |= event_is_relevant(
                                &event,
                                &active.scope,
                                &active.adapters,
                                &config_path,
                            );
                        }
                        Err(error) => tracing::warn!("watch error: {error}"),
                    }
                }
                if should_run {
                    reload_and_run(
                        &mut active,
                        &mut watcher,
                        &mut watched,
                        &config_path,
                        cli_roots,
                        state,
                        options,
                    )?;
                    last_run = Instant::now();
                }
            }
            Ok(Err(error)) => {
                tracing::warn!("watch error: {error}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                reload_and_run(
                    &mut active,
                    &mut watcher,
                    &mut watched,
                    &config_path,
                    cli_roots,
                    state,
                    options,
                )?;
                last_run = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("filesystem watcher disconnected");
            }
        }
    }
}

fn periodic_interval(config: &WatchConfig) -> Duration {
    let reconcile = Duration::from_secs(config.reconcile_interval_minutes.max(1) * 60);
    let full_rescan = Duration::from_secs(config.full_rescan_interval_hours.max(1) * 60 * 60);
    reconcile.min(full_rescan)
}

fn reload_and_run(
    active: &mut ActiveConfig,
    watcher: &mut RecommendedWatcher,
    watched: &mut WatchTargets,
    config_path: &Path,
    cli_roots: &[PathBuf],
    state: &State,
    options: RunOptions,
) -> Result<bool> {
    let next = match reload_active_config(config_path, cli_roots) {
        Ok(active) => active,
        Err(error) => {
            tracing::warn!("failed to reload config; skipping reconcile: {error:#}");
            return Ok(false);
        }
    };
    sync_watches(
        watcher,
        watched,
        desired_watch_targets(&next.scope, config_path),
    )?;
    *active = next;
    run_once(active, cli_roots, state, options)?;
    Ok(true)
}

fn run_once(
    active: &ActiveConfig,
    cli_roots: &[PathBuf],
    state: &State,
    options: RunOptions,
) -> Result<()> {
    let report = reconciler::apply(
        &active.config,
        active.config_existed,
        cli_roots,
        state,
        ReconcileOptions {
            dry_run: options.dry_run,
        },
    )?;
    if options.json {
        print_json(&report);
    } else {
        print_plain(&report, options.dry_run);
    }
    Ok(())
}

fn active_config(loaded: LoadedConfig, cli_roots: &[PathBuf]) -> Result<ActiveConfig> {
    let scope = loaded.config.scan_scope(loaded.existed, cli_roots)?;
    let adapters = adapters::enabled_adapters(&loaded.config)?;
    Ok(ActiveConfig {
        config: loaded.config,
        config_existed: loaded.existed,
        scope,
        adapters,
    })
}

fn reload_active_config(config_path: &Path, cli_roots: &[PathBuf]) -> Result<ActiveConfig> {
    active_config(config::load(Some(config_path))?, cli_roots)
}

fn sync_watches(
    watcher: &mut RecommendedWatcher,
    current: &mut WatchTargets,
    desired: WatchTargets,
) -> Result<()> {
    let removals = current
        .iter()
        .filter(|(path, mode)| desired.get(*path) != Some(*mode))
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    for path in removals {
        if let Err(error) = watcher.unwatch(&path) {
            tracing::warn!("failed to unwatch {}: {error}", path.display());
        }
        current.remove(&path);
    }

    for (path, mode) in desired {
        if current.get(&path) == Some(&mode) {
            continue;
        }
        watcher
            .watch(&path, mode)
            .with_context(|| format!("failed to watch {}", path.display()))?;
        current.insert(path, mode);
    }

    Ok(())
}

fn desired_watch_targets(scope: &ScanScope, config_path: &Path) -> WatchTargets {
    let mut targets = WatchTargets::new();
    for root in &scope.roots {
        insert_watch_target(&mut targets, root.clone(), RecursiveMode::Recursive);
    }
    if let Some(parent) = config_path.parent().filter(|parent| parent.exists()) {
        insert_watch_target(
            &mut targets,
            parent.to_path_buf(),
            RecursiveMode::NonRecursive,
        );
    }
    targets
}

fn insert_watch_target(targets: &mut WatchTargets, path: PathBuf, mode: RecursiveMode) {
    if targets.get(&path) == Some(&RecursiveMode::Recursive) {
        return;
    }
    targets.insert(path, mode);
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
    if path == config_path || config_path.parent().is_some_and(|parent| path == parent) {
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
    use std::{collections::BTreeSet, path::PathBuf, time::Duration};

    use notify::{Event, EventKind, RecursiveMode, event::EventAttributes};

    use super::{desired_watch_targets, event_is_relevant, periodic_interval};
    use crate::{
        adapters::Adapter,
        config::{ScanScope, SourceMissingBehavior, WatchConfig},
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
        assert!(event_is_relevant(
            &event(config_path.parent().unwrap().to_path_buf()),
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

    #[test]
    fn watch_targets_include_roots_and_config_parent() {
        let root = PathBuf::from("/workspace");
        let scope = scope(root.clone());
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("claudectomy.toml");

        let targets = desired_watch_targets(&scope, &config_path);

        assert_eq!(targets.get(&root), Some(&RecursiveMode::Recursive));
        assert_eq!(
            targets.get(config_dir.path()),
            Some(&RecursiveMode::NonRecursive)
        );
    }

    #[test]
    fn periodic_interval_uses_shortest_configured_reconcile_interval() {
        let default_like = WatchConfig {
            enabled: true,
            reconcile_interval_minutes: 30,
            full_rescan_interval_hours: 12,
        };
        assert_eq!(
            periodic_interval(&default_like),
            Duration::from_secs(30 * 60)
        );

        let shorter_full_rescan = WatchConfig {
            enabled: true,
            reconcile_interval_minutes: 120,
            full_rescan_interval_hours: 1,
        };
        assert_eq!(
            periodic_interval(&shorter_full_rescan),
            Duration::from_secs(60 * 60)
        );
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
