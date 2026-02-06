//! SSH connection to reMarkable and streaming from remote /dev/input.

use std::io::Read;
use std::net::TcpStream;
use std::path::Path;

use ssh2::Session;

use crate::config::{HOST, USER};

/// Connect to the reMarkable and run either the grabber (if use_grab and pidfile given) or `cat`
/// on the device, returning the channel to read from. The session must be kept alive while reading.
/// When using the grabber, the remote process holds EVIOCGRAB so the reMarkable UI does not see input.
pub fn open_input_stream(
    device_path: &str,
    key_path: &Path,
    use_grab: bool,
    pidfile: Option<&str>,
) -> Result<(Session, impl Read + Send), Box<dyn std::error::Error + Send + Sync>> {
    log::info!("SSH connecting to {}…", HOST);
    let tcp = TcpStream::connect((HOST, 22))?;
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake()?;
    sess.userauth_pubkey_file(USER, None, key_path, None)?;
    if !sess.authenticated() {
        return Err("SSH auth failed".into());
    }
    let mut channel = sess.channel_session()?;
    let cmd = if use_grab && pidfile.is_some() {
        let path = crate::config::GRABBER_PATH;
        let pid = pidfile.unwrap();
        let alive = crate::config::ALIVE_FILE;
        let stale = crate::config::STALE_SEC;
        log::info!(
            "SSH connected, running grabber {} --device {} --pidfile {} --alive-file {} --stale-sec {}…",
            path, device_path, pid, alive, stale
        );
        format!(
            "{} --device {} --pidfile {} --alive-file {} --stale-sec {}",
            path, device_path, pid, alive, stale
        )
    } else {
        log::info!("SSH connected, running cat {}…", device_path);
        format!("cat {}", device_path)
    };
    channel.exec(&cmd)?;
    channel.handle_extended_data(ssh2::ExtendedData::Merge)?;
    log::info!("stream ready for {}", device_path);
    Ok((sess, channel))
}

/// Run a single command on the reMarkable (e.g. `touch /tmp/rm-mouse-alive` for keepalive).
/// Used so the watchdog can detect that the host is still connected.
pub fn run_command(
    key_path: &Path,
    command: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let tcp = TcpStream::connect((HOST, 22))?;
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake()?;
    sess.userauth_pubkey_file(USER, None, key_path, None)?;
    if !sess.authenticated() {
        return Err("SSH auth failed".into());
    }
    let mut channel = sess.channel_session()?;
    channel.exec(command)?;
    channel.wait_close()?;
    Ok(())
}
