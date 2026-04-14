//! Tablet-side receiver: TCP → LZ4 → framebuffer partial updates.
//!
//! Run as `rm-client-screen [HOST] [PORT] [SRC_W SRC_H [ORIENTATION]]` (defaults `127.0.0.1` `9876`).
//! SRC_W/SRC_H are the host capture size (e.g. 1920×1200); regions are letterboxed to fit
//! the device framebuffer. **ORIENTATION** must match rm-pad (`portrait`, `landscape-right`,
//! `landscape-left`, `inverted`). **`rm-screen` always passes it as the 6th argument** when
//! launching the client; `RM_PAD_ORIENTATION` is still read if that argument is omitted (manual runs).
//!
//! **Canonical wire** uses `min(var.xres,yres) × max(...)` so it matches the touch/framebuffer
//! portrait grid (`rm_common::device` / rm-screen letterbox) when `var_screen_info` is transposed.
//!
//! **Portrait buffer, host rotation:** The qtfb/rm2fb shim exposes a **portrait** framebuffer
//! (short×long in row-major). There is no kernel “orientation” for mirroring — `rm-screen` already
//! folds `--orientation` into each wire pixel when encoding (see `mirror_wire_pixel_to_view`). This
//! client **must not** rotate again: it blits patch rows as **wire** coordinates, only applying
//! [`protocol_point_to_mmap`] when `xres×yres` is transposed vs wire.
//!
//! **Observed on stock OS + qtfb-shim (e.g. 3.26 xovi debug):** Qt reports touch `max X: 1403`,
//! `max Y: 1871` on `/dev/input/event2`. When AppLoad starts this client, `[QTFB]` logs
//! `Resolution is set to 1404x1872`, SHM size `5256576` (= 1404×1872×2), and an image with
//! `bpl = 2808` (= `xres`×2 for 16bpp). That matches `libremarkable`’s `var_screen_info` /
//! `fix_screen_info.line_length` in the successful session (`EPD line_length=2808`).
//!
//! **TCP `Connection refused`:** the PC must be listening on the configured host:port *before*
//! launching Screen Mirror from AppLoad (start `rm-screen` and any SSH reverse tunnel first).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::time::Instant;

use libc::{poll, pollfd, POLLIN};
use libremarkable::framebuffer::common::{
    color, dither_mode, display_temp, mxcfb_rect, waveform_mode,
};
use libremarkable::framebuffer::core::Framebuffer;
use libremarkable::framebuffer::{FramebufferIO, FramebufferRefresh, PartialRefreshMode};
use log::{error, info, warn};
use rm_common::expand_rect_to_epdc_grid;
use rm_common::orientation::{
    mirror_view_pixel_to_wire, mirror_wire_pixel_to_view, Orientation,
};
use rm_common::protocol::{UpdateHeader, HEADER_SIZE, UPDATE_COORDS_FRAMEBUFFER};

type DynResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const IDLE_MS: i32 = 3000;

