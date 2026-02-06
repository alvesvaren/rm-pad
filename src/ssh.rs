//! SSH connection to reMarkable and streaming from remote /dev/input.
//! Auth via key file or password (user is always root).
//! Optional UI pause: stop xochitl with kill -STOP so it doesn't see input; resume with kill -CONT.

use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ssh2::Session;

use crate::config::{Auth, Config};

const USER: &str = "root";
/// Shell command template that pauses xochitl, sets up a trap to resume on exit, then cats the device.
/// When the SSH connection dies for any reason (network timeout, Ctrl+C, laptop crash), the trap fires
/// and resumes xochitl automatically. {} is the device path.
const CAT_WITH_TRAP_CMD: &str = "p=$(pidof xochitl); [ -n \"$p\" ] && kill -STOP $p && trap 'kill -CONT $p' EXIT; cat {}";
/// Plain cat command for when grab is not used.
const CAT_CMD: &str = "cat {}";
const XOCHITL_RESUME_CMD: &str = "p=$(pidof xochitl); [ -n \"$p\" ] && kill -CONT $p";

fn authenticate(sess: &mut Session, auth: &Auth) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match auth {
        Auth::Key(path) => {
            sess.userauth_pubkey_file(USER, None, path.as_ref(), None)?;
        }
        Auth::Password(pass) => {
            sess.userauth_password(USER, pass)?;
        }
    }
    if !sess.authenticated() {
        return Err("SSH auth failed".into());
    }
    Ok(())
}

/// Resume the reMarkable UI (xochitl).
pub fn resume_xochitl(config: &Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cmd = format!("sh -c '{}'", XOCHITL_RESUME_CMD);
    log::info!("Resuming xochitl (kill -CONT)…");
    run_command(config, &cmd)?;
    Ok(())
}

/// Guard that resumes xochitl when the last stream using "grab" (pause) is dropped.
pub struct XochitlPauseGuard {
    config: Config,
    refcount: Arc<AtomicUsize>,
}

impl XochitlPauseGuard {
    fn new(config: Config, refcount: Arc<AtomicUsize>) -> Self {
        Self { config, refcount }
    }
}

impl Drop for XochitlPauseGuard {
    fn drop(&mut self) {
        if self.refcount.fetch_sub(1, Ordering::SeqCst) == 1 {
            if let Err(e) = resume_xochitl(&self.config) {
                log::warn!("Resume xochitl: {}", e);
            }
        }
    }
}

/// Connect to the reMarkable and run `cat` on the device. If use_grab is true, pauses xochitl
/// with a shell trap that automatically resumes it when the connection dies for any reason
/// (network timeout, Ctrl+C, laptop crash, etc.) - no cleanup needed on the host side.
/// Returns (session, channel, optional guard). The guard provides backup resume on clean local exit.
pub fn open_input_stream(
    device_path: &str,
    config: &Config,
    use_grab: bool,
    pause_refcount: Option<Arc<AtomicUsize>>,
) -> Result<(Session, ssh2::Channel, Option<XochitlPauseGuard>), Box<dyn std::error::Error + Send + Sync>> {
    let auth = config.auth();
    log::info!("SSH connecting to {}…", config.host);
    let tcp = TcpStream::connect((config.host.as_str(), 22))?;
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake()?;
    authenticate(&mut sess, &auth)?;
    let mut channel = sess.channel_session()?;

    // Build the command: if grabbing, use trap-based command that auto-resumes on exit
    let cmd = if use_grab {
        log::info!("Using grab mode with shell trap (auto-resume on connection loss)");
        CAT_WITH_TRAP_CMD.replace("{}", device_path)
    } else {
        CAT_CMD.replace("{}", device_path)
    };

    // Create guard as backup for clean local exits (e.g., graceful shutdown)
    let guard = if use_grab {
        let refcount = match &pause_refcount {
            Some(r) => r.clone(),
            None => Arc::new(AtomicUsize::new(0)),
        };
        refcount.fetch_add(1, Ordering::SeqCst);
        Some(XochitlPauseGuard::new(config.clone(), refcount))
    } else {
        None
    };

    log::info!("SSH connected, running: {}", cmd);
    channel.exec(&cmd)?;
    channel.handle_extended_data(ssh2::ExtendedData::Merge)?;
    log::info!("stream ready for {}", device_path);
    Ok((sess, channel, guard))
}

/// Run a single command on the reMarkable.
pub fn run_command(
    config: &Config,
    command: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let auth = config.auth();
    let tcp = TcpStream::connect((config.host.as_str(), 22))?;
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake()?;
    authenticate(&mut sess, &auth)?;
    let mut channel = sess.channel_session()?;
    channel.exec(command)?;
    channel.wait_close()?;
    let status = channel.exit_status().unwrap_or(-1);
    if status != 0 {
        return Err(format!("command exited with status {}", status).into());
    }
    Ok(())
}
