use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::{
    cli::{ServiceCommand, ServiceInstallArgs, ServiceUnitArgs},
    config::{self, LoadedConfig},
};

const MANAGED_MARKER: &str = "# claudemdeez managed systemd user unit";

#[derive(Debug, Serialize)]
struct ServiceReport {
    action: String,
    unit_name: String,
    unit_path: PathBuf,
    dry_run: bool,
    message: String,
}

#[derive(Debug, Clone)]
struct UnitSpec {
    unit_name: String,
    unit_path: PathBuf,
    config_path: PathBuf,
    bin_path: PathBuf,
    data_dir: PathBuf,
}

pub fn run(
    command: &ServiceCommand,
    loaded: Option<&LoadedConfig>,
    dry_run: bool,
    json: bool,
) -> Result<u8> {
    ensure_linux()?;

    match command {
        ServiceCommand::Install(args) => install(
            args,
            loaded.context("service install requires loaded config")?,
            dry_run,
            json,
        ),
        ServiceCommand::Uninstall(args) => uninstall(args, dry_run, json),
        ServiceCommand::Start(args) => systemctl_unit_action("start", args, dry_run, json),
        ServiceCommand::Stop(args) => systemctl_unit_action("stop", args, dry_run, json),
        ServiceCommand::Restart(args) => systemctl_unit_action("restart", args, dry_run, json),
        ServiceCommand::Status(args) => status(args, dry_run, json),
    }
}

fn install(
    args: &ServiceInstallArgs,
    loaded: &LoadedConfig,
    dry_run: bool,
    json: bool,
) -> Result<u8> {
    ensure_service_scan_paths_are_absolute(&loaded.config)?;
    loaded
        .config
        .scan_scope(loaded.existed, &[])
        .context("service install requires configured scan roots")?;

    let spec = install_spec(args, loaded)?;
    ensure_existing_unit_is_managed_or_absent(&spec.unit_path)?;
    let unit = build_unit(&spec);

    if dry_run {
        print_report(
            json,
            ServiceReport {
                action: "install".to_string(),
                unit_name: spec.unit_name,
                unit_path: spec.unit_path,
                dry_run,
                message: "would write systemd user unit".to_string(),
            },
        )?;
        return Ok(0);
    }

    ensure_systemd_user_available()?;
    ensure_existing_unit_is_managed_or_absent(&spec.unit_path)?;

    if let Some(parent) = spec.unit_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create systemd user dir {}", parent.display()))?;
    }
    fs::write(&spec.unit_path, unit)
        .with_context(|| format!("failed to write {}", spec.unit_path.display()))?;

    run_systemctl_checked(&["daemon-reload"])?;
    if !args.no_enable {
        run_systemctl_checked(&["enable", &spec.unit_name])?;
    }
    if args.now {
        run_systemctl_checked(&["start", &spec.unit_name])?;
    }

    print_report(
        json,
        ServiceReport {
            action: "install".to_string(),
            unit_name: spec.unit_name,
            unit_path: spec.unit_path,
            dry_run,
            message: if args.now {
                "installed and started systemd user unit".to_string()
            } else if args.no_enable {
                "installed systemd user unit".to_string()
            } else {
                "installed and enabled systemd user unit".to_string()
            },
        },
    )?;
    Ok(0)
}

