use std::{
    env, fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    cli::{InstallArgs, UninstallArgs},
    config,
    observe::{self, ObserveReport},
    state::{State, StateCounts},
};

const HOOK_MARKER: &str = "claude-md-symlinker managed hook v1";
const UNIT_MARKER: &str = "# claude-md-symlinker managed systemd user unit";
const DEFAULT_UNIT_NAME: &str = "claude-md-symlinker.service";
const HOOK_EVENTS: &[&str] = &["SessionStart", "CwdChanged", "UserPromptSubmit"];

#[derive(Debug, Serialize)]
pub struct InstallReport {
    pub hooks: ActionReport,
    pub service: ActionReport,
    pub auto_migrate: bool,
    pub observed: Option<ObserveReport>,
    pub dry_run: bool,
}

#[derive(Debug, Serialize)]
pub struct UninstallReport {
    pub hooks: ActionReport,
    pub service: ActionReport,
    pub purged: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionReport {
    pub changed: bool,
    pub path: Option<PathBuf>,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub hooks_installed: bool,
    pub hook_binary: Option<PathBuf>,
    pub service_installed: bool,
    pub service_active: Option<bool>,
    pub unit_path: PathBuf,
    pub state: StateCounts,
    pub auto_migrate: bool,
}

pub fn install(args: &InstallArgs, state: &State, dry_run: bool, json: bool) -> Result<u8> {
    let bin = args
        .bin
        .clone()
        .map(|path| absolute_path(&path))
        .transpose()?
        .unwrap_or(env::current_exe().context("failed to resolve current executable")?);
    validate_binary(&bin)?;

    let auto_migrate = resolve_auto_migrate_choice(args, json)?;
    if !dry_run {
        state.set_setting("auto_migrate", if auto_migrate { "true" } else { "false" })?;
    }

    let hooks = if args.no_hooks {
        ActionReport {
            changed: false,
            path: Some(claude_settings_path()?),
            message: "skipped Claude hooks".to_string(),
        }
    } else {
        install_hooks(&bin, dry_run)?
    };
    let service = if args.no_service {
        ActionReport {
            changed: false,
            path: Some(unit_path(unit_name(args.unit_name.as_deref())?)?),
            message: "skipped user service".to_string(),
        }
    } else {
        install_service(&bin, args.unit_name.as_deref(), dry_run)?
    };

    let observed = observe::observe(
        &crate::cli::ObserveArgs {
            no_apply: false,
            strict: false,
            cwd: Some(env::current_dir().context("failed to read current directory")?),
        },
        state,
        dry_run,
    )
    .ok();

    let report = InstallReport {
        hooks,
        service,
        auto_migrate,
        observed,
        dry_run,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_install(&report);
    }
    Ok(0)
}

pub fn uninstall(args: &UninstallArgs, dry_run: bool, json: bool) -> Result<UninstallReport> {
    let report = uninstall_report(args, dry_run)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_uninstall(&report);
    }
    Ok(report)
}

pub fn uninstall_report(args: &UninstallArgs, dry_run: bool) -> Result<UninstallReport> {
    let hooks = if args.no_hooks {
        ActionReport {
            changed: false,
            path: Some(claude_settings_path()?),
            message: "skipped Claude hooks".to_string(),
        }
    } else {
        uninstall_hooks(dry_run)?
    };
    let service = if args.no_service {
        ActionReport {
            changed: false,
            path: Some(unit_path(unit_name(args.unit_name.as_deref())?)?),
            message: "skipped user service".to_string(),
        }
    } else {
        uninstall_service(args.unit_name.as_deref(), dry_run)?
    };
    let report = UninstallReport {
        hooks,
        service,
        purged: args.purge,
        dry_run,
    };
    Ok(report)
}

