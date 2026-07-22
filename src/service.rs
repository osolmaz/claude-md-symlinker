use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[cfg(unix)]
use std::{ffi::CString, os::unix::ffi::OsStrExt};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::{
    adapters,
    cli::{ServiceCommand, ServiceInstallArgs, ServiceUnitArgs},
    config::{self, LoadedConfig},
    exclude,
};

const MANAGED_MARKER: &str = "# claude-md-symlinker managed systemd user unit";

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
    ensure_watch_enabled(&loaded.config)?;
    exclude::validate_mode(loaded.config.git.exclude_mode)
        .context("service install requires valid Git exclude mode")?;
    adapters::enabled_adapters(&loaded.config)
        .context("service install requires valid adapters")?;
    loaded
        .config
        .scan_scope(loaded.existed, &[])
        .context("service install requires configured scan roots")?;

    let spec = install_spec(args, loaded)?;
    ensure_existing_unit_is_managed_or_absent(&spec.unit_path)?;
    ensure_unit_name_is_available(&spec, false)?;
    validate_unit_path_writable(&spec.unit_path)?;

    if dry_run {
        ensure_service_data_dir_ready(&spec.data_dir, true)?;
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
    ensure_unit_name_is_available(&spec, true)?;
    validate_unit_path_writable(&spec.unit_path)?;
    ensure_service_data_dir_ready(&spec.data_dir, false)?;

    if let Some(parent) = spec.unit_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create systemd user dir {}", parent.display()))?;
    }
    let unit = build_unit(&spec);
    fs::write(&spec.unit_path, unit)
        .with_context(|| format!("failed to write {}", spec.unit_path.display()))?;

    run_systemctl_checked(&["daemon-reload"])?;
    if !args.no_enable {
        run_systemctl_checked(&["enable", &spec.unit_name])?;
    }
    if args.now {
        run_systemctl_checked(&["restart", &spec.unit_name])?;
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
    let existing = existing_managed_unit(&unit_path)?;

    if dry_run {
        if existing.is_some() {
            ensure_manager_resolves_to_managed_unit(&unit_name, &unit_path, false)?;
        }
        print_report(
            json,
            ServiceReport {
                action: "uninstall".to_string(),
                unit_name,
                unit_path,
                dry_run,
                message: if existing.is_some() {
                    "would remove managed systemd user unit".to_string()
                } else {
                    "systemd user unit is not installed".to_string()
                },
            },
        )?;
        return Ok(0);
    }

    if existing.is_none() {
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

    ensure_systemd_user_available()?;
    run_systemctl_checked(&["daemon-reload"])?;
    ensure_manager_resolves_to_managed_unit(&unit_name, &unit_path, true)?;
    run_systemctl_checked(&["disable", "--now", &unit_name])?;
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
    ensure_managed_unit_installed(&unit_path)?;
    ensure_manager_resolves_to_managed_unit(&unit_name, &unit_path, false)?;
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
    run_systemctl_checked(&["daemon-reload"])?;
    ensure_manager_resolves_to_managed_unit(&unit_name, &unit_path, true)?;
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

    let spec = UnitSpec {
        unit_path: unit_path(&unit_name)?,
        unit_name,
        config_path: loaded.path.clone(),
        bin_path,
        data_dir,
    };
    validate_unit_spec(&spec)?;
    Ok(spec)
}

fn build_unit(spec: &UnitSpec) -> String {
    let exec_args = [
        quote_systemd_exec_command(&spec.bin_path.to_string_lossy()),
        quote_systemd_exec_arg("--config"),
        quote_systemd_exec_arg(&spec.config_path.to_string_lossy()),
        quote_systemd_exec_arg("watch"),
    ]
    .iter()
    .map(String::as_str)
    .collect::<Vec<_>>()
    .join(" ");
    let data_env = format!("CLAUDE_MD_SYMLINKER_DATA_DIR={}", spec.data_dir.display());

    format!(
        r#"{MANAGED_MARKER}
[Unit]
Description=claude-md-symlinker AGENTS.md compatibility watcher
Documentation=https://github.com/osolmaz/claude-md-symlinker

[Service]
Type=exec
ExecStart={exec_args}
Restart=on-failure
RestartSec=10s
Environment={}
Environment={}

[Install]
WantedBy=default.target
"#,
        quote_systemd_env_value(&data_env),
        quote_systemd_env_value("RUST_LOG=info")
    )
}

fn ensure_watch_enabled(config: &config::AppConfig) -> Result<()> {
    if !config.watch.enabled {
        bail!("service install requires watch to be enabled; set `watch.enabled = true`");
    }
    Ok(())
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
                "service install requires absolute scan paths; run `claude-md-symlinker init <root...>` to store canonical roots"
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
    if trimmed.starts_with('-') {
        bail!("service unit name must not start with '-'");
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
    let stem = trimmed.strip_suffix(".service").unwrap_or(trimmed);
    if !stem.chars().any(|ch| ch.is_ascii_alphanumeric()) {
        bail!("service unit name must include a name before `.service`");
    }
    let template_parts = stem.matches('@').count();
    if stem.starts_with('@') || stem.ends_with('@') || template_parts > 1 {
        bail!(
            "service unit name must be a plain service name or an instantiated name like `name@instance.service`"
        );
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
    if let Some(path) = manager_user_unit_dir()? {
        return Ok(path);
    }

    Ok(process_user_config_dir()?.join("systemd/user"))
}

fn manager_user_unit_dir() -> Result<Option<PathBuf>> {
    let output = match Command::new("systemctl")
        .arg("--user")
        .arg("show")
        .arg("--property=UnitPath")
        .arg("--value")
        .output()
    {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };
    if !output.status.success() {
        return Ok(None);
    }

    let text = String::from_utf8_lossy(&output.stdout);
    for token in systemd_show_list_tokens(&text)? {
        let path = PathBuf::from(token);
        if path.is_absolute() && path.ends_with("systemd/user") {
            return Ok(Some(path));
        }
    }

    Ok(None)
}

fn systemd_show_list_tokens(value: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    for token in value.split_whitespace() {
        let decoded = decode_systemd_show_token(token)?;
        if !decoded.is_empty() {
            tokens.push(decoded);
        }
    }
    Ok(tokens)
}

fn decode_systemd_show_token(value: &str) -> Result<String> {
    decode_ansi_c_quoted(value)
}

fn decode_ansi_c_quoted(value: &str) -> Result<String> {
    let mut decoded = Vec::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            let mut buffer = [0; 4];
            decoded.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
            continue;
        }

        let Some(escaped) = chars.next() else {
            decoded.push(b'\\');
            break;
        };
        match escaped {
            'a' => decoded.push(0x07),
            'b' => decoded.push(0x08),
            'f' => decoded.push(0x0c),
            'n' => decoded.push(b'\n'),
            'r' => decoded.push(b'\r'),
            't' => decoded.push(b'\t'),
            'v' => decoded.push(0x0b),
            '\\' => decoded.push(b'\\'),
            '\'' => decoded.push(b'\''),
            '"' => decoded.push(b'"'),
            'x' => {
                let mut hex = String::new();
                for _ in 0..2 {
                    if chars.peek().is_some_and(|ch| ch.is_ascii_hexdigit()) {
                        hex.push(chars.next().expect("peeked hex digit"));
                    }
                }
                if hex.is_empty() {
                    decoded.push(b'x');
                } else {
                    let value = u8::from_str_radix(&hex, 16)
                        .context("failed to decode systemd hex escape")?;
                    decoded.push(value);
                }
            }
            '0'..='7' => {
                let mut octal = String::from(escaped);
                for _ in 0..2 {
                    if chars.peek().is_some_and(|ch| matches!(ch, '0'..='7')) {
                        octal.push(chars.next().expect("peeked octal digit"));
                    }
                }
                let value = u8::from_str_radix(&octal, 8)
                    .context("failed to decode systemd octal escape")?;
                decoded.push(value);
            }
            other => {
                let mut buffer = [0; 4];
                decoded.extend_from_slice(other.encode_utf8(&mut buffer).as_bytes());
            }
        }
    }
    String::from_utf8(decoded).context("failed to decode systemd escaped text as UTF-8")
}

fn process_user_config_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME")
        && !path.as_os_str().is_empty()
    {
        return absolute_expanded_path(&PathBuf::from(path));
    }
    let home = env::var_os("HOME").context("HOME is not set; cannot locate systemd user dir")?;
    if home.as_os_str().is_empty() {
        bail!("HOME is empty; cannot locate systemd user dir");
    }
    Ok(PathBuf::from(home).join(".config"))
}

