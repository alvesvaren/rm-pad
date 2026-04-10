//! PC-side screen mirror: PipeWire (via portal) → dirty regions → LZ4 → TCP to tablet.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::Child;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

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
use rm_common::protocol::{unix_time_millis, BatchAck, UpdateHeader, ACK_OK, ACK_SIZE, UPDATE_COORDS_FRAMEBUFFER};
use rm_common::screen_client::{
    self, ensure_client_on_device, load_client_binary, spawn_remote_exec, spawn_reverse_tunnel,
};
use rm_common::ssh;
use socket2::{Socket, TcpKeepalive};

type DynResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// PipeWire → main queue depth. Larger values let `recv_latest_drained` pick a fresher frame while
/// the main task is encoding or waiting for ACK (capacity 2 was stalling the bridge often).
const FRAME_CHANNEL_CAP: usize = 32;
/// Keep only one batch in flight by default. The tablet-side display path is still the visible
/// bottleneck, so letting the host queue multiple batches mainly increases perceived lag.
const MAX_INFLIGHT_BATCHES: usize = 1;

fn elapsed_ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

fn elapsed_ms_between(start: Instant, end: Instant) -> f64 {
    end.saturating_duration_since(start).as_secs_f64() * 1000.0
}

struct PendingBatch {
    batch_id: u32,
    batch_start_seq: u64,
    part_logs: Vec<StreamSendStats>,
    parts: usize,
    first_host_unix_ms: u64,
    encode_ms: f64,
    tcp_write_ms: f64,
    sent_at: Instant,
    recv_ms: f64,
    drain_try_ms: f64,
    skipped: u64,
    send_iter_ms: f64,
    pw_header_age_ms: Option<f64>,
    local_queue_age_ms: Option<f64>,
    pw_seq: Option<u64>,
}

fn next_nonzero_batch_id(next_batch_id: &mut u32) -> u32 {
    let out = *next_batch_id;
    *next_batch_id = next_batch_id.wrapping_add(1);
    if *next_batch_id == 0 {
        *next_batch_id = 1;
    }
    out
}

fn system_time_age_ms(at: SystemTime) -> Option<f64> {
    SystemTime::now()
        .duration_since(at)
        .ok()
        .map(|d| d.as_secs_f64() * 1000.0)
}

fn pipewire_header_age_ms(frame: &VideoFrame) -> Option<f64> {
    let hdr = frame.meta.header.as_ref()?;
    let hdr_pts = u64::try_from(hdr.pts).ok()?;
    Some(frame.pts.saturating_sub(hdr_pts) as f64 / 1_000_000.0)
}

fn fmt_opt_ms(v: Option<f64>) -> String {
    match v {
        Some(ms) => format!("{ms:.2}"),
        None => "na".to_string(),
    }
}

fn fmt_opt_u64(v: Option<u64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "na".to_string(),
    }
}

