use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use claude_md_symlinker::{
    cleaner, cli, config, daemon,
    doctor::DoctorReport,
    install, migration, observe, purge,
    reconciler::{self, ReconcileOptions},
    reporting::{print_json, print_plain},
    repos, service,
    state::State,
    watch,
};

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .without_time()
        .init();

    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<u8> {
    let args = cli::Cli::parse();

    match args.command {
        cli::Command::Install(install_args) => {
            let state = command_state(args.dry_run)?;
            install::install(&install_args, &state, args.dry_run, args.json)
        }
        cli::Command::Observe(observe_args) => {
            let state = command_state(args.dry_run)?;
            observe::run(&observe_args, &state, args.dry_run, args.json)
        }
        cli::Command::Daemon(daemon_args) => {
            let state = command_state(args.dry_run)?;
            daemon::run(&daemon_args, &state, args.dry_run, args.json)
        }
        cli::Command::Status => {
            let state = State::open_default_read_only_if_exists()?;
            install::status(&state, args.json)
        }
        cli::Command::Repos(repos_args) => {
            let state = command_state(args.dry_run)?;
            repos::run(&repos_args, &state, args.json)
        }
        cli::Command::Migrate(migrate_args) => {
            let state = command_state(args.dry_run)?;
            let report = migration::migrate(
                &state,
                migration::MigrateOptions {
                    dry_run: args.dry_run,
                    auto_safe_only: migrate_args.auto_safe_only,
                    replace_existing: migrate_args.replace_existing,
                    git_add: !migrate_args.no_git_add,
                },
            )?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                migration::print_plain(&report);
            }
            Ok(
                if report.needs_review.is_empty() && report.skipped.is_empty() {
                    0
                } else {
                    2
                },
            )
        }
        cli::Command::Settings(settings_args) => {
            let state = command_state(args.dry_run)?;
            run_settings(settings_args, &state, args.dry_run, args.json)
        }
        cli::Command::Purge(purge_args) => {
            let state = command_state(args.dry_run)?;
            purge::run(&purge_args, &state, args.dry_run, args.json)
        }
        cli::Command::Uninstall(uninstall_args) => {
            let state = command_state(args.dry_run)?;
            let uninstall_report = install::uninstall_report(&uninstall_args, args.dry_run)?;
            let purge_report = if uninstall_args.purge {
                Some(purge::purge(&state, args.dry_run)?)
            } else {
                None
            };
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "uninstall": uninstall_report,
                        "purge": purge_report,
                    }))?
                );
            } else {
                install::print_uninstall(&uninstall_report);
                if let Some(purge_report) = &purge_report {
                    println!("Purged {} managed shims.", purge_report.removed.len());
                }
            }
            Ok(0)
        }
        cli::Command::Init(init) => {
            let loaded = config::load(args.config.as_deref())?;
            let mut cfg = loaded.config;
            cfg.set_scan_roots(init.roots)?;
            let saved_path = if args.dry_run {
                loaded.path
            } else {
                config::save(args.config.as_deref().or(Some(loaded.path.as_path())), &cfg)?
            };

            if init.apply {
                if !args.json {
                    if args.dry_run {
                        println!("Would write {}", saved_path.display());
                    } else {
                        println!("Wrote {}", saved_path.display());
                    }
                }
                let state = command_state(args.dry_run)?;
                let report = reconciler::apply(
                    &cfg,
                    true,
                    &[],
                    &state,
                    ReconcileOptions {
                        dry_run: args.dry_run,
                    },
                )?;
                output_report(args.json, args.dry_run, &report);
                return Ok(report.exit_code());
            }

            if args.json {
                println!(
                    "{}",
                    serde_json::json!({ "config_path": saved_path, "dry_run": args.dry_run })
                );
            } else if args.dry_run {
                println!("Would write {}", saved_path.display());
            } else {
                println!("Wrote {}", saved_path.display());
            }

            Ok(0)
        }
        cli::Command::Apply(apply) => {
            let loaded = config::load(args.config.as_deref())?;
            let state = command_state(args.dry_run)?;
            let report = reconciler::apply(
                &loaded.config,
                loaded.existed,
                &apply.roots,
                &state,
                ReconcileOptions {
                    dry_run: args.dry_run,
                },
            )?;
            output_report(args.json, args.dry_run, &report);
            Ok(report.exit_code())
        }
        cli::Command::Clean(clean) => {
            let loaded = config::load(args.config.as_deref())?;
            let state = command_state(args.dry_run)?;
            let report = cleaner::clean(
                &loaded.config,
                loaded.existed,
                &clean.roots,
                &state,
                cleaner::CleanOptions {
                    dry_run: args.dry_run,
                    remove_if_source_missing: clean.remove_if_source_missing,
                },
            )?;
            output_report(args.json, args.dry_run, &report);
            Ok(report.exit_code())
        }
        cli::Command::Doctor => {
            let loaded = config::load(args.config.as_deref())?;
            let report = DoctorReport::run(&loaded, args.dry_run)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                report.print_plain();
            }
            Ok(report.exit_code())
        }
        cli::Command::Watch(watch_args) => {
            let loaded = config::load(args.config.as_deref())?;
            let state = command_state(args.dry_run)?;
            watch::run(loaded, &watch_args.roots, &state, args.dry_run, args.json)?;
            Ok(0)
        }
        cli::Command::Service(service_args) => {
            let loaded = if matches!(&service_args.command, cli::ServiceCommand::Install(_)) {
                Some(config::load(args.config.as_deref())?)
            } else {
                None
            };
            service::run(
                &service_args.command,
                loaded.as_ref(),
                args.dry_run,
                args.json,
            )
        }
    }
}

fn run_settings(args: cli::SettingsArgs, state: &State, dry_run: bool, json: bool) -> Result<u8> {
    match args.command {
        cli::SettingsCommand::Set(set) => {
            let key = normalize_setting_key(&set.key)?;
            let value = normalize_setting_value(&set.value)?;
            if !dry_run {
                state.set_setting(&key, &value)?;
            }
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "key": key, "value": value, "dry_run": dry_run })
                );
            } else if dry_run {
                println!("Would set {key} = {value}.");
            } else {
                println!("Set {key} = {value}.");
            }
            Ok(0)
        }
        cli::SettingsCommand::Get(get) => {
            let key = normalize_setting_key(&get.key)?;
            let value = state.get_setting(&key)?;
            if json {
                println!("{}", serde_json::json!({ "key": key, "value": value }));
            } else if let Some(value) = value {
                println!("{value}");
            }
            Ok(0)
        }
    }
}

fn normalize_setting_key(key: &str) -> Result<String> {
    match key {
        "auto-migrate" | "auto_migrate" => Ok("auto_migrate".to_string()),
        other => anyhow::bail!("unsupported setting `{other}`"),
    }
}

fn normalize_setting_value(value: &str) -> Result<String> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok("true".to_string()),
        "false" | "0" | "no" | "off" => Ok("false".to_string()),
        other => anyhow::bail!("unsupported boolean value `{other}`"),
    }
}

fn command_state(dry_run: bool) -> Result<State> {
    if dry_run {
        State::open_default_read_only_if_exists()
    } else {
        State::open_default()
    }
}

fn output_report(json: bool, dry_run: bool, report: &claude_md_symlinker::reporting::Report) {
    if json {
        print_json(report);
    } else {
        print_plain(report, dry_run);
    }
}
