//! Screen orientation handling for input coordinate transforms.

use serde::Deserialize;
use std::fmt;
use std::str::FromStr;

/// Screen orientation relative to the default portrait mode (buttons at top).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Orientation {
    /// Portrait mode (buttons at top, no rotation).
    Portrait,
    /// Landscape with buttons on the right side (90° clockwise).
    #[default]
    LandscapeRight,
    /// Landscape with buttons on the left side (90° counter-clockwise).
    LandscapeLeft,
    /// Inverted portrait (buttons at bottom, 180° rotation).
    Inverted,
}

impl Orientation {
    /// Transform touch coordinates from device space to output space.
    /// Touch is natively portrait-oriented but with Y=0 at bottom.
    pub fn transform_touch(&self, x: i32, y: i32, x_max: i32, y_max: i32) -> (i32, i32) {
        match self {
            // Portrait: flip Y only (device has Y=0 at bottom)
            Orientation::Portrait => (x, y_max - y),
            // LandscapeRight: swap X/Y (original working behavior)
            Orientation::LandscapeRight => (y, x),
            // LandscapeLeft: swap X/Y and invert both
            Orientation::LandscapeLeft => (y_max - y, x_max - x),
            // Inverted: flip Y and invert X
            Orientation::Inverted => (x_max - x, y),
        }
    }

    /// Transform pen coordinates from device space to output space.
    /// Pen is natively landscape-oriented (LandscapeRight = identity).
    pub fn transform_pen(&self, x: i32, y: i32, x_max: i32, y_max: i32) -> (i32, i32) {
        match self {
            // LandscapeRight: native pen orientation, no transform
            Orientation::LandscapeRight => (x, y),
            // Portrait: swap X/Y and flip what becomes Y
            Orientation::Portrait => (y, x_max - x),
            // LandscapeLeft: invert both axes
            Orientation::LandscapeLeft => (x_max - x, y_max - y),
            // Inverted: swap X/Y and flip what becomes X
            Orientation::Inverted => (y_max - y, x),
        }
    }

    /// Transform tilt values to match the orientation.
    /// Tilt follows pen orientation (LandscapeRight is native).
    pub fn transform_tilt(&self, tilt_x: i32, tilt_y: i32) -> (i32, i32) {
        match self {
            // LandscapeRight: native, no transform
            Orientation::LandscapeRight => (tilt_x, tilt_y),
            // Portrait: swap tilt axes, negate new Y
            Orientation::Portrait => (tilt_y, -tilt_x),
            // LandscapeLeft: invert both
            Orientation::LandscapeLeft => (-tilt_x, -tilt_y),
            // Inverted: swap and negate new X
            Orientation::Inverted => (-tilt_y, tilt_x),
        }
    }

    /// Get output dimensions for touch after rotation.
    /// Touch is natively portrait-oriented.
    pub fn touch_output_dimensions(&self, x_max: i32, y_max: i32) -> (i32, i32) {
        match self {
            Orientation::Portrait | Orientation::Inverted => (x_max, y_max),
            Orientation::LandscapeRight | Orientation::LandscapeLeft => (y_max, x_max),
        }
    }

    /// Get output dimensions for pen after rotation.
    /// Pen is natively landscape-oriented (x > y in raw coords).
    pub fn pen_output_dimensions(&self, x_max: i32, y_max: i32) -> (i32, i32) {
        match self {
            // LandscapeRight/Left: native pen orientation, keep dimensions
            Orientation::LandscapeRight | Orientation::LandscapeLeft => (x_max, y_max),
            // Portrait/Inverted: swap dimensions
            Orientation::Portrait | Orientation::Inverted => (y_max, x_max),
        }
    }

    /// Clockwise quarter turns around the framebuffer center for screen mirroring (before any
    /// [`Self::mirror_landscape_left_flip_y`]).
    ///
    /// **Both** landscape modes use **one** quarter turn from portrait wire layout so the panel
    /// long edge matches the desktop horizontal axis. They differ only by a vertical mirror; see
    /// [`Self::mirror_landscape_left_flip_y`].
    pub const fn mirror_quarter_turns_cw(self) -> u8 {
        match self {
            Orientation::Portrait => 0,
            Orientation::LandscapeRight | Orientation::LandscapeLeft => 1,
            Orientation::Inverted => 2,
        }
    }

    /// After rotation, mirror framebuffer **Y** so landscape-left matches rm-pad (buttons-left hold)
    /// vs landscape-right.
    pub const fn mirror_landscape_left_flip_y(self) -> bool {
        matches!(self, Orientation::LandscapeLeft)
    }

    /// Width × height used to **contain** the host capture when letterboxing.
    ///
    /// Landscape holds use the panel’s long side as the logical horizontal axis, so the fit
    /// rectangle is transposed relative to mmap order (`fb_w` × `fb_h`).
    pub const fn mirror_letterbox_fit_dimensions(self, fb_w: u32, fb_h: u32) -> (u32, u32) {
        match self {
            Orientation::Portrait | Orientation::Inverted => (fb_w, fb_h),
            Orientation::LandscapeRight | Orientation::LandscapeLeft => (fb_h, fb_w),
        }
    }
}