fn log_batch_ack(batch: PendingBatch, ack: BatchAck, latency_log: bool, inflight_now: usize) {
    let ms_ack_batch = elapsed_ms(batch.sent_at);
    if ack.status != ACK_OK {
        warn!(
            "rm-screen batch {} unexpected ACK status 0x{:02x} (expected 0x{:02x})",
            ack.batch_id,
            ack.status,
            ACK_OK,
        );
    }
    if latency_log {
        let host_wall_ms = unix_time_millis();
        info!(
            target: "rm_mirror_latency",
            "host batch id={} seq={}..{} parts={} encode_ms={:.2} tcp_write_ms={:.2} ack_wait_ms={:.2} inflight_now={} first_host_unix_ms={} host_wall_now_ms={} \
             pw_header_age_ms={} local_queue_age_ms={} pw_seq={} \
             | high ack_wait_ms means the tablet/tunnel is still lagging; with windowing the host no longer blocks every iteration on it",
            batch.batch_id,
            batch.batch_start_seq + 1,
            batch.batch_start_seq + batch.parts as u64,
            batch.parts,
            batch.encode_ms,
            batch.tcp_write_ms,
            ms_ack_batch,
            inflight_now,
            batch.first_host_unix_ms,
            host_wall_ms,
            fmt_opt_ms(batch.pw_header_age_ms),
            fmt_opt_ms(batch.local_queue_age_ms),
            fmt_opt_u64(batch.pw_seq),
        );
    }
    let ack_warn_ms = 1500.0 * batch.parts.max(1) as f64;
    if ms_ack_batch >= ack_warn_ms {
        warn!(
            "rm-screen batch id={} seq={}..{} ACK slow: ack_wait_ms={ms_ack_batch:.1} (tablet / tunnel / EPDC?)",
            batch.batch_id,
            batch.batch_start_seq + 1,
            batch.batch_start_seq + batch.parts as u64,
        );
    }
    if !latency_log {
        let ms_write_per = batch.tcp_write_ms / batch.parts.max(1) as f64;
        for (part_i, mut stats) in batch.part_logs.into_iter().enumerate() {
            let is_last_part = part_i + 1 == batch.parts;
            stats.ms_write = ms_write_per;
            let this_seq = batch.batch_start_seq + part_i as u64 + 1;
            info!(
                "rm-screen stream seq={} batch_id={} part={}/{} send_iter_ms={:.2} recv_ms={:.1} drain_try_ms={:.1} channel_dropped={} \
                 capture={}×{}@({},{}) fb={}×{}@({},{}) regions={} sparse_parts={} \
                 pack_ms={:.2} lz4_ms={:.2} write_ms={:.2} payload_B={} wire_B={} \
                 batch_ack_ms={:.2} {}",
                this_seq,
                batch.batch_id,
                part_i + 1,
                batch.parts,
                batch.send_iter_ms,
                batch.recv_ms,
                batch.drain_try_ms,
                batch.skipped,
                stats.capture_w,
                stats.capture_h,
                stats.capture_x0,
                stats.capture_y0,
                stats.fb_w,
                stats.fb_h,
                stats.fb_x,
                stats.fb_y,
                stats.region_rects,
                stats.sparse_parts,
                stats.ms_pack,
                stats.ms_lz4,
                stats.ms_write,
                stats.payload_bytes,
                stats.wire_bytes,
                ms_ack_batch,
                if is_last_part {
                    "(batch ACK; host send is windowed)"
                } else {
                    ""
                },
            );
        }
    }
}

fn handle_batch_ack(
    inflight: &mut VecDeque<PendingBatch>,
    ack: BatchAck,
    latency_log: bool,
) -> DynResult<()> {
    let pos = inflight
        .iter()
        .position(|batch| batch.batch_id == ack.batch_id)
        .ok_or_else(|| format!("received ACK for unknown batch id {}", ack.batch_id))?;
    let batch = inflight
        .remove(pos)
        .ok_or_else(|| format!("missing inflight batch id {}", ack.batch_id))?;
    log_batch_ack(batch, ack, latency_log, inflight.len());
    Ok(())
}

fn drain_batch_acks(
    ack_rx: &mpsc::Receiver<Result<BatchAck, String>>,
    inflight: &mut VecDeque<PendingBatch>,
    latency_log: bool,
) -> DynResult<()> {
    loop {
        match ack_rx.try_recv() {
            Ok(Ok(ack)) => handle_batch_ack(inflight, ack, latency_log)?,
            Ok(Err(e)) => return Err(e.into()),
            Err(mpsc::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err("ACK reader disconnected".into());
            }
        }
    }
}

fn wait_for_batch_slot(
    ack_rx: &mpsc::Receiver<Result<BatchAck, String>>,
    inflight: &mut VecDeque<PendingBatch>,
    latency_log: bool,
) -> DynResult<()> {
    while inflight.len() >= MAX_INFLIGHT_BATCHES {
        match ack_rx.recv() {
            Ok(Ok(ack)) => handle_batch_ack(inflight, ack, latency_log)?,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => return Err("ACK reader disconnected".into()),
        }
    }
    Ok(())
}

