//! Tablet-side receiver: TCP → LZ4 → framebuffer partial updates.

use std::io::Read;
use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::time::Instant;

use libc::{poll, pollfd, POLLIN};
use libremarkable::framebuffer::common::{
    color, dither_mode, display_temp, mxcfb_rect, waveform_mode,
};
use libremarkable::framebuffer::core::Framebuffer;
use libremarkable::framebuffer::{FramebufferIO, FramebufferRefresh, PartialRefreshMode};
use log::{error, info, warn};
use rm_common::protocol::{UpdateHeader, HEADER_SIZE};

type DynResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const IDLE_MS: i32 = 3000;

fn main() -> DynResult<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "0.0.0.0:9876".to_string());
    let listener = TcpListener::bind(&addr)?;
    info!("rm-client-screen listening on {}", addr);

    let (mut stream, peer) = listener.accept()?;
    info!("accepted {}", peer);
    drop(listener);

    stream.set_nonblocking(true)?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;

    let mut fb = Framebuffer::new();
    run_stream(&mut fb, &mut stream)?;

    Ok(())
}

fn run_stream(fb: &mut Framebuffer, stream: &mut std::net::TcpStream) -> DynResult<()> {
    let fd = stream.as_raw_fd();
    let mut buf: Vec<u8> = Vec::new();
    let mut last_data = Instant::now();

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
        let rect = mxcfb_rect {
            top: header.y as u32,
            left: header.x as u32,
            width: w,
            height: h,
        };

        if let Err(e) = fb.restore_region(rect, &rgb565) {
            error!("restore_region: {e}");
            continue;
        }

        let wf = waveform_from_wire(header.waveform);
        fb.partial_refresh(
            &rect,
            PartialRefreshMode::Async,
            wf,
            display_temp::TEMP_USE_REMARKABLE_DRAW,
            dither_mode::EPDC_FLAG_USE_DITHERING_PASSTHROUGH,
            0,
            false,
        );
    }

    Ok(())
}

fn ensure_min_bytes(
    stream: &mut std::net::TcpStream,
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

fn read_available(stream: &mut std::net::TcpStream, buf: &mut Vec<u8>) -> DynResult<bool> {
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

fn waveform_from_wire(v: u8) -> waveform_mode {
    match v {
        1 => waveform_mode::WAVEFORM_MODE_DU,
        2 => waveform_mode::WAVEFORM_MODE_GC16,
        _ => waveform_mode::WAVEFORM_MODE_AUTO,
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
