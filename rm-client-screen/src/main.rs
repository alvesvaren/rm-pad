//! Tablet-side receiver: TCP → LZ4 RGB565 patches → framebuffer partial updates.
//!
//! Run as `rm-client-screen [HOST] [PORT] [SRC_W SRC_H]` (defaults `127.0.0.1` `9876`).
//! SRC_W/SRC_H are the host capture size (e.g. 1920×1200); regions are letterboxed to fit
//! the device framebuffer. `rm-screen` passes these automatically.
//!
//! Defaults follow rmkit `RemarkableFB::perform_redraw` (Harmony / github.com/rmkit-dev/rmkit
//! `src/rmkit/fb/fb.cpy`): **DU** waveform + **EXP1** dither for fast partials.
//! For slower, higher-quality grays: `RM_CLIENT_SCREEN_WAVEFORM=gl16_fast` and
//! `RM_CLIENT_SCREEN_DITHER=passthrough`.
//!
//! End-to-end timing: `RM_MIRROR_LATENCY_LOG=1` on **both** PC and tablet, plus
//! `RUST_LOG=rm_mirror_latency=info` (or `trace`) to see where time goes.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::time::Instant;
use libc::{poll, pollfd, POLLIN};
use libremarkable::framebuffer::common::{dither_mode, display_temp, mxcfb_rect, waveform_mode};
use libremarkable::framebuffer::core::Framebuffer;
use libremarkable::framebuffer::{FramebufferIO, FramebufferRefresh, PartialRefreshMode};
use log::{error, info, warn};
use rm_common::expand_rect_to_epdc_grid;
use rm_common::protocol::{unix_time_millis, BatchAck, UpdateHeader, HEADER_SIZE, UPDATE_COORDS_FRAMEBUFFER};

type DynResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct ParsedBatch {
    batch_id: u32,
    updates: Vec<(UpdateHeader, Vec<u8>)>,
}

const FBSPY_TYPE_RGB565: u32 = 1;
const FBSPY_TYPE_RGBA: u32 = 2;

struct FramebufferSpyConfig {
    address: u64,
    width: u32,
    height: u32,
    pixel_type: u32,
    bpl: u32,
    requires_reload: bool,
}

enum PatchWriter {
    Libremarkable,
    XochitlMem(XochitlMemFramebuffer),
}

struct XochitlMemFramebuffer {
    mem: File,
    config: FramebufferSpyConfig,
}

impl FramebufferSpyConfig {
    fn parse(raw: &str) -> DynResult<Self> {
        let trimmed = raw.trim();
        let mut parts = trimmed.split(',');
        let address = parts.next().ok_or("missing framebuffer address")?;
        let width = parts.next().ok_or("missing framebuffer width")?.parse()?;
        let height = parts.next().ok_or("missing framebuffer height")?.parse()?;
        let pixel_type = parts.next().ok_or("missing framebuffer type")?.parse()?;
        let bpl = parts.next().ok_or("missing framebuffer bpl")?.parse()?;
        let requires_reload = match parts.next().ok_or("missing framebuffer reload flag")? {
            "0" => false,
            "1" => true,
            _ => return Err("invalid framebuffer reload flag".into()),
        };
        if parts.next().is_some() {
            return Err("unexpected extra framebuffer config fields".into());
        }
        let address = address
            .strip_prefix("0x")
            .ok_or("framebuffer address missing 0x prefix")?;
        Ok(Self {
            address: u64::from_str_radix(address, 16)?,
            width,
            height,
            pixel_type,
            bpl,
            requires_reload,
        })
    }

    fn bytes_per_pixel(&self) -> DynResult<usize> {
        match self.pixel_type {
            FBSPY_TYPE_RGB565 => Ok(2),
            FBSPY_TYPE_RGBA => Ok(4),
            _ => Err(format!("unsupported framebuffer-spy pixel type {}", self.pixel_type).into()),
        }
    }

    fn pixel_type_name(&self) -> &'static str {
        match self.pixel_type {
            FBSPY_TYPE_RGB565 => "RGB565",
            FBSPY_TYPE_RGBA => "RGBA/BGRA8888",
            _ => "unknown",
        }
    }
}

