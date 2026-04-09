//! PC-side screen mirror: PipeWire (via portal) → dirty regions → LZ4 → TCP to tablet.

use std::io::Write;
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::Child;
use std::sync::mpsc;
use std::time::Duration;

use ashpd::desktop::PersistMode;
use clap::Parser;
use lamco_pipewire::damage::{DamageConfig, DamageDetector, DetectedRegion};
use lamco_pipewire::format::PixelFormat;
use lamco_pipewire::{PipeWireConfig, PipeWireManager, SourceType, StreamInfo, VideoFrame};
use lamco_portal::{PortalConfig, PortalManager};
use log::{error, info, warn};
use rm_common::config::Config;
use rm_common::device::DeviceProfile;
use rm_common::grab;
use rm_common::protocol::UpdateHeader;
use rm_common::screen_client::{ensure_client_on_device, load_client_binary, spawn_reverse_tunnel};
use rm_common::ssh;
use socket2::{Socket, TcpKeepalive};

type DynResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const WAVEFORM_DU: u8 = 1;

#[derive(Parser)]
#[command(name = "rm-screen")]
#[command(about = "Mirror PC screen to reMarkable over PipeWire and custom TCP")]
struct ScreenCli {
    /// reMarkable host (IP or hostname)
    #[arg(long, env = "RMPAD_HOST")]
    host: Option<String>,

    #[arg(long)]
    key_path: Option<String>,

    #[arg(long, env = "RMPAD_PASSWORD")]
    password: Option<String>,

    #[arg(long, env = "RMPAD_CONFIG")]
    config: Option<PathBuf>,

    /// Cross-compiled rm-client-screen binary (armv7 or aarch64)
    #[arg(long, env = "RM_CLIENT_SCREEN_BIN")]
    client_binary: PathBuf,

    /// Local port the tablet connects to (via reverse SSH)
    #[arg(long, default_value_t = 9876)]
    local_port: u16,

    /// Remote port on the tablet side (forwarded to local_port)
    #[arg(long, default_value_t = 9876)]
    remote_port: u16,
}

