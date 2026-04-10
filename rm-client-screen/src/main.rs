//! **reMarkable 2 only** — TCP → LZ4 RGB565 patches → mmap `/dev/fb0` (`mxs-lcdif`) for high-rate
//! mirroring without the Qt/qtfb path that throttles updates (~2 fps).
//!
//! Framebuffer modeset matches libremarkable by default (**keeps kernel bpp**, usually 32‑bit, and
//! expands RGB565 in software). See `direct_fb` env: `RM_CLIENT_SCREEN_FB_FORCE_RGB565`,
//! `RM_CLIENT_SCREEN_FB_NO_MODESET`.
//!
//! The stock RM2 framebuffer does not implement MXCFB ink ioctls; `partial_refresh` is a no-op,
//! but the same batching + env vars are kept so the wire protocol stays unchanged.
//!
//! Run as `rm-client-screen [HOST] [PORT] [SRC_W SRC_H]` (defaults `127.0.0.1` `9876`).
//! SRC_W/SRC_H are the host capture size (e.g. 1920×1200); regions are letterboxed to fit
//! the device framebuffer. `rm-screen` passes these automatically.
//!
//! End-to-end timing: `RM_MIRROR_LATENCY_LOG=1` on **both** PC and tablet, plus
//! `RUST_LOG=rm_mirror_latency=info` (or `trace`) to see where time goes.

mod direct_fb;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::time::Instant;
use libc::{poll, pollfd, POLLIN};
use direct_fb::{DirectFramebuffer, DEFAULT_FB_DEVICE};
use libremarkable::framebuffer::common::{
    dither_mode, display_temp, mxcfb_rect, waveform_mode, DRAWING_QUANT_BIT,
};
use libremarkable::framebuffer::PartialRefreshMode;
use log::{error, info, warn};
use rm_common::expand_rect_to_epdc_grid;
use rm_common::protocol::{unix_time_millis, BatchAck, UpdateHeader, HEADER_SIZE, UPDATE_COORDS_FRAMEBUFFER};

type DynResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct ParsedBatch {
    batch_id: u32,
    updates: Vec<(UpdateHeader, Vec<u8>)>,
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

fn require_remarkable_2() -> DynResult<()> {
    const MODEL_PATH: &str = "/proc/device-tree/model";
    let raw = std::fs::read(MODEL_PATH)
        .map_err(|e| format!("read {MODEL_PATH}: {e} — is this a reMarkable tablet?"))?;
    let model = String::from_utf8_lossy(&raw)
        .trim_end_matches('\0')
        .trim()
        .to_string();
    if !model.contains("reMarkable 2.0") {
        return Err(format!(
            "rm-client-screen supports reMarkable 2 only (device-tree model: {model:?})"
        )
        .into());
    }
    info!(
        "device checks out as reMarkable 2 ({})",
        model
    );
    Ok(())
}

fn main() -> DynResult<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    require_remarkable_2()?;

    let (addr, source_dims) = parse_args()?;

    let fb_path = std::env::var("RM_CLIENT_SCREEN_FB_DEVICE")
        .unwrap_or_else(|_| DEFAULT_FB_DEVICE.to_string());
    info!("opening {}", fb_path);
    let mut fb = DirectFramebuffer::open(fb_path.as_str())?;
    let fb_w = fb.xres();
    let fb_h = fb.yres();

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
        "framebuffer stride_bytes={} (fix.line_length={}) bpp={}",
        fb.stride_bytes(),
        fb.fix_screen_info.line_length,
        fb.var_screen_info.bits_per_pixel,
    );

    // On RM2, `partial_refresh` does not call the kernel (no EPDC on this fb node). We still parse
    // these for protocol/env parity with the host.
    let refresh_mode = match std::env::var("RM_CLIENT_SCREEN_WAIT_REFRESH").as_deref() {
        Ok("1") => {
            info!("RM_CLIENT_SCREEN_WAIT_REFRESH=1 — kept for env parity; no hardware wait on RM2 fb");
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
        "refresh mode={} waveform={:?} dither={:?} (ignored for RM2 fbdraw; WAVEFORM/DITHER kept for tooling)",
        refresh_label, waveform, dither
    );

    // Default on for Async: ACK after FB writes + msync/no-op refresh pass, so the host is not blocked.
    let early_ack = matches!(refresh_mode, PartialRefreshMode::Async)
        && !matches!(std::env::var("RM_CLIENT_SCREEN_EARLY_ACK").as_deref(), Ok("0"));
    if early_ack {
        info!(
            "early ACK after batch FB writes (set RM_CLIENT_SCREEN_EARLY_ACK=0 to ACK after msync/pass)"
        );
    } else if matches!(refresh_mode, PartialRefreshMode::Async) {
        info!("RM_CLIENT_SCREEN_EARLY_ACK=0 — ACK after msync/pass");
    }

    let latency_log = mirror_latency_log_enabled();
    if latency_log {
        info!("RM_MIRROR_LATENCY_LOG=1 — also set RUST_LOG=rm_mirror_latency=info (tablet) and run rm-screen with --latency-log (PC)");
    }

    run_stream(
        &mut fb,
        &mut stream,
        fb_w,
        fb_h,
        scale,
        off_x,
        off_y,
        &refresh_mode,
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
    fb: &mut DirectFramebuffer,
    stream: &mut TcpStream,
    fb_w: u32,
    fb_h: u32,
    scale: f64,
    off_x: u32,
    off_y: u32,
    refresh_mode: &PartialRefreshMode,
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
                match write_patch_to_fb(fb, rect, &patch, fb_w, fb_h) {
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
        let t_msync_pass_start = Instant::now();
        if !refresh_rects.is_empty() {
            if let Err(e) = fb.msync_full() {
                warn!("framebuffer msync: {e}");
            }
        }
        for rect in &refresh_rects {
            fb.partial_refresh(
                rect,
                refresh_mode,
                waveform,
                display_temp::TEMP_USE_REMARKABLE_DRAW,
                dither,
                DRAWING_QUANT_BIT,
                false,
            );
        }
        let t_msync_pass_end = Instant::now();
        if !early_ack {
            send_batch_ack(stream, batch.batch_id);
        }
        if latency_log {
            let client_wall_ms = unix_time_millis();
            let wall_skew_ms = client_wall_ms as i128 - first_host_unix_ms as i128;
            let batch_total_ms = ms_between(t_batch, t_msync_pass_end);
            info!(
                target: "rm_mirror_latency",
                "tablet batch id={} parts_ok={} refreshes={} lz4_ms={:.2} fb_write_ms={:.2} msync_pass_ms={:.2} batch_total_ms={:.2} \
                 wall_now−host_stamp={}ms (±clock skew; large with tiny local stages → delay before host stamped or network) host_stamp_ms={} early_ack={}",
                batch.batch_id,
                batch_parts_ok,
                refresh_rects.len(),
                batch_lz4_ms,
                batch_fb_write_ms,
                ms_between(t_msync_pass_start, t_msync_pass_end),
                batch_total_ms,
                wall_skew_ms,
                first_host_unix_ms,
                early_ack,
            );
        }
    }

    info!("stream ended: {updates_ok} patches applied");
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
/// `patch` is always RGB565 (`rect.width * rect.height * 2`); device may be 16 or 32 bpp.
fn write_patch_to_fb(
    fb: &mut DirectFramebuffer,
    rect: mxcfb_rect,
    patch: &[u8],
    fb_w: u32,
    fb_h: u32,
) -> Result<(), &'static str> {
    const PROTO_BPP: usize = 2;
    let expect = (rect.width as usize)
        .checked_mul(rect.height as usize)
        .and_then(|p| p.checked_mul(PROTO_BPP))
        .ok_or("rect size overflow")?;
    if patch.len() != expect {
        return Err("patch length does not match rect");
    }

    let al = expand_to_8px_grid(rect, fb_w, fb_h);
    if al.width < 2 || al.height < 1 {
        return Ok(());
    }

    let dev_bpp = fb.bytes_per_pixel();
    if dev_bpp != 2 && dev_bpp != 4 {
        return Err("framebuffer must be 16 or 32 bpp for mirror");
    }

    if al.left == rect.left && al.top == rect.top && al.width == rect.width && al.height == rect.height
    {
        fb.restore_region_rgb565(al, patch)?;
        return Ok(());
    }

    let mut canvas = fb.dump_region(al)?;
    let ox_px = rect.left.saturating_sub(al.left);
    let oy_px = rect.top.saturating_sub(al.top);
    DirectFramebuffer::blit_rgb565_into_native_canvas(
        &mut canvas,
        al.width,
        ox_px,
        oy_px,
        rect.width,
        rect.height,
        patch,
        dev_bpp,
    )?;
    let _ = fb.restore_region(al, &canvas)?;
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

/// Align dirty rects to the same 8×8 grid as `rm-screen` / `rm_common::expand_rect_to_epdc_grid`.
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

/// Block until the socket is readable (no periodic wakeup).
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