fn uninstall(args: &ServiceUnitArgs, dry_run: bool, json: bool) -> Result<u8> {
    let unit_name = normalize_unit_name(&args.unit_name)?;
    let unit_path = unit_path(&unit_name)?;

    if dry_run {
        print_report(
            json,
            ServiceReport {
                action: "uninstall".to_string(),
                unit_name,
                unit_path,
                dry_run,
                message: "would remove managed systemd user unit".to_string(),
            },
        )?;
        return Ok(0);
    }

    let existing = match fs::read_to_string(&unit_path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            print_report(
                json,
                ServiceReport {
                    action: "uninstall".to_string(),
                    unit_name,
                    unit_path,
                    dry_run,
                    message: "systemd user unit is not installed".to_string(),
                },
            )?;
            return Ok(0);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", unit_path.display()));
        }
    };
    if !is_managed_unit(&existing) {
        bail!(
            "refusing to remove unmanaged systemd user unit {}",
            unit_path.display()
        );
    }

    ensure_systemd_user_available()?;
    let _ = run_systemctl_status(&["disable", "--now", &unit_name])?;
    fs::remove_file(&unit_path)
        .with_context(|| format!("failed to remove {}", unit_path.display()))?;
    run_systemctl_checked(&["daemon-reload"])?;
    let _ = run_systemctl_status(&["reset-failed", &unit_name])?;

    print_report(
        json,
        ServiceReport {
            action: "uninstall".to_string(),
            unit_name,
            unit_path,
            dry_run,
            message: "removed managed systemd user unit".to_string(),
        },
    )?;
    Ok(0)
}

fn systemctl_unit_action(
    action: &str,
    args: &ServiceUnitArgs,
    dry_run: bool,
    json: bool,
) -> Result<u8> {
    let unit_name = normalize_unit_name(&args.unit_name)?;
    let unit_path = unit_path(&unit_name)?;
    if dry_run {
        print_report(
            json,
            ServiceReport {
                action: action.to_string(),
                unit_name,
                unit_path,
                dry_run,
                message: format!("would run systemctl --user {action}"),
            },
        )?;
        return Ok(0);
    }

    ensure_systemd_user_available()?;
    run_systemctl_checked(&[action, &unit_name])?;
    print_report(
        json,
        ServiceReport {
            action: action.to_string(),
            unit_name,
            unit_path,
            dry_run,
            message: format!("systemd user unit {action} complete"),
        },
    )?;
    Ok(0)
}

fn status(args: &ServiceUnitArgs, dry_run: bool, json: bool) -> Result<u8> {
    let unit_name = normalize_unit_name(&args.unit_name)?;
    let unit_path = unit_path(&unit_name)?;
    if dry_run || json {
        if !dry_run {
            ensure_systemd_user_available()?;
        }
        let active = if dry_run {
            None
        } else {
            Some(systemctl_output_status(&["is-active", &unit_name])?)
        };
        let message = active
            .as_ref()
            .map(|active| active.stdout.trim())
            .filter(|text| !text.is_empty())
            .unwrap_or("would query systemd user unit status")
            .to_string();
        let exit_code = active.as_ref().map(|active| active.exit_code).unwrap_or(0);
        print_report(
            json,
            ServiceReport {
                action: "status".to_string(),
                unit_name,
                unit_path,
                dry_run,
                message,
            },
        )?;
        return Ok(exit_code);
    }

    ensure_systemd_user_available()?;
    let status = Command::new("systemctl")
        .arg("--user")
        .arg("status")
        .arg(&unit_name)
        .status()
        .context("failed to run systemctl --user status")?;
    Ok(status.code().unwrap_or(1) as u8)
}

fn install_spec(args: &ServiceInstallArgs, loaded: &LoadedConfig) -> Result<UnitSpec> {
    let unit_name = normalize_unit_name(&args.unit_name)?;
    let bin_path = match &args.bin {
        Some(path) => absolute_expanded_path(path)?,
        None => env::current_exe().context("failed to resolve current executable")?,
    };
    let data_dir = match &args.data_dir {
        Some(path) => absolute_expanded_path(path)?,
        None => absolute_expanded_path(&config::data_dir()?)?,
    };

    Ok(UnitSpec {
        unit_path: unit_path(&unit_name)?,
        unit_name,
        config_path: loaded.path.clone(),
        bin_path,
        data_dir,
    })
}

fn build_unit(spec: &UnitSpec) -> String {
    let exec_args = [
        spec.bin_path.as_path(),
        Path::new("--config"),
        spec.config_path.as_path(),
        Path::new("watch"),
    ]
    .iter()
    .map(|arg| quote_systemd_arg(&arg.to_string_lossy()))
    .collect::<Vec<_>>()
    .join(" ");
    let data_env = format!("CLAUDEMDEEZ_DATA_DIR={}", spec.data_dir.display());

    format!(
        r#"{MANAGED_MARKER}
[Unit]
Description=CLAUDE.mdeez AGENTS.md compatibility watcher
Documentation=https://github.com/dutifuldev/claudemdeez

[Service]
Type=simple
ExecStart={exec_args}
Restart=on-failure
RestartSec=10s
Environment={}
Environment={}

[Install]
WantedBy=default.target
"#,
        quote_systemd_arg(&data_env),
        quote_systemd_arg("RUST_LOG=info")
    )
}