fn absolute_expanded_path(path: &Path) -> Result<PathBuf> {
    let expanded = config::expand_tilde(path);
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(env::current_dir()?.join(expanded))
    }
}

fn quote_systemd_exec_arg(value: &str) -> String {
    quote_systemd_value(value, true)
}

fn quote_systemd_exec_command(value: &str) -> String {
    quote_systemd_value(value, false)
}

fn quote_systemd_env_value(value: &str) -> String {
    quote_systemd_value(value, false)
}

fn validate_unit_spec(spec: &UnitSpec) -> Result<()> {
    validate_systemd_path("unit file", &spec.unit_path)?;
    validate_systemd_command_path(&spec.bin_path)?;
    validate_systemd_path("config", &spec.config_path)?;
    validate_systemd_path("data directory", &spec.data_dir)?;
    Ok(())
}

fn validate_systemd_command_path(path: &Path) -> Result<()> {
    validate_systemd_path("binary", path)?;
    let text = path.to_str().expect("validated UTF-8 path");
    if text.chars().any(|ch| matches!(ch, '\'' | '"' | '\\')) {
        bail!(
            "service binary path contains characters systemd cannot use in ExecStart: {}",
            path.display()
        );
    }

    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to inspect service binary {}", path.display()))?;
    if !metadata.is_file() {
        bail!("service binary path is not a file: {}", path.display());
    }
    if !current_user_can_execute(path)? {
        bail!("service binary path is not executable: {}", path.display());
    }
    Ok(())
}

