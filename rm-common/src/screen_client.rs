//! Upload and run the tablet screen client; reverse SSH tunnel for TCP.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use sha2::{Digest, Sha256};
use ssh2::Session;

use crate::config::{Auth, Config};
use crate::grab::Arch;

pub const REMOTE_CLIENT_PATH: &str = "/tmp/rm-client-screen";

/// ELF `e_machine` values we care about.
const EM_ARM: u16 = 40;
const EM_AARCH64: u16 = 183;

/// Ensure `data` is an ELF executable built for the tablet architecture.
///
/// Catches the common mistake of passing `target/debug/rm-client-screen` built for the host
/// (e.g. x86_64), which cannot run on the reMarkable and causes the reverse tunnel to never connect.
pub fn validate_client_elf_for_arch(data: &[u8], arch: Arch) -> Result<(), String> {
    if data.len() < 20 {
        return Err("client binary too small to be a valid ELF".into());
    }
    if data[0] != 0x7f || data[1] != b'E' || data[2] != b'L' || data[3] != b'F' {
        return Err("client binary is not an ELF file (did you pass the right path?)".into());
    }
    let ei_class = data[4];
    if ei_class != 1 && ei_class != 2 {
        return Err("unsupported ELF class".into());
    }
    let machine = u16::from_le_bytes([data[18], data[19]]);
    let ok = match arch {
        Arch::Armv7 => machine == EM_ARM,
        Arch::Aarch64 => machine == EM_AARCH64,
    };
    if !ok {
        let arch_name = match arch {
            Arch::Armv7 => "armv7 (32-bit ARM, e_machine=40)",
            Arch::Aarch64 => "aarch64 (e_machine=183)",
        };
        return Err(format!(
            "client binary ELF architecture does not match tablet ({arch_name}); got e_machine={machine}. \
             Build for the tablet, e.g. `cargo build -p rm-client-screen --release --target armv7-unknown-linux-gnueabihf` \
             (reMarkable 2) or `--target aarch64-unknown-linux-gnu` (Paper Pro), then pass that binary to --client-binary."
        ));
    }
    Ok(())
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

/// Spawn `ssh -R remote_port:local_host:local_port ... exec remote_cmd`.
/// Keep the child alive for the lifetime of the screen session; kill it on shutdown.
///
/// When `stderr_inherit` is true, the child's stderr is inherited so OpenSSH messages
/// (e.g. "Exec format error", permission denied) show in the terminal.
pub fn spawn_reverse_tunnel(
    config: &Config,
    remote_forward_port: u16,
    local_host: &str,
    local_port: u16,
    remote_cmd: &str,
    stderr_inherit: bool,
) -> Result<Child, Box<dyn std::error::Error + Send + Sync>> {
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
    args.push(remote_cmd.into());

    log::info!("Starting reverse SSH tunnel: ssh {}", args.join(" "));

    let stderr = if stderr_inherit {
        Stdio::inherit()
    } else {
        Stdio::piped()
    };

    let child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()?;

    Ok(child)
}

/// Load client binary for the given tablet architecture from disk.
pub fn load_client_binary(path: &Path) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let data = std::fs::read(path)?;
    if data.is_empty() {
        return Err(format!("client binary is empty: {}", path.display()).into());
    }
    Ok(data)
}
