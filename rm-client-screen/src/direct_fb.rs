//! reMarkable **2** only: mmap `/dev/fb0` (`mxs-lcdif`) and write pixels (host wire is RGB565;
//! expanded when the device is 32bpp).
//!
//! **Stock caveat:** The panel is usually fed by the SWTCON path inside `xochitl` (rm2fb shared
//! mem + message queue). Raw `mxs-lcdif` may not be what is scanned out; **`rm-screen` stops
//! `xochitl` so the kernel fb is the only owner** — that only works if this mmap is actually
//! wired to the EPD on your OS build. On many retail/chromium images **no picture appears on the
//! glass** even when mmap writes succeed (stride/bpp correct): the EPD path simply does not consume
//! this buffer. High-FPS mirroring then needs either the in-stack mechanism (historically rm2fb
//! while `xochitl` runs), a compositor path (e.g. Qt qtfb), or whatever the vendor documents for
//! Vellum/alternate OSes—not bare `fbdev` alone.
//!
//! **Do not force 16bpp by default:** libremarkable's `Framebuffer::device()` keeps the kernel's
//! pixel format and only applies timing (`xres`/`yres`/margins). Forcing RGB565 via
//! `FBIOPUT_VSCREENINFO` can switch the controller to a mode nothing displays. Use
//! `RM_CLIENT_SCREEN_FB_FORCE_RGB565=1` only if you know you need it.
//!
//! Kernel **MXCFB** ioctls are often **`EINVAL`** on this node. `partial_refresh` stays a no-op
//! for protocol compatibility.

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::Path;

use libc::ioctl;
use memmap2::{MmapMut, MmapOptions};
use libremarkable::framebuffer::common::{
    dither_mode, display_temp, mxcfb_rect, waveform_mode, FBIOGET_FSCREENINFO, FBIOGET_VSCREENINFO,
    FBIOPUT_VSCREENINFO,
};
use libremarkable::framebuffer::screeninfo::{Bitfield, FixScreeninfo, VarScreeninfo};
use libremarkable::framebuffer::PartialRefreshMode;

type DynResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub const DEFAULT_FB_DEVICE: &str = "/dev/fb0";