fn validate_systemd_path(label: &str, path: &Path) -> Result<()> {
    let Some(text) = path.to_str() else {
        bail!("service {label} path must be valid UTF-8");
    };
    if text.chars().any(char::is_control) {
        bail!("service {label} path must not contain control characters");
    }
    Ok(())
}

fn quote_systemd_value(value: &str, escape_dollar: bool) -> String {
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '%' => quoted.push_str("%%"),
            '$' if escape_dollar => quoted.push_str("$$"),
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
    existing_managed_unit(unit_path).map(|_| ())
}

fn ensure_managed_unit_installed(unit_path: &Path) -> Result<()> {
    if existing_managed_unit(unit_path)?.is_some() {
        return Ok(());
    }
    bail!(
        "managed systemd user unit is not installed; run `claude-md-symlinker service install` first"
    );
}

fn ensure_unit_name_is_available(spec: &UnitSpec, require_systemd: bool) -> Result<()> {
    let Some(fragment_path) = manager_unit_fragment_path(&spec.unit_name, require_systemd)? else {
        return Ok(());
    };
    if paths_match(&fragment_path, &spec.unit_path) {
        return Ok(());
    }
    bail!(
        "refusing to shadow existing systemd user unit {} at {}",
        spec.unit_name,
        fragment_path.display()
    );
}

fn ensure_manager_resolves_to_managed_unit(
    unit_name: &str,
    unit_path: &Path,
    require_systemd: bool,
) -> Result<()> {
    let Some(fragment_path) = manager_unit_fragment_path(unit_name, require_systemd)? else {
        if require_systemd {
            bail!("systemd user manager does not resolve managed unit {unit_name}");
        }
        return Ok(());
    };
    if paths_match(&fragment_path, unit_path) {
        return Ok(());
    }
    bail!(
        "refusing to control systemd user unit {unit_name}; manager resolves it to {} instead of managed unit {}",
        fragment_path.display(),
        unit_path.display()
    );
}

