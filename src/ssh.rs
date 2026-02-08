use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ssh2::Session;

use crate::config::{Auth, Config};
use crate::grab;

/// Watchdog file path on the tablet
pub const WATCHDOG_FILE: &str = "/tmp/rm-pad-watchdog";

/// How often to touch the watchdog file
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(2);

/// Timeout for SSH operations
const SSH_TIMEOUT: Duration = Duration::from_secs(5);

/// Guard that holds the SSH session.
pub struct GrabCleanup {
    #[allow(dead_code)]
    session: Session,
}

impl GrabCleanup {
    pub fn new(session: Session) -> Self {
        Self { session }
    }
}

const SSH_USER: &str = "root";
const SSH_PORT: u16 = 22;

/// Open an SSH connection and stream input from a device.
pub fn open_input_stream(
    device_path: &str,
    config: &Config,
    grab: bool,
) -> Result<(GrabCleanup, ssh2::Channel), Box<dyn std::error::Error + Send + Sync>> {
    log::info!("Connecting to {}", config.host);

    let session = connect_and_authenticate(config)?;

    if grab {
        prepare_grab(&session)?;
    }

    let mut channel = session.channel_session()?;

    let cmd = build_stream_command(device_path, grab);
    log::debug!("Executing: {}", cmd);

    channel.exec(&cmd)?;

    log::info!("Stream ready for {}", device_path);
    Ok((GrabCleanup::new(session), channel))
}

fn connect_and_authenticate(
    config: &Config,
) -> Result<Session, Box<dyn std::error::Error + Send + Sync>> {
    let addr = (config.host.as_str(), SSH_PORT)
        .to_socket_addrs()?
        .next()
        .ok_or("Could not resolve host address")?;
    let tcp = TcpStream::connect_timeout(&addr, SSH_TIMEOUT)?;

    let mut session = Session::new()?;
    session.set_tcp_stream(tcp);
    session.handshake()?;
    authenticate(&mut session, &config.auth())?;

    Ok(session)
}

/// Connect to the device via SSH for device detection purposes.
/// Returns None if connection fails (e.g., device not available).
pub fn connect_for_detection(config: &Config) -> Result<Session, Box<dyn std::error::Error + Send + Sync>> {
    connect_and_authenticate(config)
}

fn authenticate(
    session: &mut Session,
    auth: &Auth,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match auth {
        Auth::Key(path) => {
            session.userauth_pubkey_file(SSH_USER, None, path.as_ref(), None)?;
        }
        Auth::Password(pass) => {
            session.userauth_password(SSH_USER, pass)?;
        }
    }

    if !session.authenticated() {
        return Err("SSH authentication failed".into());
    }

    Ok(())
}

fn prepare_grab(session: &Session) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let arch = grab::detect_arch(session)?;
    log::info!("Detected tablet architecture: {}", arch);
    grab::ensure_binary_valid(session, arch)?;
    Ok(())
}

fn build_stream_command(device_path: &str, grab: bool) -> String {
    if grab {
        log::info!("Using grab mode (input restored automatically on disconnect)");
        grab::grab_command(device_path)
    } else {
        format!("cat {}", device_path)
    }
}

/// Touch the watchdog file once. Blocks until success or error.
/// This MUST be called before starting grabbers.
pub fn touch_watchdog_once(config: &Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr = (config.host.as_str(), SSH_PORT)
        .to_socket_addrs()?
        .next()
        .ok_or("Could not resolve host address")?;
    let tcp = TcpStream::connect_timeout(&addr, SSH_TIMEOUT)?;

    let mut session = Session::new()?;
    session.set_tcp_stream(tcp);
    session.handshake()?;
    authenticate(&mut session, &config.auth())?;

    let mut channel = session.channel_session()?;
    channel.exec(&format!("touch {}", WATCHDOG_FILE))?;

    let mut output = String::new();
    channel.read_to_string(&mut output)?;
    channel.wait_close()?;

    log::info!("Watchdog file touched");
    Ok(())
}

/// Spawn a thread that periodically touches the watchdog file.
/// Returns a stop flag.
pub fn spawn_watchdog(config: &Config) -> Arc<AtomicBool> {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_clone = stop_flag.clone();
    let host = config.host.clone();
    let auth = config.auth();

    thread::spawn(move || {
        log::info!("Watchdog thread started");

        loop {
            if stop_flag_clone.load(Ordering::Relaxed) {
                log::debug!("Watchdog thread stopping");
                break;
            }

            if let Err(e) = touch_watchdog(&host, &auth) {
                log::warn!("Watchdog touch failed: {}", e);
            }

            thread::sleep(WATCHDOG_INTERVAL);
        }
    });

    stop_flag
}

fn touch_watchdog(host: &str, auth: &Auth) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr = (host, SSH_PORT)
        .to_socket_addrs()?
        .next()
        .ok_or("Could not resolve host address")?;
    let tcp = TcpStream::connect_timeout(&addr, SSH_TIMEOUT)?;

    let mut session = Session::new()?;
    session.set_tcp_stream(tcp);
    session.handshake()?;
    authenticate(&mut session, auth)?;

    let mut channel = session.channel_session()?;
    channel.exec(&format!("touch {}", WATCHDOG_FILE))?;

    let mut output = String::new();
    channel.read_to_string(&mut output)?;
    channel.wait_close()?;

    Ok(())
}