impl PatchWriter {
    fn discover(fb_w: u32, fb_h: u32) -> DynResult<Self> {
        let mode = std::env::var("RM_CLIENT_SCREEN_FB_BACKEND")
            .unwrap_or_else(|_| "libremarkable".to_string());
        match mode.as_str() {
            "libremarkable" => {
                info!("framebuffer pixel backend: libremarkable restore_region");
                Ok(Self::Libremarkable)
            }
            "fbspy" | "auto" => match XochitlMemFramebuffer::discover(fb_w, fb_h) {
                Ok(writer) => Ok(Self::XochitlMem(writer)),
                Err(err) => {
                    warn!(
                        "framebuffer-spy backend unavailable ({err}); falling back to libremarkable restore_region"
                    );
                    Ok(Self::Libremarkable)
                }
            },
            other => Err(format!(
                "unsupported RM_CLIENT_SCREEN_FB_BACKEND={other} (expected fbspy|auto|libremarkable)"
            )
            .into()),
        }
    }
}

impl XochitlMemFramebuffer {
    fn discover(fb_w: u32, fb_h: u32) -> DynResult<Self> {
        let config = FramebufferSpyConfig::parse(&query_framebuffer_spy_config_string()?)?;
        let xochitl_pid = find_xochitl_pid()?;
        let mem = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/proc/{xochitl_pid}/mem"))?;
        if config.width != fb_w || config.height != fb_h {
            warn!(
                "framebuffer-spy geometry {}×{} differs from libremarkable {}×{}; using framebuffer-spy rows for writes",
                config.width, config.height, fb_w, fb_h
            );
        }
        if config.requires_reload {
            warn!(
                "framebuffer-spy requested reload-before-access; continuing anyway (expected false on RM2/qtfb paths)"
            );
        }
        info!(
            "framebuffer pixel backend: framebuffer-spy via /proc/{xochitl_pid}/mem addr=0x{:x} size={}×{} stride={} format={}",
            config.address,
            config.width,
            config.height,
            config.bpl,
            config.pixel_type_name()
        );
        Ok(Self { mem, config })
    }

    fn read_region_rgb565(&self, rect: mxcfb_rect) -> DynResult<Vec<u8>> {
        let row_out = rect.width as usize * 2;
        let mut out = vec![0u8; row_out * rect.height as usize];
        let bytes_per_pixel = self.config.bytes_per_pixel()?;
        let row_in = rect.width as usize * bytes_per_pixel;
        let mut row_buf = vec![0u8; row_in];
        for row in 0..rect.height as usize {
            let offset = self.byte_offset(rect.left, rect.top + row as u32)?;
            self.mem.read_exact_at(&mut row_buf, offset)?;
            match self.config.pixel_type {
                FBSPY_TYPE_RGB565 => {
                    out[row * row_out..(row + 1) * row_out].copy_from_slice(&row_buf);
                }
                FBSPY_TYPE_RGBA => {
                    for col in 0..rect.width as usize {
                        let src = col * 4;
                        let dst = row * row_out + col * 2;
                        let b = row_buf[src];
                        let g = row_buf[src + 1];
                        let r = row_buf[src + 2];
                        let pixel = rgb888_to_rgb565(r, g, b).to_le_bytes();
                        out[dst] = pixel[0];
                        out[dst + 1] = pixel[1];
                    }
                }
                _ => unreachable!(),
            }
        }
        Ok(out)
    }

    fn write_region_rgb565(&mut self, rect: mxcfb_rect, patch: &[u8]) -> DynResult<()> {
        let row_in = rect.width as usize * 2;
        if patch.len() != row_in * rect.height as usize {
            return Err("patch length does not match xochitl mem rect".into());
        }
        let bytes_per_pixel = self.config.bytes_per_pixel()?;
        let row_out = rect.width as usize * bytes_per_pixel;
        let mut row_buf = vec![0u8; row_out];
        for row in 0..rect.height as usize {
            let src = &patch[row * row_in..(row + 1) * row_in];
            match self.config.pixel_type {
                FBSPY_TYPE_RGB565 => row_buf.copy_from_slice(src),
                FBSPY_TYPE_RGBA => {
                    for col in 0..rect.width as usize {
                        let src_px = col * 2;
                        let dst_px = col * 4;
                        let px = u16::from_le_bytes([src[src_px], src[src_px + 1]]);
                        let (r, g, b) = rgb565_to_rgb888(px);
                        row_buf[dst_px] = b;
                        row_buf[dst_px + 1] = g;
                        row_buf[dst_px + 2] = r;
                        row_buf[dst_px + 3] = 0xFF;
                    }
                }
                _ => unreachable!(),
            }
            let offset = self.byte_offset(rect.left, rect.top + row as u32)?;
            self.mem.write_all_at(&row_buf, offset)?;
        }
        Ok(())
    }

