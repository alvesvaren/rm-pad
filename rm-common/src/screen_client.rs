//! Upload and run the tablet screen client; reverse SSH tunnel for TCP.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use sha2::{Digest, Sha256};
use ssh2::Session;

use crate::config::{Auth, Config};

pub const REMOTE_CLIENT_PATH: &str = "/tmp/rm-client-screen";

#[derive(Debug, Clone)]
pub struct QtfbShimConfig {
    pub input: bool,
    pub model: bool,
    pub mode: String,
    pub dont_grab_input: bool,
}

impl Default for QtfbShimConfig {
    fn default() -> Self {
        Self {
            input: false,
            model: false,
            mode: "RGB565".to_string(),
            dont_grab_input: true,
        }
    }
}

impl QtfbShimConfig {
    fn bool_str(v: bool) -> &'static str {
        if v {
            "true"
        } else {
            "false"
        }
    }

    fn shell_env_pairs(&self) -> Vec<(String, String)> {
        vec![
            ("QTFB_SHIM_INPUT".to_string(), Self::bool_str(self.input).to_string()),
            ("QTFB_SHIM_MODEL".to_string(), Self::bool_str(self.model).to_string()),
            ("QTFB_SHIM_MODE".to_string(), self.mode.clone()),
            (
                "KO_DONT_GRAB_INPUT".to_string(),
                if self.dont_grab_input { "1" } else { "0" }.to_string(),
            ),
            (
                "LIBREMARKABLE_FB_DISFAVOR_INTERNAL_RM2FB".to_string(),
                "1".to_string(),
            ),
        ]
    }

    fn shell_env_prefix(&self) -> String {
        self.shell_env_pairs()
            .into_iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn manifest_env_lines(&self) -> String {
        self.shell_env_pairs()
            .into_iter()
            .map(|(key, value)| format!(r#",
    "{}": "{}""#, key, value))
            .collect::<String>()
    }
}

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

/// Framebuffer shim needed on the reMarkable 2, where the raw EPDC is not
/// directly usable by third-party apps.  Two mechanisms exist in the wild:
///
/// 1. **qtfb-shim** (AppLoad / Vellum) — the modern path.  Needs
///    `LD_PRELOAD` plus `QTFB_SHIM_MODEL` and `QTFB_SHIM_INPUT_MODE` env vars.
/// 2. **librm2fb_client.so** (Toltec / rm2fb) — the legacy path.  Needs
///    `LD_PRELOAD` only.
#[derive(Debug, Clone)]
pub enum FbShim {
    /// AppLoad qtfb-shim at the given path (e.g. `/home/root/shims/qtfb-shim.so`).
    QtfbShim(String),
    /// Legacy rm2fb client library (e.g. `/opt/lib/librm2fb_client.so`).
    Rm2fb(String),
}

const QTFB_SHIM_PATH: &str = "/home/root/shims/qtfb-shim.so";
const RM2FB_CLIENT_LIB: &str = "/opt/lib/librm2fb_client.so";

fn remote_file_exists(session: &Session, path: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let mut channel = session.channel_session()?;
    channel.exec(&format!("test -f {path} && echo found"))?;
    let mut output = String::new();
    channel.read_to_string(&mut output)?;
    channel.close()?;
    channel.wait_close()?;
    Ok(output.trim() == "found")
}

/// Probe the device for a usable framebuffer shim.
/// Checks for the AppLoad qtfb-shim first, then falls back to the legacy
/// rm2fb client library.
pub fn detect_fb_shim(session: &Session) -> Result<Option<FbShim>, Box<dyn std::error::Error + Send + Sync>> {
    if remote_file_exists(session, QTFB_SHIM_PATH)? {
        log::info!("AppLoad qtfb-shim detected at {}", QTFB_SHIM_PATH);
        return Ok(Some(FbShim::QtfbShim(QTFB_SHIM_PATH.to_string())));
    }
    if remote_file_exists(session, RM2FB_CLIENT_LIB)? {
        log::info!("rm2fb client library detected at {}", RM2FB_CLIENT_LIB);
        return Ok(Some(FbShim::Rm2fb(RM2FB_CLIENT_LIB.to_string())));
    }
    log::debug!("no framebuffer shim found (checked {} and {})", QTFB_SHIM_PATH, RM2FB_CLIENT_LIB);
    Ok(None)
}

/// Build the `exec …` remote command for the screen client, setting the
/// appropriate environment variables for the detected framebuffer shim.
///
/// `connect_host` / `connect_port` are where the tablet process reaches the PC listener
/// (`127.0.0.1` + remote forwarded port over SSH reverse tunnel, or the PC LAN/USB IP
/// + `local_port` for direct TCP).
pub fn remote_screen_command(
    connect_host: &str,
    connect_port: u16,
    src_w: u32,
    src_h: u32,
    shim: Option<&FbShim>,
    latency_log: bool,
    qtfb: &QtfbShimConfig,
    exec_client: bool,
) -> String {
    let env_prefix = match shim {
        Some(FbShim::QtfbShim(path)) => format!("LD_PRELOAD={path} {} ", qtfb.shell_env_prefix()),
        Some(FbShim::Rm2fb(path)) => format!("LD_PRELOAD={path} "),
        None => String::new(),
    };
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
    let exec_prefix = if exec_client { "exec " } else { "" };
    format!(
        "RUST_LOG={rust_log} {latency_env}{env_prefix}{exec_prefix}{} {} {} {} {} 2>/tmp/rm-client-screen.log",
        REMOTE_CLIENT_PATH, connect_host, connect_port, src_w, src_h,
    )
}

const APPLOAD_APP_DIR: &str = "/home/root/xovi/exthome/appload/rm-screen";

/// Install (or update) an AppLoad `external.manifest.json` so the screen
/// client appears in the AppLoad launcher and the user can close it from
/// the UI with the standard top-swipe gesture.
///
/// The manifest includes connection args (port, capture dimensions) so the
/// client knows where to connect when launched from AppLoad.
pub fn ensure_appload_manifest(
    session: &Session,
    shim: &FbShim,
    host: &str,
    port: u16,
    src_w: u32,
    src_h: u32,
    latency_log: bool,
    qtfb: &QtfbShimConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (preload, extra_env) = match shim {
        FbShim::QtfbShim(path) => (path.as_str(), qtfb.manifest_env_lines()),
        FbShim::Rm2fb(path) => (path.as_str(), String::new()),
    };
    let rust_log = if latency_log {
        "info,rm_mirror_latency=info"
    } else {
        "info"
    };
    let latency_env = if latency_log {
        r#",
    "RM_MIRROR_LATENCY_LOG": "1""#
    } else {
        ""
    };

    let manifest = format!(
        r#"{{
  "name": "Screen Mirror",
  "application": "{}",
  "args": ["{}", "{}", "{}", "{}"],
  "environment": {{
    "LD_PRELOAD": "{}",
    "RUST_LOG": "{}"{}{}
  }},
  "qtfb": true
}}"#,
        REMOTE_CLIENT_PATH, host, port, src_w, src_h, preload, rust_log, latency_env, extra_env,
    );

    let cmd = format!(
        "mkdir -p {dir} && cat > {dir}/external.manifest.json",
        dir = APPLOAD_APP_DIR,
    );
    let mut channel = session.channel_session()?;
    channel.exec(&cmd)?;
    channel.write_all(manifest.as_bytes())?;
    channel.send_eof()?;
    channel.wait_eof()?;
    channel.close()?;
    channel.wait_close()?;
    if channel.exit_status()? != 0 {
        return Err("failed to write AppLoad manifest".into());
    }
    log::info!("AppLoad manifest installed at {}/external.manifest.json", APPLOAD_APP_DIR);
    Ok(())
}

pub fn describe_shim_launch_env(shim: Option<&FbShim>, qtfb: &QtfbShimConfig) -> String {
    match shim {
        Some(FbShim::QtfbShim(path)) => {
            format!("LD_PRELOAD={path} {}", qtfb.shell_env_prefix())
        }
        Some(FbShim::Rm2fb(path)) => format!("LD_PRELOAD={path}"),
        None => "(no shim env)".to_string(),
    }
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

pub fn wrap_xochitl_exclusive_command(client_cmd: &str) -> String {
    let script = format!(
        "had_xochitl=0; \
         if systemctl is-active --quiet xochitl; then \
           had_xochitl=1; \
           systemctl stop xochitl || true; \
         fi; \
         cleanup() {{ \
           if [ \"$had_xochitl\" -eq 1 ]; then \
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
