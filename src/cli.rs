use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "claudectomy")]
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
