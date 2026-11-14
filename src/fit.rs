//! Aspect-ratio fit modes, applied as an input-side warp.
//!
//! The reMarkable's active area and the target (the whole desktop, or a single
//! display) rarely share an aspect ratio. This correction is applied here, on
//! the virtual device, rather than left to a compositor-level tablet mapping:
//! that mapping is only honored by programs that respect it, and some apps
//! (osu!lazer, for one) take over the tablet and handle coordinates themselves.
//! Warping in input space bakes the fit into what *every* program receives, so
//! the chosen aspect-ratio behavior is consistent across all of them.
//!
//! A [`FitMap`] warps pen coordinates *within pen-input space* (`0..=in`), so
//! that a later linear stretch — the compositor stretching the pen-sized axes
//! across the desktop, or a screen-selection map onto one display — lands the
//! pen with the chosen fit:
//!
//! - [`FitMode::Fill`]: no warp; the stretch fills the target, ignoring aspect.
//! - [`FitMode::Contain`]: compress the axis that would otherwise be
//!   over-stretched, so the whole pen area maps to a centered, aspect-preserving
//!   band (letterboxed — the pen can't reach two edges of the target).
//! - [`FitMode::Cover`]: grow the under-stretched axis (clamped), so the pen
//!   covers the whole target with its aspect preserved (edges cropped).
//!
//! Because the warp only depends on the pen-vs-target aspect ratio, the same
//! [`FitMap`] composes whether the downstream stretch is the compositor (whole
//! desktop) or a display map. The target is the bounding box of the screens
//! selected via `--screen` (see [`resolve`]), so a non-fill fit is only
//! meaningful once at least one screen is chosen.

use serde::Deserialize;
use std::fmt;
use std::str::FromStr;

use crate::display::SizeData;
use crate::pen_map::PenInputMap;

/// How the pen's active area is fitted into the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FitMode {
    /// Stretch to fill the target, ignoring aspect ratio.
    #[default]
    Fill,
    /// Fit the whole pen area inside the target, preserving aspect ratio.
    Contain,
    /// Cover the whole target, preserving aspect ratio (pen edges cropped).
    Cover,
}

impl FitMode {
    /// Warp a pen point in `0..=in_*` space so a downstream linear stretch onto
    /// a target of aspect `target_w:target_h` produces this fit mode.
    ///
    /// Each axis uses a centered fraction `f = a/b` of its range:
    /// `out = in*(1-f)/2 + coord*f`, clamped to `[0, in]`. `f < 1` letterboxes
    /// (contain); `f > 1` overflows and the clamp crops (cover).
    fn warp(&self, x: i64, y: i64, in_x: i64, in_y: i64, target_w: i64, target_h: i64) -> (i64, i64) {
        let in_x = in_x.max(1);
        let in_y = in_y.max(1);

        // How much each axis is stretched by the downstream map, compared via
        // cross-multiplication: rx = x-stretch, ry = y-stretch (up to a shared
        // factor). Only their ratio matters.
        let rx = target_w * in_y;
        let ry = target_h * in_x;

        let (fx, fy) = match self {
            FitMode::Fill => ((1, 1), (1, 1)),
            // Contain: shrink the more-stretched axis to match the other.
            FitMode::Contain => {
                if rx >= ry {
                    ((ry, rx), (1, 1))
                } else {
                    ((1, 1), (rx, ry))
                }
            }
            // Cover: grow the less-stretched axis past the target (clamp crops).
            FitMode::Cover => {
                if rx >= ry {
                    ((1, 1), (rx, ry))
                } else {
                    ((ry, rx), (1, 1))
                }
            }
        };

        (warp_axis(x, in_x, fx), warp_axis(y, in_y, fy))
    }
}

/// Apply a centered `f = a/b` fraction to one axis, clamped to `[0, in_max]`.
fn warp_axis(coord: i64, in_max: i64, (a, b): (i64, i64)) -> i64 {
    // in*(1-f)/2 + coord*f  with f = a/b  ==  (in*(b-a) + 2*coord*a) / (2*b)
    let out = (in_max * (b - a) + 2 * coord * a) / (2 * b);
    out.clamp(0, in_max)
}

/// A [`PenInputMap`] stage that warps pen coordinates for an aspect-ratio fit
/// against a target of `target_w x target_h`.
#[derive(Debug, Clone)]
pub struct FitMap {
    fit: FitMode,
    size_data: SizeData,
    label: String,
}