fn ensure_service_scan_paths_are_absolute(config: &config::AppConfig) -> Result<()> {
    for path in config
        .scan
        .roots
        .iter()
        .chain(config.scan.include_paths.iter())
        .chain(config.scan.exclude_paths.iter())
    {
        if !config::expand_tilde(path).is_absolute() {
            bail!(
                "service install requires absolute scan paths; run `claudemdeez init <root...>` to store canonical roots"
            );
        }
    }
    Ok(())
}

fn normalize_unit_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("service unit name must not be empty");
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains('\0') {
        bail!("service unit name must not contain path separators");
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '@'))
    {
        bail!("service unit name contains unsupported characters: {trimmed}");
    }
    if trimmed.ends_with(".service") {
        Ok(trimmed.to_string())
    } else {
        Ok(format!("{trimmed}.service"))
    }
}

fn unit_path(unit_name: &str) -> Result<PathBuf> {
    Ok(systemd_user_dir()?.join(unit_name))
}

fn systemd_user_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(absolute_expanded_path(&PathBuf::from(path))?.join("systemd/user"));
    }
    let home = env::var_os("HOME").context("HOME is not set; cannot locate systemd user dir")?;
    Ok(PathBuf::from(home).join(".config/systemd/user"))
}

fn absolute_expanded_path(path: &Path) -> Result<PathBuf> {
    let expanded = config::expand_tilde(path);
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(env::current_dir()?.join(expanded))
    }
}

fn quote_systemd_arg(value: &str) -> String {
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '%' => quoted.push_str("%%"),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

fn is_managed_unit(text: &str) -> bool {
    text.lines().any(|line| line == MANAGED_MARKER)
}

fn ensure_existing_unit_is_managed_or_absent(unit_path: &Path) -> Result<()> {
    match fs::read_to_string(unit_path) {
        Ok(existing) if !is_managed_unit(&existing) => {
            bail!(
                "refusing to overwrite unmanaged systemd user unit {}",
                unit_path.display()
            );
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            bail!(
                "refusing to overwrite unreadable systemd user unit {}: {}",
                unit_path.display(),
                error
            );
        }
    }
}

fn ensure_linux() -> Result<()> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else {
        bail!("service management is only supported on Linux");
    }
}

fn ensure_systemd_user_available() -> Result<()> {
    run_systemctl_checked(&["show-environment"]).map(|_| ())
}

fn run_systemctl_checked(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .with_context(|| format!("failed to run systemctl --user {}", args.join(" ")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "systemctl --user {} failed: {}",
        args.join(" "),
        stderr.trim()
    );
}

fn run_systemctl_status(args: &[&str]) -> Result<i32> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to run systemctl --user {}", args.join(" ")))?;
    Ok(status.code().unwrap_or(1))
}

struct SystemctlOutput {
    exit_code: u8,
    stdout: String,
}

fn systemctl_output_status(args: &[&str]) -> Result<SystemctlOutput> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .with_context(|| format!("failed to run systemctl --user {}", args.join(" ")))?;
    Ok(SystemctlOutput {
        exit_code: output.status.code().unwrap_or(1) as u8,
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
    })
}