#[inline]
fn rotate_vec_k_times(mut px: f64, mut py: f64, k: u8) -> (f64, f64) {
    for _ in 0..(k % 4) {
        let nx = -py;
        let ny = px;
        px = nx;
        py = ny;
    }
    (px, py)
}

/// Wire / mmap pixel → physical draw pixel (same transform the tablet uses when placing patches).
pub fn mirror_wire_to_physical(
    wire_x: u32,
    wire_y: u32,
    orientation: Orientation,
    fb_w: u32,
    fb_h: u32,
) -> (u32, u32) {
    let k = orientation.mirror_quarter_turns_cw() % 4;
    if k == 0 && !orientation.mirror_landscape_left_flip_y() {
        return (
            wire_x.min(fb_w.saturating_sub(1)),
            wire_y.min(fb_h.saturating_sub(1)),
        );
    }
    let c_wx = fb_w as f64 / 2.0;
    let c_wy = fb_h as f64 / 2.0;
    let mut px = wire_x as f64 + 0.5 - c_wx;
    let mut py = wire_y as f64 + 0.5 - c_wy;
    let (rx, ry) = rotate_vec_k_times(px, py, k);
    px = rx;
    py = ry;
    let max_x = fb_w.saturating_sub(1) as f64;
    let max_y = fb_h.saturating_sub(1) as f64;
    let ox = (px + c_wx - 0.5).round().clamp(0.0, max_x) as u32;
    let mut oy = (py + c_wy - 0.5).round().clamp(0.0, max_y) as u32;
    if orientation.mirror_landscape_left_flip_y() {
        oy = fb_h.saturating_sub(1).saturating_sub(oy);
    }
    (ox, oy)
}

/// Wire / protocol pixel (mmap order) → letterbox **view** pixel (`fit_w` × `fit_h`).
pub fn mirror_wire_pixel_to_view(
    wire_x: u32,
    wire_y: u32,
    orientation: Orientation,
    wire_w: u32,
    wire_h: u32,
) -> (u32, u32) {
    let k = orientation.mirror_quarter_turns_cw() % 4;
    let (fit_w, fit_h) = orientation.mirror_letterbox_fit_dimensions(wire_w, wire_h);
    if k == 0 && !orientation.mirror_landscape_left_flip_y() {
        return (
            wire_x.min(fit_w.saturating_sub(1)),
            wire_y.min(fit_h.saturating_sub(1)),
        );
    }
    let c_vx = fit_w as f64 / 2.0;
    let c_vy = fit_h as f64 / 2.0;
    let c_wx = wire_w as f64 / 2.0;
    let c_wy = wire_h as f64 / 2.0;
    let mut px = wire_x as f64 + 0.5 - c_wx;
    let mut py = wire_y as f64 + 0.5 - c_wy;
    let (rx, ry) = rotate_vec_k_times(px, py, k);
    px = rx;
    py = ry;
    let max_x = fit_w.saturating_sub(1) as f64;
    let max_y = fit_h.saturating_sub(1) as f64;
    let vx = (px + c_vx - 0.5).round().clamp(0.0, max_x) as u32;
    let mut vy = (py + c_vy - 0.5).round().clamp(0.0, max_y) as u32;
    if orientation.mirror_landscape_left_flip_y() {
        vy = fit_h.saturating_sub(1).saturating_sub(vy);
    }
    (vx, vy)
}

/// View / letterbox pixel → wire / mmap pixel (inverse of [`mirror_wire_pixel_to_view`]).
pub fn mirror_view_pixel_to_wire(
    view_x: u32,
    view_y: u32,
    orientation: Orientation,
    wire_w: u32,
    wire_h: u32,
) -> (u32, u32) {
    let k = orientation.mirror_quarter_turns_cw() % 4;
    let (fit_w, fit_h) = orientation.mirror_letterbox_fit_dimensions(wire_w, wire_h);
    let mut view_y = view_y;
    if orientation.mirror_landscape_left_flip_y() {
        view_y = fit_h.saturating_sub(1).saturating_sub(view_y);
    }
    if k == 0 {
        return (
            view_x.min(wire_w.saturating_sub(1)),
            view_y.min(wire_h.saturating_sub(1)),
        );
    }
    let c_vx = fit_w as f64 / 2.0;
    let c_vy = fit_h as f64 / 2.0;
    let c_wx = wire_w as f64 / 2.0;
    let c_wy = wire_h as f64 / 2.0;
    let mut px = view_x as f64 + 0.5 - c_vx;
    let mut py = view_y as f64 + 0.5 - c_vy;
    // wire_vec = F^{-k} view_vec  →  apply (4−k) forward quarter-turns (same as F^k inverse in Z_4).
    let steps = (4 - k) % 4;
    let (rx, ry) = rotate_vec_k_times(px, py, steps);
    px = rx;
    py = ry;
    let max_x = wire_w.saturating_sub(1) as f64;
    let max_y = wire_h.saturating_sub(1) as f64;
    let wx = (px + c_wx - 0.5).round().clamp(0.0, max_x) as u32;
    let wy = (py + c_wy - 0.5).round().clamp(0.0, max_y) as u32;
    (wx, wy)
}

