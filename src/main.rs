use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use claude_md_symlinker::{
    cleaner, cli, config,
    doctor::DoctorReport,
    reconciler::{self, ReconcileOptions},
    reporting::{print_json, print_plain},
    service,
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