    fn byte_offset(&self, left: u32, top: u32) -> DynResult<u64> {
        if left >= self.config.width || top >= self.config.height {
            return Err("framebuffer-spy write outside bounds".into());
        }
        let bytes_per_pixel = self.config.bytes_per_pixel()? as u64;
        let offset = self.config.address
            + top as u64 * self.config.bpl as u64
            + left as u64 * bytes_per_pixel;
        Ok(offset)
    }
}

fn mirror_latency_log_enabled() -> bool {
    matches!(
        std::env::var("RM_MIRROR_LATENCY_LOG").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    )
}

fn ms_between(start: Instant, end: Instant) -> f64 {
    end.saturating_duration_since(start).as_secs_f64() * 1000.0
}

fn main() -> DynResult<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let (addr, source_dims) = parse_args()?;

    let mut stream = TcpStream::connect(&addr)?;
    info!("rm-client-screen connected to {}", addr);

    stream.set_nodelay(true)?;
    stream.set_nonblocking(true)?;
    // Small receive buffer to limit how much data can queue on our side.
    unsafe {
        let buf_size: libc::c_int = 32 * 1024;
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &buf_size as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }

    let mut fb = Framebuffer::new();
    let fb_w = fb.var_screen_info.xres;
    let fb_h = fb.var_screen_info.yres;
    let mut patch_writer = PatchWriter::discover(fb_w, fb_h)?;

    let (src_w, src_h) = match source_dims {
        Some((w, h)) if w > 0 && h > 0 => (w, h),
        _ => {
            warn!(
                "no valid SRC_W SRC_H; assuming source equals device {}×{} (wrong unless resolutions match)",
                fb_w, fb_h
            );
            (fb_w, fb_h)
        }
    };

    let scale = (fb_w as f64 / src_w as f64).min(fb_h as f64 / src_h as f64);
    let dst_fit_w = ((src_w as f64 * scale).floor() as u32) & !1;
    let dst_fit_h = (src_h as f64 * scale).floor() as u32;
    let off_x = fb_w.saturating_sub(dst_fit_w) / 2;
    let off_y = fb_h.saturating_sub(dst_fit_h) / 2;

    info!(
        "framebuffer {}×{}, host capture {}×{} — scale {:.4} letterbox offset ({}, {}) fitted {}×{}",
        fb_w, fb_h, src_w, src_h, scale, off_x, off_y, dst_fit_w, dst_fit_h
    );
    info!(
        "EPD line_length={} bpp={} (RM2: buffer is usually /dev/shm/swtfb.01 + imx epdc; updates go via MXCFB_SEND_UPDATE / rm2fb)",
        fb.fix_screen_info.line_length,
        fb.var_screen_info.bits_per_pixel
    );

    // Native pen ink uses async-style updates; `Wait` adds ~1 EPDC frame time per mirror patch.
    // With host-side windowed batches, Async is usually best. We default to early ACK after the full
    // batch has been written to the framebuffer, before `partial_refresh`, so the PC can move on while
    // the EPDC drains in the background.
    // RM_CLIENT_SCREEN_WAIT_REFRESH=1 if you see ghosting; RM_CLIENT_SCREEN_EARLY_ACK=0 if tearing.
    let refresh_mode = match std::env::var("RM_CLIENT_SCREEN_WAIT_REFRESH").as_deref() {
        Ok("1") => {
            info!("RM_CLIENT_SCREEN_WAIT_REFRESH=1 — block until EPDC accepts update (slower, steadier)");
            PartialRefreshMode::Wait
        }
        _ => PartialRefreshMode::Async,
    };

    let waveform = waveform_from_env();
    let dither = dither_from_env();
    let refresh_label = if matches!(refresh_mode, PartialRefreshMode::Wait) {
        "Wait"
    } else {
        "Async"
    };
    info!(
        "EPDC refresh={} waveform={:?} dither={:?} (WAVEFORM=du|gl16_fast|… DITHER=exp1|passthrough|drawing)",
        refresh_label, waveform, dither
    );

    // Default on for Async: otherwise the host waits for the whole batch only after the last
    // `partial_refresh`, which puts EPDC latency directly back into the send window.
    let early_ack = matches!(refresh_mode, PartialRefreshMode::Async)
        && !matches!(std::env::var("RM_CLIENT_SCREEN_EARLY_ACK").as_deref(), Ok("0"));
    if early_ack {
        info!(
            "early ACK after batch FB writes, before partial_refresh (set RM_CLIENT_SCREEN_EARLY_ACK=0 to ACK after EPDC; slower host loop)"
        );
    } else if matches!(refresh_mode, PartialRefreshMode::Async) {
        info!("RM_CLIENT_SCREEN_EARLY_ACK=0 — ACK after partial_refresh (slower; steadier if you see tearing)");
    }

    let latency_log = mirror_latency_log_enabled();
    if latency_log {
        info!("RM_MIRROR_LATENCY_LOG=1 — also set RUST_LOG=rm_mirror_latency=info (tablet) and run rm-screen with --latency-log (PC)");
    }

    run_stream(
        &mut patch_writer,
        &mut fb,
        &mut stream,
        fb_w,
        fb_h,
        scale,
        off_x,
        off_y,
        refresh_mode,
        waveform,
        dither,
        early_ack,
        latency_log,
    )?;

    Ok(())
}