#[tokio::main]
async fn main() -> DynResult<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let screen_cli = ScreenCli::parse();
    let device = DeviceProfile::current();
    let config = load_merged_config(&screen_cli, device);

    info!("Connecting to {} for device detection…", config.host);
    let session = ssh::connect_for_detection(&config)?;
    let device = DeviceProfile::detect_via_ssh(&session)?;
    info!("Device: {}", device.name);

    let _arch = grab::detect_arch(&session)?;
    let client_bytes = load_client_binary(&screen_cli.client_binary)?;
    ensure_client_on_device(&session, &client_bytes)?;
    drop(session);

    let listener = TcpListener::bind(("127.0.0.1", screen_cli.local_port))?;

    lamco_pipewire::init();

    // Remote-desktop portal sessions must not request persistence (GNOME/KDE reject it).
    let portal_config = PortalConfig::builder()
        .persist_mode(PersistMode::DoNot)
        .build();
    let portal = PortalManager::new(portal_config).await?;
    let (portal_session, _token) = portal
        .create_session("rm-screen".to_string(), None)
        .await
        .map_err(|e| format!("portal session failed: {e}"))?;

    let pw_fd = unsafe { OwnedFd::from_raw_fd(portal_session.pipewire_fd()) };

    let pw_config = PipeWireConfig::builder()
        .use_dmabuf(false)
        .preferred_format(PixelFormat::BGRA)
        .enable_damage_tracking(true)
        .frame_buffer_size(8)
        .build();

    let mut pw = PipeWireManager::new(pw_config)?;
    pw.connect(pw_fd).await?;

    let streams = portal_session.streams();
    if streams.is_empty() {
        return Err("portal returned no PipeWire streams".into());
    }
    let s0 = &streams[0];
    let stream_info = StreamInfo {
        node_id: s0.node_id,
        position: s0.position,
        size: s0.size,
        source_type: match s0.source_type {
            lamco_portal::SourceType::Monitor => SourceType::Monitor,
            lamco_portal::SourceType::Window => SourceType::Window,
            lamco_portal::SourceType::Virtual => SourceType::Virtual,
        },
    };

    let handle = pw.create_stream(&stream_info).await?;
    let mut rx = pw
        .frame_receiver(handle.id)
        .await
        .ok_or("frame receiver already taken")?;

    let remote_cmd = format!(
        "exec {} 127.0.0.1 {}",
        rm_common::screen_client::REMOTE_CLIENT_PATH,
        screen_cli.remote_port
    );
    let mut tunnel: Child = spawn_reverse_tunnel(
        &config,
        screen_cli.remote_port,
        "127.0.0.1",
        screen_cli.local_port,
        &remote_cmd,
    )?;

    let (tx, rx_sock) = mpsc::channel::<std::io::Result<std::net::TcpStream>>();
    let lis = listener;
    std::thread::spawn(move || {
        let _ = tx.send(lis.accept().map(|(s, _)| s));
    });

    let sock = match rx_sock.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("TCP accept failed: {e}").into()),
        Err(_) => {
            let _ = tunnel.kill();
            return Err("timed out waiting for tablet TCP connection (check SSH reverse tunnel)".into());
        }
    };
    configure_tcp_stream(&sock)?;

    let mut sock = sock;
    let mut damage = DamageDetector::new(DamageConfig::low_bandwidth());

    while let Some(frame) = rx.recv().await {
        if let Err(e) = process_frame(&mut sock, &mut damage, &frame) {
            error!("stream error: {e}");
            break;
        }
    }

    let _ = tunnel.kill();
    let _ = tunnel.wait();
    pw.shutdown().await.ok();
    lamco_pipewire::deinit();
    portal.cleanup().await.ok();

    Ok(())
}

fn load_merged_config(cli: &ScreenCli, device: &'static DeviceProfile) -> Config {
    let fake_cli = rm_common::config::Cli {
        command: None,
        host: cli.host.clone(),
        key_path: cli.key_path.clone(),
        password: cli.password.clone(),
        pen_device: None,
        touch_device: None,
        touch_only: false,
        pen_only: false,
        grab_input: true,
        no_grab_input: false,
        no_palm_rejection: false,
        palm_grace_ms: None,
        orientation: None,
        config: cli.config.clone(),
    };
    Config::load(&fake_cli, device)
}

fn configure_tcp_stream(sock: &std::net::TcpStream) -> DynResult<()> {
    sock.set_nodelay(true)?;
    sock.set_write_timeout(Some(Duration::from_secs(30)))?;
    let raw = sock.as_raw_fd();
    let s = unsafe { Socket::from_raw_fd(raw) };
    let ka = TcpKeepalive::new().with_time(Duration::from_secs(30));
    s.set_tcp_keepalive(&ka)?;
    std::mem::forget(s);
    Ok(())
}

fn process_frame(
    sock: &mut std::net::TcpStream,
    damage: &mut DamageDetector,
    frame: &VideoFrame,
) -> DynResult<()> {
    let Some(data_arc) = frame.data() else {
        warn!("DMA-BUF frame skipped (no CPU pixels); disable GPU-only capture in compositor if this repeats");
        damage.invalidate();
        return Ok(());
    };
    let data = data_arc.as_slice();
    let w = frame.width;
    let h = frame.height;

    let regions = regions_for_frame(frame, damage, data, w, h);

    for r in regions {
        let (x, y, rw, rh) = clip_region(r, w, h);
        if rw < 2 || rh < 1 {
            continue;
        }
        let rw = rw & !1;
        let packed = pack_region_gray4(data, frame.stride, frame.format, x, y, rw, rh)?;
        let compressed = lz4_flex::block::compress_prepend_size(&packed);
        let header = UpdateHeader {
            x,
            y,
            width: rw,
            height: rh,
            waveform: WAVEFORM_DU,
            payload_size: compressed.len() as u32,
        };
        sock.write_all(&header.to_bytes())?;
        sock.write_all(&compressed)?;
        sock.flush()?;
    }

    Ok(())
}

