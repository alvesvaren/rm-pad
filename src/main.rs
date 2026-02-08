mod config;
mod device;
mod dump;
mod grab;
mod input;
mod orientation;
mod palm;
mod ssh;

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
    let device = DeviceProfile::current();
    let config = Config::load(&cli, device);

    init_logging(cli.command.is_some());

    if let Some(command) = cli.command {
        return run_subcommand(command, &config);
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

fn run_subcommand(command: Command, config: &Config) -> Result<()> {
    match command {
        Command::Dump { device } => match device.as_str() {
            "touch" => dump::run_touch(config),
            "pen" => dump::run_pen(config),
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

    let pen_handle = spawn_pen_thread(&config, device, &palm_state);
    let touch_handle = spawn_touch_thread(&config, device, &palm_state);

    join_threads(pen_handle, touch_handle)
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

fn run_with_reconnect<F>(name: &str, mut run_fn: F)
where
    F: FnMut() -> Result<()>,
{
    loop {
        log::info!("[{}] Starting", name);

        if let Err(e) = run_fn() {
            log::error!("[{}] Error: {}", name, e);
        }

        log::warn!("[{}] Disconnected, reconnecting in 2s", name);
        thread::sleep(Duration::from_secs(2));
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