fn parse_args() -> DynResult<(String, Option<(u32, u32)>)> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.len() {
        0 => Ok(("127.0.0.1:9876".to_string(), None)),
        1 => {
            if argv[0].contains(':') {
                Ok((argv[0].clone(), None))
            } else {
                Err("usage: rm-client-screen HOST PORT [SRC_W SRC_H]".into())
            }
        }
        2 => {
            let port: u16 = argv[1].parse().map_err(|_| "PORT must be a number")?;
            Ok((format!("{}:{}", argv[0], port), None))
        }
        4 => {
            let port: u16 = argv[1].parse().map_err(|_| "PORT must be a number")?;
            let sw: u32 = argv[2].parse().map_err(|_| "SRC_W must be u32")?;
            let sh: u32 = argv[3].parse().map_err(|_| "SRC_H must be u32")?;
            Ok((format!("{}:{}", argv[0], port), Some((sw, sh))))
        }
        _ => Err("usage: rm-client-screen HOST PORT [SRC_W SRC_H]".into()),
    }
}

fn run_stream(
    patch_writer: &mut PatchWriter,
    fb: &mut Framebuffer,
    stream: &mut TcpStream,
    fb_w: u32,
    fb_h: u32,
    scale: f64,
    off_x: u32,
    off_y: u32,
    refresh_mode: PartialRefreshMode,
    waveform: waveform_mode,
    dither: dither_mode,
    early_ack: bool,
    latency_log: bool,
) -> DynResult<()> {
    let fd = stream.as_raw_fd();
    let mut buf: Vec<u8> = Vec::new();
    let mut updates_ok: u64 = 0;

    loop {
        ensure_min_bytes(stream, fd, &mut buf, HEADER_SIZE)?;
        if buf.is_empty() {
            break;
        }

        // Greedily read all available data so we can skip stale updates.
        drain_available(stream, &mut buf);

        let mut batches = parse_complete_batches(&mut buf);
        if batches.is_empty() {
            continue;
        }
        if batches.len() > 1 {
            let dropped = batches.len() - 1;
            for stale in batches.drain(..dropped) {
                info!(
                    "dropping stale batch {} ({} parts already superseded by newer data)",
                    stale.batch_id,
                    stale.updates.len()
                );
                send_batch_ack(stream, stale.batch_id);
            }
        }
        let batch = batches.pop().unwrap();
        let t_batch = Instant::now();
        let first_host_unix_ms = batch
            .updates
            .first()
            .map(|(header, _)| header.host_unix_ms)
            .unwrap_or(0);
        let mut batch_lz4_ms = 0.0;
        let mut batch_fb_write_ms = 0.0;
        let mut refresh_rects: Vec<mxcfb_rect> = Vec::new();
        let mut batch_parts_ok = 0usize;

        for (header, payload) in batch.updates {
            let t_msg = Instant::now();
            let raw = match lz4_flex::block::decompress_size_prepended(&payload) {
                Ok(v) => v,
                Err(e) => {
                    warn!("LZ4 error: {e}");
                    continue;
                }
            };
            let t_after_lz4 = Instant::now();
            batch_lz4_ms += ms_between(t_msg, t_after_lz4);

            let w = header.width as u32;
            let h = header.height as u32;
            let expected = w * h * 2;
            if raw.len() != expected as usize {
                warn!("payload mismatch: {} vs {} for {}×{}", raw.len(), expected, w, h);
                continue;
            }

            let mapped = if header.waveform == UPDATE_COORDS_FRAMEBUFFER {
                let rect = mxcfb_rect {
                    top: header.y as u32,
                    left: header.x as u32,
                    width: w,
                    height: h,
                };
                if rect.width < 2 || rect.height < 1 {
                    None
                } else {
                    Some((rect, raw))
                }
            } else {
                map_region_rgb565_to_fb(
                    &raw, w, h, header.x as u32, header.y as u32,
                    scale, off_x, off_y, fb_w, fb_h,
                )
            };
            if let Some((rect, patch)) = mapped {
                match write_patch_to_fb(patch_writer, fb, rect, &patch, fb_w, fb_h) {
                    Ok(()) => {
                        let t_after_fb_write = Instant::now();
                        batch_fb_write_ms += ms_between(t_after_lz4, t_after_fb_write);
                        refresh_rects.push(expand_to_8px_grid(rect, fb_w, fb_h));
                        batch_parts_ok += 1;
                        updates_ok += 1;
                        if updates_ok <= 3 {
                            info!(
                                "update #{updates_ok}: queued {}×{}@({},{}) in batch {}",
                                rect.width, rect.height, rect.left, rect.top, header.batch_id,
                            );
                        }
                    }
                    Err(e) => {
                        error!("fb write {:?}: {e}", rect);
                    }
                }
            }
        }
        if early_ack {
            send_batch_ack(stream, batch.batch_id);
        }
        let t_before_epdc = Instant::now();
        for rect in &refresh_rects {
            fb.partial_refresh(
                rect,
                match refresh_mode {
                    PartialRefreshMode::Async => PartialRefreshMode::Async,
                    PartialRefreshMode::Wait => PartialRefreshMode::Wait,
                    PartialRefreshMode::DryRun => PartialRefreshMode::DryRun,
                },
                waveform,
                display_temp::TEMP_USE_REMARKABLE_DRAW,
                dither,
                0,
                false,
            );
        }
        let t_after_epdc = Instant::now();
        if !early_ack {
            send_batch_ack(stream, batch.batch_id);
        }
        if latency_log {
            let client_wall_ms = unix_time_millis();
            let wall_skew_ms = client_wall_ms as i128 - first_host_unix_ms as i128;
            let batch_total_ms = ms_between(t_batch, t_after_epdc);
            info!(
                target: "rm_mirror_latency",
                "tablet batch id={} parts_ok={} refreshes={} lz4_ms={:.2} fb_write_ms={:.2} epdc_ioctl_ms={:.2} batch_total_ms={:.2} \
                 wall_now−host_stamp={}ms (±clock skew; large with tiny local stages → delay before host stamped or network) host_stamp_ms={} early_ack={}",
                batch.batch_id,
                batch_parts_ok,
                refresh_rects.len(),
                batch_lz4_ms,
                batch_fb_write_ms,
                ms_between(t_before_epdc, t_after_epdc),
                batch_total_ms,
                wall_skew_ms,
                first_host_unix_ms,
                early_ack,
            );
        }
    }

    info!("stream ended: {updates_ok} partial updates applied");
    Ok(())
}

