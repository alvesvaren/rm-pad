use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::orientation::Orientation;

const DEFAULT_HOST: &str = "10.11.99.1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default = "default_host")]
    pub host: String,
    pub key_path: Option<String>,
    pub password: Option<String>,
    pub pen_device: Option<String>,
    pub touch_device: Option<String>,
    #[serde(default)]
    pub touch_only: bool,
    #[serde(default)]
    pub pen_only: bool,
    #[serde(default = "default_true")]
    pub grab_input: bool,
    #[serde(default)]
    pub no_palm_rejection: bool,
    pub palm_grace_ms: Option<u64>,
    #[serde(default)]
    pub orientation: Orientation,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.into(),
            grab_input: true,
            key_path: None,
            password: None,
            pen_device: None,
            touch_device: None,
            touch_only: false,
            pen_only: false,
            no_palm_rejection: false,
            palm_grace_ms: None,
            orientation: Orientation::default(),
        }
    }
}

fn default_host() -> String {
    DEFAULT_HOST.into()
}

fn default_true() -> bool {
    true
}

pub fn load_from_path(path: &Path) -> Option<FileConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    match toml::from_str(&content) {
        Ok(config) => {
            log::debug!("Loaded config from {}", path.display());
            Some(config)
        }
        Err(e) => {
            log::warn!("Failed to parse {}: {}", path.display(), e);
            None
        }
    }
}

pub fn load_from_default_paths() -> Option<FileConfig> {
    for path in default_config_paths() {
        if path.exists() {
            if let Some(config) = load_from_path(&path) {
                return Some(config);
            }
        }
    }
    None
}

fn default_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    paths.push(PathBuf::from("rm-pad.toml"));

    if let Ok(home) = std::env::var("HOME") {
        paths.push(PathBuf::from(home).join(".config").join("rm-pad.toml"));
    }

    paths
}