fn manager_unit_fragment_path(unit_name: &str, require_systemd: bool) -> Result<Option<PathBuf>> {
    let output = match Command::new("systemctl")
        .arg("--user")
        .arg("show")
        .arg(unit_name)
        .arg("--property=LoadState")
        .arg("--property=FragmentPath")
        .arg("--value")
        .output()
    {
        Ok(output) => output,
        Err(_error) if !require_systemd => return Ok(None),
        Err(error) => {
            bail!("failed to query systemd user unit {unit_name}: {error}");
        }
    };
    if !output.status.success() {
        if require_systemd {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "systemctl --user show {unit_name} failed: {}",
                stderr.trim()
            );
        }
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let load_state = lines.next().unwrap_or_default().trim();
    let fragment_path = lines.next().unwrap_or_default().trim();
    if load_state.is_empty() || load_state == "not-found" {
        return Ok(None);
    }
    if fragment_path.is_empty() {
        bail!(
            "refusing to shadow existing systemd user unit {unit_name} with load state {load_state}"
        );
    }
    Ok(Some(PathBuf::from(fragment_path)))
}

fn existing_managed_unit(unit_path: &Path) -> Result<Option<String>> {
    let metadata = match fs::symlink_metadata(unit_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            bail!(
                "refusing to inspect systemd user unit {}: {}",
                unit_path.display(),
                error
            );
        }
    };

    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!(
            "refusing to use symlinked systemd user unit {}",
            unit_path.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "refusing to use non-file systemd user unit {}",
            unit_path.display()
        );
    }

    let existing = fs::read_to_string(unit_path).map_err(|error| {
        anyhow::anyhow!(
            "refusing to read systemd user unit {}: {}",
            unit_path.display(),
            error
        )
    })?;
    if !is_managed_unit(&existing) {
        bail!(
            "refusing to use unmanaged systemd user unit {}",
            unit_path.display()
        );
    }
    Ok(Some(existing))
}

fn validate_unit_path_writable(unit_path: &Path) -> Result<()> {
    match fs::symlink_metadata(unit_path) {
        Ok(metadata) => {
            if metadata.permissions().readonly() || !current_user_can_write(unit_path)? {
                bail!("systemd user unit {} is not writable", unit_path.display());
            }
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            bail!(
                "failed to inspect systemd user unit {}: {}",
                unit_path.display(),
                error
            );
        }
    }

    let parent = unit_path
        .parent()
        .with_context(|| format!("systemd user unit {} has no parent", unit_path.display()))?;
    let existing_parent = nearest_existing_ancestor(parent).with_context(|| {
        format!(
            "systemd user unit parent {} has no existing ancestor",
            parent.display()
        )
    })?;
    let metadata = fs::metadata(existing_parent).with_context(|| {
        format!(
            "failed to inspect systemd user unit parent {}",
            existing_parent.display()
        )
    })?;
    if !metadata.is_dir() {
        bail!(
            "systemd user unit parent {} is not a directory",
            existing_parent.display()
        );
    }
    if metadata.permissions().readonly() || !current_user_can_write_directory(existing_parent)? {
        bail!(
            "systemd user unit parent {} is not writable",
            existing_parent.display()
        );
    }
    Ok(())
}

fn ensure_service_data_dir_ready(data_dir: &Path, dry_run: bool) -> Result<()> {
    if dry_run {
        return validate_service_data_dir_writable(data_dir);
    }

    fs::create_dir_all(data_dir).with_context(|| {
        format!(
            "failed to create service data directory {}",
            data_dir.display()
        )
    })?;
    validate_existing_service_data_dir(data_dir)
}

fn validate_service_data_dir_writable(data_dir: &Path) -> Result<()> {
    match fs::metadata(data_dir) {
        Ok(_) => validate_existing_service_data_dir(data_dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            validate_missing_service_data_dir(data_dir)
        }
        Err(error) => {
            bail!(
                "failed to inspect service data directory {}: {}",
                data_dir.display(),
                error
            );
        }
    }
}

