mod cli;
mod file;

pub use cli::{Cli, Command};

use std::path::PathBuf;

use crate::device::DeviceProfile;
use crate::display::{AspectRatio, Resolution};
use crate::fit::FitMode;
use crate::orientation::Orientation;

/// Authentication method for SSH connection.
#[derive(Clone)]
pub enum Auth {
    Key(PathBuf),
    Password(String),
}

/// Merged configuration from CLI args and TOML file.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub key_path: Option<String>,
    pub password: Option<String>,
    pub pen_device: String,
    pub touch_device: String,
    pub touch_only: bool,
    pub pen_only: bool,
    pub grab_input: bool,
    pub no_palm_rejection: bool,
    pub palm_grace_ms: u64,
    pub orientation: Orientation,
    pub fit: FitMode,
    pub aspect_ratio: Option<AspectRatio>,
    pub resolution: Option<Resolution>,
}

impl Config {
    /// Load configuration by merging TOML file with CLI overrides.
    pub fn load(cli: &Cli, device: &DeviceProfile) -> Self {
        let file_config = cli
            .config
            .as_ref()
            .and_then(|p| file::load_from_path(p))
            .or_else(file::load_from_default_paths)
            .unwrap_or_default();

        Self {
            host: cli.host.clone().unwrap_or(file_config.host),
            key_path: cli.key_path.clone().or(file_config.key_path),
            password: cli.password.clone().or(file_config.password),
            pen_device: cli
                .pen_device
                .clone()
                .unwrap_or_else(|| file_config.pen_device.unwrap_or(device.pen_device.into())),
            touch_device: cli
                .touch_device
                .clone()
                .unwrap_or_else(|| file_config.touch_device.unwrap_or(device.touch_device.into())),
            touch_only: cli.touch_only || file_config.touch_only,
            pen_only: cli.pen_only || file_config.pen_only,
            grab_input: if cli.no_grab_input {
                false
            } else {
                cli.grab_input || file_config.grab_input
            },
            no_palm_rejection: cli.no_palm_rejection || file_config.no_palm_rejection,
            palm_grace_ms: cli
                .palm_grace_ms
                .or(file_config.palm_grace_ms)
                .unwrap_or(500),
            orientation: cli.orientation.unwrap_or(file_config.orientation),
            fit: cli.fit.unwrap_or(file_config.fit),
            aspect_ratio: if let Some(ratio) = cli.aspect_ratio {
                Some(ratio)
            } else {
                file_config.aspect_ratio
            },
            resolution: if let Some(res) = cli.resolution {
                Some(res)
            } else {
                file_config.resolution
            },
        }
    }

    pub fn auth(&self) -> Auth {
        if let Some(ref password) = self.password {
            return Auth::Password(password.clone());
        }
        let path = self.key_path.as_deref().unwrap_or("rm-key");
        Auth::Key(expand_tilde(path))
    }

    pub fn run_pen(&self) -> bool {
        !self.touch_only
    }

    pub fn run_touch(&self) -> bool {
        !self.pen_only
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.touch_only && self.pen_only {
            return Err("Cannot use both --touch-only and --pen-only");
        }
        if !self.run_pen() && !self.run_touch() {
            return Err("No input device enabled");
        }
        // Aspect ratio and resolution describe the same target two ways; clap
        // rejects both on the CLI, but the file config can still set both.
        if self.aspect_ratio.is_some() && self.resolution.is_some() {
            return Err("Cannot set both aspect_ratio and resolution");
        }
        // Fit warps the pen into a given bounding box, so a
        // non-fill fit needs --aspect-ratio or --resolution
        if self.fit != FitMode::Fill && self.aspect_ratio.is_none() && self.resolution.is_none() {
            return Err("--fit (other than fill) requires either --aspect-ratio or --resolution");
        }
        Ok(())
    }
}

/// Expand a leading `~` or `~/` to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}
