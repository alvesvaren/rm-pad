//! Upload and run the tablet screen client; reverse SSH tunnel for TCP.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use sha2::{Digest, Sha256};
use ssh2::Session;

use crate::config::{Auth, Config};

pub const REMOTE_CLIENT_PATH: &str = "/tmp/rm-client-screen";

fn compute_hash(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn remote_hash_matches(session: &Session, expected: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let mut channel = session.channel_session()?;
    channel.exec(&format!(
        "sha256sum {} 2>/dev/null | cut -d' ' -f1",
        REMOTE_CLIENT_PATH
    ))?;
    let mut output = String::new();
    channel.read_to_string(&mut output)?;
    channel.close()?;
    channel.wait_close()?;
    if channel.exit_status()? != 0 {
        return Ok(false);
    }
    Ok(output.trim() == expected)
}

fn upload_bytes(
    session: &Session,
    data: &[u8],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut channel = session.channel_session()?;
    let path = REMOTE_CLIENT_PATH;
    channel.exec(&format!(
        "cat > {path}.$$ && chmod +x {path}.$$ && mv -f {path}.$$ {path}"
    ))?;
    channel.write_all(data)?;
    channel.send_eof()?;
    channel.wait_eof()?;
    channel.close()?;
    channel.wait_close()?;
    if channel.exit_status()? != 0 {
        return Err("upload rm-client-screen failed".into());
    }
    Ok(())
}

/// Ensure `/tmp/rm-client-screen` exists and matches `binary` bytes.
pub fn ensure_client_on_device(
    session: &Session,
    binary: &[u8],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let h = compute_hash(binary);
    if remote_hash_matches(session, &h)? {
        log::debug!("rm-client-screen already up to date on device");
        return Ok(());
    }
    log::info!(
        "Uploading rm-client-screen ({} bytes, sha256 {}...) to {}",
        binary.len(),
        &h[..16.min(h.len())],
        REMOTE_CLIENT_PATH
    );
    upload_bytes(session, binary)?;
    Ok(())
}

/// Build `ssh` argv for key-based auth (password auth is not supported for the tunnel subprocess).
pub fn ssh_base_args(config: &Config) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    match config.auth() {
        Auth::Key(path) => {
            let expanded = crate::config::expand_tilde(path.to_string_lossy().as_ref());
            Ok(vec![
                "-i".into(),
                expanded.to_string_lossy().into_owned(),
            ])
        }
        Auth::Password(_) => Err(
            "rm-screen reverse tunnel needs SSH public-key auth (password-only is not supported for OpenSSH -R). \
             Set key_path in rm-pad.toml or use --key-path."
                .into(),
        ),
    }
}

fn ssh_tunnel_args(
    config: &Config,
    remote_forward_port: u16,
    local_host: &str,
    local_port: u16,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let mut args = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-p".into(),
        "22".into(),
    ];
    args.extend(ssh_base_args(config)?);
    args.push("-R".into());
    args.push(format!("{}:{}:{}", remote_forward_port, local_host, local_port));
    args.push(format!("root@{}", config.host));
    Ok(args)
}

/// Run a remote shell command over SSH without TCP port forwarding (direct TCP mode).
pub fn spawn_remote_exec(
    config: &Config,
    remote_cmd: &str,
) -> Result<Child, Box<dyn std::error::Error + Send + Sync>> {
    let mut args = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-p".into(),
        "22".into(),
    ];
    args.extend(ssh_base_args(config)?);
    args.push(format!("root@{}", config.host));
    args.push(remote_cmd.into());

    log::info!("Starting remote screen client: ssh {}", args.join(" "));

    let child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    Ok(child)
}

/// Spawn `ssh -R remote_port:local_host:local_port ... exec remote_cmd`.
/// Keep the child alive for the lifetime of the screen session; kill it on shutdown.
pub fn spawn_reverse_tunnel(
    config: &Config,
    remote_forward_port: u16,
    local_host: &str,
    local_port: u16,
    remote_cmd: &str,
) -> Result<Child, Box<dyn std::error::Error + Send + Sync>> {
    let mut args = ssh_tunnel_args(config, remote_forward_port, local_host, local_port)?;
    args.push(remote_cmd.into());

    log::info!("Starting reverse SSH tunnel: ssh {}", args.join(" "));

    let child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    Ok(child)
}

/// Spawn an SSH tunnel (`-R` + `-N`) without running a remote command.
/// Used when the tablet-side client is launched from AppLoad instead of SSH.
pub fn spawn_tunnel_only(
    config: &Config,
    remote_forward_port: u16,
    local_host: &str,
    local_port: u16,
) -> Result<Child, Box<dyn std::error::Error + Send + Sync>> {
    let mut args = ssh_tunnel_args(config, remote_forward_port, local_host, local_port)?;
    args.push("-N".into());

    log::info!("Starting SSH tunnel (no remote command): ssh {}", args.join(" "));

    let child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    Ok(child)
}

/// Shell snippet to run the tablet client: logging env + binary + args.
///
/// Deliberately **no** leading `exec`: [`wrap_xochitl_exclusive_command`] wraps this in `sh -lc`
/// so `xochitl` can be restarted on exit.
///
/// `connect_host` / `connect_port` are where the tablet reaches the PC (`127.0.0.1` + reverse
/// tunnel port, or LAN/USB IP + `--local-port` in direct TCP mode).
pub fn remote_screen_command(
    connect_host: &str,
    connect_port: u16,
    src_w: u32,
    src_h: u32,
    latency_log: bool,
) -> String {
    let rust_log = if latency_log {
        "info,rm_mirror_latency=info"
    } else {
        "info"
    };
    let latency_env = if latency_log {
        "RM_MIRROR_LATENCY_LOG=1 "
    } else {
        ""
    };
    format!(
        "RUST_LOG={rust_log} {latency_env}{} {} {} {} {} 2>/tmp/rm-client-screen.log",
        REMOTE_CLIENT_PATH, connect_host, connect_port, src_w, src_h,
    )
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

pub fn wrap_xochitl_exclusive_command(client_cmd: &str) -> String {
    let script = format!(
        "echo '[rm-screen] exclusive mirror: stopping xochitl (restarts when client exits)' >&2; \
         had_xochitl=0; \
         if systemctl is-active --quiet xochitl; then \
           had_xochitl=1; \
           systemctl stop xochitl || true; \
         fi; \
         cleanup() {{ \
           if [ \"$had_xochitl\" -eq 1 ]; then \
             echo '[rm-screen] restarting xochitl' >&2; \
             systemctl start xochitl || true; \
           fi; \
         }}; \
         trap cleanup EXIT INT TERM; \
         {client_cmd}; \
         status=$?; \
         exit $status"
    );
    format!("sh -lc {}", shell_single_quote(&script))
}

/// Load client binary for the given tablet architecture from disk.
pub fn load_client_binary(path: &Path) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let data = std::fs::read(path)?;
    if data.is_empty() {
        return Err(format!("client binary is empty: {}", path.display()).into());
    }
    Ok(data)
}