fn validate_existing_service_data_dir(data_dir: &Path) -> Result<()> {
    let metadata = fs::metadata(data_dir).with_context(|| {
        format!(
            "failed to inspect service data directory {}",
            data_dir.display()
        )
    })?;
    if !metadata.is_dir() {
        bail!(
            "service data directory {} is not a directory",
            data_dir.display()
        );
    }
    if metadata.permissions().readonly() || !current_user_can_write_directory(data_dir)? {
        bail!(
            "service data directory {} is not writable",
            data_dir.display()
        );
    }
    Ok(())
}

fn validate_missing_service_data_dir(data_dir: &Path) -> Result<()> {
    let parent = data_dir.parent().with_context(|| {
        format!(
            "service data directory {} has no parent",
            data_dir.display()
        )
    })?;
    let existing_parent = nearest_existing_ancestor(parent).with_context(|| {
        format!(
            "service data directory parent {} has no existing ancestor",
            parent.display()
        )
    })?;
    let metadata = fs::metadata(existing_parent).with_context(|| {
        format!(
            "failed to inspect service data directory parent {}",
            existing_parent.display()
        )
    })?;
    if !metadata.is_dir() {
        bail!(
            "service data directory parent {} is not a directory",
            existing_parent.display()
        );
    }
    if metadata.permissions().readonly() || !current_user_can_write_directory(existing_parent)? {
        bail!(
            "service data directory parent {} is not writable",
            existing_parent.display()
        );
    }
    Ok(())
}

fn current_user_can_write(path: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        const W_OK: i32 = 2;
        current_user_can_access(path, W_OK)
    }

    #[cfg(not(unix))]
    {
        Ok(!fs::metadata(path)?.permissions().readonly())
    }
}

fn current_user_can_execute(path: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        const X_OK: i32 = 1;
        current_user_can_access(path, X_OK)
    }

    #[cfg(not(unix))]
    {
        Ok(fs::metadata(path)?.is_file())
    }
}

fn current_user_can_write_directory(path: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        const W_OK: i32 = 2;
        const X_OK: i32 = 1;
        current_user_can_access(path, W_OK | X_OK)
    }

    #[cfg(not(unix))]
    {
        Ok(!fs::metadata(path)?.permissions().readonly())
    }
}

#[cfg(unix)]
fn current_user_can_access(path: &Path, mode: i32) -> Result<bool> {
    unsafe extern "C" {
        fn access(pathname: *const std::os::raw::c_char, mode: i32) -> i32;
    }

    let path = CString::new(path.as_os_str().as_bytes()).with_context(|| {
        format!(
            "systemd user unit path {} contains an interior NUL",
            path.display()
        )
    })?;
    Ok(unsafe { access(path.as_ptr(), mode) == 0 })
}

fn paths_match(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }

    let (Ok(left), Ok(right)) = (left.canonicalize(), right.canonicalize()) else {
        return false;
    };
    left == right
}