/// Tuned between `low_bandwidth` (noisy → huge merged bboxes) and overly strict thresholds
/// that drop real UI updates.
fn damage_config_for_screen_mirror() -> DamageConfig {
    DamageConfig {
        tile_size: 16,
        diff_threshold: 0.055,
        pixel_threshold: 6,
        merge_distance: 10,
        min_region_area: 80,
    }
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

    /// Log per-batch timing breakdown (`RM_MIRROR_LATENCY_LOG=1` or `true`).
    #[arg(long, default_value_t = false, env = "RM_MIRROR_LATENCY_LOG", value_parser = clap::builder::BoolishValueParser::new())]
    latency_log: bool,

    /// Max separate bboxes (TCP+EPDC) per PipeWire frame when damage is spatially sparse.
    /// **1** = single merged rect (min ioctl count; can look like a big “flash” when holes are sparse).
    /// **8** (default) splits disjoint damage without exploding into 16+ serial refreshes.
    #[arg(long, default_value_t = 8, env = "RM_SCREEN_SPARSE_PARTS_MAX")]
    sparse_parts_max: usize,

    /// Modern qtfb-shim input handling toggle (`QTFB_SHIM_INPUT`).
    #[arg(long, default_value_t = false, env = "RM_SCREEN_QTFB_SHIM_INPUT", value_parser = clap::builder::BoolishValueParser::new())]
    qtfb_shim_input: bool,

    /// Modern qtfb-shim model handling toggle (`QTFB_SHIM_MODEL`).
    #[arg(long, default_value_t = false, env = "RM_SCREEN_QTFB_SHIM_MODEL", value_parser = clap::builder::BoolishValueParser::new())]
    qtfb_shim_model: bool,

    /// qtfb-shim framebuffer mode (`QTFB_SHIM_MODE`), e.g. `RGB565`.
    #[arg(long, default_value = "RGB565", env = "RM_SCREEN_QTFB_SHIM_MODE")]
    qtfb_shim_mode: String,

    /// Export `KO_DONT_GRAB_INPUT=1` for qtfb-shim launches.
    #[arg(long, default_value_t = true, env = "RM_SCREEN_QTFB_SHIM_DONT_GRAB_INPUT", value_parser = clap::builder::BoolishValueParser::new())]
    qtfb_shim_dont_grab_input: bool,

    /// Experimental: stop `xochitl` and run the mirror client directly for the session,
    /// bypassing AppLoad/qtfb-shim presentation cadence entirely.
    #[arg(long, default_value_t = false, env = "RM_SCREEN_EXCLUSIVE_DISPLAY", value_parser = clap::builder::BoolishValueParser::new())]
    exclusive_display: bool,
}

