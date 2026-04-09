//! Tablet-side receiver: TCP → LZ4 → framebuffer partial updates.
//!
//! Run as `rm-client-screen [HOST] [PORT] [SRC_W SRC_H]` (defaults `127.0.0.1` `9876`).
//! SRC_W/SRC_H are the host capture size (e.g. 1920×1200); regions are letterboxed to fit
//! the device framebuffer. `rm-screen` passes these automatically.

use std::io::Read;
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::time::Instant;

use libc::{poll, pollfd, POLLIN};
use libremarkable::framebuffer::common::{
    color, dither_mode, display_temp, mxcfb_rect, waveform_mode,
};
use libremarkable::framebuffer::core::Framebuffer;
use libremarkable::framebuffer::{FramebufferIO, FramebufferRefresh, PartialRefreshMode};
use log::{debug, error, info, warn};
use rm_common::protocol::{UpdateHeader, HEADER_SIZE};

type DynResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const IDLE_MS: i32 = 3000;

fn main() -> DynResult<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let (addr, source_dims) = parse_args()?;

    let mut stream = TcpStream::connect(&addr)?;
    info!("rm-client-screen connected to {}", addr);

    stream.set_nonblocking(true)?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;

    let mut fb = Framebuffer::new();
    let fb_w = fb.var_screen_info.xres;
    let fb_h = fb.var_screen_info.yres;

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

    let refresh_mode = match std::env::var("RM_CLIENT_SCREEN_WAIT_REFRESH").as_deref() {
        Ok("1") => {
            info!("RM_CLIENT_SCREEN_WAIT_REFRESH=1 — blocking after each EPD partial update (slower, easier to debug)");
            PartialRefreshMode::Wait
        }
        _ => PartialRefreshMode::Async,
    };

    run_stream(
        &mut fb,
        &mut stream,
        fb_w,
        fb_h,
        scale,
        off_x,
        off_y,
        refresh_mode,
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
    fb: &mut Framebuffer,
    stream: &mut TcpStream,
    fb_w: u32,
    fb_h: u32,
    scale: f64,
    off_x: u32,
    off_y: u32,
    refresh_mode: PartialRefreshMode,
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

        let hdr_slice: [u8; HEADER_SIZE] = buf[..HEADER_SIZE].try_into().unwrap();
        let Some(header) = UpdateHeader::from_bytes(&hdr_slice) else {
            return Err("invalid header".into());
        };

        let total = HEADER_SIZE + header.payload_size as usize;
        ensure_min_bytes(stream, fd, &mut buf, total, &mut last_data, fb)?;

        let payload = buf[HEADER_SIZE..total].to_vec();
        buf.drain(..total);

        let raw = match lz4_flex::block::decompress_size_prepended(&payload) {
            Ok(v) => v,
            Err(e) => {
                warn!("LZ4 error: {e}");
                continue;
            }
        };

        let w = header.width as u32;
        let h = header.height as u32;
        let expected_packed = (w / 2) * h;
        if raw.len() != expected_packed as usize {
            warn!(
                "payload size mismatch: got {} expected {} for {}x{}",
                raw.len(),
                expected_packed,
                w,
                h
            );
            continue;
        }

        let rgb565 = expand_gray4_packed(&raw, w, h);
        let sx = header.x as u32;
        let sy = header.y as u32;

        let Some((rect, patch)) = map_region_rgb565_to_fb(
            &rgb565, w, h, sx, sy, scale, off_x, off_y, fb_w, fb_h,
        ) else {
            debug!(
                "skip region source {}×{}@({},{}): maps outside visible framebuffer",
                w, h, sx, sy
            );
            continue;
        };

        if let Err(e) = restore_merge_refresh_8(fb, rect, &patch, &refresh_mode) {
            error!("EPD update {:?}: {e}", rect);
            continue;
        }

        updates_ok += 1;
        if updates_ok <= 3 {
            let al = expand_to_8px_grid(rect, fb_w, fb_h);
            info!(
                "first updates: #{} patch ({},{}) {}×{} ← host {}×{}@({sx},{sy}); 8px-aligned refresh {}×{}@({},{})",
                updates_ok,
                rect.left,
                rect.top,
                rect.width,
                rect.height,
                w,
                h,
                al.width,
                al.height,
                al.left,
                al.top,
            );
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

/// EPDC partial updates should use 8×8 boundaries (see libremarkable `partial_refresh` docs).
fn expand_to_8px_grid(rect: mxcfb_rect, fb_w: u32, fb_h: u32) -> mxcfb_rect {
    let left = (rect.left / 8) * 8;
    let top = (rect.top / 8) * 8;
    let right = ((rect.left + rect.width + 7) / 8) * 8;
    let bottom = ((rect.top + rect.height + 7) / 8) * 8;
    let right = right.min(fb_w);
    let bottom = bottom.min(fb_h);
    let width = right.saturating_sub(left);
    let height = bottom.saturating_sub(top);
    mxcfb_rect {
        left,
        top,
        width,
        height,
    }
}

/// Copy `patch` (size `rect`×RGB565) into a dumped 8×8-aligned region, then refresh.
fn restore_merge_refresh_8(
    fb: &mut Framebuffer,
    rect: mxcfb_rect,
    patch: &[u8],
    refresh_mode: &PartialRefreshMode,
) -> Result<(), &'static str> {
    let bpp = 2usize;
    let expect = (rect.width as usize)
        .checked_mul(rect.height as usize)
        .and_then(|p| p.checked_mul(bpp))
        .ok_or("rect size overflow")?;
    if patch.len() != expect {
        return Err("patch length does not match rect");
    }

    let fb_w = fb.var_screen_info.xres;
    let fb_h = fb.var_screen_info.yres;
    let al = expand_to_8px_grid(rect, fb_w, fb_h);
    if al.width < 1 || al.height < 1 {
        return Ok(());
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

    // Grayscale mirroring: prefer GC16_FAST. DU is for 1-bit style content and can no-op or
    // behave badly when pixels are not saturated black/white on stock imx epdc.
    let mode = match refresh_mode {
        PartialRefreshMode::Async => PartialRefreshMode::Async,
        PartialRefreshMode::Wait => PartialRefreshMode::Wait,
        PartialRefreshMode::DryRun => PartialRefreshMode::DryRun,
    };
    fb.partial_refresh(
        &al,
        mode,
        waveform_mode::WAVEFORM_MODE_GC16_FAST,
        display_temp::TEMP_USE_REMARKABLE_DRAW,
        dither_mode::EPDC_FLAG_USE_DITHERING_PASSTHROUGH,
        0,
        false,
    );
    Ok(())
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