fn regions_for_frame(
    frame: &VideoFrame,
    damage: &mut DamageDetector,
    data: &[u8],
    w: u32,
    h: u32,
) -> Vec<DetectedRegion> {
    let mut from_meta: Vec<DetectedRegion> = frame
        .damage_regions
        .iter()
        .filter(|d| d.is_valid())
        .map(|d| DetectedRegion {
            x: d.x.max(0) as u32,
            y: d.y.max(0) as u32,
            width: d.width,
            height: d.height,
        })
        .collect();

    if !from_meta.is_empty() {
        from_meta.retain(|r| r.x < w && r.y < h);
        for r in &mut from_meta {
            r.width = r.width.min(w.saturating_sub(r.x));
            r.height = r.height.min(h.saturating_sub(r.y));
        }
        from_meta.retain(|r| r.width >= 2 && r.height >= 1);
        if !from_meta.is_empty() {
            return from_meta;
        }
    }

    damage.detect(data, w, h)
}

fn clip_region(r: DetectedRegion, w: u32, h: u32) -> (u16, u16, u16, u16) {
    let x = r.x.min(w.saturating_sub(1));
    let y = r.y.min(h.saturating_sub(1));
    let rw = r.width.min(w.saturating_sub(x));
    let rh = r.height.min(h.saturating_sub(y));
    (x as u16, y as u16, rw as u16, rh as u16)
}

fn gray_from_pixel(format: PixelFormat, chunk: &[u8]) -> Option<u8> {
    if chunk.len() < 3 {
        return None;
    }
    let (r, g, b) = match format {
        PixelFormat::BGRA | PixelFormat::BGRx => (chunk[2], chunk[1], chunk[0]),
        PixelFormat::RGBA | PixelFormat::RGBx => (chunk[0], chunk[1], chunk[2]),
        PixelFormat::RGB => {
            if chunk.len() < 3 {
                return None;
            }
            (chunk[0], chunk[1], chunk[2])
        }
        PixelFormat::BGR => (chunk[2], chunk[1], chunk[0]),
        PixelFormat::GRAY8 => return Some(chunk[0]),
        _ => {
            if chunk.len() >= 4 {
                (chunk[2], chunk[1], chunk[0])
            } else {
                return None;
            }
        }
    };
    Some(((r as u16 * 77 + g as u16 * 150 + b as u16 * 29) >> 8) as u8)
}

fn pack_region_gray4(
    data: &[u8],
    stride: u32,
    format: PixelFormat,
    ox: u16,
    oy: u16,
    w: u16,
    h: u16,
) -> DynResult<Vec<u8>> {
    let bpp = format.bytes_per_pixel().max(3);
    let stride = stride as usize;
    let mut out = Vec::with_capacity((w as usize / 2) * h as usize);
    for row in 0..h {
        let y = oy as usize + row as usize;
        let row_off = y * stride;
        for col in (0..w).step_by(2) {
            let x0 = ox as usize + col as usize;
            let x1 = x0 + 1;
            let o0 = row_off + x0 * bpp;
            let o1 = row_off + x1 * bpp;
            let g0 = gray_from_pixel(format, &data[o0..data.len().min(o0 + bpp)]).unwrap_or(0);
            let g1 = if x1 * bpp <= stride {
                gray_from_pixel(format, &data[o1..data.len().min(o1 + bpp)]).unwrap_or(0)
            } else {
                0
            };
            let n0 = (g0 >> 4) & 0x0f;
            let n1 = (g1 >> 4) & 0x0f;
            out.push((n0 << 4) | n1);
        }
    }
    Ok(out)
}