/// `FBIOBLANK` / `FB_BLANK_UNBLANK` (linux/fb.h) — best-effort; ignored if unsupported.
fn request_fb_unblank(device: &File) {
    // _IOW('F', 0x71, u32) on Linux ABIs we care about (matches musl `linux/fb.h`).
    const FBIOBLANK: libc::c_ulong = 0x4611;
    let level: libc::c_uint = 0; // FB_BLANK_UNBLANK
    unsafe {
        if ioctl(device.as_raw_fd(), FBIOBLANK, &level) != 0 {
            log::debug!(
                "FBIOBLANK(FB_BLANK_UNBLANK): {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

pub struct DirectFramebuffer {
    #[allow(dead_code)]
    device: File,
    pub var_screen_info: VarScreeninfo,
    pub fix_screen_info: FixScreeninfo,
    /// Bytes per row in memory (`fix.line_length`, or `smem_len / yres` when the driver lies).
    stride_bytes: u32,
    mmap: MmapMut,
}

impl DirectFramebuffer {
    pub fn open(path: impl AsRef<Path>) -> DynResult<Self> {
        let device = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.as_ref())?;

        let no_modeset = matches!(
            std::env::var("RM_CLIENT_SCREEN_FB_NO_MODESET").as_deref(),
            Ok("1") | Ok("true") | Ok("TRUE")
        );
        let force_rgb565 = matches!(
            std::env::var("RM_CLIENT_SCREEN_FB_FORCE_RGB565").as_deref(),
            Ok("1") | Ok("true") | Ok("TRUE")
        );
        // Deprecated alias: previously "allow 32bpp" meant "don't force 16"; treat as inverse of force.
        if matches!(
            std::env::var("RM_CLIENT_SCREEN_FB_ALLOW_32BPP").as_deref(),
            Ok("1") | Ok("true") | Ok("TRUE")
        ) {
            log::warn!(
                "RM_CLIENT_SCREEN_FB_ALLOW_32BPP is deprecated (native bpp is now the default); remove it"
            );
        }

        let (var_screen_info, fix_screen_info) = if no_modeset {
            log::info!("direct fb: RM_CLIENT_SCREEN_FB_NO_MODESET=1 — using kernel var/fix as-is");
            (
                Self::get_var_screeninfo(&device)?,
                Self::get_fix_screeninfo(&device)?,
            )
        } else {
            let mut var_screen_info = Self::get_var_screeninfo(&device)?;

            var_screen_info.xres = 1404;
            var_screen_info.yres = 1872;
            var_screen_info.rotate = 1;
            var_screen_info.width = 0xffff_ffff;
            var_screen_info.height = 0xffff_ffff;
            var_screen_info.pixclock = 6250;
            var_screen_info.left_margin = 32;
            var_screen_info.right_margin = 326;
            var_screen_info.upper_margin = 4;
            var_screen_info.lower_margin = 12;
            var_screen_info.hsync_len = 44;
            var_screen_info.vsync_len = 1;
            var_screen_info.sync = 0;
            var_screen_info.vmode = 0;
            var_screen_info.accel_flags = 0;

            if force_rgb565 {
                var_screen_info.bits_per_pixel = 16;
                var_screen_info.grayscale = 0;
                var_screen_info.red = Bitfield {
                    offset: 11,
                    length: 5,
                    msb_right: 0,
                };
                var_screen_info.green = Bitfield {
                    offset: 5,
                    length: 6,
                    msb_right: 0,
                };
                var_screen_info.blue = Bitfield {
                    offset: 0,
                    length: 5,
                    msb_right: 0,
                };
                var_screen_info.transp = Bitfield {
                    offset: 0,
                    length: 0,
                    msb_right: 0,
                };
                var_screen_info.xres_virtual = var_screen_info.xres;
                var_screen_info.yres_virtual = var_screen_info.yres;
                var_screen_info.xoffset = 0;
                var_screen_info.yoffset = 0;
                log::info!("direct fb: RM_CLIENT_SCREEN_FB_FORCE_RGB565=1 (16 bpp)");
            } else {
                log::info!(
                    "direct fb: keeping kernel {} bpp (set RM_CLIENT_SCREEN_FB_FORCE_RGB565=1 for 16 bpp)",
                    var_screen_info.bits_per_pixel
                );
            }

            if !Self::put_var_screeninfo(&device, &mut var_screen_info) {
                return Err(format!(
                    "FBIOPUT_VSCREENINFO failed on {} — wrong device or kernel? Try RM_CLIENT_SCREEN_FB_NO_MODESET=1",
                    path.as_ref().display()
                )
                .into());
            }

            (
                Self::get_var_screeninfo(&device)?,
                Self::get_fix_screeninfo(&device)?,
            )
        };
        let fb_id = fix_id_str(&fix_screen_info);

        let bytespp = (var_screen_info.bits_per_pixel / 8).max(1) as u32;
        let min_stride = var_screen_info.xres.saturating_mul(bytespp);
        let mut stride_bytes = fix_screen_info.line_length;
        if stride_bytes < min_stride {
            let need = (min_stride as u64).saturating_mul(var_screen_info.yres as u64);
            if fix_screen_info.smem_len as u64 >= need {
                // mxs-lcdif often reports a bogus line_length while smem_len is a large carve-out
                // (not always divisible by yres). A linear top-left image still uses xres×bpp row stride.
                log::warn!(
                    "direct fb: FIX.line_length ({}) < xres×bpp ({}); using {} byte stride (smem_len={}, yres={}, smem_len % yres={})",
                    stride_bytes,
                    min_stride,
                    min_stride,
                    fix_screen_info.smem_len,
                    var_screen_info.yres,
                    fix_screen_info.smem_len % var_screen_info.yres,
                );
                stride_bytes = min_stride;
            } else if fix_screen_info.smem_len > 0 && var_screen_info.yres > 0 {
                let rem = fix_screen_info.smem_len % var_screen_info.yres;
                let from_smem = fix_screen_info.smem_len / var_screen_info.yres;
                if rem == 0 && from_smem >= min_stride {
                    log::warn!(
                        "direct fb: FIX.line_length ({}) < xres×bpp ({}); using smem_len/yres={}",
                        stride_bytes,
                        min_stride,
                        from_smem
                    );
                    stride_bytes = from_smem;
                } else {
                    return Err(format!(
                        "framebuffer stride incoherent: line_length={} need ≥{} for {}×{} @ {}bpp; smem_len={} (need {} bytes minimum)",
                        fix_screen_info.line_length,
                        min_stride,
                        var_screen_info.xres,
                        var_screen_info.yres,
                        var_screen_info.bits_per_pixel,
                        fix_screen_info.smem_len,
                        need
                    )
                    .into());
                }
            } else {
                return Err(format!(
                    "framebuffer line_length {} too small for xres {} @ {}bpp (need {} bytes/row)",
                    fix_screen_info.line_length,
                    var_screen_info.xres,
                    var_screen_info.bits_per_pixel,
                    min_stride
                )
                .into());
            }
        }

        let frame_length = fix_screen_info.smem_len as usize;
        let frame_length = if frame_length > 0 {
            frame_length
        } else {
            (stride_bytes as usize).saturating_mul(var_screen_info.yres as usize)
        };
        if frame_length == 0 {
            return Err("framebuffer smem length is zero".into());
        }
        let need = (stride_bytes as u64).saturating_mul(var_screen_info.yres as u64);
        if (frame_length as u64) < need {
            return Err(format!(
                "mmap {} bytes < stride×yres={}×{}={}",
                frame_length, stride_bytes, var_screen_info.yres, need
            )
            .into());
        }

        let mmap = unsafe { MmapOptions::new().len(frame_length).map_mut(&device)? };

        request_fb_unblank(&device);

        log::info!(
            "direct fb {}: reMarkable 2 {}×{} driver={} line_length={} stride_bytes={} bpp={} smem_len={} mmap {} bytes",
            path.as_ref().display(),
            var_screen_info.xres,
            var_screen_info.yres,
            fb_id,
            fix_screen_info.line_length,
            stride_bytes,
            var_screen_info.bits_per_pixel,
            fix_screen_info.smem_len,
            frame_length,
        );

        Ok(Self {
            device,
            var_screen_info,
            fix_screen_info,
            stride_bytes,
            mmap,
        })
    }

    pub fn msync_full(&self) -> std::io::Result<()> {
        let ptr = self.mmap.as_ptr();
        let len = self.mmap.len();
        let r = unsafe { libc::msync(ptr as *mut libc::c_void, len, libc::MS_SYNC) };
        if r != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINVAL) {
                return Ok(());
            }
            return Err(err);
        }
        Ok(())
    }

    #[inline]
    pub fn xres(&self) -> u32 {
        self.var_screen_info.xres
    }

    #[inline]
    pub fn yres(&self) -> u32 {
        self.var_screen_info.yres
    }

    #[inline]
    pub fn bytes_per_pixel(&self) -> usize {
        (self.var_screen_info.bits_per_pixel / 8) as usize
    }

    #[inline]
    pub fn stride_bytes(&self) -> u32 {
        self.stride_bytes
    }

    pub fn blit_rgb565_into_native_canvas(
        canvas: &mut [u8],
        al_width: u32,
        ox_px: u32,
        oy_px: u32,
        rect_w: u32,
        rect_h: u32,
        patch_rgb565: &[u8],
        bpp: usize,
    ) -> Result<(), &'static str> {
        let expect_patch = rect_w as usize * rect_h as usize * 2;
        if patch_rgb565.len() != expect_patch {
            return Err("RGB565 patch size mismatch in blit");
        }
        for row in 0..rect_h as usize {
            for col in 0..rect_w as usize {
                let si = (row * rect_w as usize + col) * 2;
                let px = u16::from_le_bytes([patch_rgb565[si], patch_rgb565[si + 1]]);
                let base =
                    ((oy_px as usize + row) * al_width as usize + (ox_px as usize + col)) * bpp;
                match bpp {
                    2 => {
                        canvas[base] = patch_rgb565[si];
                        canvas[base + 1] = patch_rgb565[si + 1];
                    }
                    4 => {
                        let b = rgb565_to_bgra(px);
                        canvas[base..base + 4].copy_from_slice(&b);
                    }
                    _ => return Err("blit: bpp must be 2 or 4"),
                }
            }
        }
        Ok(())
    }

    pub fn restore_region_rgb565(
        &mut self,
        rect: mxcfb_rect,
        patch_rgb565: &[u8],
    ) -> Result<u32, &'static str> {
        let w = rect.width as usize;
        let h = rect.height as usize;
        if patch_rgb565.len() != w.checked_mul(h).and_then(|n| n.checked_mul(2)).unwrap_or(0) {
            return Err("RGB565 patch size does not match rect");
        }
        match self.bytes_per_pixel() {
            2 => self.restore_region(rect, patch_rgb565),
            4 => {
                let native = rgb565_buffer_to_native32(patch_rgb565, w, h);
                self.restore_region(rect, &native)
            }
            _ => Err("mirror supports framebuffer bytes/px of 2 or 4 only"),
        }
    }

    pub fn dump_region(&self, rect: mxcfb_rect) -> Result<Vec<u8>, &'static str> {
        if rect.width == 0 || rect.height == 0 {
            return Err("Unable to dump a region with zero height/width");
        }
        if rect.top + rect.height > self.var_screen_info.yres {
            return Err("Vertically out of bounds");
        }
        if rect.left + rect.width > self.var_screen_info.xres {
            return Err("Horizontally out of bounds");
        }

        let stride = self.stride_bytes;
        let bytespp = (self.var_screen_info.bits_per_pixel / 8) as usize;
        let inbuffer = self.mmap.as_ptr();
        let mut outbuffer: Vec<u8> =
            Vec::with_capacity(rect.height as usize * rect.width as usize * bytespp);
        let outbuffer_ptr = outbuffer.as_mut_ptr();

        let mut written = 0;
        let chunk_size = bytespp * rect.width as usize;
        for row in 0..rect.height {
            let curr_index =
                (row + rect.top) * stride + (bytespp * rect.left as usize) as u32;
            unsafe {
                inbuffer
                    .add(curr_index as usize)
                    .copy_to_nonoverlapping(outbuffer_ptr.add(written), chunk_size);
            }
            written += chunk_size;
        }
        unsafe {
            outbuffer.set_len(written);
        }

        Ok(outbuffer)
    }

    pub fn restore_region(&mut self, rect: mxcfb_rect, data: &[u8]) -> Result<u32, &'static str> {
        if rect.width == 0 || rect.height == 0 {
            return Err("Unable to restore a region with zero height/width");
        }
        if rect.top + rect.height > self.var_screen_info.yres {
            return Err("Vertically out of bounds");
        }
        if rect.left + rect.width > self.var_screen_info.xres {
            return Err("Horizontally out of bounds");
        }

        let bytespp = (self.var_screen_info.bits_per_pixel / 8) as usize;
        if data.len() as u32 != rect.width * rect.height * bytespp as u32 {
            return Err("Cannot restore region due to mismatched size");
        }

        let stride = self.stride_bytes;
        let chunk_size = bytespp * rect.width as usize;
        let outbuffer = self.mmap.as_mut_ptr();
        let inbuffer = data.as_ptr();
        let mut written: u32 = 0;
        for y in 0..rect.height {
            let curr_index = (y + rect.top) * stride + (bytespp * rect.left as usize) as u32;
            unsafe {
                outbuffer
                    .add(curr_index as usize)
                    .copy_from(inbuffer.add(written as usize), chunk_size);
            }
            written += chunk_size as u32;
        }
        Ok(written)
    }

    /// No-op on RM2 (`mxs-lcdif`): MXCFB is not available on `/dev/fb0`. Signature kept so the host
    /// batching path can stay unchanged (waveform/dither args ignored).
    #[allow(unused_variables)]
    pub fn partial_refresh(
        &self,
        _region: &mxcfb_rect,
        _mode: &PartialRefreshMode,
        _waveform_mode: waveform_mode,
        _temperature: display_temp,
        _dither_mode: dither_mode,
        _quant_bit: i32,
        _force_full_refresh: bool,
    ) -> u32 {
        0
    }

    fn get_fix_screeninfo(device: &File) -> DynResult<FixScreeninfo> {
        let mut info: FixScreeninfo = Default::default();
        let result = unsafe {
            ioctl(
                device.as_raw_fd(),
                FBIOGET_FSCREENINFO as libc::c_ulong,
                &mut info,
            )
        };
        if result != 0 {
            return Err(format!("FBIOGET_FSCREENINFO failed: {}", std::io::Error::last_os_error()).into());
        }
        Ok(info)
    }

    fn get_var_screeninfo(device: &File) -> DynResult<VarScreeninfo> {
        let mut info: VarScreeninfo = Default::default();
        let result = unsafe {
            ioctl(
                device.as_raw_fd(),
                FBIOGET_VSCREENINFO as libc::c_ulong,
                &mut info,
            )
        };
        if result != 0 {
            return Err(format!("FBIOGET_VSCREENINFO failed: {}", std::io::Error::last_os_error()).into());
        }
        Ok(info)
    }

    fn put_var_screeninfo(device: &File, var_screen_info: &mut VarScreeninfo) -> bool {
        unsafe {
            ioctl(
                device.as_raw_fd(),
                FBIOPUT_VSCREENINFO as libc::c_ulong,
                var_screen_info,
            ) == 0
        }
    }
}

fn fix_id_str(fix: &FixScreeninfo) -> String {
    let nul = fix.id.iter().position(|&b| b == 0).unwrap_or(fix.id.len());
    String::from_utf8_lossy(&fix.id[..nul]).into_owned()
}

fn rgb565_to_bgra(px: u16) -> [u8; 4] {
    let r5 = ((px >> 11) & 0x1f) as u32;
    let g6 = ((px >> 5) & 0x3f) as u32;
    let b5 = (px & 0x1f) as u32;
    let r = ((r5 * 255) / 31) as u8;
    let g = ((g6 * 255) / 63) as u8;
    let b = ((b5 * 255) / 31) as u8;
    [b, g, r, 0xff]
}

fn rgb565_buffer_to_native32(patch_rgb565: &[u8], w: usize, h: usize) -> Vec<u8> {
    let count = w * h;
    let mut out = vec![0u8; count * 4];
    for i in 0..count {
        let px = u16::from_le_bytes([patch_rgb565[i * 2], patch_rgb565[i * 2 + 1]]);
        let pxb = rgb565_to_bgra(px);
        out[i * 4..i * 4 + 4].copy_from_slice(&pxb);
    }
    out
}