fn print_report(json: bool, report: ServiceReport) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "{}: {} ({})",
            report.action,
            report.message,
            report.unit_path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        MANAGED_MARKER, UnitSpec, build_unit, ensure_existing_unit_is_managed_or_absent,
        ensure_service_scan_paths_are_absolute, is_managed_unit, normalize_unit_name,
    };
    use crate::config::AppConfig;

    #[test]
    fn unit_name_is_normalized_and_validated() {
        assert_eq!(
            normalize_unit_name("claudemdeez").unwrap(),
            "claudemdeez.service"
        );
        assert_eq!(
            normalize_unit_name("claudemdeez-smoke.service").unwrap(),
            "claudemdeez-smoke.service"
        );
        assert!(normalize_unit_name("../bad").is_err());
        assert!(normalize_unit_name("bad name").is_err());
    }

    #[test]
    fn generated_unit_runs_watch_with_explicit_config_and_data_dir() {
        let spec = UnitSpec {
            unit_name: "claudemdeez.service".to_string(),
            unit_path: PathBuf::from("/home/user/.config/systemd/user/claudemdeez.service"),
            config_path: PathBuf::from("/home/user/.config/claudemdeez/claudemdeez.toml"),
            bin_path: PathBuf::from("/home/user/.cargo/bin/claudemdeez"),
            data_dir: PathBuf::from("/home/user/.local/share/claudemdeez"),
        };

        let unit = build_unit(&spec);

        assert!(is_managed_unit(&unit));
        assert!(unit.contains("ExecStart=\"/home/user/.cargo/bin/claudemdeez\" \"--config\" \"/home/user/.config/claudemdeez/claudemdeez.toml\" \"watch\""));
        assert!(
            unit.contains(
                "Environment=\"CLAUDEMDEEZ_DATA_DIR=/home/user/.local/share/claudemdeez\""
            )
        );
        assert!(unit.contains("Restart=on-failure"));
    }

    #[test]
    fn generated_unit_escapes_systemd_special_characters() {
        let spec = UnitSpec {
            unit_name: "claudemdeez.service".to_string(),
            unit_path: PathBuf::from("/home/user/.config/systemd/user/claudemdeez.service"),
            config_path: PathBuf::from("/home/user/configs/claude\"mdeez.toml"),
            bin_path: PathBuf::from("/home/user/bin/claude%mdeez"),
            data_dir: PathBuf::from("/home/user/data%dir"),
        };

        let unit = build_unit(&spec);

        assert!(
            unit.contains(
                "ExecStart=\"/home/user/bin/claude%%mdeez\" \"--config\" \"/home/user/configs/claude\\\"mdeez.toml\" \"watch\""
            )
        );
        assert!(unit.contains("Environment=\"CLAUDEMDEEZ_DATA_DIR=/home/user/data%%dir\""));
    }

    #[test]
    fn existing_unit_must_be_readable_and_managed() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing.service");
        let managed = temp.path().join("managed.service");
        let unmanaged = temp.path().join("unmanaged.service");
        let invalid_utf8 = temp.path().join("invalid.service");

        assert!(ensure_existing_unit_is_managed_or_absent(&missing).is_ok());

        std::fs::write(&managed, format!("{MANAGED_MARKER}\n[Service]\n")).unwrap();
        assert!(ensure_existing_unit_is_managed_or_absent(&managed).is_ok());

        std::fs::write(&unmanaged, "[Service]\n").unwrap();
        let error = ensure_existing_unit_is_managed_or_absent(&unmanaged).unwrap_err();
        assert!(error.to_string().contains("unmanaged systemd user unit"));

        std::fs::write(&invalid_utf8, b"\xff").unwrap();
        let error = ensure_existing_unit_is_managed_or_absent(&invalid_utf8).unwrap_err();
        assert!(error.to_string().contains("unreadable systemd user unit"));
    }

    #[test]
    fn service_scan_paths_must_be_absolute_after_tilde_expansion() {
        let mut config = AppConfig::default();
        config.scan.roots = vec![PathBuf::from("/workspace")];
        config.scan.include_paths = vec![PathBuf::from("~/workspace/project")];
        config.scan.exclude_paths = vec![PathBuf::from("/workspace/archive")];
        assert!(ensure_service_scan_paths_are_absolute(&config).is_ok());

        config.scan.roots = vec![PathBuf::from(".")];
        let error = ensure_service_scan_paths_are_absolute(&config).unwrap_err();
        assert!(error.to_string().contains("absolute scan paths"));
    }
}
