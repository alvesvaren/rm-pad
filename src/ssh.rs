use std::net::TcpStream;

use ssh2::Session;

use crate::config::{Auth, Config};
use crate::grab;

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
    let tcp = TcpStream::connect((config.host.as_str(), SSH_PORT))?;
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
    // Kill any existing rm-mouse-grab processes before starting a new one
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
