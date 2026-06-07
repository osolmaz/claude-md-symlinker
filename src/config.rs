use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use directories::{ProjectDirs, UserDirs};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: AppConfig,
    pub path: PathBuf,
    pub existed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    pub scan: ScanConfig,
    pub git: GitConfig,
    pub watch: WatchConfig,
    pub materialization: MaterializationConfig,
    pub adapters: AdaptersConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanConfig {
    pub roots: Vec<PathBuf>,
    pub include_paths: Vec<PathBuf>,
    pub exclude_paths: Vec<PathBuf>,
    pub exclude_dir_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitConfig {
    pub exclude_mode: ExcludeMode,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExcludeMode {
    #[default]
    PerRepo,
    Global,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchConfig {
    pub enabled: bool,
    pub reconcile_interval_minutes: u64,
    pub full_rescan_interval_hours: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MaterializationConfig {
    pub strategy: MaterializationStrategy,
    pub allow_hardlink: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MaterializationStrategy {
    #[default]
    Auto,
    Symlink,
    Copy,
    Hardlink,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AdaptersConfig {
    pub claude: AdapterConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdapterConfig {
    pub enabled: bool,
    pub source: PathBuf,
    pub target: PathBuf,
    pub on_source_missing: SourceMissingBehavior,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SourceMissingBehavior {
    #[default]
    Leave,
    RemoveIfManaged,
}

#[derive(Debug, Clone)]
pub struct ScanScope {
    pub roots: Vec<PathBuf>,
    pub include_paths: Vec<PathBuf>,
    pub exclude_paths: Vec<PathBuf>,
    pub exclude_dir_names: BTreeSet<String>,
}

impl AppConfig {
    pub fn set_scan_roots(&mut self, roots: Vec<PathBuf>) -> Result<()> {
        if roots.is_empty() {
            bail!("init requires at least one root");
        }

        self.scan.roots = canonicalize_existing_paths(&roots)?;
        Ok(())
    }

    pub fn scan_scope(&self, config_existed: bool, cli_roots: &[PathBuf]) -> Result<ScanScope> {
        let configured_roots = canonicalize_existing_paths(&self.scan.roots)?;
        let roots = if cli_roots.is_empty() {
            configured_roots.clone()
        } else {
            let requested_roots = canonicalize_existing_paths(cli_roots)?;
            if config_existed {
                if configured_roots.is_empty() {
                    bail!(
                        "requested roots cannot be used because no scan roots are configured; run `claude-md-symlinker init <root...>` first"
                    );
                }
                for root in &requested_roots {
                    if !configured_roots
                        .iter()
                        .any(|allowed| root.starts_with(allowed))
                    {
                        bail!(
                            "requested root {} is outside configured scan scope",
                            root.display()
                        );
                    }
                }
            }
            requested_roots
        };

        if roots.is_empty() {
            bail!(
                "no scan roots configured; run `claude-md-symlinker init <root...>` or pass roots"
            );
        }

        let include_paths = canonicalize_existing_paths(&self.scan.include_paths)?;
        let exclude_paths = canonicalize_maybe_existing_paths(&self.scan.exclude_paths)?;
        let exclude_dir_names = self
            .scan
            .exclude_dir_names
            .iter()
            .filter(|name| !name.trim().is_empty())
            .cloned()
            .collect();

        Ok(ScanScope {
            roots,
            include_paths,
            exclude_paths,
            exclude_dir_names,
        })
    }
}

impl ScanScope {
    pub fn path_is_excluded(&self, path: &Path) -> bool {
        self.exclude_paths
            .iter()
            .any(|excluded| path.starts_with(excluded))
    }

    pub fn repo_is_allowed(&self, repo_root: &Path) -> bool {
        if !self.roots.iter().any(|root| repo_root.starts_with(root)) {
            return false;
        }

        if self.path_is_excluded(repo_root) {
            return false;
        }

        self.include_paths.is_empty()
            || self
                .include_paths
                .iter()
                .any(|included| repo_root.starts_with(included))
    }

    pub fn should_descend(&self, path: &Path) -> bool {
        if self.path_is_excluded(path) {
            return false;
        }

        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| !self.exclude_dir_names.contains(name))
            .unwrap_or(true)
    }
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            include_paths: Vec::new(),
            exclude_paths: Vec::new(),
            exclude_dir_names: ["node_modules", ".cache", ".venv", "target", "dist", "build"]
                .into_iter()
                .map(String::from)
                .collect(),
        }
    }
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            exclude_mode: ExcludeMode::PerRepo,
        }
    }
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            reconcile_interval_minutes: 30,
            full_rescan_interval_hours: 12,
        }
    }
}

impl Default for MaterializationConfig {
    fn default() -> Self {
        Self {
            strategy: MaterializationStrategy::Auto,
            allow_hardlink: false,
        }
    }
}

impl Default for AdapterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            source: PathBuf::from("AGENTS.md"),
            target: PathBuf::from("CLAUDE.md"),
            on_source_missing: SourceMissingBehavior::Leave,
        }
    }
}

pub fn load(path_override: Option<&Path>) -> Result<LoadedConfig> {
    let path = config_path(path_override)?;
    let existed = path.exists();
    let config = if existed {
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        toml::from_str(&text)
            .with_context(|| format!("failed to parse config {}", path.display()))?
    } else {
        AppConfig::default()
    };

    Ok(LoadedConfig {
        config,
        path,
        existed,
    })
}

pub fn save(path_override: Option<&Path>, config: &AppConfig) -> Result<PathBuf> {
    let path = config_path(path_override)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(config)?;
    fs::write(&path, text).with_context(|| format!("failed to write config {}", path.display()))?;
    Ok(path)
}

pub fn config_path(path_override: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = path_override {
        return absolute_expanded_path(path);
    }

    if let Ok(path) = env::var("CLAUDE_MD_SYMLINKER_CONFIG") {
        return absolute_expanded_path(Path::new(&path));
    }

    let dirs = ProjectDirs::from("dev", "dutiful", "claude-md-symlinker")
        .context("could not determine platform config directory")?;
    Ok(dirs.config_dir().join("claude-md-symlinker.toml"))
}

fn absolute_expanded_path(path: &Path) -> Result<PathBuf> {
    let expanded = expand_tilde(path);
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(env::current_dir()?.join(expanded))
    }
}

pub fn data_dir() -> Result<PathBuf> {
    if let Ok(path) = env::var("CLAUDE_MD_SYMLINKER_DATA_DIR") {
        return Ok(expand_tilde(Path::new(&path)));
    }

    let dirs = ProjectDirs::from("dev", "dutiful", "claude-md-symlinker")
        .context("could not determine platform data directory")?;
    Ok(dirs.data_dir().to_path_buf())
}

pub fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| path.to_path_buf());
    }
    path.to_path_buf()
}

fn home_dir() -> Option<PathBuf> {
    UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
}

fn canonicalize_existing_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    paths
        .iter()
        .map(|path| {
            let expanded = expand_tilde(path);
            expanded
                .canonicalize()
                .with_context(|| format!("path does not exist: {}", expanded.display()))
        })
        .collect()
}

fn canonicalize_maybe_existing_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    paths
        .iter()
        .map(|path| {
            let expanded = expand_tilde(path);
            if expanded.exists() {
                expanded
                    .canonicalize()
                    .with_context(|| format!("failed to canonicalize {}", expanded.display()))
            } else if expanded.is_absolute() {
                Ok(expanded)
            } else {
                Ok(env::current_dir()?.join(expanded))
            }
        })
        .collect()
}