fn nearest_existing_ancestor(mut path: &Path) -> Option<&Path> {
    loop {
        if path.exists() {
            return Some(path);
        }
        path = path.parent()?;
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
        MANAGED_MARKER, UnitSpec, build_unit, current_user_can_write_directory,
        ensure_existing_unit_is_managed_or_absent, ensure_service_scan_paths_are_absolute,
        is_managed_unit, normalize_unit_name, systemd_show_list_tokens,
    };
    use crate::config::AppConfig;

    #[test]
    fn unit_name_is_normalized_and_validated() {
        assert_eq!(
            normalize_unit_name("claude-md-symlinker").unwrap(),
            "claude-md-symlinker.service"
        );
        assert_eq!(
            normalize_unit_name("claude-md-symlinker-smoke.service").unwrap(),
            "claude-md-symlinker-smoke.service"
        );
        assert!(normalize_unit_name("-bad").is_err());
        assert!(normalize_unit_name("../bad").is_err());
        assert!(normalize_unit_name("bad name").is_err());
        assert!(normalize_unit_name(".service").is_err());
        assert!(normalize_unit_name(".").is_err());
        assert!(normalize_unit_name("@bad").is_err());
        assert!(normalize_unit_name("bad@").is_err());
        assert!(normalize_unit_name("bad@@name").is_err());
        assert_eq!(
            normalize_unit_name("claude-md-symlinker@work").unwrap(),
            "claude-md-symlinker@work.service"
        );
    }

    #[test]
    fn generated_unit_runs_watch_with_explicit_config_and_data_dir() {
        let spec = UnitSpec {
            unit_name: "claude-md-symlinker.service".to_string(),
            unit_path: PathBuf::from("/home/user/.config/systemd/user/claude-md-symlinker.service"),
            config_path: PathBuf::from(
                "/home/user/.config/claude-md-symlinker/claude-md-symlinker.toml",
            ),
            bin_path: PathBuf::from("/home/user/.cargo/bin/claude-md-symlinker"),
            data_dir: PathBuf::from("/home/user/.local/share/claude-md-symlinker"),
        };

        let unit = build_unit(&spec);

        assert!(is_managed_unit(&unit));
        assert!(unit.contains("ExecStart=\"/home/user/.cargo/bin/claude-md-symlinker\" \"--config\" \"/home/user/.config/claude-md-symlinker/claude-md-symlinker.toml\" \"watch\""));
        assert!(
            unit.contains(
                "Environment=\"CLAUDE_MD_SYMLINKER_DATA_DIR=/home/user/.local/share/claude-md-symlinker\""
            )
        );
        assert!(unit.contains("Type=exec"));
        assert!(unit.contains("Restart=on-failure"));
    }

    #[test]
    fn generated_unit_escapes_systemd_special_characters() {
        let spec = UnitSpec {
            unit_name: "claude-md-symlinker.service".to_string(),
            unit_path: PathBuf::from("/home/user/.config/systemd/user/claude-md-symlinker.service"),
            config_path: PathBuf::from("/home/user/configs/claude-md-symlinker\"cfg$cfg.toml"),
            bin_path: PathBuf::from("/home/user/bin/claude-md-symlinker%tool$tool"),
            data_dir: PathBuf::from("/home/user/data%dir$extra"),
        };

        let unit = build_unit(&spec);

        assert!(
            unit.contains(
                "ExecStart=\"/home/user/bin/claude-md-symlinker%%tool$tool\" \"--config\" \"/home/user/configs/claude-md-symlinker\\\"cfg$$cfg.toml\" \"watch\""
            )
        );
        assert!(
            unit.contains(
                "Environment=\"CLAUDE_MD_SYMLINKER_DATA_DIR=/home/user/data%%dir$extra\""
            )
        );
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
        assert!(
            error
                .to_string()
                .contains("refusing to read systemd user unit")
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_unit_symlinks_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.service");
        let link = temp.path().join("link.service");
        std::fs::write(&target, format!("{MANAGED_MARKER}\n[Service]\n")).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let error = ensure_existing_unit_is_managed_or_absent(&link).unwrap_err();
        assert!(error.to_string().contains("symlinked systemd user unit"));
    }

    #[cfg(unix)]
    #[test]
    fn directory_writability_requires_search_permission() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("write-only-dir");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o222)).unwrap();

        assert!(!current_user_can_write_directory(&directory).unwrap());

        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(current_user_can_write_directory(&directory).unwrap());
    }

    #[test]
    fn systemd_unit_path_tokens_are_decoded() {
        assert_eq!(
            systemd_show_list_tokens(
                "/tmp/xdg\\x20config/systemd/user.control /tmp/xdg\\x20config/systemd/user /tmp/\\303\\274/systemd/user"
            )
            .unwrap(),
            vec![
                "/tmp/xdg config/systemd/user.control".to_string(),
                "/tmp/xdg config/systemd/user".to_string(),
                "/tmp/\u{fc}/systemd/user".to_string()
            ]
        );
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
