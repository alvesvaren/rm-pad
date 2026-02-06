//! SSH connection to reMarkable and streaming from remote /dev/input.
//! Auth via key file or password (user is always root).
//! Optional UI pause: stop xochitl with kill -STOP so it doesn't see input; resume with kill -CONT.

use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ssh2::Session;

use crate::config::{Auth, Config};

const USER: &str = "root";
const XOCHITL_PAUSE_CMD: &str = "p=$(pidof xochitl); [ -n \"$p\" ] && kill -STOP $p";
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

/// Pause the reMarkable UI (xochitl) so it does not see input. No files on device needed.
pub fn pause_xochitl(config: &Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cmd = format!("sh -c '{}'", XOCHITL_PAUSE_CMD);
    log::info!("Pausing xochitl (kill -STOP)…");
    run_command(config, &cmd)?;
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

/// Connect to the reMarkable and run `cat` on the device. Optionally pause xochitl first so the UI
/// does not see input (no binary on device: uses kill -STOP / -CONT over SSH).
/// Returns (session, channel, optional guard). Keep the guard until done reading; when dropped it
/// resumes xochitl if this was the last stream using pause.
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

    let guard = if use_grab {
        let refcount = match &pause_refcount {
            Some(r) => r.clone(),
            None => Arc::new(AtomicUsize::new(0)),
        };
        let prev = refcount.fetch_add(1, Ordering::SeqCst);
        if prev == 0 {
            if let Err(e) = pause_xochitl(config) {
                refcount.fetch_sub(1, Ordering::SeqCst);
                return Err(e.into());
            }
        }
        Some(XochitlPauseGuard::new(config.clone(), refcount))
    } else {
        None
    };

    log::info!("SSH connected, running cat {}…", device_path);
    channel.exec(&format!("cat {}", device_path))?;
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
