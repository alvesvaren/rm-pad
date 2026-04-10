//! PC-side screen mirror: PipeWire (via portal) → dirty regions → LZ4 → TCP to tablet.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::Child;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
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
use rm_common::expand_rect_to_epdc_grid;
use rm_common::grab;
use rm_common::protocol::{UpdateHeader, UPDATE_COORDS_FRAMEBUFFER};
use rm_common::screen_client::{
    self, ensure_client_on_device, load_client_binary, spawn_remote_exec, spawn_reverse_tunnel,
};
use rm_common::ssh;
use socket2::{Socket, TcpKeepalive};

type DynResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// PipeWire → main queue depth. Larger values let `recv_latest_drained` pick a fresher frame while
/// the main task is encoding or waiting for ACK (capacity 2 was stalling the bridge often).
const FRAME_CHANNEL_CAP: usize = 32;

fn elapsed_ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

/// Block on first `recv`, drain `try_recv`, return freshest frame.
async fn recv_latest_drained(
    rx: &mut tokio::sync::mpsc::Receiver<VideoFrame>,
) -> Option<(VideoFrame, u64, f64)> {
    let tw = Instant::now();
    let first = rx.recv().await?;
    let mut latest = first;
    let mut skipped = 0u64;
    while let Ok(newer) = rx.try_recv() {
        latest = newer;
        skipped += 1;
    }
    Some((latest, skipped, elapsed_ms(tw)))
}

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

    /// Tablet framebuffer width in pixels (default: from detected device model).
    #[arg(long)]
    tablet_fb_w: Option<u32>,

    /// Tablet framebuffer height in pixels (default: from detected device model).
    #[arg(long)]
    tablet_fb_h: Option<u32>,

    /// Skip the SSH reverse tunnel and listen for direct TCP (e.g. USB Ethernet).
    /// Requires `--advertise-host` (PC address reachable from the tablet).
    #[arg(long)]
    direct_tcp: bool,

    /// PC hostname or IP placed in the AppLoad manifest / remote client command.
    #[arg(long)]
    advertise_host: Option<String>,

    /// Address for `TcpListener` (use `0.0.0.0` to accept USB/LAN; may need a firewall rule).
    #[arg(long, default_value = "127.0.0.1")]
    bind_addr: String,

    /// Log every main-loop iteration (including waits with no dirty region). Very noisy.
    #[arg(long)]
    stream_trace: bool,
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
    let (fb_w, fb_h) = (
        screen_cli.tablet_fb_w.unwrap_or_else(|| device.framebuffer_size().0),
        screen_cli.tablet_fb_h.unwrap_or_else(|| device.framebuffer_size().1),
    );
    drop(session);

    let bind_sa: std::net::SocketAddr = format!("{}:{}", screen_cli.bind_addr, screen_cli.local_port)
        .parse()
        .map_err(|e| format!("invalid bind address {}:{} — {e}", screen_cli.bind_addr, screen_cli.local_port))?;
    let listener = TcpListener::bind(bind_sa)?;
    info!(
        "TCP listener on {} (tablet framebuffer {}×{} for host-side scaling)",
        listener.local_addr()?,
        fb_w,
        fb_h
    );
    if screen_cli.direct_tcp && screen_cli.bind_addr != "127.0.0.1" {
        info!(
            "direct TCP: ensure the host firewall allows inbound TCP {} from the tablet",
            screen_cli.local_port
        );
    }

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

    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<VideoFrame>(FRAME_CHANNEL_CAP);
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
                let tw_push = Instant::now();
                if frame_tx.blocking_send(frame).is_err() {
                    debug!(
                        "frame bridge: stopping after {} PipeWire frames (Tokio receiver dropped)",
                        bridged
                    );
                    break;
                }
                let push_ms = elapsed_ms(tw_push);
                if push_ms > 250.0 {
                    warn!(
                        "frame bridge: PipeWire→tokio mpsc blocked {:.1}ms (capacity {} — main loop slow vs PipeWire)",
                        push_ms, FRAME_CHANNEL_CAP
                    );
                }
            }
        }
        debug!("frame bridge: shutting down PipeWire thread");
        let _ = pw_thread.shutdown();
    });

    if screen_cli.direct_tcp && screen_cli.advertise_host.is_none() {
        return Err(
            "--advertise-host is required with --direct-tcp (tablet must reach your PC; try the USB NIC IP)"
                .into(),
        );
    }

    let tunnel: Option<Child> = if screen_cli.direct_tcp {
        let pc_host = screen_cli.advertise_host.as_deref().expect("checked above");
        let pc_port = screen_cli.local_port;
        if appload_launch {
            let session = ssh::connect_for_detection(&config)?;
            screen_client::ensure_appload_manifest(
                &session,
                fb_shim.as_ref().expect("appload_launch implies shim"),
                pc_host,
                pc_port,
                stream_info.size.0,
                stream_info.size.1,
            )?;
            drop(session);
            info!(
                "AppLoad manifest: connect to {}:{} (direct TCP, capture {}×{}). \
                 Launch \"Screen Mirror\" on the tablet.",
                pc_host, pc_port, stream_info.size.0, stream_info.size.1
            );
            None
        } else {
            let remote_cmd = screen_client::remote_screen_command(
                pc_host,
                pc_port,
                stream_info.size.0,
                stream_info.size.1,
                fb_shim.as_ref(),
            );
            info!("remote client command: {}", remote_cmd);
            Some(spawn_remote_exec(&config, &remote_cmd)?)
        }
    } else if appload_launch {
        let session = ssh::connect_for_detection(&config)?;
        screen_client::ensure_appload_manifest(
            &session,
            fb_shim.as_ref().expect("appload_launch implies shim"),
            "127.0.0.1",
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
        Some(screen_client::spawn_tunnel_only(
            &config,
            screen_cli.remote_port,
            "127.0.0.1",
            screen_cli.local_port,
        )?)
    } else {
        let remote_cmd = screen_client::remote_screen_command(
            "127.0.0.1",
            screen_cli.remote_port,
            stream_info.size.0,
            stream_info.size.1,
            fb_shim.as_ref(),
        );
        info!(
            "remote client command: {} (capture {}×{})",
            remote_cmd, stream_info.size.0, stream_info.size.1
        );
        Some(spawn_reverse_tunnel(
            &config,
            screen_cli.remote_port,
            "127.0.0.1",
            screen_cli.local_port,
            &remote_cmd,
        )?)
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
    } else if screen_cli.direct_tcp {
        info!(
            "Waiting for tablet TCP to {}:{} (timeout {}s)…",
            screen_cli.bind_addr,
            screen_cli.local_port,
            timeout.as_secs()
        );
    } else {
        info!("Waiting for tablet to connect via reverse SSH (timeout {}s)…", timeout.as_secs());
    }
    let sock = match rx_sock.recv_timeout(timeout) {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("TCP accept failed: {e}").into()),
        Err(_) => {
            if let Some(mut t) = tunnel {
                let _ = t.kill();
            }
            if appload_launch {
                return Err("timed out — launch \"Screen Mirror\" from AppLoad on the tablet while rm-screen is running".into());
            }
            return Err("timed out waiting for tablet TCP connection (check network / SSH tunnel)".into());
        }
    };
    match (sock.peer_addr(), sock.local_addr()) {
        (Ok(peer), Ok(local)) => info!("tablet connected (peer {peer}, local {local}); starting screen encode"),
        _ => info!("tablet connected; starting screen encode"),
    }
    configure_tcp_stream(&sock)?;
    let sock_read = Arc::new(Mutex::new(
        sock.try_clone()
            .map_err(|e| format!("TCP try_clone for ACK reads: {e}"))?,
    ));

    let mut sock = sock;
    let mut damage = DamageDetector::new(DamageConfig::low_bandwidth());
    // Portal `stream_info.size` can disagree with actual PipeWire `VideoFrame` dimensions; scaling
    // and `gray_at_fb_pixel` must use the frame buffer size we read from.
    let mut lb = Letterbox::new(stream_info.size.0, stream_info.size.1, fb_w, fb_h);
    info!(
        "letterbox (portal): scale {:.4} offset ({}, {}) fitted {}×{} for source {}×{}",
        lb.scale,
        lb.off_x,
        lb.off_y,
        lb.dst_fit_w,
        lb.dst_fit_h,
        stream_info.size.0,
        stream_info.size.1
    );
    info!(
        "rm-screen stream log: one INFO line per update (ACK runs in parallel with fetching the next frame). \
         Use --stream-trace for iterations with no dirty region."
    );

    let mut frame_count: u64 = 0;
    let mut frames_dropped: u64 = 0;
    let mut last_progress = Instant::now();
    let mut stream_seq: u64 = 0;

    let (mut latest, mut prev_channel_skipped, mut prev_recv_ms) =
        match recv_latest_drained(&mut frame_rx).await {
            Some(x) => x,
            None => {
                info!("frame pipeline ended before first frame (sender closed)");
                drop(frame_rx);
                let _ = bridge.join();
                return Ok(());
            }
        };
    frames_dropped += prev_channel_skipped;

    loop {
        let tw_cycle = Instant::now();
        let skipped = prev_channel_skipped;
        let ms_recv_wait = prev_recv_ms;
        let ms_drain_spin = 0.0_f64;

        frame_count += 1;
        if frame_count <= 3 {
            info!(
                "frame #{} {}×{} format={:?} compositor_damage_rects={} (skipped {skipped})",
                frame_count,
                latest.width,
                latest.height,
                latest.format,
                latest.damage_regions.len(),
            );
        } else if last_progress.elapsed() >= Duration::from_secs(5) {
            info!(
                "streaming: {frame_count} sent, {frames_dropped} dropped (latest {}×{})",
                latest.width, latest.height
            );
            last_progress = Instant::now();
        }

        if latest.width != lb.src_w || latest.height != lb.src_h {
            warn!(
                "capture buffer differs from letterbox source: PipeWire {}×{} vs letterbox {}×{} — \
                 recomputing letterbox (portal stream size was {}×{})",
                latest.width,
                latest.height,
                lb.src_w,
                lb.src_h,
                stream_info.size.0,
                stream_info.size.1
            );
            lb = Letterbox::new(latest.width, latest.height, fb_w, fb_h);
            damage.invalidate();
            info!(
                "letterbox (actual frames): scale {:.4} offset ({}, {}) fitted {}×{} for source {}×{}",
                lb.scale,
                lb.off_x,
                lb.off_y,
                lb.dst_fit_w,
                lb.dst_fit_h,
                lb.src_w,
                lb.src_h
            );
        }

        let encode_out = match encode_update(&mut damage, &latest, &lb) {
            Ok(v) => v,
            Err(e) => {
                error!("stream error: {e}");
                info!("stopped after {frame_count} frames sent (encode failed)");
                break;
            }
        };

        if let Some((wire_buf, mut stats)) = encode_out {
            stream_seq += 1;

            let tw_write = Instant::now();
            if let Err(e) = sock.write_all(&wire_buf) {
                error!("TCP write failed: {e}");
                info!("stopped after {frame_count} frames sent");
                break;
            }
            if let Err(e) = sock.flush() {
                error!("TCP flush failed: {e}");
                break;
            }
            stats.ms_write = elapsed_ms(tw_write);

            let sock_r = sock_read.clone();
            let ack_task = tokio::task::spawn_blocking(move || {
                let mut guard = sock_r
                    .lock()
                    .map_err(|e| format!("ACK lock poisoned: {e}"))?;
                let t_ack = Instant::now();
                let mut ack = [0u8; 1];
                guard
                    .read_exact(&mut ack)
                    .map_err(|e| format!("ACK read: {e}"))?;
                Ok::<_, String>((ack[0], elapsed_ms(t_ack)))
            });

            let recv_task = recv_latest_drained(&mut frame_rx);

            let (ack_join, recv_out) = tokio::join!(ack_task, recv_task);

            let (ack_byte, ms_ack) = match ack_join {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => {
                    error!("ACK task failed: {e}");
                    break;
                }
                Err(e) => {
                    error!("ACK join failed: {e}");
                    break;
                }
            };

            match recv_out {
                None => {
                    info!("frame pipeline ended after {frame_count} iterations, {frames_dropped} dropped (sender closed)");
                    break;
                }
                Some((next_latest, ch_sk, ms_recv_parallel)) => {
                    latest = next_latest;
                    prev_channel_skipped = ch_sk;
                    prev_recv_ms = ms_recv_parallel;
                    frames_dropped += ch_sk;
                }
            }

            if ack_byte != 0x06 {
                warn!(
                    "rm-screen stream seq={stream_seq} unexpected ACK byte 0x{:02x} (expected 0x06)",
                    ack_byte
                );
            }
            if ms_ack >= 1500.0 {
                warn!(
                    "rm-screen stream seq={stream_seq} ACK slow: ack_wait_ms={ms_ack:.1} (tablet / tunnel / EPDC?)",
                );
            }

            let full_iter_ms = elapsed_ms(tw_cycle);
            info!(
                "rm-screen stream seq={} full_iter_ms={:.2} recv_ms={:.1} drain_try_ms={:.1} channel_dropped={} \
                 capture={}×{}@({},{}) fb={}×{}@({},{}) regions={} \
                 pack_ms={:.2} lz4_ms={:.2} write_ms={:.2} gray4_B={} wire_B={} \
                 ack_byte=0x{:02x} ack_wait_ms={:.2} (recv overlapped w/ ACK)",
                stream_seq,
                full_iter_ms,
                ms_recv_wait,
                ms_drain_spin,
                skipped,
                stats.capture_w,
                stats.capture_h,
                stats.capture_x0,
                stats.capture_y0,
                stats.fb_w,
                stats.fb_h,
                stats.fb_x,
                stats.fb_y,
                stats.region_rects,
                stats.ms_pack,
                stats.ms_lz4,
                stats.ms_write,
                stats.gray4_bytes,
                stats.wire_bytes,
                ack_byte,
                ms_ack,
            );
        } else {
            if screen_cli.stream_trace {
                info!(
                    "rm-screen stream trace iter={frame_count} recv_wait_ms={ms_recv_wait:.1} drain_try_ms={ms_drain_spin:.1} \
                     channel_dropped={skipped} (no encoded update; sleeping 16ms)",
                );
            } else if ms_recv_wait >= 2000.0 {
                warn!(
                    "rm-screen stream iter={frame_count} recv_wait_ms={ms_recv_wait:.1} — blocked ~{:.0}s waiting for PipeWire frame",
                    ms_recv_wait / 1000.0,
                );
            }
            tokio::time::sleep(Duration::from_millis(16)).await;
            match recv_latest_drained(&mut frame_rx).await {
                None => {
                    info!("frame pipeline ended after {frame_count} iterations (sender closed)");
                    break;
                }
                Some((lf, sk, ms)) => {
                    latest = lf;
                    prev_channel_skipped = sk;
                    prev_recv_ms = ms;
                    frames_dropped += sk;
                }
            }
        }
    }
    drop(frame_rx);
    if let Err(e) = bridge.join() {
        error!("frame bridge thread join: {e:?}");
    }

    info!("rm-screen session teardown…");
    if let Some(mut t) = tunnel {
        let _ = t.kill();
        let _ = t.wait();
    }
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
    sock.set_write_timeout(Some(Duration::from_secs(5)))?;
    let raw = sock.as_raw_fd();
    let s = unsafe { Socket::from_raw_fd(raw) };
    s.set_send_buffer_size(32 * 1024)?;
    let ka = TcpKeepalive::new().with_time(Duration::from_secs(30));
    s.set_tcp_keepalive(&ka)?;
    let eff_snd = s.send_buffer_size().unwrap_or(0);
    let eff_rcv = s.recv_buffer_size().unwrap_or(0);
    info!(
        "rm-screen TCP peer tuning: TCP_NODELAY on, SO_SNDBUF={} SO_RCVBUF={} (OS may round)",
        eff_snd, eff_rcv,
    );
    std::mem::forget(s);
    Ok(())
}