fn main() -> DynResult<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let (addr, source_dims, orientation_from_argv) = parse_args()?;

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
    let mmap_w = fb.var_screen_info.xres as u32;
    let mmap_h = fb.var_screen_info.yres as u32;
    // Same convention as rm-screen `DeviceProfile::framebuffer_size`: portrait short × long.
    let protocol_w = mmap_w.min(mmap_h);
    let protocol_h = mmap_w.max(mmap_h);
    if protocol_w != mmap_w || protocol_h != mmap_h {
        info!(
            "var_screen {}×{} — using canonical wire {}×{} for mirror math (transpose to mmap when blitting)",
            mmap_w, mmap_h, protocol_w, protocol_h
        );
    }

    let (src_w, src_h) = match source_dims {
        Some((w, h)) if w > 0 && h > 0 => (w, h),
        _ => {
            warn!(
                "no valid SRC_W SRC_H; assuming source equals protocol wire {}×{} (wrong unless resolutions match)",
                protocol_w, protocol_h
            );
            (protocol_w, protocol_h)
        }
    };

    let orientation_cli = orientation_from_argv;
    let orient_from_argv = orientation_cli.is_some();
    let orientation = orientation_cli.unwrap_or_else(|| {
        std::env::var("RM_PAD_ORIENTATION")
            .ok()
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse::<Orientation>().ok())
            .unwrap_or(Orientation::LandscapeRight)
    });
    let (fit_w, fit_h) = orientation.mirror_letterbox_fit_dimensions(protocol_w, protocol_h);
    let scale = (fit_w as f64 / src_w as f64).min(fit_h as f64 / src_h as f64);
    let dst_fit_w = ((src_w as f64 * scale).floor() as u32) & !1;
    let dst_fit_h = (src_h as f64 * scale).floor() as u32;
    let off_x = fit_w.saturating_sub(dst_fit_w) / 2;
    let off_y = fit_h.saturating_sub(dst_fit_h) / 2;

    info!(
        "var_screen {}×{}, protocol wire {}×{}, host capture {}×{} — orientation={} fit {}×{} scale {:.4} offset ({}, {}) fitted {}×{}",
        mmap_w,
        mmap_h,
        protocol_w,
        protocol_h,
        src_w,
        src_h,
        orientation,
        fit_w,
        fit_h,
        scale,
        off_x,
        off_y,
        dst_fit_w,
        dst_fit_h
    );
    info!(
        "EPD line_length={} bpp={} (RM2: buffer is usually /dev/shm/swtfb.01 + imx epdc; updates go via MXCFB_SEND_UPDATE / rm2fb)",
        fb.fix_screen_info.line_length,
        fb.var_screen_info.bits_per_pixel
    );

    let orient_source = if orient_from_argv {
        "argv (set by rm-screen)"
    } else if std::env::var("RM_PAD_ORIENTATION")
        .ok()
        .filter(|s| !s.is_empty())
        .is_some()
    {
        "RM_PAD_ORIENTATION"
    } else {
        "default (landscape-right); prefer argv from rm-screen"
    };
    info!(
        "mirror orientation: {orientation} (source: {orient_source}) — host encodes rotation into portrait wire; client blits wire→mmap transpose only",
    );

    // Native pen ink uses async-style updates; `Wait` adds ~1 EPDC frame time per mirror patch.
    // With host-side single-flight TCP, Async is usually best. Use RM_CLIENT_SCREEN_WAIT_REFRESH=1
    // if you see ghosting or runaway EPDC queues.
    let refresh_mode = match std::env::var("RM_CLIENT_SCREEN_WAIT_REFRESH").as_deref() {
        Ok("1") => {
            info!("RM_CLIENT_SCREEN_WAIT_REFRESH=1 — block until EPDC accepts update (slower, steadier)");
            PartialRefreshMode::Wait
        }
        _ => PartialRefreshMode::Async,
    };

    let waveform = waveform_from_env();
    let refresh_label = if matches!(refresh_mode, PartialRefreshMode::Wait) {
        "Wait"
    } else {
        "Async"
    };
    info!(
        "EPDC refresh={} waveform={:?} (RM_CLIENT_SCREEN_WAVEFORM=gc16_fast|gl16_fast|reagl|gc16|du)",
        refresh_label, waveform
    );

    run_stream(
        &mut fb,
        &mut stream,
        protocol_w,
        protocol_h,
        mmap_w,
        mmap_h,
        orientation,
        scale,
        off_x,
        off_y,
        refresh_mode,
        waveform,
    )?;

    Ok(())
}

fn parse_args() -> DynResult<(String, Option<(u32, u32)>, Option<Orientation>)> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.len() {
        0 => Ok(("127.0.0.1:9876".to_string(), None, None)),
        1 => {
            if argv[0].contains(':') {
                Ok((argv[0].clone(), None, None))
            } else {
                Err(
                    "usage: rm-client-screen HOST PORT [SRC_W SRC_H [ORIENTATION]]".into(),
                )
            }
        }
        2 => {
            let port: u16 = argv[1].parse().map_err(|_| "PORT must be a number")?;
            Ok((format!("{}:{}", argv[0], port), None, None))
        }
        4 => {
            let port: u16 = argv[1].parse().map_err(|_| "PORT must be a number")?;
            let sw: u32 = argv[2].parse().map_err(|_| "SRC_W must be u32")?;
            let sh: u32 = argv[3].parse().map_err(|_| "SRC_H must be u32")?;
            Ok((format!("{}:{}", argv[0], port), Some((sw, sh)), None))
        }
        5 => {
            let port: u16 = argv[1].parse().map_err(|_| "PORT must be a number")?;
            let sw: u32 = argv[2].parse().map_err(|_| "SRC_W must be u32")?;
            let sh: u32 = argv[3].parse().map_err(|_| "SRC_H must be u32")?;
            let orientation = argv[4]
                .parse::<Orientation>()
                .map_err(|e| format!("ORIENTATION: {e}"))?;
            Ok((
                format!("{}:{}", argv[0], port),
                Some((sw, sh)),
                Some(orientation),
            ))
        }
        _ => Err(
            "usage: rm-client-screen [HOST:PORT | HOST PORT [SRC_W SRC_H [ORIENTATION]]]".into(),
        ),
    }
}

