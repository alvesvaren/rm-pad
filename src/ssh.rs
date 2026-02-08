use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use socket2::SockRef;
use ssh2::Session;

use crate::config::{Auth, Config};
use crate::grab;

/// Watchdog file path on the tablet
pub const WATCHDOG_FILE: &str = "/tmp/rm-pad-watchdog";

/// How often to touch the watchdog file
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(2);

/// Guard that ensures remote grab processes are killed when dropped.
pub struct GrabCleanup {
    session: Option<Session>,
    grab_enabled: bool,
}

impl GrabCleanup {
    pub fn new(session: Session, grab_enabled: bool) -> Self {
        Self {
            session: Some(session),
            grab_enabled,
        }
    }
}

impl Drop for GrabCleanup {
    fn drop(&mut self) {
        if self.grab_enabled {
            if let Some(ref session) = self.session {
                // Try to kill processes, but don't panic if it fails
                if let Err(e) = grab::kill_existing_processes(session) {
                    log::debug!("Failed to kill grab processes on cleanup: {}", e);
                }
            }
        }
    }
}

const SSH_USER: &str = "root";
const SSH_PORT: u16 = 22;

/// Timeout for the initial TCP connection attempt.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long before TCP sends the first keepalive probe on an idle connection.
const TCP_KEEPALIVE_TIME: Duration = Duration::from_secs(5);

/// Interval between TCP keepalive probes.
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// Number of unanswered probes before TCP considers the connection dead.
const TCP_KEEPALIVE_RETRIES: u32 = 3;

/// Open an SSH connection and stream input from a device.
///
/// When `grab` is true, the evgrab helper is uploaded to the tablet and
/// used to exclusively grab the device (EVIOCGRAB). This prevents xochitl
/// from seeing input without stopping the process. The grab is automatically
/// released when the SSH channel closes (disconnect, signal, etc.).
///
/// When `grab` is false, plain `cat` is used and xochitl also sees events.
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
    Ok((GrabCleanup::new(session, grab), channel))
}

fn connect_and_authenticate(
    config: &Config,
) -> Result<Session, Box<dyn std::error::Error + Send + Sync>> {
    let addr = (config.host.as_str(), SSH_PORT)
        .to_socket_addrs()?
        .next()
        .ok_or("Could not resolve host address")?;
    let tcp = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;

    // Enable TCP keepalive so the OS kernel detects dead connections quickly.
    // Without this a silently-dropped connection (e.g. USB cable unplugged)
    // can block reads for minutes waiting for the default TCP timeout.
    // With these settings the kernel detects a dead peer in roughly
    // KEEPALIVE_TIME + KEEPALIVE_INTERVAL * KEEPALIVE_RETRIES ≈ 20 seconds.
    let sock = SockRef::from(&tcp);
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(TCP_KEEPALIVE_TIME)
        .with_interval(TCP_KEEPALIVE_INTERVAL)
        .with_retries(TCP_KEEPALIVE_RETRIES);
    sock.set_tcp_keepalive(&keepalive)?;

    let mut session = Session::new()?;
    session.set_tcp_stream(tcp);
    session.handshake()?;
    authenticate(&mut session, &config.auth())?;

    Ok(session)
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

/// Detect the tablet architecture and upload the grab helper via SFTP.
fn prepare_grab(session: &Session) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Kill any existing rm-pad-grab processes before starting a new one
    grab::kill_existing_processes(session)?;

    let arch = grab::detect_arch(session)?;
    log::info!("Detected tablet architecture: {}", arch);

    // Ensure binary exists and matches our embedded version
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

/// Spawn a thread that periodically touches the watchdog file on the tablet.
/// Returns a stop flag that can be set to stop the watchdog.
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

            // Try to connect and touch the watchdog file
            match touch_watchdog(&host, &auth) {
                Ok(()) => {
                    log::trace!("Watchdog file touched");
                }
                Err(e) => {
                    log::warn!("Failed to touch watchdog: {}", e);
                }
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
    let tcp = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;

    let mut session = Session::new()?;
    session.set_tcp_stream(tcp);
    session.handshake()?;
    authenticate(&mut session, auth)?;

    let mut channel = session.channel_session()?;
    channel.exec(&format!("touch {}", WATCHDOG_FILE))?;

    // Read any output and wait for the command to complete
    let mut output = String::new();
    channel.read_to_string(&mut output)?;
    channel.wait_close()?;

    Ok(())
}