fn send_batch_ack(stream: &mut TcpStream, batch_id: u32) {
    // Socket is non-blocking; `write_all` returns WouldBlock on a full send buffer and would drop
    // the ACK, so the host blocks on `read_exact` and the tunnel piles up data.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.write_all(&BatchAck::ok(batch_id).to_bytes());
    let _ = stream.flush();
    let _ = stream.set_nonblocking(true);
}

/// Parse all complete batches from the front of `buf`, leaving any trailing incomplete batch bytes.
fn parse_complete_batches(buf: &mut Vec<u8>) -> Vec<ParsedBatch> {
    let mut results = Vec::new();
    let mut pos = 0;
    loop {
        let batch_start = pos;
        if pos + HEADER_SIZE > buf.len() {
            break;
        }
        let hdr_slice: [u8; HEADER_SIZE] = buf[pos..pos + HEADER_SIZE].try_into().unwrap();
        let Some(first) = UpdateHeader::from_bytes(&hdr_slice) else {
            break;
        };
        if first.part_count == 0 {
            break;
        }
        let mut batch = Vec::with_capacity(first.part_count as usize);
        let mut expected_part = 0u16;
        while expected_part < first.part_count {
            if pos + HEADER_SIZE > buf.len() {
                pos = batch_start;
                break;
            }
            let hdr_slice: [u8; HEADER_SIZE] = buf[pos..pos + HEADER_SIZE].try_into().unwrap();
            let Some(header) = UpdateHeader::from_bytes(&hdr_slice) else {
                pos = batch_start;
                break;
            };
            if header.batch_id != first.batch_id
                || header.part_count != first.part_count
                || header.part_index != expected_part
            {
                pos = batch_start;
                break;
            }
            let total = HEADER_SIZE + header.payload_size as usize;
            if pos + total > buf.len() {
                pos = batch_start;
                break;
            }
            let payload = buf[pos + HEADER_SIZE..pos + total].to_vec();
            batch.push((header, payload));
            pos += total;
            expected_part += 1;
        }
        if batch.len() != first.part_count as usize {
            break;
        }
        results.push(ParsedBatch {
            batch_id: first.batch_id,
            updates: batch,
        });
    }
    if pos > 0 {
        buf.drain(..pos);
    }
    results
}