fn run_stream(
    fb: &mut Framebuffer,
    stream: &mut TcpStream,
    protocol_w: u32,
    protocol_h: u32,
    mmap_w: u32,
    mmap_h: u32,
    orientation: Orientation,
    scale: f64,
    off_x: u32,
    off_y: u32,
    refresh_mode: PartialRefreshMode,
    waveform: waveform_mode,
) -> DynResult<()> {
    let fd = stream.as_raw_fd();
    let mut buf: Vec<u8> = Vec::new();
    let mut last_data = Instant::now();
    let mut updates_ok: u64 = 0;

    loop {
        ensure_min_bytes(stream, fd, &mut buf, HEADER_SIZE, &mut last_data, fb)?;
        if buf.is_empty() {
            break;
        }

        // Greedily read all available data so we can skip stale updates.
        drain_available(stream, &mut buf);

        let updates = parse_complete_updates(&mut buf);
        if updates.is_empty() {
            continue;
        }

        // One ACK per message: the host sends multiple partials per PipeWire frame (sparse
        // damage). TCP may deliver several complete packets in one read — we must apply each in
        // order; keeping only the last would drop tiles and stall the host.
        for (header, payload) in updates {
            let raw = match lz4_flex::block::decompress_size_prepended(&payload) {
                Ok(v) => v,
                Err(e) => {
                    warn!("LZ4 error: {e}");
                    send_ack(stream);
                    continue;
                }
            };

            let w = header.width as u32;
            let h = header.height as u32;
            let expected = (w / 2) * h;
            if raw.len() != expected as usize {
                warn!("payload mismatch: {} vs {} for {}×{}", raw.len(), expected, w, h);
                send_ack(stream);
                continue;
            }

            let rgb565 = expand_gray4_packed(&raw, w, h);
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
                    Some((rect, rgb565))
                }
            } else {
                map_region_rgb565_to_fb(
                    &rgb565,
                    w,
                    h,
                    header.x as u32,
                    header.y as u32,
                    scale,
                    off_x,
                    off_y,
                    protocol_w,
                    protocol_h,
                    orientation,
                )
            };
            if let Some((rect, patch)) = mapped {
                match write_patch_to_fb(
                    fb,
                    rect,
                    &patch,
                    protocol_w,
                    protocol_h,
                    mmap_w,
                    mmap_h,
                    orientation,
                ) {
                    Err(e) => error!("fb write {:?}: {e}", rect),
                    Ok(None) => {}
                    Ok(Some(al)) => {
                        let mode = match refresh_mode {
                            PartialRefreshMode::Async => PartialRefreshMode::Async,
                            PartialRefreshMode::Wait => PartialRefreshMode::Wait,
                            PartialRefreshMode::DryRun => PartialRefreshMode::DryRun,
                        };
                        fb.partial_refresh(
                            &al,
                            mode,
                            waveform,
                            display_temp::TEMP_USE_REMARKABLE_DRAW,
                            dither_mode::EPDC_FLAG_USE_DITHERING_PASSTHROUGH,
                            0,
                            false,
                        );

                        updates_ok += 1;
                        if updates_ok <= 3 {
                            info!(
                                "update #{updates_ok}: refresh {}×{}@({},{})",
                                al.width, al.height, al.left, al.top,
                            );
                        }
                    }
                }
            }

            send_ack(stream);
        }
    }

    info!("stream ended: {updates_ok} partial updates applied");
    Ok(())
}

