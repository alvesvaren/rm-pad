mod config;
mod device;
mod dump;
mod grab;
mod input;
mod orientation;
mod palm;
mod ssh;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use clap::Parser;

use config::{Cli, Command, Config};
use device::DeviceProfile;
use palm::{PalmState, SharedPalmState};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn main() -> Result<()> {
    let cli = Cli::parse();
    
    init_logging(cli.command.is_some());
    
    // Detect device via SSH (required)
    let config_for_detection = Config::load(&cli, DeviceProfile::current());
    let session = ssh::connect_for_detection(&config_for_detection)?;
    let device = DeviceProfile::detect_via_ssh(&session)?;
    log::info!("Using device profile: {}", device.name);
    
    let config = Config::load(&cli, device);

    if let Some(command) = cli.command {
        return run_subcommand(command, &config, device);
    }

    if let Err(msg) = config.validate() {
        eprintln!("Error: {}", msg);
        eprintln!("\nRun with --help for usage information");
        std::process::exit(1);
    }

    log_startup_info(&config);
    run_input_forwarding(config, device)
}

fn init_logging(is_dump: bool) {
    let default_level = if is_dump { "warn" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_level)).init();
}

fn run_subcommand(
    command: Command,
    config: &Config,
    device_profile: &'static DeviceProfile,
) -> Result<()> {
    match command {
        Command::Dump { device } => match device.as_str() {
            "touch" => dump::run_touch(config, device_profile),
            "pen" => dump::run_pen(config, device_profile),
            _ => {
                eprintln!("Unknown dump device: {}. Use 'touch' or 'pen'.", device);
                std::process::exit(1);
            }
        },
    }
}

fn log_startup_info(config: &Config) {
    let palm_info = if config.no_palm_rejection {
        "off".into()
    } else {
        format!("on (grace {}ms)", config.palm_grace_ms)
    };

    log::info!(
        "Starting rm-pad: host={}, pen={}, touch={}, palm_rejection={}, grab_input={}, orientation={}",
        config.host,
        if config.run_pen() { &config.pen_device } else { "off" },
        if config.run_touch() { &config.touch_device } else { "off" },
        palm_info,
        config.grab_input,
        config.orientation
    );
}

fn run_input_forwarding(config: Config, device: &'static DeviceProfile) -> Result<()> {
    let palm_state = create_palm_state(&config);
    let config = Arc::new(config);

    // If grabbing, touch the watchdog file FIRST, then start watchdog thread
    let watchdog_stop = if config.grab_input {
        // Touch once before starting anything - this ensures the file exists
        // and is fresh before any grabber starts
        log::info!("Touching watchdog file before starting...");
        if let Err(e) = ssh::touch_watchdog_once(&config) {
            log::error!("Failed to touch watchdog: {}", e);
            return Err(e);
        }

        // Now start the background watchdog thread
        Some(ssh::spawn_watchdog(&config))
    } else {
        None
    };

    let pen_handle = spawn_pen_thread(&config, device, &palm_state);
    let touch_handle = spawn_touch_thread(&config, device, &palm_state);

    let result = join_threads(pen_handle, touch_handle);

    // Stop watchdog thread
    if let Some(stop_flag) = watchdog_stop {
        stop_flag.store(true, Ordering::Relaxed);
    }

    result
}

fn create_palm_state(config: &Config) -> Option<SharedPalmState> {
    if config.no_palm_rejection {
        return None;
    }
    if !config.run_pen() || !config.run_touch() {
        return None;
    }

    Some(Arc::new(std::sync::Mutex::new(PalmState::new())))
}

fn spawn_pen_thread(
    config: &Arc<Config>,
    device: &'static DeviceProfile,
    palm_state: &Option<SharedPalmState>,
) -> Option<thread::JoinHandle<()>> {
    if !config.run_pen() {
        return None;
    }

    let config = config.clone();
    let palm = palm_state.clone();

    Some(thread::spawn(move || {
        run_with_reconnect("pen", || {
            input::run_pen(&config, device, palm.clone())
        });
    }))
}

fn spawn_touch_thread(
    config: &Arc<Config>,
    device: &'static DeviceProfile,
    palm_state: &Option<SharedPalmState>,
) -> Option<thread::JoinHandle<()>> {
    if !config.run_touch() {
        return None;
    }

    let config = config.clone();
    let palm = palm_state.clone();

    Some(thread::spawn(move || {
        run_with_reconnect("touch", || {
            input::run_touch(&config, device, palm.clone())
        });
    }))
}

/// Delay between reconnection attempts.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

fn run_with_reconnect<F>(name: &str, mut run_fn: F)
where
    F: FnMut() -> Result<()>,
{
    loop {
        log::info!("[{}] Connecting", name);

        if let Err(e) = run_fn() {
            log::error!("[{}] Error: {}", name, e);
        }

        log::warn!(
            "[{}] Disconnected, reconnecting in {}s",
            name,
            RECONNECT_DELAY.as_secs()
        );
        thread::sleep(RECONNECT_DELAY);
    }
}

fn join_threads(
    pen: Option<thread::JoinHandle<()>>,
    touch: Option<thread::JoinHandle<()>>,
) -> Result<()> {
    if let Some(h) = pen {
        h.join().unwrap();
    }
    if let Some(h) = touch {
        h.join().unwrap();
    }
    Ok(())
}