/// Non-blocking drain: read everything currently available from the socket.
fn drain_available(stream: &mut TcpStream, buf: &mut Vec<u8>) {
    let mut tmp = [0u8; 32 * 1024];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }
}

fn waveform_from_env() -> waveform_mode {
    match std::env::var("RM_CLIENT_SCREEN_WAVEFORM").as_deref() {
        Ok(s) if s.eq_ignore_ascii_case("gc16") => waveform_mode::WAVEFORM_MODE_GC16,
        Ok(s) if s.eq_ignore_ascii_case("gc16_fast") => waveform_mode::WAVEFORM_MODE_GC16_FAST,
        Ok(s) if s.eq_ignore_ascii_case("gl16_fast") => waveform_mode::WAVEFORM_MODE_GL16_FAST,
        Ok(s) if s.eq_ignore_ascii_case("reagl") => waveform_mode::WAVEFORM_MODE_REAGL,
        Ok(s) if s.eq_ignore_ascii_case("du") => waveform_mode::WAVEFORM_MODE_DU,
        // Default DU: same class of updates as rmkit Harmony drawing (fast partials; more ghosting than GL16).
        _ => waveform_mode::WAVEFORM_MODE_DU,
    }
}

fn dither_from_env() -> dither_mode {
    match std::env::var("RM_CLIENT_SCREEN_DITHER").as_deref() {
        Ok(s) if s.eq_ignore_ascii_case("passthrough") => {
            dither_mode::EPDC_FLAG_USE_DITHERING_PASSTHROUGH
        }
        Ok(s) if s.eq_ignore_ascii_case("drawing") => dither_mode::EPDC_FLAG_USE_DITHERING_DRAWING,
        // Default EXP1 — rmkit `RemarkableFB::perform_redraw` uses this for routine partials.
        _ => dither_mode::EPDC_FLAG_EXP1,
    }
}