fn send_ack(stream: &mut TcpStream) {
    // Socket is non-blocking; `write_all` returns WouldBlock on a full send buffer and would drop
    // the ACK, so the host blocks on `read_exact` and the tunnel piles up data.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.write_all(&[0x06]);
    let _ = stream.flush();
    let _ = stream.set_nonblocking(true);
}

/// Parse all complete (header + full payload) updates from the front of `buf`,
/// draining consumed bytes. Leaves any trailing incomplete data in `buf`.
fn parse_complete_updates(buf: &mut Vec<u8>) -> Vec<(UpdateHeader, Vec<u8>)> {
    let mut results = Vec::new();
    let mut pos = 0;
    loop {
        if pos + HEADER_SIZE > buf.len() {
            break;
        }
        let hdr_slice: [u8; HEADER_SIZE] = buf[pos..pos + HEADER_SIZE].try_into().unwrap();
        let Some(header) = UpdateHeader::from_bytes(&hdr_slice) else {
            break;
        };
        let total = HEADER_SIZE + header.payload_size as usize;
        if pos + total > buf.len() {
            break;
        }
        let payload = buf[pos + HEADER_SIZE..pos + total].to_vec();
        results.push((header, payload));
        pos += total;
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

/// Map protocol **portrait wire** coordinates (short×long on RM2) to `var_screen_info` row-major
/// addresses. Identity when `xres×yres` matches wire; when the kernel swaps dimensions, use `(py, px)`
/// so mmap indexing matches `line_length`.
#[inline]
fn protocol_point_to_mmap(
    px: u32,
    py: u32,
    protocol_w: u32,
    protocol_h: u32,
    mmap_w: u32,
    mmap_h: u32,
) -> (u32, u32) {
    if protocol_w == mmap_w && protocol_h == mmap_h {
        (px, py)
    } else if protocol_w == mmap_h && protocol_h == mmap_w {
        (py, px)
    } else {
        (px, py)
    }
}

fn waveform_from_env() -> waveform_mode {
    match std::env::var("RM_CLIENT_SCREEN_WAVEFORM").as_deref() {
        Ok(s) if s.eq_ignore_ascii_case("gc16") => waveform_mode::WAVEFORM_MODE_GC16,
        Ok(s) if s.eq_ignore_ascii_case("gc16_fast") => waveform_mode::WAVEFORM_MODE_GC16_FAST,
        Ok(s) if s.eq_ignore_ascii_case("gl16_fast") => waveform_mode::WAVEFORM_MODE_GL16_FAST,
        Ok(s) if s.eq_ignore_ascii_case("reagl") => waveform_mode::WAVEFORM_MODE_REAGL,
        Ok(s) if s.eq_ignore_ascii_case("du") => waveform_mode::WAVEFORM_MODE_DU,
        _ => waveform_mode::WAVEFORM_MODE_GL16_FAST,
    }
}

/// Bounding box in mmap space for a wire-rectangle update (transpose only; no orientation rotation).
fn wire_rect_to_mmap_aabb(
    rect: mxcfb_rect,
    protocol_w: u32,
    protocol_h: u32,
    mmap_w: u32,
    mmap_h: u32,
) -> mxcfb_rect {
    if protocol_w == mmap_w && protocol_h == mmap_h {
        return rect;
    }
    let l = rect.left;
    let t = rect.top;
    let r = rect.left.saturating_add(rect.width).saturating_sub(1);
    let b = rect.top.saturating_add(rect.height).saturating_sub(1);
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    for (x, y) in [(l, t), (r, t), (l, b), (r, b)] {
        let (mx, my) = protocol_point_to_mmap(x, y, protocol_w, protocol_h, mmap_w, mmap_h);
        min_x = min_x.min(mx);
        min_y = min_y.min(my);
        max_x = max_x.max(mx);
        max_y = max_y.max(my);
    }
    mxcfb_rect {
        left: min_x,
        top: min_y,
        width: max_x.saturating_sub(min_x).saturating_add(1),
        height: max_y.saturating_sub(min_y).saturating_add(1),
    }
}

/// Write a region patch to the framebuffer without triggering a refresh. Returns the 8×8-aligned
/// rectangle to pass to `partial_refresh`, or `None` if nothing was written.
fn write_patch_to_fb(
    fb: &mut Framebuffer,
    rect: mxcfb_rect,
    patch: &[u8],
    protocol_w: u32,
    protocol_h: u32,
    mmap_w: u32,
    mmap_h: u32,
    _orientation: Orientation,
) -> Result<Option<mxcfb_rect>, &'static str> {
    let bpp = 2usize;
    let expect = (rect.width as usize)
        .checked_mul(rect.height as usize)
        .and_then(|p| p.checked_mul(bpp))
        .ok_or("rect size overflow")?;
    if patch.len() != expect {
        return Err("patch length does not match rect");
    }

    let mmap_transpose = !(protocol_w == mmap_w && protocol_h == mmap_h);

    if !mmap_transpose {
        let al = expand_to_8px_grid(rect, mmap_w, mmap_h);
        if al.width < 2 || al.height < 1 {
            return Ok(None);
        }

        // Host pre-aligns to this grid; skip dump_region + merge (saves a full framebuffer read).
        if al.left == rect.left && al.top == rect.top && al.width == rect.width && al.height == rect.height
        {
            fb.restore_region(al, patch)?;
            return Ok(Some(al));
        }

        let mut canvas = fb.dump_region(al)?;
        let row_patch = rect.width as usize * bpp;
        let row_canvas = al.width as usize * bpp;
        let ox = (rect.left.saturating_sub(al.left)) as usize * bpp;
        let oy = rect.top.saturating_sub(al.top) as usize;
        for row in 0..rect.height as usize {
            let dst = (oy + row) * row_canvas + ox;
            let src = row * row_patch;
            canvas[dst..dst + row_patch].copy_from_slice(&patch[src..src + row_patch]);
        }

        fb.restore_region(al, &canvas)?;
        return Ok(Some(al));
    }

    let loose = wire_rect_to_mmap_aabb(rect, protocol_w, protocol_h, mmap_w, mmap_h);
    let al = expand_to_8px_grid(loose, mmap_w, mmap_h);
    if al.width < 2 || al.height < 1 {
        return Ok(None);
    }

    let mut canvas = fb.dump_region(al)?;
    let row_canvas = al.width as usize * bpp;
    let row_patch = rect.width as usize * bpp;

    for row in 0..rect.height as usize {
        for col in 0..rect.width as usize {
            let wire_x = rect.left + col as u32;
            let wire_y = rect.top + row as u32;
            let (mx, my) = protocol_point_to_mmap(wire_x, wire_y, protocol_w, protocol_h, mmap_w, mmap_h);
            if mx < al.left
                || my < al.top
                || mx >= al.left.saturating_add(al.width)
                || my >= al.top.saturating_add(al.height)
            {
                continue;
            }
            let ox = (mx.saturating_sub(al.left)) as usize * bpp;
            let oy = (my.saturating_sub(al.top)) as usize;
            let di = oy * row_canvas + ox;
            let si = row * row_patch + col * bpp;
            canvas[di..di + bpp].copy_from_slice(&patch[si..si + bpp]);
        }
    }

    fb.restore_region(al, &canvas)?;
    Ok(Some(al))
}

/// Map a source RGB565 patch (sw×sh at compositor coords sx,sy) into wire framebuffer space.
fn map_region_rgb565_to_fb(
    rgb565: &[u8],
    sw: u32,
    sh: u32,
    sx: u32,
    sy: u32,
    scale: f64,
    off_x: u32,
    off_y: u32,
    protocol_w: u32,
    protocol_h: u32,
    orientation: Orientation,
) -> Option<(mxcfb_rect, Vec<u8>)> {
    if sw == 0 || sh == 0 {
        return None;
    }

    let (fit_w, fit_h) = orientation.mirror_letterbox_fit_dimensions(protocol_w, protocol_h);
    let dx0 = off_x as f64 + sx as f64 * scale;
    let dy0 = off_y as f64 + sy as f64 * scale;
    let dx1 = dx0 + sw as f64 * scale;
    let dy1 = dy0 + sh as f64 * scale;

    let ixv0 = dx0.floor().max(0.0) as u32;
    let iyv0 = dy0.floor().max(0.0) as u32;
    let ixv1 = dx1.ceil().min(fit_w as f64) as u32;
    let iyv1 = dy1.ceil().min(fit_h as f64) as u32;
    if ixv0 >= ixv1 || iyv0 >= iyv1 {
        return None;
    }
    let x1m = ixv1.saturating_sub(1);
    let y1m = iyv1.saturating_sub(1);
    let corners = [
        (ixv0, iyv0),
        (x1m, iyv0),
        (ixv0, y1m),
        (x1m, y1m),
    ];
    let mut ix0 = u32::MAX;
    let mut iy0 = u32::MAX;
    let mut ix1 = 0u32;
    let mut iy1 = 0u32;
    for (vx, vy) in corners {
        let (wx, wy) = mirror_view_pixel_to_wire(vx, vy, orientation, protocol_w, protocol_h);
        ix0 = ix0.min(wx);
        iy0 = iy0.min(wy);
        ix1 = ix1.max(wx);
        iy1 = iy1.max(wy);
    }
    let out_w = ix1.saturating_sub(ix0).saturating_add(1);
    let out_h = iy1.saturating_sub(iy0).saturating_add(1);
    let out_w = out_w & !1;
    if out_w < 2 || out_h < 1 {
        return None;
    }

    let mut patch = vec![0u8; (out_w * out_h * 2) as usize];

    for iy in 0..out_h {
        let screen_y = iy0 + iy;
        for ix in 0..out_w {
            let screen_x = ix0 + ix;
            let (vx, vy) = mirror_wire_pixel_to_view(screen_x, screen_y, orientation, protocol_w, protocol_h);
            let u = (vx as f64 + 0.5 - off_x as f64) / scale - sx as f64;
            let v = (vy as f64 + 0.5 - off_y as f64) / scale - sy as f64;
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


fn ensure_min_bytes(
    stream: &mut TcpStream,
    fd: i32,
    buf: &mut Vec<u8>,
    min: usize,
    last_data: &mut Instant,
    fb: &mut Framebuffer,
) -> DynResult<()> {
    while buf.len() < min {
        match poll_fd(fd, IDLE_MS)? {
            PollOutcome::Timeout => {
                if last_data.elapsed().as_millis() as i32 >= IDLE_MS {
                    ghost_clear(fb);
                    *last_data = Instant::now();
                }
            }
            PollOutcome::Ready => {
                if !read_available(stream, buf)? {
                    buf.clear();
                    return Ok(());
                }
                *last_data = Instant::now();
            }
        }
    }
    Ok(())
}

enum PollOutcome {
    Ready,
    Timeout,
}

fn poll_fd(fd: i32, timeout_ms: i32) -> DynResult<PollOutcome> {
    let mut pfd = pollfd {
        fd,
        events: POLLIN as i16,
        revents: 0,
    };
    let r = unsafe { poll(&mut pfd, 1, timeout_ms) };
    if r < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if r == 0 {
        return Ok(PollOutcome::Timeout);
    }
    Ok(PollOutcome::Ready)
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

fn ghost_clear(fb: &mut Framebuffer) {
    info!("idle ≥3s — full GC16 refresh");
    let _ = fb.full_refresh(
        waveform_mode::WAVEFORM_MODE_GC16,
        display_temp::TEMP_USE_REMARKABLE_DRAW,
        dither_mode::EPDC_FLAG_USE_DITHERING_PASSTHROUGH,
        0,
        false,
    );
}

fn expand_gray4_packed(packed: &[u8], w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 2) as usize);
    let half_w = (w / 2) as usize;
    for y in 0..h as usize {
        let row = y * half_w;
        for x in 0..half_w {
            let b = packed[row + x];
            let n0 = (b >> 4) & 0x0f;
            let n1 = b & 0x0f;
            out.extend_from_slice(&nibble_gray_rgb565(n0));
            out.extend_from_slice(&nibble_gray_rgb565(n1));
        }
    }
    out
}

fn nibble_gray_rgb565(n: u8) -> [u8; 2] {
    let g = (n & 0x0f) * 17;
    color::RGB(g, g, g).as_native()
}