#[tokio::main]
async fn main() -> DynResult<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let screen_cli = ScreenCli::parse();
    if cfg!(debug_assertions) {
        warn!(
            "rm-screen is running as a debug build; host encode latency can be dramatically worse. \
             For real performance measurements use `cargo run --release -p rm-screen -- ...` \
             or `target/release/rm-screen`."
        );
    }
    if screen_cli.latency_log {
        info!("--latency-log: enable RUST_LOG=rm_mirror_latency=info,info for timing lines; set RM_MIRROR_LATENCY_LOG=1 on the tablet client too");
    }
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
    let qtfb_shim = screen_client::QtfbShimConfig {
        input: screen_cli.qtfb_shim_input,
        model: screen_cli.qtfb_shim_model,
        mode: screen_cli.qtfb_shim_mode.clone(),
        dont_grab_input: screen_cli.qtfb_shim_dont_grab_input,
    };
    let launch_shim = if screen_cli.exclusive_display {
        None
    } else {
        fb_shim.as_ref()
    };
    let appload_launch = !screen_cli.exclusive_display
        && matches!(launch_shim, Some(screen_client::FbShim::QtfbShim(_)));
    if appload_launch {
        info!("qtfb-shim detected — client will be launched from AppLoad on the tablet");
    }
    if screen_cli.exclusive_display {
        warn!(
            "--exclusive-display: stopping xochitl for the session and launching rm-client-screen directly; \
             this bypasses AppLoad/qtfb-shim but is more invasive"
        );
    } else if launch_shim.is_some() {
        info!(
            "tablet launch env: {}",
            screen_client::describe_shim_launch_env(launch_shim, &qtfb_shim)
        );
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
        .with_framerate(60)
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
            // Lower cap than 100ms so a lull in PipeWire delivery does not add up to 600ms-class holes
            // before the next frame reaches the encoder (timeout only applies when no frame is queued).
            if let Some(frame) = pw_thread.recv_frame_timeout(Duration::from_millis(16)) {
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
                launch_shim.expect("appload_launch implies shim"),
                pc_host,
                pc_port,
                stream_info.size.0,
                stream_info.size.1,
                screen_cli.latency_log,
                &qtfb_shim,
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
                launch_shim,
                screen_cli.latency_log,
                &qtfb_shim,
                !screen_cli.exclusive_display,
            );
            let remote_cmd = if screen_cli.exclusive_display {
                screen_client::wrap_xochitl_exclusive_command(&remote_cmd)
            } else {
                remote_cmd
            };
            info!("remote client command: {}", remote_cmd);
            Some(spawn_remote_exec(&config, &remote_cmd)?)
        }
    } else if appload_launch {
        let session = ssh::connect_for_detection(&config)?;
        screen_client::ensure_appload_manifest(
            &session,
            launch_shim.expect("appload_launch implies shim"),
            "127.0.0.1",
            screen_cli.remote_port,
            stream_info.size.0,
            stream_info.size.1,
            screen_cli.latency_log,
            &qtfb_shim,
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
            launch_shim,
            screen_cli.latency_log,
            &qtfb_shim,
            !screen_cli.exclusive_display,
        );
        let remote_cmd = if screen_cli.exclusive_display {
            screen_client::wrap_xochitl_exclusive_command(&remote_cmd)
        } else {
            remote_cmd
        };
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
    let mut ack_sock = sock
        .try_clone()
        .map_err(|e| format!("TCP try_clone for ACK reads: {e}"))?;
    let (ack_tx, ack_rx) = mpsc::channel::<Result<BatchAck, String>>();
    let ack_reader = std::thread::spawn(move || {
        loop {
            let mut buf = [0u8; ACK_SIZE];
            match ack_sock.read_exact(&mut buf) {
                Ok(()) => {
                    let Some(ack) = BatchAck::from_bytes(&buf) else {
                        let _ = ack_tx.send(Err("malformed batch ACK".to_string()));
                        break;
                    };
                    if ack_tx.send(Ok(ack)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = ack_tx.send(Err(format!("ACK read: {e}")));
                    break;
                }
            }
        }
    });

    let mut sock = sock;
    let mut damage = DamageDetector::new(damage_config_for_screen_mirror());
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
    if screen_cli.latency_log {
        info!("rm-screen latency mode: logging concise batch timing summaries plus rm_mirror_latency lines");
    } else {
        info!(
            "rm-screen stream log: one INFO line per update (ACK runs in parallel with fetching the next frame). \
             Use --stream-trace for iterations with no dirty region."
        );
    }
    let sparse_raw = screen_cli.sparse_parts_max;
    let sparse_parts_max = sparse_raw.clamp(1, 16);
    if sparse_raw != sparse_parts_max {
        warn!(
            "--sparse-parts-max {sparse_raw} clamped to {sparse_parts_max} (valid range 1–16)",
        );
    }
    info!("sparse_parts_max={sparse_parts_max}");
    info!("damage: host pixel diff vs previous PipeWire frame (BGRx tiles → merged regions)");

    let mut frame_count: u64 = 0;
    let mut frames_dropped: u64 = 0;
    let mut last_progress = Instant::now();
    let mut stream_seq: u64 = 0;
    let mut next_batch_id: u32 = 1;
    let mut inflight_batches: VecDeque<PendingBatch> = VecDeque::new();
    // Reused packed BGRx rows when PipeWire uses stride > width×4 (compositor padding).
    let mut tight_bgra_scratch: Vec<u8> = Vec::new();
    let mut logged_stride_pack: bool = false;

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

    'stream: loop {
        if let Err(e) = drain_batch_acks(&ack_rx, &mut inflight_batches, screen_cli.latency_log) {
            error!("ACK drain failed: {e}");
            break;
        }
        if let Err(e) = wait_for_batch_slot(&ack_rx, &mut inflight_batches, screen_cli.latency_log) {
            error!("ACK wait failed: {e}");
            break;
        }
        let tw_cycle = Instant::now();
        let skipped = prev_channel_skipped;
        let ms_recv_wait = prev_recv_ms;
        let ms_drain_spin = 0.0_f64;
        let pw_header_age_ms = pipewire_header_age_ms(&latest);
        let local_queue_age_ms = system_time_age_ms(latest.capture_time);
        let pw_seq = latest.meta.header.as_ref().map(|hdr| hdr.seq);

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

        let encode_workloads = match encode_frame_workloads(
            &mut damage,
            &latest,
            &lb,
            next_batch_id,
            sparse_parts_max,
            &mut tight_bgra_scratch,
            &mut logged_stride_pack,
        ) {
            Ok(v) => v,
            Err(e) => {
                error!("stream error: {e}");
                info!("stopped after {frame_count} frames sent (encode failed)");
                break;
            }
        };

        if !encode_workloads.is_empty() {
            let batch_id = next_nonzero_batch_id(&mut next_batch_id);
            let workloads = encode_workloads;
            let n_parts = workloads.len();
            let batch_start_seq = stream_seq;

            let tw_write = Instant::now();
            let mut write_ok = true;
            for (wire_buf, _) in workloads.iter() {
                if let Err(e) = sock.write_all(wire_buf) {
                    error!("TCP write failed: {e}");
                    info!("stopped after {frame_count} frames sent");
                    write_ok = false;
                    break;
                }
            }
            if !write_ok {
                break 'stream;
            }
            if let Err(e) = sock.flush() {
                error!("TCP flush failed: {e}");
                break 'stream;
            }
            let ms_write_batch = elapsed_ms(tw_write);
            let ms_encode_batch = elapsed_ms_between(tw_cycle, tw_write);
            let first_host_unix_ms = workloads
                .first()
                .map(|(_, s)| s.host_unix_ms)
                .unwrap_or(0);

            stream_seq += n_parts as u64;
            let send_iter_ms = elapsed_ms(tw_cycle);
            inflight_batches.push_back(PendingBatch {
                batch_id,
                batch_start_seq,
                part_logs: workloads.into_iter().map(|(_, stats)| stats).collect(),
                parts: n_parts,
                first_host_unix_ms,
                encode_ms: ms_encode_batch,
                tcp_write_ms: ms_write_batch,
                sent_at: Instant::now(),
                recv_ms: ms_recv_wait,
                drain_try_ms: ms_drain_spin,
                skipped,
                send_iter_ms,
                pw_header_age_ms,
                local_queue_age_ms,
                pw_seq,
            });
            if screen_cli.latency_log {
                let host_wall_ms = unix_time_millis();
                info!(
                    target: "rm_mirror_latency",
                    "host batch sent id={} seq={}..{} parts={} encode_ms={:.2} tcp_write_ms={:.2} send_iter_ms={:.2} inflight_now={} first_host_unix_ms={} host_wall_now_ms={} \
                     pw_header_age_ms={} local_queue_age_ms={} pw_seq={} \
                     | the host can now keep newer batches in flight instead of blocking on every ACK",
                    batch_id,
                    batch_start_seq + 1,
                    batch_start_seq + n_parts as u64,
                    n_parts,
                    ms_encode_batch,
                    ms_write_batch,
                    send_iter_ms,
                    inflight_batches.len(),
                    first_host_unix_ms,
                    host_wall_ms,
                    fmt_opt_ms(pw_header_age_ms),
                    fmt_opt_ms(local_queue_age_ms),
                    fmt_opt_u64(pw_seq),
                );
            }
        } else {
            if screen_cli.stream_trace {
                info!(
                    "rm-screen stream trace iter={frame_count} recv_wait_ms={ms_recv_wait:.1} drain_try_ms={ms_drain_spin:.1} \
                     channel_dropped={skipped} (no encoded update; waiting for next frame)",
                );
            } else if ms_recv_wait >= 2000.0 {
                warn!(
                    "rm-screen stream iter={frame_count} recv_wait_ms={ms_recv_wait:.1} — blocked ~{:.0}s waiting for PipeWire frame",
                    ms_recv_wait / 1000.0,
                );
            }
        }
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
    drop(frame_rx);
    drop(sock);
    if let Err(e) = ack_reader.join() {
        error!("ACK reader thread join: {e:?}");
    }
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
    fb_x_to_src_x: Vec<u32>,
    fb_y_to_src_y: Vec<u32>,
}

impl Letterbox {
    fn new(src_w: u32, src_h: u32, fb_w: u32, fb_h: u32) -> Self {
        let scale = (fb_w as f64 / src_w as f64).min(fb_h as f64 / src_h as f64);
        let dst_fit_w = ((src_w as f64 * scale).floor() as u32) & !1;
        let dst_fit_h = (src_h as f64 * scale).floor() as u32;
        let off_x = fb_w.saturating_sub(dst_fit_w) / 2;
        let off_y = fb_h.saturating_sub(dst_fit_h) / 2;
        let fb_x_to_src_x = build_fb_to_src_axis_map(fb_w, off_x, scale, src_w);
        let fb_y_to_src_y = build_fb_to_src_axis_map(fb_h, off_y, scale, src_h);
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
            fb_x_to_src_x,
            fb_y_to_src_y,
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

    #[inline]
    fn src_x_at_fb(&self, screen_x: u32) -> u32 {
        self.fb_x_to_src_x[screen_x.min(self.fb_w.saturating_sub(1)) as usize]
    }

    #[inline]
    fn src_y_at_fb(&self, screen_y: u32) -> u32 {
        self.fb_y_to_src_y[screen_y.min(self.fb_h.saturating_sub(1)) as usize]
    }
}

fn build_fb_to_src_axis_map(fb_len: u32, off: u32, scale: f64, src_len: u32) -> Vec<u32> {
    let mut out = Vec::with_capacity(fb_len as usize);
    let src_max = src_len.saturating_sub(1) as f64;
    let off = off as f64;
    for screen in 0..fb_len {
        let pos = (screen as f64 + 0.5 - off) / scale;
        out.push(pos.floor().clamp(0.0, src_max) as u32);
    }
    out
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
    /// Same PipeWire frame split into N TCP updates (1 = single merged rect).
    sparse_parts: u8,
    payload_bytes: usize,
    wire_bytes: usize,
    ms_pack: f64,
    ms_lz4: f64,
    ms_write: f64,
    /// `UpdateHeader.host_unix_ms` stamped when this part was encoded (before TCP).
    host_unix_ms: u64,
}

/// If several damage rects are far apart, their merged bounding box is huge — EPDC refresh then costs
/// almost as much as a full screen. Split into separate updates when merged area ≫ sum of rect areas.
const SPARSE_MERGE_AREA_FACTOR: u64 = 3;
/// Merge many small dirty rects into at most `max_parts` capture bboxes (sort by `(y,x)`, slice, bbox each slice).
fn cluster_clip_rects_to_bboxes(clipped: Vec<(u32, u32, u32, u32)>, max_parts: usize) -> Vec<(u32, u32, u32, u32)> {
    let k = max_parts.min(clipped.len()).max(1);
    let mut items = clipped;
    items.sort_by(|a, b| (a.1, a.0).cmp(&(b.1, b.0)));
    let n = items.len();
    let mut out = Vec::with_capacity(k);
    let base = n / k;
    let rem = n % k;
    let mut idx = 0usize;
    for part_i in 0..k {
        let take = base + usize::from(part_i < rem);
        if take == 0 {
            continue;
        }
        let chunk = &items[idx..idx + take];
        idx += take;
        let mut x0 = u32::MAX;
        let mut y0 = u32::MAX;
        let mut x1 = 0u32;
        let mut y1 = 0u32;
        for (rx, ry, rw, rh) in chunk {
            x0 = x0.min(*rx);
            y0 = y0.min(*ry);
            x1 = x1.max(rx + rw);
            y1 = y1.max(ry + rh);
        }
        if x0 < x1 && y0 < y1 {
            out.push((x0, y0, x1, y1));
        }
    }
    out
}

fn plan_sparse_capture_bboxes(
    regions: &[DetectedRegion],
    w: u32,
    h: u32,
    max_split_parts: usize,
) -> Vec<(u32, u32, u32, u32)> {
    let mut clipped: Vec<(u32, u32, u32, u32)> = Vec::new();
    for r in regions {
        let (rx, ry, rw, rh) = clip_region(*r, w, h);
        if rw < 2 || rh < 1 {
            continue;
        }
        clipped.push((rx as u32, ry as u32, rw as u32, rh as u32));
    }
    if clipped.is_empty() {
        return vec![];
    }

    let mut mx0 = w;
    let mut my0 = h;
    let mut mx1 = 0u32;
    let mut my1 = 0u32;
    for (rx, ry, rw, rh) in &clipped {
        mx0 = mx0.min(*rx);
        my0 = my0.min(*ry);
        mx1 = mx1.max(rx + rw);
        my1 = my1.max(ry + rh);
    }
    if mx0 >= mx1 || my0 >= my1 {
        return vec![];
    }

    let sum_area: u64 = clipped
        .iter()
        .map(|(_, _, rw, rh)| *rw as u64 * *rh as u64)
        .sum();
    let merged_area = (mx1 - mx0) as u64 * (my1 - my0) as u64;
    let sparse =
        clipped.len() > 1 && merged_area > sum_area.saturating_mul(SPARSE_MERGE_AREA_FACTOR);

    if !sparse || max_split_parts <= 1 {
        return vec![(mx0, my0, mx1, my1)];
    }

    if clipped.len() <= max_split_parts {
        let mut v: Vec<(u32, u32, u32, u32, u64)> = clipped
            .into_iter()
            .map(|(rx, ry, rw, rh)| {
                let area = rw as u64 * rh as u64;
                (rx, ry, rx + rw, ry + rh, area)
            })
            .collect();
        v.sort_by_key(|e| e.4);
        return v.into_iter().map(|(a, b, c, d, _)| (a, b, c, d)).collect();
    }

    cluster_clip_rects_to_bboxes(clipped, max_split_parts)
}

/// One capture-space bbox → wire buffer (`ms_write` filled by caller).
fn encode_capture_bbox(
    frame: &VideoFrame,
    lb: &Letterbox,
    data: &[u8],
    w: u32,
    h: u32,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    batch_id: u32,
    part_index: u16,
    part_count: u16,
    region_rects: usize,
    sparse_parts: u8,
) -> DynResult<Option<(Vec<u8>, StreamSendStats)>> {
    if x0 >= x1 || y0 >= y1 {
        return Ok(None);
    }
    let mw = (x1 - x0) as u16;
    let mh = (y1 - y0) as u16;
    if mw < 1 || mh < 1 {
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
    let packed = pack_region_rgb565_fb(
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

    let host_unix_ms = unix_time_millis();
    let header = UpdateHeader {
        x: fb_ix as u16,
        y: fb_iy as u16,
        width: fb_mw,
        height: fb_mh,
        waveform: UPDATE_COORDS_FRAMEBUFFER,
        payload_size: compressed.len() as u32,
        host_unix_ms,
        batch_id,
        part_index,
        part_count,
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
            sparse_parts,
            payload_bytes: packed.len(),
            wire_bytes,
            ms_pack,
            ms_lz4,
            ms_write: 0.0,
            host_unix_ms,
        },
    )))
}

/// Dirty regions for the current PipeWire frame → one or more bbox encodes (sparse split when cheap).
fn encode_frame_workloads(
    damage: &mut DamageDetector,
    frame: &VideoFrame,
    lb: &Letterbox,
    batch_id: u32,
    sparse_parts_max: usize,
    tight_bgra_scratch: &mut Vec<u8>,
    logged_stride_pack: &mut bool,
) -> DynResult<Vec<(Vec<u8>, StreamSendStats)>> {
    let Some(data_arc) = frame.data() else {
        warn!("DMA-BUF frame skipped (no CPU pixels); disable GPU-only capture in compositor if this repeats");
        damage.invalidate();
        return Ok(vec![]);
    };
    let data = data_arc.as_slice();
    let w = frame.width;
    let h = frame.height;

    let regions = regions_for_frame(
        frame,
        damage,
        data,
        w,
        h,
        tight_bgra_scratch,
        logged_stride_pack,
    );
    let region_rects = regions.len();
    let bboxes = plan_sparse_capture_bboxes(&regions, w, h, sparse_parts_max);
    let sparse_parts = u8::try_from(bboxes.len()).unwrap_or(u8::MAX);
    let part_count = u16::try_from(bboxes.len()).unwrap_or(u16::MAX);

    let mut out = Vec::with_capacity(bboxes.len());
    for (part_i, (x0, y0, x1, y1)) in bboxes.into_iter().enumerate() {
        if let Some(pair) = encode_capture_bbox(
            frame,
            lb,
            data,
            w,
            h,
            x0,
            y0,
            x1,
            y1,
            batch_id,
            u16::try_from(part_i).unwrap_or(u16::MAX),
            part_count,
            region_rects,
            sparse_parts,
        )?
        {
            out.push(pair);
        }
    }
    Ok(out)
}

fn regions_for_frame(
    frame: &VideoFrame,
    damage: &mut DamageDetector,
    data: &[u8],
    w: u32,
    h: u32,
    tight_bgra_scratch: &mut Vec<u8>,
    logged_stride_pack: &mut bool,
) -> Vec<DetectedRegion> {
    let regions = detect_damage_bgra(
        frame,
        damage,
        data,
        w,
        h,
        tight_bgra_scratch,
        logged_stride_pack,
    )
    .unwrap_or_else(|| {
        warn!(
            "cannot run host pixel diff (format={:?} stride={} len={} for {}×{}); no update this frame",
            frame.format,
            frame.stride,
            data.len(),
            w,
            h
        );
        vec![]
    });

    regions
}

/// BGRA/BGRx/RGBA/RGBx only.
fn detect_damage_bgra(
    frame: &VideoFrame,
    damage: &mut DamageDetector,
    data: &[u8],
    w: u32,
    h: u32,
    tight_bgra_scratch: &mut Vec<u8>,
    logged_stride_pack: &mut bool,
) -> Option<Vec<DetectedRegion>> {
    if !frame_format_is_packed_rgba_family(frame.format) {
        return None;
    }
    let w_us = w as usize;
    let h_us = h as usize;
    let stride = frame.stride as usize;
    let tight_bytes = w_us.checked_mul(h_us)?.checked_mul(4)?;
    let row = w_us * 4;
    if stride < row {
        return None;
    }
    let need_data = stride.checked_mul(h_us.saturating_sub(1))?.checked_add(row)?;
    if data.len() < need_data {
        return None;
    }

    if frame.stride == w.saturating_mul(4) && data.len() >= tight_bytes {
        return Some(damage.detect(&data[..tight_bytes], w, h));
    }

    tight_bgra_scratch.clear();
    tight_bgra_scratch.resize(tight_bytes, 0);
    for y in 0..h_us {
        let src = y * stride;
        let dst = y * row;
        tight_bgra_scratch[dst..dst + row].copy_from_slice(&data[src..src + row]);
    }

    if !*logged_stride_pack {
        *logged_stride_pack = true;
        info!(
            "host pixel diff: packing stride={} rows to tight {}×{}×4 (compositor stride padding — bbox from real pixels only)",
            frame.stride, w, h
        );
    }

    Some(damage.detect(tight_bgra_scratch.as_slice(), w, h))
}

fn frame_format_is_packed_rgba_family(format: PixelFormat) -> bool {
    matches!(
        format,
        PixelFormat::BGRA | PixelFormat::BGRx | PixelFormat::RGBA | PixelFormat::RGBx
    )
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
#[inline]
fn gray_at_fb_pixel(
    data: &[u8],
    stride: u32,
    format: PixelFormat,
    lb: &Letterbox,
    screen_x: u32,
    screen_y: u32,
) -> u8 {
    let src_ix = lb.src_x_at_fb(screen_x);
    let src_iy = lb.src_y_at_fb(screen_y);
    let bpp = format.bytes_per_pixel().max(3);
    let stride = stride as usize;
    let row_off = src_iy as usize * stride;
    let x0 = src_ix as usize * bpp;
    let o0 = row_off + x0;
    gray_from_pixel(format, &data[o0..data.len().min(o0 + bpp)]).unwrap_or(0)
}

/// Pack a region that already lies in framebuffer space (downsampled from capture on the host).
fn pack_region_rgb565_fb(
    data: &[u8],
    stride: u32,
    format: PixelFormat,
    lb: &Letterbox,
    fb_ix0: u32,
    fb_iy0: u32,
    w: u16,
    h: u16,
) -> DynResult<Vec<u8>> {
    let mut out = Vec::with_capacity(w as usize * h as usize * 2);
    for row in 0..h {
        let screen_y = fb_iy0 + row as u32;
        for col in 0..w {
            let screen_x = fb_ix0 + col as u32;
            let gray = gray_at_fb_pixel(data, stride, format, lb, screen_x, screen_y);
            out.extend_from_slice(&gray8_to_rgb565(gray));
        }
    }
    Ok(out)
}

#[inline]
fn gray8_to_rgb565(gray: u8) -> [u8; 2] {
    let g = gray as u16;
    let rgb565 = ((g >> 3) << 11) | ((g >> 2) << 5) | (g >> 3);
    rgb565.to_le_bytes()
}
