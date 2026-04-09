//! Upload and manage the evgrab helper binary on the reMarkable.
//!
//! The evgrab helper is a tiny static ARM binary that exclusively grabs an
//! evdev device via EVIOCGRAB and pipes events to stdout. This prevents
//! xochitl from seeing input without stopping the process (which would
//! trigger the watchdog).
//!
//! Binaries for both armv7 (rM2) and aarch64 (rMPP/rMPM) are embedded at
//! compile time and the correct one is uploaded over SSH on first connect.

use std::fmt;
use std::io::{Read, Write};

use sha2::{Digest, Sha256};
use ssh2::Session;

const GRAB_ARMV7: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/evgrab-armv7"));
const GRAB_AARCH64: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/evgrab-aarch64"));

const REMOTE_PATH: &str = "/tmp/rm-pad-grab";

#[derive(Debug, Clone, Copy)]
pub enum Arch {
    Armv7,
    Aarch64,
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Arch::Armv7 => write!(f, "armv7"),
            Arch::Aarch64 => write!(f, "aarch64"),
        }
    }
}

/// Detect the tablet's CPU architecture by running `uname -m` over SSH.
pub fn detect_arch(session: &Session) -> Result<Arch, Box<dyn std::error::Error + Send + Sync>> {
    let mut channel = session.channel_session()?;
    channel.exec("uname -m")?;

    let mut output = String::new();
    channel.read_to_string(&mut output)?;

    // Explicitly close our end so the session is left in a clean state
    // for subsequent channels (SFTP, exec, etc.).
    channel.close()?;
    channel.wait_close()?;

    match output.trim() {
        "armv7l" => Ok(Arch::Armv7),
        "aarch64" => Ok(Arch::Aarch64),
        other => Err(format!("Unsupported tablet architecture: {}", other).into()),
    }
}

/// Compute SHA256 hash of the embedded binary for the given architecture.
fn compute_binary_hash(arch: Arch) -> String {
    let binary = match arch {
        Arch::Armv7 => GRAB_ARMV7,
        Arch::Aarch64 => GRAB_AARCH64,
    };
    let mut hasher = Sha256::new();
    hasher.update(binary);
    format!("{:x}", hasher.finalize())
}

/// Check if the remote binary exists and matches our embedded binary hash.
/// Returns Ok(true) if hash matches, Ok(false) if file doesn't exist or hash doesn't match.
fn check_remote_binary_hash(
    session: &Session,
    arch: Arch,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let expected_hash = compute_binary_hash(arch);
    
    let mut channel = session.channel_session()?;
    channel.exec(&format!("sha256sum {} 2>/dev/null | cut -d' ' -f1", REMOTE_PATH))?;

    let mut output = String::new();
    channel.read_to_string(&mut output)?;
    channel.close()?;
    channel.wait_close()?;

    let status = channel.exit_status()?;
    if status != 0 {
        // File doesn't exist or command failed
        log::debug!("Remote binary not found or sha256sum failed");
        return Ok(false);
    }

    let remote_hash = output.trim();
    if remote_hash == expected_hash {
        log::debug!("Remote binary hash matches: {}", &expected_hash[..16]);
        Ok(true)
    } else {
        log::debug!(
            "Remote binary hash mismatch: expected {}..., got {}...",
            &expected_hash[..16],
            &remote_hash[..16.min(remote_hash.len())]
        );
        Ok(false)
    }
}

/// Remove the remote binary if it exists.
fn remove_remote_binary(
    session: &Session,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut channel = session.channel_session()?;
    channel.exec(&format!("rm -f {}", REMOTE_PATH))?;

    channel.close()?;
    channel.wait_close()?;

    let status = channel.exit_status()?;
    if status != 0 {
        return Err(format!("Failed to remove remote binary (exit status {})", status).into());
    }

    log::debug!("Removed remote binary");
    Ok(())
}

/// Upload the correct grab helper binary to the tablet.
///
/// Pipes the binary through `cat` into a file on the tablet and marks it
/// executable. This avoids the SFTP subsystem which can hang on some
/// SSH implementations.
pub fn upload_helper(
    session: &Session,
    arch: Arch,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let binary = match arch {
        Arch::Armv7 => GRAB_ARMV7,
        Arch::Aarch64 => GRAB_AARCH64,
    };

    log::info!(
        "Uploading grab helper ({}, {} bytes) to {}",
        arch,
        binary.len(),
        REMOTE_PATH
    );

    let mut channel = session.channel_session()?;
    // Write to a PID-unique temp file and atomically rename into place.
    // This avoids corruption when pen and touch threads upload concurrently.
    channel.exec(&format!(
        "cat > {path}.$$ && chmod +x {path}.$$ && mv -f {path}.$$ {path}",
        path = REMOTE_PATH
    ))?;

    channel.write_all(binary)?;
    channel.send_eof()?;
    channel.wait_eof()?;
    channel.close()?;
    channel.wait_close()?;

    let status = channel.exit_status()?;
    if status != 0 {
        return Err(format!("Failed to upload grab helper (exit status {})", status).into());
    }

    log::info!("Grab helper uploaded successfully");
    Ok(())
}

/// Ensure the remote binary exists and matches our embedded binary.
/// Removes and re-uploads if hash doesn't match.
pub fn ensure_binary_valid(
    session: &Session,
    arch: Arch,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match check_remote_binary_hash(session, arch)? {
        true => {
            log::debug!("Using existing remote binary (hash verified)");
            Ok(())
        }
        false => {
            log::info!("Remote binary missing or hash mismatch, removing and re-uploading");
            remove_remote_binary(session)?;
            upload_helper(session, arch)
        }
    }
}

/// Build the remote command that grabs a device and streams events.
///
/// Stderr is redirected to a log file on the tablet for diagnostics.
/// Uses `exec` to replace the shell with the grab helper so that signal
/// delivery (on SSH disconnect) goes directly to the right process.
pub fn grab_command(device_path: &str) -> String {
    format!(
        "exec {} {} 2>>{}.log",
        REMOTE_PATH, device_path, REMOTE_PATH
    )
}