pub fn status(state: &State, json: bool) -> Result<u8> {
    let hooks = hooks_status()?;
    let service = service_status(None)?;
    let counts = state.counts()?;
    let auto_migrate = state.setting_bool("auto_migrate", false)?;
    let report = StatusReport {
        hooks_installed: hooks.0,
        hook_binary: hooks.1,
        service_installed: service.installed,
        service_active: service.active,
        unit_path: service.unit_path,
        state: counts,
        auto_migrate,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_status(&report);
    }
    Ok(if report.hooks_installed && report.service_installed {
        0
    } else {
        3
    })
}

pub fn install_hooks(bin: &Path, dry_run: bool) -> Result<ActionReport> {
    let path = claude_settings_path()?;
    let mut settings = read_settings(&path)?;
    let command = hook_command(bin);
    let before = settings.clone();
    remove_managed_hooks(&mut settings);
    add_managed_hooks(&mut settings, &command);
    let changed = settings != before;

    if dry_run {
        return Ok(ActionReport {
            changed,
            path: Some(path),
            message: if changed {
                "would install Claude hooks".to_string()
            } else {
                "Claude hooks already installed".to_string()
            },
        });
    }

    if changed {
        backup_file(&path)?;
        write_settings(&path, &settings)?;
    }
    Ok(ActionReport {
        changed,
        path: Some(path),
        message: if changed {
            "installed Claude hooks".to_string()
        } else {
            "Claude hooks already installed".to_string()
        },
    })
}

pub fn uninstall_hooks(dry_run: bool) -> Result<ActionReport> {
    let path = claude_settings_path()?;
    let mut settings = read_settings(&path)?;
    let before = settings.clone();
    remove_managed_hooks(&mut settings);
    let changed = settings != before;

    if dry_run {
        return Ok(ActionReport {
            changed,
            path: Some(path),
            message: if changed {
                "would remove Claude hooks".to_string()
            } else {
                "Claude hooks are not installed".to_string()
            },
        });
    }
    if changed {
        backup_file(&path)?;
        write_settings(&path, &settings)?;
    }
    Ok(ActionReport {
        changed,
        path: Some(path),
        message: if changed {
            "removed Claude hooks".to_string()
        } else {
            "Claude hooks are not installed".to_string()
        },
    })
}

fn add_managed_hooks(settings: &mut Value, command: &str) {
    let root = ensure_object(settings);
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks_object = ensure_object(hooks);
    for event in HOOK_EVENTS {
        let event_hooks = hooks_object.entry(*event).or_insert_with(|| json!([]));
        let Some(array) = event_hooks.as_array_mut() else {
            *event_hooks = json!([]);
            let array = event_hooks.as_array_mut().expect("just wrote array");
            array.push(hook_entry(command));
            continue;
        };
        if !array
            .iter()
            .any(|entry| entry_contains_command(entry, command))
        {
            array.push(hook_entry(command));
        }
    }
}

fn remove_managed_hooks(settings: &mut Value) {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for value in hooks.values_mut() {
        let Some(entries) = value.as_array_mut() else {
            continue;
        };
        for entry in entries.iter_mut() {
            if let Some(hook_array) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                hook_array.retain(|hook| !hook_is_managed(hook));
            }
        }
        entries.retain(|entry| {
            entry
                .get("hooks")
                .and_then(Value::as_array)
                .map(|hooks| !hooks.is_empty())
                .unwrap_or(true)
        });
    }
}

fn hook_entry(command: &str) -> Value {
    json!({
        "matcher": "*",
        "hooks": [
            {
                "type": "command",
                "command": command,
                "timeout": 30
            }
        ]
    })
}

fn hook_is_managed(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .map(|command| command.contains(HOOK_MARKER))
        .unwrap_or(false)
}

fn entry_contains_command(entry: &Value, command: &str) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .map(|existing| existing == command)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn hook_command(bin: &Path) -> String {
    let bin = shell_quote(&bin.to_string_lossy());
    format!(
        "log_dir=\"${{HOME:-/tmp}}/.local/state/claude-md-symlinker\"; mkdir -p \"$log_dir\" >/dev/null 2>&1 || log_dir=\"/tmp\"; {bin} observe >/dev/null 2>>\"$log_dir/hooks.log\" || true # {HOOK_MARKER}"
    )
}

