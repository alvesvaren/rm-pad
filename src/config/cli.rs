use clap::{Parser, Subcommand};
use std::path::PathBuf;

use crate::display::{AspectRatio, Resolution};
use crate::fit::FitMode;
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

    /// Grab input exclusively [default: true]
    #[arg(long)]
    pub grab_input: bool,

    /// Don't grab input (tablet UI will also see input)
    #[arg(long)]
    pub no_grab_input: bool,

    /// Disable palm rejection
    #[arg(long)]
    pub no_palm_rejection: bool,

    /// Palm rejection grace period in milliseconds
    #[arg(long)]
    pub palm_grace_ms: Option<u64>,

    /// Screen orientation (portrait, landscape-right, landscape-left, inverted)
    #[arg(long, value_parser = clap::value_parser!(Orientation))]
    pub orientation: Option<Orientation>,
    /// How the pen area fits the target: fill/stretch (default),
    /// contain/fit (letterbox, keep aspect), or cover (crop, keep aspect).
    /// Anything other than fill/stretch requires --aspect-ratio or --resolution.
    #[arg(long, value_parser = clap::value_parser!(FitMode))]
    pub fit: Option<FitMode>,

    /// Target aspect ratio the pen area is fitted into, as WIDTH:HEIGHT (e.g.
    /// 16:9). Mutually exclusive with --resolution.
    #[arg(long, value_parser = clap::value_parser!(AspectRatio), conflicts_with = "resolution")]
    pub aspect_ratio: Option<AspectRatio>,

    /// Target resolution the pen area is fitted into, as WIDTHxHEIGHT (e.g.
    /// 1920x1080). Mutually exclusive with --aspect-ratio.
    #[arg(long, value_parser = clap::value_parser!(Resolution))]
    pub resolution: Option<Resolution>,

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
