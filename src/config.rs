//! Configuration: TOML file + defaults, overridable by CLI.
//! User is always root; auth via key_path or password.

use std::path::{Path, PathBuf};

/// Full application config. Load from TOML then override with CLI.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    // --- Connection (SSH) ---
    /// reMarkable host (IP or hostname).
    #[serde(default = "default_host")]
    pub host: String,
    /// SSH key path for pubkey auth. Ignored if `password` is set.
    #[serde(default)]
    pub key_path: Option<String>,
    /// SSH password for root. If set, key_path is not used (login without key).
    #[serde(default)]
    pub password: Option<String>,

    // --- Device paths on reMarkable ---
    /// Pen input device (e.g. event1).
    #[serde(default = "default_pen_device")]
    pub pen_device: String,
    /// Touch input device (e.g. event2).
    #[serde(default = "default_touch_device")]
    pub touch_device: String,

    // --- CLI-equivalent options ---
    /// Run only touch (no pen).
    #[serde(default)]
    pub touch_only: bool,
    /// Run only pen (no touch).
    #[serde(default)]
    pub pen_only: bool,
    /// If false, pause the tablet UI (xochitl) via kill -STOP so it doesn't see input; resume on exit with kill -CONT. Default true = no pause (UI sees input).
    #[serde(default = "default_no_grab")]
    pub no_grab: bool,
    /// Disable palm rejection.
    #[serde(default)]
    pub no_palm_rejection: bool,
    /// Palm rejection grace period in ms.
    #[serde(default = "default_palm_grace_ms")]
    pub palm_grace_ms: u64,
}

fn default_host() -> String { "10.11.99.1".into() }
fn default_pen_device() -> String { "/dev/input/event1".into() }
fn default_touch_device() -> String { "/dev/input/event2".into() }
fn default_palm_grace_ms() -> u64 { 500 }
fn default_no_grab() -> bool { true }

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            key_path: Some("rm-key".into()),
            password: None,
            pen_device: default_pen_device(),
            touch_device: default_touch_device(),
            touch_only: false,
            pen_only: false,
            no_grab: default_no_grab(),
            no_palm_rejection: false,
            palm_grace_ms: default_palm_grace_ms(),
        }
    }
}

/// Auth method: either key file or password (user is always root).
#[derive(Clone)]
pub enum Auth {
    Key(PathBuf),
    Password(String),
}

impl Config {
    /// Auth to use: password if set, else key at key_path (default "rm-key").
    pub fn auth(&self) -> Auth {
        if let Some(ref p) = self.password {
            return Auth::Password(p.clone());
        }
        let path = self.key_path.as_deref().unwrap_or("rm-key");
        Auth::Key(PathBuf::from(path))
    }
}

/// Locations to try for config file (first existing wins).
pub fn config_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(p) = std::env::var("RMMOUSE_CONFIG") {
        out.push(PathBuf::from(p));
    }
    out.push(PathBuf::from("rm-mouse.toml"));
    if let Ok(home) = std::env::var("HOME") {
        out.push(PathBuf::from(home).join(".config").join("rm-mouse").join("config.toml"));
    }
    out
}

/// Load config from first existing path in config_paths(); otherwise Default.
/// CLI overrides are applied in main after parsing args.
pub fn load() -> Config {
    for path in config_paths() {
        if path.exists() {
            match load_file(&path) {
                Ok(cfg) => {
                    log::debug!("Loaded config from {}", path.display());
                    return cfg;
                }
                Err(e) => log::warn!("Failed to load {}: {}", path.display(), e),
            }
        }
    }
    Config::default()
}

fn load_file(path: &Path) -> Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let s = std::fs::read_to_string(path)?;
    let cfg: Config = toml::from_str(&s)?;
    Ok(cfg)
}
