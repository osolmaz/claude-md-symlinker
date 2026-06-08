use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "claude-md-symlinker")]
#[command(about = "Keep AGENTS.md canonical and generate local agent-tool shims")]
#[command(version)]
pub struct Cli {
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    #[arg(long, global = true)]
    pub dry_run: bool,

    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Install Claude hooks and the Linux user service.
    Install(InstallArgs),
    /// Observe a Claude hook cwd and reconcile matching instruction files.
    Observe(ObserveArgs),
    /// Run the background repair daemon.
    Daemon(DaemonArgs),
    /// Show install and state health.
    Status,
    /// Manage observed repositories.
    Repos(ReposArgs),
    /// Migrate detected CLAUDE.md files to AGENTS.md.
    Migrate(MigrateArgs),
    /// Manage local settings stored in state.
    Settings(SettingsArgs),
    /// Remove managed shims recorded in state.
    Purge(PurgeArgs),
    /// Remove Claude hooks and the Linux user service.
    Uninstall(UninstallArgs),
    /// Create or update config scan roots.
    Init(InitArgs),
    /// Reconcile configured or supplied roots.
    Apply(RootArgs),
    /// Watch roots and trigger reconciliation.
    Watch(RootArgs),
    /// Check local setup.
    Doctor,
    /// Remove stale managed shims conservatively.
    Clean(CleanArgs),
    /// Manage the Linux user service for watch mode.
    Service(ServiceArgs),
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    #[arg(long)]
    pub no_service: bool,

    #[arg(long)]
    pub no_hooks: bool,

    #[arg(long, conflicts_with = "no_auto_migrate")]
    pub auto_migrate: bool,

    #[arg(long, conflicts_with = "auto_migrate")]
    pub no_auto_migrate: bool,

    #[arg(long, hide = true)]
    pub unit_name: Option<String>,

    #[arg(long, hide = true)]
    pub bin: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ObserveArgs {
    #[arg(long)]
    pub no_apply: bool,

    #[arg(long)]
    pub strict: bool,

    #[arg(long, hide = true)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct DaemonArgs {
    #[arg(long, hide = true)]
    pub once: bool,

    #[arg(long, hide = true)]
    pub interval_seconds: Option<u64>,

    #[arg(long, hide = true)]
    pub max_instruction_dirs: Option<usize>,

    #[arg(long, hide = true)]
    pub unit_name: Option<String>,
}

#[derive(Debug, Args)]
pub struct ReposArgs {
    #[command(subcommand)]
    pub command: ReposCommand,
}

#[derive(Debug, Subcommand)]
pub enum ReposCommand {
    /// List observed repositories.
    List,
    /// Stop managing a repository.
    Remove(ReposRemoveArgs),
    /// Remove missing repositories and instruction directories from active scope.
    Prune(ReposPruneArgs),
}

#[derive(Debug, Args)]
pub struct ReposRemoveArgs {
    pub repo: PathBuf,

    #[arg(long)]
    pub clean_exclude: bool,
}

#[derive(Debug, Args)]
pub struct ReposPruneArgs {
    #[arg(long)]
    pub forget_history: bool,
}

#[derive(Debug, Args)]
pub struct MigrateArgs {
    #[arg(long)]
    pub auto_safe_only: bool,

    #[arg(long)]
    pub replace_existing: bool,

    #[arg(long)]
    pub no_git_add: bool,
}

#[derive(Debug, Args)]
pub struct SettingsArgs {
    #[command(subcommand)]
    pub command: SettingsCommand,
}

#[derive(Debug, Subcommand)]
pub enum SettingsCommand {
    /// Set a local setting.
    Set(SettingsSetArgs),
    /// Print a local setting.
    Get(SettingsGetArgs),
}

#[derive(Debug, Args)]
pub struct SettingsSetArgs {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Args)]
pub struct SettingsGetArgs {
    pub key: String,
}

#[derive(Debug, Args)]
pub struct PurgeArgs {}

#[derive(Debug, Args)]
pub struct UninstallArgs {
    #[arg(long)]
    pub purge: bool,

    #[arg(long)]
    pub no_service: bool,

    #[arg(long)]
    pub no_hooks: bool,

    #[arg(long, hide = true)]
    pub unit_name: Option<String>,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    pub roots: Vec<PathBuf>,

    #[arg(long)]
    pub apply: bool,
}

#[derive(Debug, Args)]
pub struct RootArgs {
    pub roots: Vec<PathBuf>,
}

#[derive(Debug, Args)]
pub struct CleanArgs {
    pub roots: Vec<PathBuf>,

    #[arg(long)]
    pub remove_if_source_missing: bool,
}

#[derive(Debug, Args)]
pub struct ServiceArgs {
    #[command(subcommand)]
    pub command: ServiceCommand,
}

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Install the managed systemd user unit.
    Install(ServiceInstallArgs),
    /// Remove the managed systemd user unit.
    Uninstall(ServiceUnitArgs),
    /// Start the systemd user unit.
    Start(ServiceUnitArgs),
    /// Stop the systemd user unit.
    Stop(ServiceUnitArgs),
    /// Restart the systemd user unit.
    Restart(ServiceUnitArgs),
    /// Show systemd status for the user unit.
    Status(ServiceUnitArgs),
}

#[derive(Debug, Args)]
pub struct ServiceInstallArgs {
    /// Unit name to install. `.service` is added when omitted.
    #[arg(long, default_value = "claude-md-symlinker.service")]
    pub unit_name: String,

    /// Binary path to run from the service. Defaults to the current executable.
    #[arg(long)]
    pub bin: Option<PathBuf>,

    /// Data directory to expose through CLAUDE_MD_SYMLINKER_DATA_DIR.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Do not enable the service after installation.
    #[arg(long)]
    pub no_enable: bool,

    /// Start the service immediately after installation.
    #[arg(long)]
    pub now: bool,
}

#[derive(Debug, Args)]
pub struct ServiceUnitArgs {
    /// Unit name. `.service` is added when omitted.
    #[arg(long, default_value = "claude-md-symlinker.service")]
    pub unit_name: String,
}