/// Host-side letterboxing: same geometry as `rm-client-screen` uses for capture → FB fit.
struct Letterbox {
    src_w: u32,
    src_h: u32,
    fb_w: u32,
    fb_h: u32,
    scale: f64,
    off_x: u32,
    off_y: u32,
    dst_fit_w: u32,
    dst_fit_h: u32,
}

impl Letterbox {
    fn new(src_w: u32, src_h: u32, fb_w: u32, fb_h: u32) -> Self {
        let scale = (fb_w as f64 / src_w as f64).min(fb_h as f64 / src_h as f64);
        let dst_fit_w = ((src_w as f64 * scale).floor() as u32) & !1;
        let dst_fit_h = (src_h as f64 * scale).floor() as u32;
        let off_x = fb_w.saturating_sub(dst_fit_w) / 2;
        let off_y = fb_h.saturating_sub(dst_fit_h) / 2;
        Self {
            src_w,
            src_h,
            fb_w,
            fb_h,
            scale,
            off_x,
            off_y,
            dst_fit_w,
            dst_fit_h,
        }
    }

    /// Map a capture-space bounding box to framebuffer pixels (even width), matching tablet rounding.
    fn capture_bbox_to_fb(&self, x0: u32, y0: u32, x1: u32, y1: u32) -> Option<(u32, u32, u16, u16)> {
        let dx0 = self.off_x as f64 + x0 as f64 * self.scale;
        let dy0 = self.off_y as f64 + y0 as f64 * self.scale;
        let dx1 = self.off_x as f64 + x1 as f64 * self.scale;
        let dy1 = self.off_y as f64 + y1 as f64 * self.scale;
        let ix0 = dx0.floor().max(0.0) as u32;
        let iy0 = dy0.floor().max(0.0) as u32;
        let ix1 = dx1.ceil().min(self.fb_w as f64) as u32;
        let iy1 = dy1.ceil().min(self.fb_h as f64) as u32;
        let mut out_w = ix1.saturating_sub(ix0);
        out_w &= !1;
        let out_h = iy1.saturating_sub(iy0);
        if out_w < 2 || out_h < 1 {
            return None;
        }
        Some((ix0, iy0, out_w as u16, out_h as u16))
    }
}

