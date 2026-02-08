use clap::{Parser, Subcommand};
use std::path::PathBuf;

use crate::orientation::Orientation;

#[derive(Parser)]
#[command(name = "rm-pad")]
#[command(about = "Forward reMarkable tablet input to your computer")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// reMarkable host (IP or hostname)
    #[arg(long, env = "RMPAD_HOST")]
    pub host: Option<String>,

    /// SSH key path for authentication
    #[arg(long)]
    pub key_path: Option<String>,

    /// SSH password (if set, key_path is ignored)
    #[arg(long, env = "RMPAD_PASSWORD")]
    pub password: Option<String>,

    /// Pen input device path on reMarkable
    #[arg(long)]
    pub pen_device: Option<String>,

    /// Touch input device path on reMarkable
    #[arg(long)]
    pub touch_device: Option<String>,

    /// Run touch input only (no pen)
    #[arg(long)]
    pub touch_only: bool,

    /// Run pen input only (no touch)
    #[arg(long)]
    pub pen_only: bool,

    /// Stop xochitl UI while streaming input
    #[arg(long)]
    pub stop_ui: bool,

    /// Keep xochitl UI running (overrides config file)
    #[arg(long)]
    pub no_stop_ui: bool,

    /// Disable palm rejection
    #[arg(long)]
    pub no_palm_rejection: bool,

    /// Palm rejection grace period in milliseconds
    #[arg(long)]
    pub palm_grace_ms: Option<u64>,

    /// Screen orientation (portrait, landscape-right, landscape-left, inverted)
    #[arg(long, value_parser = clap::value_parser!(Orientation))]
    pub orientation: Option<Orientation>,

    /// Path to config file
    #[arg(long, env = "RMPAD_CONFIG")]
    pub config: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Dump raw input events for debugging
    Dump {
        /// Device to dump: "touch" or "pen"
        device: String,
    },
}