pub fn install_service(
    bin: &Path,
    unit_name_override: Option<&str>,
    dry_run: bool,
) -> Result<ActionReport> {
    ensure_linux()?;
    let unit_name = unit_name(unit_name_override)?;
    let path = unit_path(unit_name.clone())?;
    let unit = service_unit(bin)?;
    ensure_existing_unit_is_managed_or_absent(&path)?;
    let changed = fs::read_to_string(&path)
        .map(|current| current != unit)
        .unwrap_or(true);

    if dry_run {
        return Ok(ActionReport {
            changed,
            path: Some(path),
            message: if changed {
                "would install and start user service".to_string()
            } else {
                "user service already installed".to_string()
            },
        });
    }

    if changed {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&path, unit).with_context(|| format!("failed to write {}", path.display()))?;
    }
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", &unit_name])?;
    systemctl(&["restart", &unit_name])?;
    Ok(ActionReport {
        changed,
        path: Some(path),
        message: if changed {
            "installed and started user service".to_string()
        } else {
            "user service already installed and restarted".to_string()
        },
    })
}

pub fn uninstall_service(unit_name_override: Option<&str>, dry_run: bool) -> Result<ActionReport> {
    ensure_linux()?;
    let unit_name = unit_name(unit_name_override)?;
    let path = unit_path(unit_name.clone())?;
    let exists = fs::symlink_metadata(&path).is_ok();
    if exists {
        ensure_existing_unit_is_managed_or_absent(&path)?;
    }
    if dry_run {
        return Ok(ActionReport {
            changed: exists,
            path: Some(path),
            message: if exists {
                "would remove user service".to_string()
            } else {
                "user service is not installed".to_string()
            },
        });
    }
    if exists {
        let _ = systemctl(&["disable", "--now", &unit_name]);
        fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
        systemctl(&["daemon-reload"])?;
        let _ = systemctl(&["reset-failed", &unit_name]);
    }
    Ok(ActionReport {
        changed: exists,
        path: Some(path),
        message: if exists {
            "removed user service".to_string()
        } else {
            "user service is not installed".to_string()
        },
    })
}

struct ServiceStatus {
    installed: bool,
    active: Option<bool>,
    unit_path: PathBuf,
}

fn service_status(unit_name_override: Option<&str>) -> Result<ServiceStatus> {
    let unit_name = unit_name(unit_name_override)?;
    let path = unit_path(unit_name.clone())?;
    let installed = path.exists() && existing_unit_is_managed(&path)?;
    let active = if installed {
        systemctl_output(&["is-active", &unit_name])
            .ok()
            .map(|output| output.trim() == "active")
    } else {
        None
    };
    Ok(ServiceStatus {
        installed,
        active,
        unit_path: path,
    })
}

fn service_unit(bin: &Path) -> Result<String> {
    let data_dir = config::data_dir()?;
    Ok(format!(
        r#"{UNIT_MARKER}
[Unit]
Description=claude-md-symlinker AGENTS.md compatibility repair daemon
Documentation=https://github.com/dutifuldev/claude-md-symlinker

[Service]
Type=exec
ExecStart={} daemon
Restart=on-failure
RestartSec=10s
Environment={}
Environment="RUST_LOG=info"

[Install]
WantedBy=default.target
"#,
        quote_systemd(&bin.to_string_lossy()),
        quote_systemd(&format!(
            "CLAUDE_MD_SYMLINKER_DATA_DIR={}",
            data_dir.display()
        )),
    ))
}