impl fmt::Display for Orientation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Orientation::Portrait => write!(f, "portrait"),
            Orientation::LandscapeRight => write!(f, "landscape-right"),
            Orientation::LandscapeLeft => write!(f, "landscape-left"),
            Orientation::Inverted => write!(f, "inverted"),
        }
    }
}

impl FromStr for Orientation {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "portrait" => Ok(Orientation::Portrait),
            "landscape-right" | "landscaperight" | "landscape_right" => Ok(Orientation::LandscapeRight),
            "landscape-left" | "landscapeleft" | "landscape_left" => Ok(Orientation::LandscapeLeft),
            "inverted" => Ok(Orientation::Inverted),
            _ => Err(format!(
                "Invalid orientation '{}'. Valid values: portrait, landscape-right, landscape-left, inverted",
                s
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_landscape_right_transform() {
        let o = Orientation::LandscapeRight;
        // LandscapeRight just swaps X and Y
        assert_eq!(o.transform_touch(0, 0, 100, 200), (0, 0));
        assert_eq!(o.transform_touch(50, 100, 100, 200), (100, 50));
        assert_eq!(o.transform_touch(100, 200, 100, 200), (200, 100));
    }

    #[test]
    fn test_output_dimensions() {
        let portrait = Orientation::Portrait;
        let landscape = Orientation::LandscapeRight;
        
        assert_eq!(portrait.touch_output_dimensions(100, 200), (100, 200));
        assert_eq!(landscape.touch_output_dimensions(100, 200), (200, 100));
    }

    #[test]
    fn test_from_str() {
        assert_eq!("portrait".parse::<Orientation>().unwrap(), Orientation::Portrait);
        assert_eq!("landscape-right".parse::<Orientation>().unwrap(), Orientation::LandscapeRight);
        assert_eq!("landscape_left".parse::<Orientation>().unwrap(), Orientation::LandscapeLeft);
        assert!("invalid".parse::<Orientation>().is_err());
    }

    #[test]
    fn mirror_quarter_turns_for_portrait_framebuffer_wire() {
        assert_eq!(Orientation::Portrait.mirror_quarter_turns_cw(), 0);
        assert_eq!(Orientation::LandscapeRight.mirror_quarter_turns_cw(), 1);
        assert_eq!(Orientation::LandscapeLeft.mirror_quarter_turns_cw(), 1);
        assert!(!Orientation::LandscapeRight.mirror_landscape_left_flip_y());
        assert!(Orientation::LandscapeLeft.mirror_landscape_left_flip_y());
        assert_eq!(Orientation::Inverted.mirror_quarter_turns_cw(), 2);
    }

    #[test]
    fn mirror_view_wire_roundtrip_landscape_right() {
        let o = Orientation::LandscapeRight;
        let (fw, fh) = (1404u32, 1872u32);
        let (fit_w, fit_h) = o.mirror_letterbox_fit_dimensions(fw, fh);
        assert_eq!((fit_w, fit_h), (1872, 1404));
        for wx in [0u32, 100, 703, 1403] {
            for wy in [0u32, 200, 936, 1871] {
                let (vx, vy) = mirror_wire_pixel_to_view(wx, wy, o, fw, fh);
                let (wx2, wy2) = mirror_view_pixel_to_wire(vx, vy, o, fw, fh);
                assert!(
                    wx2.abs_diff(wx) <= 1 && wy2.abs_diff(wy) <= 1,
                    "wire ({wx},{wy}) -> view ({vx},{vy}) -> wire ({wx2},{wy2})"
                );
            }
        }
    }

    #[test]
    fn mirror_view_wire_roundtrip_landscape_left() {
        let o = Orientation::LandscapeLeft;
        let (fw, fh) = (1404u32, 1872u32);
        let (fit_w, fit_h) = o.mirror_letterbox_fit_dimensions(fw, fh);
        assert_eq!((fit_w, fit_h), (1872, 1404));
        for wx in [0u32, 100, 703, 1403] {
            for wy in [0u32, 200, 936, 1871] {
                let (vx, vy) = mirror_wire_pixel_to_view(wx, wy, o, fw, fh);
                let (wx2, wy2) = mirror_view_pixel_to_wire(vx, vy, o, fw, fh);
                assert!(
                    wx2.abs_diff(wx) <= 1 && wy2.abs_diff(wy) <= 1,
                    "wire ({wx},{wy}) -> view ({vx},{vy}) -> wire ({wx2},{wy2})"
                );
            }
        }
    }
}