/// Write a region patch to the framebuffer without triggering a refresh.
fn write_patch_to_fb(
    patch_writer: &mut PatchWriter,
    fb: &mut Framebuffer,
    rect: mxcfb_rect,
    patch: &[u8],
    fb_w: u32,
    fb_h: u32,
) -> Result<(), &'static str> {
    let bpp = 2usize;
    let expect = (rect.width as usize)
        .checked_mul(rect.height as usize)
        .and_then(|p| p.checked_mul(bpp))
        .ok_or("rect size overflow")?;
    if patch.len() != expect {
        return Err("patch length does not match rect");
    }

    let al = expand_to_8px_grid(rect, fb_w, fb_h);
    if al.width < 2 || al.height < 1 {
        return Ok(());
    }

    let mut canvas = if let PatchWriter::XochitlMem(xochitl_fb) = patch_writer {
        // Host pre-aligns to this grid; skip a read/merge round-trip when we can write rows directly.
        if al.left == rect.left && al.top == rect.top && al.width == rect.width && al.height == rect.height
        {
            xochitl_fb
                .write_region_rgb565(al, patch)
                .map_err(|_| "framebuffer-spy write failed")?;
            return Ok(());
        }
        xochitl_fb
            .read_region_rgb565(al)
            .map_err(|_| "framebuffer-spy read failed")?
    } else {
        // Host pre-aligns to this grid; skip dump_region + merge (saves a full framebuffer read).
        if al.left == rect.left && al.top == rect.top && al.width == rect.width && al.height == rect.height
        {
            fb.restore_region(al, patch)?;
            return Ok(());
        }
        fb.dump_region(al)?
    };
    let row_patch = rect.width as usize * bpp;
    let row_canvas = al.width as usize * bpp;
    let ox = (rect.left.saturating_sub(al.left)) as usize * bpp;
    let oy = rect.top.saturating_sub(al.top) as usize;
    for row in 0..rect.height as usize {
        let dst = (oy + row) * row_canvas + ox;
        let src = row * row_patch;
        canvas[dst..dst + row_patch].copy_from_slice(&patch[src..src + row_patch]);
    }

    match patch_writer {
        PatchWriter::Libremarkable => {
            let _ = fb.restore_region(al, &canvas)?;
        }
        PatchWriter::XochitlMem(xochitl_fb) => {
            xochitl_fb
                .write_region_rgb565(al, &canvas)
                .map_err(|_| "framebuffer-spy write failed")?;
        }
    }
    Ok(())
}

/// Map a source RGB565 patch (sw×sh at compositor coords sx,sy) into device framebuffer space.
fn map_region_rgb565_to_fb(
    rgb565: &[u8],
    sw: u32,
    sh: u32,
    sx: u32,
    sy: u32,
    scale: f64,
    off_x: u32,
    off_y: u32,
    fb_w: u32,
    fb_h: u32,
) -> Option<(mxcfb_rect, Vec<u8>)> {
    if sw == 0 || sh == 0 {
        return None;
    }

    let dx0 = off_x as f64 + sx as f64 * scale;
    let dy0 = off_y as f64 + sy as f64 * scale;
    let dx1 = dx0 + sw as f64 * scale;
    let dy1 = dy0 + sh as f64 * scale;

    let ix0 = dx0.floor().max(0.0) as u32;
    let iy0 = dy0.floor().max(0.0) as u32;
    let ix1 = dx1.ceil().min(fb_w as f64) as u32;
    let iy1 = dy1.ceil().min(fb_h as f64) as u32;

    let out_w = ix1.saturating_sub(ix0);
    let out_h = iy1.saturating_sub(iy0);
    let out_w = out_w & !1;
    if out_w < 2 || out_h < 1 {
        return None;
    }

    let mut patch = vec![0u8; (out_w * out_h * 2) as usize];

    for iy in 0..out_h {
        let screen_y = iy0 + iy;
        for ix in 0..out_w {
            let screen_x = ix0 + ix;
            let u = (screen_x as f64 + 0.5 - off_x as f64) / scale - sx as f64;
            let v = (screen_y as f64 + 0.5 - off_y as f64) / scale - sy as f64;
            let src_ix = u.floor().clamp(0.0, (sw - 1) as f64) as u32;
            let src_iy = v.floor().clamp(0.0, (sh - 1) as f64) as u32;
            let si = ((src_iy * sw + src_ix) * 2) as usize;
            let di = ((iy * out_w + ix) * 2) as usize;
            patch[di] = rgb565[si];
            patch[di + 1] = rgb565[si + 1];
        }
    }

    let rect = mxcfb_rect {
        top: iy0,
        left: ix0,
        width: out_w,
        height: out_h,
    };
    Some((rect, patch))
}

/// EPDC partial updates should use 8×8 boundaries (must match `rm_common::expand_rect_to_epdc_grid`).
fn expand_to_8px_grid(rect: mxcfb_rect, fb_w: u32, fb_h: u32) -> mxcfb_rect {
    let (l, t, w, h) = expand_rect_to_epdc_grid(
        rect.left,
        rect.top,
        rect.width,
        rect.height,
        fb_w,
        fb_h,
    );
    mxcfb_rect {
        left: l,
        top: t,
        width: w,
        height: h,
    }
}