fn hooks_status() -> Result<(bool, Option<PathBuf>)> {
    let path = claude_settings_path()?;
    let settings = read_settings(&path)?;
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return Ok((false, None));
    };
    let mut installed = true;
    let mut binary = None;
    for event in HOOK_EVENTS {
        let event_has_hook = hooks
            .get(*event)
            .and_then(Value::as_array)
            .map(|entries| {
                entries.iter().any(|entry| {
                    entry
                        .get("hooks")
                        .and_then(Value::as_array)
                        .map(|hooks| {
                            hooks.iter().any(|hook| {
                                hook.get("command")
                                    .and_then(Value::as_str)
                                    .map(|command| {
                                        if command.contains(HOOK_MARKER) && binary.is_none() {
                                            binary = command_binary(command);
                                        }
                                        command.contains(HOOK_MARKER)
                                    })
                                    .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        installed &= event_has_hook;
    }
    Ok((installed, binary))
}

fn command_binary(command: &str) -> Option<PathBuf> {
    let trimmed = command.trim_start();
    if let Some(rest) = trimmed.strip_prefix('\'') {
        let end = rest.find('\'')?;
        return Some(PathBuf::from(&rest[..end]));
    }
    if let Some(path) = first_single_quoted_binary(trimmed) {
        return Some(path);
    }
    trimmed
        .split_whitespace()
        .next()
        .map(|path| PathBuf::from(path.trim_matches('"')))
}

fn first_single_quoted_binary(command: &str) -> Option<PathBuf> {
    let mut rest = command;
    while let Some(start) = rest.find('\'') {
        let after_start = &rest[start + 1..];
        let end = after_start.find('\'')?;
        let candidate = &after_start[..end];
        if candidate.contains("claude-md-symlinker") {
            return Some(PathBuf::from(candidate));
        }
        rest = &after_start[end + 1..];
    }
    None
}

fn resolve_auto_migrate_choice(args: &InstallArgs, json: bool) -> Result<bool> {
    if args.auto_migrate {
        return Ok(true);
    }
    if args.no_auto_migrate {
        return Ok(false);
    }
    if !io::stdin().is_terminal() {
        return Ok(true);
    }
    if !json {
        println!(
            "Automatically migrate safe existing CLAUDE.md files to AGENTS.md when Claude finds them while working through directories?"
        );
        println!();
        println!(
            "This does not scan your whole machine or whole repos. It only applies to CLAUDE.md files found in directories Claude actually enters, and only when the migration passes the safe checks."
        );
        println!();
        print!("Default: yes [Y/n] ");
        io::stdout().flush()?;
    }
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(!matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "n" | "no"
    ))
}

fn read_settings(path: &Path) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("failed to parse Claude settings {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => {
            Err(error).with_context(|| format!("failed to read Claude settings {}", path.display()))
        }
    }
}

fn write_settings(path: &Path, settings: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(settings)?;
    fs::write(path, format!("{text}\n"))
        .with_context(|| format!("failed to write {}", path.display()))
}

fn backup_file(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let backup = path.with_extension(format!("json.backup.{suffix}"));
    fs::copy(path, &backup).with_context(|| {
        format!(
            "failed to back up {} to {}",
            path.display(),
            backup.display()
        )
    })?;
    Ok(Some(backup))
}

fn claude_settings_path() -> Result<PathBuf> {
    if let Ok(path) = env::var("CLAUDE_MD_SYMLINKER_CLAUDE_SETTINGS") {
        return absolute_path(Path::new(&path));
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".claude/settings.json"))
}

fn ensure_object(value: &mut Value) -> &mut serde_json::Map<String, Value> {
    if !value.is_object() {
        *value = json!({});
    }
    value.as_object_mut().expect("just wrote object")
}

fn unit_name(override_name: Option<&str>) -> Result<String> {
    let name = override_name.unwrap_or(DEFAULT_UNIT_NAME).trim();
    if name.is_empty() || name.starts_with('-') || name.contains('/') || name.contains('\\') {
        bail!("invalid systemd unit name");
    }
    Ok(if name.ends_with(".service") {
        name.to_string()
    } else {
        format!("{name}.service")
    })
}

fn unit_path(unit_name: String) -> Result<PathBuf> {
    Ok(systemd_user_dir()?.join(unit_name))
}

fn systemd_user_dir() -> Result<PathBuf> {
    if let Ok(path) = env::var("CLAUDE_MD_SYMLINKER_SYSTEMD_USER_DIR") {
        return absolute_path(Path::new(&path));
    }
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME")
        && !config_home.is_empty()
    {
        return Ok(PathBuf::from(config_home).join("systemd/user"));
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/systemd/user"))
}

fn ensure_existing_unit_is_managed_or_absent(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to modify symlinked systemd unit {}",
            path.display()
        );
    }
    if metadata.is_file() && existing_unit_is_managed(path)? {
        return Ok(());
    }
    bail!(
        "refusing to modify unmanaged systemd unit {}",
        path.display()
    )
}

fn existing_unit_is_managed(path: &Path) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(false);
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(text.lines().any(|line| line == UNIT_MARKER))
}

fn systemctl(args: &[&str]) -> Result<()> {
    let output = systemctl_command(args)
        .output()
        .context("failed to run systemctl")?;
    if !output.status.success() {
        bail!(
            "systemctl --user {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn systemctl_output(args: &[&str]) -> Result<String> {
    let output = systemctl_command(args)
        .stdout(Stdio::piped())
        .output()
        .context("failed to run systemctl")?;
    if !output.status.success() {
        bail!("systemctl --user {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn systemctl_command(args: &[&str]) -> Command {
    let mut command = Command::new(
        env::var("CLAUDE_MD_SYMLINKER_SYSTEMCTL").unwrap_or_else(|_| "systemctl".to_string()),
    );
    command.arg("--user").args(args);
    command
}

fn validate_binary(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to inspect binary {}", path.display()))?;
    if !metadata.is_file() {
        bail!("binary path is not a file: {}", path.display());
    }
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    let expanded = config::expand_tilde(path);
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(env::current_dir()?.join(expanded))
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn quote_systemd(value: &str) -> String {
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '%' => quoted.push_str("%%"),
            '$' => quoted.push_str("$$"),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

fn ensure_linux() -> Result<()> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else {
        bail!("systemd user service installation is only supported on Linux")
    }
}

fn print_install(report: &InstallReport) {
    if report.dry_run {
        println!("Dry run. No filesystem changes were made.");
    }
    println!("{}", report.hooks.message);
    println!("{}", report.service.message);
    println!(
        "Auto migrate: {}.",
        if report.auto_migrate {
            "enabled"
        } else {
            "disabled"
        }
    );
    if let Some(observed) = &report.observed {
        println!(
            "Observed {} instruction directories.",
            observed.instruction_dirs.len()
        );
        println!("Created {} shims.", observed.created);
        println!(
            "Detected {} CLAUDE.md files.",
            observed.detected_claude_files.len()
        );
    }
}

pub fn print_uninstall(report: &UninstallReport) {
    if report.dry_run {
        println!("Dry run. No filesystem changes were made.");
    }
    println!("{}", report.hooks.message);
    println!("{}", report.service.message);
}

fn print_status(report: &StatusReport) {
    println!(
        "Hooks: {}",
        if report.hooks_installed {
            "installed"
        } else {
            "not installed"
        }
    );
    println!(
        "Service: {}",
        if !report.service_installed {
            "not installed".to_string()
        } else if report.service_active == Some(true) {
            "active".to_string()
        } else if report.service_active == Some(false) {
            "inactive".to_string()
        } else {
            "installed".to_string()
        }
    );
    println!("Observed repos: {}", report.state.observed_repos);
    println!("Instruction dirs: {}", report.state.instruction_dirs);
    println!(
        "Migration candidates: {}",
        report.state.migration_candidates
    );
    println!(
        "Auto migrate: {}",
        if report.auto_migrate { "on" } else { "off" }
    );
    println!(
        "Last repair: {}",
        report
            .state
            .last_reconciled_at
            .as_deref()
            .unwrap_or("never")
    );
    println!("Recent errors: {}", report.state.recent_errors);
}
