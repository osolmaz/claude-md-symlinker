use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "claudemdeez")]
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
    #[arg(long, default_value = "claudemdeez.service")]
    pub unit_name: String,

    /// Binary path to run from the service. Defaults to the current executable.
    #[arg(long)]
    pub bin: Option<PathBuf>,

    /// Data directory to expose through CLAUDEMDEEZ_DATA_DIR.
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
    #[arg(long, default_value = "claudemdeez.service")]
    pub unit_name: String,
}