fn rgb565_to_rgb888(px: u16) -> (u8, u8, u8) {
    let r5 = ((px >> 11) & 0x1f) as u32;
    let g6 = ((px >> 5) & 0x3f) as u32;
    let b5 = (px & 0x1f) as u32;
    (
        ((r5 * 255) / 31) as u8,
        ((g6 * 255) / 63) as u8,
        ((b5 * 255) / 31) as u8,
    )
}

fn rgb888_to_rgb565(r: u8, g: u8, b: u8) -> u16 {
    let r5 = (r as u16 >> 3) & 0x1f;
    let g6 = (g as u16 >> 2) & 0x3f;
    let b5 = (b as u16 >> 3) & 0x1f;
    (r5 << 11) | (g6 << 5) | b5
}

fn find_xochitl_pid() -> DynResult<u32> {
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let comm_path = entry.path().join("comm");
        if let Ok(comm) = fs::read_to_string(comm_path) {
            if comm.trim() == "xochitl" {
                return Ok(name);
            }
        }
    }
    Err("xochitl is not running; framebuffer-spy backend needs xochitl alive".into())
}

fn query_framebuffer_spy_config_string() -> DynResult<String> {
    if !std::path::Path::new("/run/xovi-mb").exists()
        || !std::path::Path::new("/run/xovi-mb-out").exists()
    {
        return Err("xovi-message-broker pipes are missing".into());
    }

    let out_path = CString::new("/run/xovi-mb-out")?;
    let out_fd = unsafe { libc::open(out_path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
    if out_fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    let in_path = CString::new("/run/xovi-mb")?;
    let in_fd = unsafe { libc::open(in_path.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
    if in_fd < 0 {
        unsafe {
            libc::close(out_fd);
        }
        return Err(std::io::Error::last_os_error().into());
    }

    let cmd = b">eframebuffer-spy$getConfigString:\n";
    let written = unsafe { libc::write(in_fd, cmd.as_ptr() as *const libc::c_void, cmd.len()) };
    unsafe {
        libc::close(in_fd);
    }
    if written != cmd.len() as isize {
        unsafe {
            libc::close(out_fd);
        }
        return Err("failed to send framebuffer-spy broker request".into());
    }

    let mut pfd = pollfd {
        fd: out_fd,
        events: POLLIN,
        revents: 0,
    };
    let poll_rc = unsafe { poll(&mut pfd as *mut pollfd, 1, 1000) };
    if poll_rc <= 0 {
        unsafe {
            libc::close(out_fd);
        }
        return Err("timed out waiting for framebuffer-spy broker response".into());
    }

    let mut out = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let read = unsafe { libc::read(out_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if read < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                break;
            }
            unsafe {
                libc::close(out_fd);
            }
            return Err(err.into());
        }
        if read == 0 {
            break;
        }
        out.extend_from_slice(&buf[..read as usize]);
    }
    unsafe {
        libc::close(out_fd);
    }

    if out.is_empty() {
        return Err("framebuffer-spy broker returned empty config".into());
    }
    Ok(String::from_utf8(out)?)
}


fn ensure_min_bytes(
    stream: &mut TcpStream,
    fd: i32,
    buf: &mut Vec<u8>,
    min: usize,
) -> DynResult<()> {
    while buf.len() < min {
        poll_until_readable(fd)?;
        if !read_available(stream, buf)? {
            buf.clear();
            return Ok(());
        }
    }
    Ok(())
}

/// Block until the socket is readable (no periodic wakeup; we do not use full-screen EPDC refresh).
fn poll_until_readable(fd: i32) -> DynResult<()> {
    let mut pfd = pollfd {
        fd,
        events: POLLIN as i16,
        revents: 0,
    };
    loop {
        let r = unsafe { poll(&mut pfd, 1, -1) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        if r > 0 {
            return Ok(());
        }
    }
}

fn read_available(stream: &mut TcpStream, buf: &mut Vec<u8>) -> DynResult<bool> {
    let mut tmp = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => return Ok(false),
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                return Ok(true);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

