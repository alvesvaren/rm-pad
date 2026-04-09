//! PC-side screen mirror: PipeWire (via portal) → dirty regions → LZ4 → TCP to tablet.

use std::io::Write;
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::Child;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ashpd::desktop::PersistMode;
use clap::Parser;
use lamco_pipewire::damage::{DamageConfig, DamageDetector, DetectedRegion};
use lamco_pipewire::format::PixelFormat;
use lamco_pipewire::{
    PipeWireConfig, PipeWireThreadCommand, PipeWireThreadManager, SourceType, StreamConfig, StreamInfo, VideoFrame,
};
use lamco_portal::{PortalConfig, PortalManager};
use log::{debug, error, info, warn};
use rm_common::config::Config;
use rm_common::device::DeviceProfile;
use rm_common::grab;
use rm_common::protocol::{UpdateHeader, HEADER_SIZE};
use rm_common::screen_client::{self, ensure_client_on_device, load_client_binary, spawn_reverse_tunnel};
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

    info!(
        "rm-screen: listening for tablet on 127.0.0.1:{} (SSH reverse → {}:{} on device)",
        screen_cli.local_port, config.host, screen_cli.remote_port
    );

    info!("Connecting to {} for device detection…", config.host);
    let session = ssh::connect_for_detection(&config)?;
    let device = DeviceProfile::detect_via_ssh(&session)?;
    info!("Device: {}", device.name);

    let _arch = grab::detect_arch(&session)?;
    let client_bytes = load_client_binary(&screen_cli.client_binary)?;
    ensure_client_on_device(&session, &client_bytes)?;
    let fb_shim = screen_client::detect_fb_shim(&session)?;
    let appload_launch = matches!(&fb_shim, Some(screen_client::FbShim::QtfbShim(_)));
    if appload_launch {
        info!("qtfb-shim detected — client will be launched from AppLoad on the tablet");
    }
    match &fb_shim {
        None if device.name == "reMarkable 2" => {
            warn!(
                "reMarkable 2 detected but no framebuffer shim found — \
                 framebuffer updates will likely be invisible. \
                 Install Vellum+AppLoad (qtfb-shim) or rm2fb on the device."
            );
        }
        _ => {}
    }
    drop(session);

    let listener = TcpListener::bind(("127.0.0.1", screen_cli.local_port))?;
    info!(
        "local TCP listener ready on {} (waiting for tunnel + rm-client-screen before capture starts)",
        listener.local_addr()?
    );

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
    info!("desktop portal screencast session active (PipeWire fd ready)");

    let pw_fd = unsafe { OwnedFd::from_raw_fd(portal_session.pipewire_fd()) };

    let pw_config = PipeWireConfig::builder()
        .use_dmabuf(false)
        .preferred_format(PixelFormat::BGRA)
        .enable_damage_tracking(true)
        .frame_buffer_size(8)
        .build();

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
    info!(
        "capture source: {:?} node_id={} size={}×{} at {:?} (portal reports {} stream(s))",
        stream_info.source_type,
        stream_info.node_id,
        stream_info.size.0,
        stream_info.size.1,
        stream_info.position,
        streams.len(),
    );
    if streams.len() > 1 {
        debug!(
            "additional portal streams ignored (using first only): {:?}",
            streams
                .iter()
                .skip(1)
                .map(|s| (s.node_id, s.size))
                .collect::<Vec<_>>()
        );
    }

    // PipeWireManager::frame_receiver is broken in lamco-pipewire 0.4.2 (sender is dropped).
    // Use PipeWireThreadManager, which delivers frames on the working std mpsc channel.
    let raw_fd = pw_fd.into_raw_fd();
    let mut stream_config = StreamConfig::new(format!("{}-0", pw_config.stream_name_prefix))
        .with_resolution(stream_info.size.0, stream_info.size.1)
        .with_dmabuf(pw_config.use_dmabuf)
        .with_buffer_count(pw_config.buffer_count);
    stream_config.preferred_format = pw_config.preferred_format;

    let pw_thread = PipeWireThreadManager::new(raw_fd).map_err(|e| e.to_string())?;
    let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
    pw_thread
        .send_command(PipeWireThreadCommand::CreateStream {
            stream_id: 0,
            node_id: stream_info.node_id,
            config: stream_config,
            response_tx,
        })
        .map_err(|e| e.to_string())?;
    response_rx
        .recv()
        .map_err(|_| "PipeWire CreateStream: response channel closed".to_string())?
        .map_err(|e| e.to_string())?;
    info!("PipeWire stream connected (waiting for tablet TCP before consuming frames)");

    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<VideoFrame>(8);
    let bridge = std::thread::spawn(move || {
        let mut pw_thread = pw_thread;
        let mut bridged: u64 = 0;
        loop {
            if let Some(frame) = pw_thread.recv_frame_timeout(Duration::from_millis(100)) {
                bridged += 1;
                if bridged == 1 {
                    debug!(
                        "frame bridge: first PipeWire frame {}×{} (format {:?})",
                        frame.width, frame.height, frame.format
                    );
                } else if bridged % 600 == 0 {
                    debug!("frame bridge: forwarded {} frames from PipeWire thread", bridged);
                }
                if frame_tx.blocking_send(frame).is_err() {
                    debug!(
                        "frame bridge: stopping after {} PipeWire frames (Tokio receiver dropped)",
                        bridged
                    );
                    break;
                }
            }
        }
        debug!("frame bridge: shutting down PipeWire thread");
        let _ = pw_thread.shutdown();
    });

    let mut tunnel: Child = if appload_launch {
        // qtfb-shim: the app must be launched from AppLoad for display compositing.
        // Write the manifest (with connection args) and start a tunnel-only SSH session.
        let session = ssh::connect_for_detection(&config)?;
        screen_client::ensure_appload_manifest(
            &session,
            fb_shim.as_ref().expect("appload_launch implies shim"),
            screen_cli.remote_port,
            stream_info.size.0,
            stream_info.size.1,
        )?;
        drop(session);
        info!(
            "AppLoad manifest updated (port={}, capture={}×{}). \
             Launch \"Screen Mirror\" from AppLoad on the tablet.",
            screen_cli.remote_port, stream_info.size.0, stream_info.size.1
        );
        screen_client::spawn_tunnel_only(
            &config,
            screen_cli.remote_port,
            "127.0.0.1",
            screen_cli.local_port,
        )?
    } else {
        let remote_cmd = screen_client::remote_screen_command(
            screen_cli.remote_port,
            stream_info.size.0,
            stream_info.size.1,
            fb_shim.as_ref(),
        );
        info!(
            "remote client command: {} (capture {}×{})",
            remote_cmd, stream_info.size.0, stream_info.size.1
        );
        spawn_reverse_tunnel(
            &config,
            screen_cli.remote_port,
            "127.0.0.1",
            screen_cli.local_port,
            &remote_cmd,
        )?
    };

    let (tx, rx_sock) = mpsc::channel::<std::io::Result<std::net::TcpStream>>();
    let lis = listener;
    std::thread::spawn(move || {
        let _ = tx.send(lis.accept().map(|(s, _)| s));
    });

    let timeout = if appload_launch {
        Duration::from_secs(120)
    } else {
        Duration::from_secs(30)
    };
    if appload_launch {
        info!("Waiting for you to launch \"Screen Mirror\" from AppLoad on the tablet (timeout {}s)…", timeout.as_secs());
    } else {
        info!("Waiting for tablet to connect via reverse SSH (timeout {}s)…", timeout.as_secs());
    }
    let sock = match rx_sock.recv_timeout(timeout) {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("TCP accept failed: {e}").into()),
        Err(_) => {
            let _ = tunnel.kill();
            if appload_launch {
                return Err("timed out — launch \"Screen Mirror\" from AppLoad on the tablet while rm-screen is running".into());
            }
            return Err("timed out waiting for tablet TCP connection (check SSH reverse tunnel)".into());
        }
    };
    match (sock.peer_addr(), sock.local_addr()) {
        (Ok(peer), Ok(local)) => info!("tablet TCP tunnel up (peer {peer}, local {local}); starting screen encode"),
        _ => info!("tablet TCP tunnel up; starting screen encode"),
    }
    configure_tcp_stream(&sock)?;

    let mut sock = sock;
    let mut damage = DamageDetector::new(DamageConfig::low_bandwidth());

    let mut frame_count: u64 = 0;
    let mut last_progress = Instant::now();
    loop {
        match frame_rx.recv().await {
            Some(frame) => {
                frame_count += 1;
                if frame_count <= 5 {
                    info!(
                        "frame #{} {}×{} format={:?} compositor_damage_rects={}",
                        frame_count,
                        frame.width,
                        frame.height,
                        frame.format,
                        frame.damage_regions.len(),
                    );
                } else if last_progress.elapsed() >= Duration::from_secs(5) {
                    info!(
                        "streaming… {} frames processed (latest {}×{})",
                        frame_count, frame.width, frame.height
                    );
                    last_progress = Instant::now();
                }
                if let Err(e) = process_frame(&mut sock, &mut damage, &frame) {
                    error!("stream error: {e}");
                    info!("stopped after {frame_count} frames (write to tablet failed)");
                    break;
                }
            }
            None => {
                info!("frame pipeline ended after {frame_count} frames (sender closed — capture or bridge stopped)");
                break;
            }
        }
    }
    drop(frame_rx);
    if let Err(e) = bridge.join() {
        error!("frame bridge thread join: {e:?}");
    }

    info!("rm-screen session teardown…");
    let _ = tunnel.kill();
    let _ = tunnel.wait();
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
    let mut regions_sent: u32 = 0;
    let mut wire_bytes: u64 = 0;

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
        regions_sent += 1;
        wire_bytes += HEADER_SIZE as u64 + compressed.len() as u64;
    }

    if regions_sent > 0 {
        debug!(
            "encoded frame {}×{} → {regions_sent} region update(s), {wire_bytes} B on wire (LZ4 payloads + headers)",
            w, h
        );
    } else {
        debug!("frame {}×{}: no regions sent (fully skipped or empty damage)", w, h);
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
