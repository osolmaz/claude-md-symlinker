use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};

use crate::config::{AdapterConfig, AppConfig, SourceMissingBehavior};

#[derive(Debug, Clone)]
pub struct Adapter {
    pub name: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub on_source_missing: SourceMissingBehavior,
}

impl Adapter {
    pub fn from_config(name: &str, config: &AdapterConfig) -> Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }

        validate_repo_relative_path(name, "source", &config.source)?;
        validate_repo_relative_path(name, "target", &config.target)?;
        if config.source == config.target {
            bail!(
                "{name} adapter source and target must be different paths: {}",
                config.source.display()
            );
        }

        Ok(Some(Self {
            name: name.to_string(),
            source: config.source.clone(),
            target: config.target.clone(),
            on_source_missing: config.on_source_missing,
        }))
    }
}

pub fn enabled_adapters(config: &AppConfig) -> Result<Vec<Adapter>> {
    let mut adapters = Vec::new();
    if let Some(adapter) = Adapter::from_config("claude", &config.adapters.claude)? {
        adapters.push(adapter);
    }
    Ok(adapters)
}

fn validate_repo_relative_path(adapter_name: &str, field: &str, path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("{adapter_name} adapter {field} path must not be empty");
    }

    if path.is_absolute() {
        bail!(
            "{adapter_name} adapter {field} path must be relative to the repository root: {}",
            path.display()
        );
    }

    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_string_lossy();
                if part.eq_ignore_ascii_case(".git") {
                    bail!(
                        "{adapter_name} adapter {field} path must not point inside Git internals: {}",
                        path.display()
                    );
                }
                if part.chars().any(char::is_control) {
                    bail!(
                        "{adapter_name} adapter {field} path must not contain control characters: {}",
                        path.display()
                    );
                }
            }
            Component::ParentDir => bail!(
                "{adapter_name} adapter {field} path must stay inside the repository root: {}",
                path.display()
            ),
            Component::CurDir => bail!(
                "{adapter_name} adapter {field} path must not contain `.` components: {}",
                path.display()
            ),
            Component::RootDir | Component::Prefix(_) => bail!(
                "{adapter_name} adapter {field} path must be relative to the repository root: {}",
                path.display()
            ),
        }
    }

    Ok(())
}
