mod cli;
mod file;

pub use cli::{Cli, Command};

use std::path::PathBuf;

use crate::device::DeviceProfile;
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
        }
    }

    pub fn auth(&self) -> Auth {
        if let Some(ref password) = self.password {
            return Auth::Password(password.clone());
        }
        let path = self.key_path.as_deref().unwrap_or("rm-key");
        Auth::Key(PathBuf::from(path))
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
        Ok(())
    }
}
