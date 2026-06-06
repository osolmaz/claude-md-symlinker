use std::{fs, path::PathBuf};

use anyhow::Result;
use serde::Serialize;
use tempfile::tempdir;

use crate::{
    adapters,
    config::ExcludeMode,
    config::{self, LoadedConfig},
    git,
    state::State,
};

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
}

#[derive(Debug, Serialize)]
pub struct DoctorCheck {
    pub name: String,
    pub ok: bool,
    pub message: String,
}

impl DoctorReport {
    pub fn run(loaded: &LoadedConfig, dry_run: bool) -> Result<Self> {
        let mut checks = Vec::new();

        checks.push(check(
            "git",
            git::git_available(),
            "local git binary is available",
            "local git binary was not found or is not executable",
        ));

        checks.push(config_check(loaded));

        checks.push(data_dir_check(dry_run));

        let state_result = if dry_run {
            State::open_default_read_only_if_exists()
        } else {
            State::open_default()
        };
        checks.push(match state_result {
            Ok(_) => DoctorCheck {
                name: "state".to_string(),
                ok: true,
                message: if dry_run {
                    "SQLite state is readable or absent".to_string()
                } else {
                    "SQLite state opened successfully".to_string()
                },
            },
            Err(error) => DoctorCheck {
                name: "state".to_string(),
                ok: false,
                message: error.to_string(),
            },
        });

        checks.push(symlink_check());

        for root in &loaded.config.scan.roots {
            let expanded = config::expand_tilde(root);
            checks.push(DoctorCheck {
                name: "scan_root".to_string(),
                ok: expanded.exists(),
                message: if expanded.exists() {
                    format!("scan root exists: {}", expanded.display())
                } else {
                    format!("scan root does not exist: {}", expanded.display())
                },
            });
        }

        checks.push(match adapters::enabled_adapters(&loaded.config) {
            Ok(adapters) => DoctorCheck {
                name: "adapters".to_string(),
                ok: !adapters.is_empty(),
                message: if adapters.is_empty() {
                    "no adapters are enabled".to_string()
                } else {
                    format!(
                        "enabled adapters: {}",
                        adapters
                            .iter()
                            .map(|a| a.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                },
            },
            Err(error) => DoctorCheck {
                name: "adapters".to_string(),
                ok: false,
                message: error.to_string(),
            },
        });

        Ok(Self { checks })
    }

    pub fn exit_code(&self) -> u8 {
        if self.checks.iter().any(|check| !check.ok) {
            1
        } else {
            0
        }
    }

    pub fn print_plain(&self) {
        for check in &self.checks {
            let status = if check.ok { "ok" } else { "fail" };
            println!("{status}\t{}\t{}", check.name, check.message);
        }
    }
}

fn data_dir_check(dry_run: bool) -> DoctorCheck {
    match config::data_dir() {
        Ok(dir) if dry_run && !dir.exists() => DoctorCheck {
            name: "data_dir".to_string(),
            ok: true,
            message: format!("data directory would be created: {}", dir.display()),
        },
        Ok(dir) if dry_run => match fs::metadata(&dir) {
            Ok(metadata) => DoctorCheck {
                name: "data_dir".to_string(),
                ok: metadata.is_dir() && !metadata.permissions().readonly(),
                message: if metadata.is_dir() && !metadata.permissions().readonly() {
                    format!(
                        "data directory exists and appears writable: {}",
                        dir.display()
                    )
                } else {
                    format!("data directory is not writable: {}", dir.display())
                },
            },
            Err(error) => DoctorCheck {
                name: "data_dir".to_string(),
                ok: false,
                message: format!("data directory is not accessible: {error}"),
            },
        },
        Ok(dir) => match fs::create_dir_all(&dir) {
            Ok(()) => DoctorCheck {
                name: "data_dir".to_string(),
                ok: true,
                message: format!("data directory is writable: {}", dir.display()),
            },
            Err(error) => DoctorCheck {
                name: "data_dir".to_string(),
                ok: false,
                message: format!("data directory is not writable: {error}"),
            },
        },
        Err(error) => DoctorCheck {
            name: "data_dir".to_string(),
            ok: false,
            message: error.to_string(),
        },
    }
}

fn check(name: &str, ok: bool, ok_message: &str, fail_message: &str) -> DoctorCheck {
    DoctorCheck {
        name: name.to_string(),
        ok,
        message: if ok { ok_message } else { fail_message }.to_string(),
    }
}

fn config_check(loaded: &LoadedConfig) -> DoctorCheck {
    if loaded.config.git.exclude_mode == ExcludeMode::Global {
        return DoctorCheck {
            name: "config".to_string(),
            ok: false,
            message: "global exclude mode is disabled; set [git] exclude_mode = \"per_repo\""
                .to_string(),
        };
    }

    DoctorCheck {
        name: "config".to_string(),
        ok: true,
        message: if loaded.existed {
            format!("config loaded from {}", loaded.path.display())
        } else {
            format!(
                "config not found; defaults would use {}",
                loaded.path.display()
            )
        },
    }
}

fn symlink_check() -> DoctorCheck {
    match symlink_probe() {
        Ok(()) => DoctorCheck {
            name: "symlink".to_string(),
            ok: true,
            message: "file symlink creation is available".to_string(),
        },
        Err(error) => DoctorCheck {
            name: "symlink".to_string(),
            ok: false,
            message: format!("file symlink creation failed: {error}"),
        },
    }
}

fn symlink_probe() -> Result<()> {
    let dir = tempdir()?;
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    fs::write(&source, "probe")?;
    symlink_file(&source, &target)?;
    Ok(())
}

#[cfg(unix)]
fn symlink_file(source: &PathBuf, target: &PathBuf) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, target)
}

#[cfg(windows)]
fn symlink_file(source: &PathBuf, target: &PathBuf) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(source, target)
}