impl PenInputMap for FitMap {
    fn map(&self, x: i32, y: i32, in_x_max: i32, in_y_max: i32) -> (i32, i32) {
        let (ox, oy) = self.fit.warp(
            x as i64,
            y as i64,
            in_x_max as i64,
            in_y_max as i64,
            self.size_data.get_width() as i64,
            self.size_data.get_height() as i64,
        );
        (ox as i32, oy as i32)
    }

    /// Pen-space warp: the axis range is unchanged (pass-through).
    fn output_bounds(&self, in_x_max: i32, in_y_max: i32) -> (i32, i32) {
        (in_x_max, in_y_max)
    }

    fn label(&self) -> String {
        self.label.clone()
    }
}

/// Resolve the configured fit mode to a coordinate mapping over the given
/// size data.
///
/// `Fill` needs no warp (`None`). `Contain`/`Cover` fit against the bounding
/// box of the given size data.
pub fn resolve(fit: FitMode, size_data: Option<SizeData>) -> Option<FitMap> {
    if fit == FitMode::Fill {
        return None;
    }
    let size_data = size_data.expect("non-fill fit should be guarded by config validation");

    Some(FitMap {
        fit,
        size_data,
        label: format!("{fit} ({size_data})"),
    })
}

impl fmt::Display for FitMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FitMode::Fill => write!(f, "fill"),
            FitMode::Contain => write!(f, "contain"),
            FitMode::Cover => write!(f, "cover"),
        }
    }
}

impl FromStr for FitMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "fill" | "stretch" => Ok(FitMode::Fill),
            "contain" | "fit" => Ok(FitMode::Contain),
            "cover" => Ok(FitMode::Cover),
            _ => Err(format!(
                "Invalid fit mode '{}'. Valid values: fill, contain, cover",
                s
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::display::Resolution;

    use super::*;

    fn fitmap(fit: FitMode, tw: u32, th: u32) -> FitMap {
        FitMap { fit, size_data: SizeData::Resolution(Resolution::new(tw, th)), label: "test".into() }
    }

    // A wide 2:1 pen; downstream target square (1:1). The x axis is
    // over-stretched, so contain compresses y and cover grows x.
    const IN_X: i32 = 2000;
    const IN_Y: i32 = 1000;

    #[test]
    fn contain_letterboxes_over_stretched_axis() {
        let m = fitmap(FitMode::Contain, 1000, 1000);
        // x passes through; y is compressed into a centered band [250, 750].
        assert_eq!(m.map(1000, 0, IN_X, IN_Y), (1000, 250));
        assert_eq!(m.map(1000, 500, IN_X, IN_Y), (1000, 500));
        assert_eq!(m.map(1000, 1000, IN_X, IN_Y), (1000, 750));
        assert_eq!(m.map(0, 500, IN_X, IN_Y), (0, 500));
        assert_eq!(m.map(2000, 500, IN_X, IN_Y), (2000, 500));
    }

    #[test]
    fn contain_compresses_the_other_axis_when_pen_is_tall() {
        // Tall 1:2 pen into a square target: x is compressed instead.
        let m = fitmap(FitMode::Contain, 1000, 1000);
        assert_eq!(m.map(0, 1000, 1000, 2000), (250, 1000));
        assert_eq!(m.map(500, 1000, 1000, 2000), (500, 1000));
        assert_eq!(m.map(1000, 1000, 1000, 2000), (750, 1000));
    }

    #[test]
    fn cover_grows_and_clamps() {
        let m = fitmap(FitMode::Cover, 1000, 1000);
        // x is grown 2x and clamped, so pen corners pin to pen corners and the
        // centre stays centred; the outer thirds crop to the edges.
        assert_eq!(m.map(0, 0, IN_X, IN_Y), (0, 0));
        assert_eq!(m.map(2000, 1000, IN_X, IN_Y), (2000, 1000));
        assert_eq!(m.map(1000, 500, IN_X, IN_Y), (1000, 500));
        assert_eq!(m.map(500, 500, IN_X, IN_Y), (0, 500)); // left third clamps
    }

    #[test]
    fn output_bounds_pass_through() {
        let m = fitmap(FitMode::Contain, 1000, 1000);
        assert_eq!(m.output_bounds(IN_X, IN_Y), (IN_X, IN_Y));
    }

    #[test]
    fn fill_warp_is_identity() {
        let m = fitmap(FitMode::Fill, 1000, 1000);
        assert_eq!(m.map(0, 0, IN_X, IN_Y), (0, 0));
        assert_eq!(m.map(1234, 567, IN_X, IN_Y), (1234, 567));
        assert_eq!(m.map(IN_X, IN_Y, IN_X, IN_Y), (IN_X, IN_Y));
    }
}