/// Host-side timing snapshot for one successfully encoded and written update (`None` = nothing sent).
struct StreamSendStats {
    capture_x0: u32,
    capture_y0: u32,
    capture_w: u16,
    capture_h: u16,
    fb_x: u32,
    fb_y: u32,
    fb_w: u16,
    fb_h: u16,
    region_rects: usize,
    gray4_bytes: usize,
    wire_bytes: usize,
    ms_pack: f64,
    ms_lz4: f64,
    ms_write: f64,
}

/// Merge all dirty regions into a single bounding-box update.
/// Returns wire buffer + stats (`ms_write` filled in by caller after `write_all`).
fn encode_update(
    damage: &mut DamageDetector,
    frame: &VideoFrame,
    lb: &Letterbox,
) -> DynResult<Option<(Vec<u8>, StreamSendStats)>> {
    let Some(data_arc) = frame.data() else {
        warn!("DMA-BUF frame skipped (no CPU pixels); disable GPU-only capture in compositor if this repeats");
        damage.invalidate();
        return Ok(None);
    };
    let data = data_arc.as_slice();
    let w = frame.width;
    let h = frame.height;

    let regions = regions_for_frame(frame, damage, data, w, h);
    let region_rects = regions.len();

    // Merge all dirty rects into one bounding box.
    let mut x0 = w;
    let mut y0 = h;
    let mut x1 = 0u32;
    let mut y1 = 0u32;
    for r in &regions {
        let (rx, ry, rw, rh) = clip_region(*r, w, h);
        if rw < 2 || rh < 1 {
            continue;
        }
        x0 = x0.min(rx as u32);
        y0 = y0.min(ry as u32);
        x1 = x1.max(rx as u32 + rw as u32);
        y1 = y1.max(ry as u32 + rh as u32);
    }
    if x0 >= x1 || y0 >= y1 {
        return Ok(None);
    }
    let mw = ((x1 - x0) as u16) & !1;
    let mh = (y1 - y0) as u16;
    if mw < 2 || mh < 1 {
        return Ok(None);
    }

    let cap_x1 = x0 + mw as u32;
    let cap_y1 = y0 + mh as u32;
    let Some((fb_ix, fb_iy, fb_mw, fb_mh)) = lb.capture_bbox_to_fb(x0, y0, cap_x1, cap_y1) else {
        return Ok(None);
    };
    let (fb_ix, fb_iy, ew, eh) = expand_rect_to_epdc_grid(
        fb_ix,
        fb_iy,
        fb_mw as u32,
        fb_mh as u32,
        lb.fb_w,
        lb.fb_h,
    );
    if ew < 2 || eh < 1 {
        return Ok(None);
    }
    let fb_mw = ew as u16;
    let fb_mh = eh as u16;

    let tw_pack = Instant::now();
    let packed = pack_region_gray4_fb(
        data,
        frame.stride,
        frame.format,
        lb,
        fb_ix,
        fb_iy,
        fb_mw,
        fb_mh,
    )?;
    let ms_pack = elapsed_ms(tw_pack);

    let tw_lz4 = Instant::now();
    let compressed = lz4_flex::block::compress_prepend_size(&packed);
    let ms_lz4 = elapsed_ms(tw_lz4);

    let header = UpdateHeader {
        x: fb_ix as u16,
        y: fb_iy as u16,
        width: fb_mw,
        height: fb_mh,
        waveform: UPDATE_COORDS_FRAMEBUFFER,
        payload_size: compressed.len() as u32,
    };

    let mut wire_buf = Vec::with_capacity(header.to_bytes().len() + compressed.len());
    wire_buf.extend_from_slice(&header.to_bytes());
    wire_buf.extend_from_slice(&compressed);
    let wire_bytes = wire_buf.len();

    debug!(
        "encoded frame {}×{} → capture {}×{} @ ({},{}) → FB {}×{} @ ({fb_ix},{fb_iy}), {} B on wire",
        w, h, mw, mh, x0, y0, fb_mw, fb_mh, wire_bytes
    );

    Ok(Some((
        wire_buf,
        StreamSendStats {
            capture_x0: x0,
            capture_y0: y0,
            capture_w: mw,
            capture_h: mh,
            fb_x: fb_ix,
            fb_y: fb_iy,
            fb_w: fb_mw,
            fb_h: fb_mh,
            region_rects,
            gray4_bytes: packed.len(),
            wire_bytes,
            ms_pack,
            ms_lz4,
            ms_write: 0.0,
        },
    )))
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

/// Sample capture at framebuffer pixel (screen_x, screen_y) using the same inverse map as the tablet.
fn gray_at_fb_pixel(
    data: &[u8],
    stride: u32,
    format: PixelFormat,
    lb: &Letterbox,
    screen_x: u32,
    screen_y: u32,
) -> u8 {
    let u = (screen_x as f64 + 0.5 - lb.off_x as f64) / lb.scale;
    let v = (screen_y as f64 + 0.5 - lb.off_y as f64) / lb.scale;
    let src_ix = u.floor().clamp(0.0, (lb.src_w.saturating_sub(1)) as f64) as u32;
    let src_iy = v.floor().clamp(0.0, (lb.src_h.saturating_sub(1)) as f64) as u32;
    let bpp = format.bytes_per_pixel().max(3);
    let stride = stride as usize;
    let row_off = src_iy as usize * stride;
    let x0 = src_ix as usize * bpp;
    let o0 = row_off + x0;
    gray_from_pixel(format, &data[o0..data.len().min(o0 + bpp)]).unwrap_or(0)
}

/// Pack a region that already lies in framebuffer space (downsampled from capture on the host).
fn pack_region_gray4_fb(
    data: &[u8],
    stride: u32,
    format: PixelFormat,
    lb: &Letterbox,
    fb_ix0: u32,
    fb_iy0: u32,
    w: u16,
    h: u16,
) -> DynResult<Vec<u8>> {
    let mut out = Vec::with_capacity((w as usize / 2) * h as usize);
    for row in 0..h {
        let screen_y = fb_iy0 + row as u32;
        for col in (0..w).step_by(2) {
            let screen_x = fb_ix0 + col as u32;
            let g0 = gray_at_fb_pixel(data, stride, format, lb, screen_x, screen_y);
            let g1 = gray_at_fb_pixel(data, stride, format, lb, screen_x + 1, screen_y);
            let n0 = (g0 >> 4) & 0x0f;
            let n1 = (g1 >> 4) & 0x0f;
            out.push((n0 << 4) | n1);
        }
    }
    Ok(out)
}
