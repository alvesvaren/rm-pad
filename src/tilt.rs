//! Tilt-based correction for the pen's hover-vs-contact offset.
//!
//! The reMarkable's EMR digitizer reports the position of the pen's internal
//! resonant coil, which sits a short distance up the barrel from the physical
//! nib. When the pen is tilted, the coil's projection onto the sensor plane
//! lands *ahead* of the nib by roughly `L * sin(tilt)` along the direction of
//! lean, where `L` is the effective coil-to-nib lever length. This is what
//! produces the small gap between the hover cursor (the coil projection) and
//! the pen-down ink (the nib).
//!
//! [`TiltCorrection`] subtracts that offset so the emitted coordinate tracks the
//! nib. The offset is driven by tilt and is essentially independent of hover
//! height; [`TiltCorrectionMode::TiltDistance`] additionally ramps the
//! correction by proximity so it eases off as the pen lifts away from the
//! surface. It is applied in raw device space, before orientation and any
//! downstream aspect-ratio warp, so the correction is a true device-space
//! displacement rather than something distorted by a later non-linear map.

use std::f64::consts::FRAC_PI_2;
use std::fmt;
use std::str::FromStr;

use serde::Deserialize;

/// Which tilt-correction model to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TiltCorrectionMode {
    /// No correction (default; preserves original behaviour).
    #[default]
    Off,
    /// Correct by `gain * sin(tilt)` whenever the pen is in range.
    Tilt,
    /// Like [`Self::Tilt`], but ramped by proximity: full strength at contact,
    /// easing to zero as the pen lifts to maximum hover distance.
    TiltDistance,
}

/// A resolved tilt correction: the model plus its tuned strength.
#[derive(Debug, Clone, Copy)]
pub struct TiltCorrection {
    pub mode: TiltCorrectionMode,
    /// Effective lever length in pen digitizer units (the offset at 90° tilt).
    pub gain: f64,
}

impl TiltCorrection {
    /// The `(dx, dy)` offset to subtract from raw device coordinates.
    ///
    /// Full tilt range maps to 90°, so `sin` covers the physical lean. The
    /// caller is responsible for clamping the corrected point into range.
    pub fn offset(
        &self,
        tilt_x: i32,
        tilt_y: i32,
        tilt_range: i32,
        distance: i32,
        distance_max: i32,
    ) -> (i32, i32) {
        if self.mode == TiltCorrectionMode::Off || self.gain == 0.0 || tilt_range == 0 {
            return (0, 0);
        }

        let ramp = match self.mode {
            TiltCorrectionMode::Off => 0.0,
            TiltCorrectionMode::Tilt => 1.0,
            TiltCorrectionMode::TiltDistance => {
                if distance_max <= 0 {
                    1.0
                } else {
                    (1.0 - distance as f64 / distance_max as f64).clamp(0.0, 1.0)
                }
            }
        };

        (
            self.axis_offset(tilt_x, tilt_range, ramp),
            self.axis_offset(tilt_y, tilt_range, ramp),
        )
    }

    fn axis_offset(&self, tilt: i32, tilt_range: i32, ramp: f64) -> i32 {
        let t = tilt as f64 / tilt_range as f64;
        (self.gain * (t * FRAC_PI_2).sin() * ramp).round() as i32
    }
}

/// Resolve a mode + gain into an active correction, or `None` when disabled.
///
/// Mirrors `fit::resolve`: the default/disabled case returns `None` so the pen
/// loop can skip the stage entirely.
pub fn resolve(mode: TiltCorrectionMode, gain: f64) -> Option<TiltCorrection> {
    if mode == TiltCorrectionMode::Off || gain == 0.0 {
        return None;
    }
    Some(TiltCorrection { mode, gain })
}

impl fmt::Display for TiltCorrectionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TiltCorrectionMode::Off => write!(f, "off"),
            TiltCorrectionMode::Tilt => write!(f, "tilt"),
            TiltCorrectionMode::TiltDistance => write!(f, "tilt-distance"),
        }
    }
}

impl FromStr for TiltCorrectionMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "off" | "none" => Ok(TiltCorrectionMode::Off),
            "tilt" => Ok(TiltCorrectionMode::Tilt),
            "tilt-distance" | "tilt_distance" | "distance" => Ok(TiltCorrectionMode::TiltDistance),
            _ => Err(format!(
                "Invalid tilt correction '{}'. Valid values: off, tilt, tilt-distance",
                s
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RANGE: i32 = 9000; // RMPP: full range = 90.00°
    const DIST_MAX: i32 = 65535;

    #[test]
    fn disabled_when_off_or_zero_gain() {
        assert!(resolve(TiltCorrectionMode::Off, 300.0).is_none());
        assert!(resolve(TiltCorrectionMode::Tilt, 0.0).is_none());
        assert!(resolve(TiltCorrectionMode::Tilt, 300.0).is_some());
    }

    #[test]
    fn off_mode_offset_is_zero() {
        let c = TiltCorrection { mode: TiltCorrectionMode::Off, gain: 300.0 };
        assert_eq!(c.offset(RANGE, RANGE, RANGE, 0, DIST_MAX), (0, 0));
    }

    #[test]
    fn full_tilt_offset_equals_gain() {
        // At full range (90°), sin(90°) = 1, so the offset equals the gain.
        let c = TiltCorrection { mode: TiltCorrectionMode::Tilt, gain: 300.0 };
        assert_eq!(c.offset(RANGE, 0, RANGE, 0, DIST_MAX), (300, 0));
        assert_eq!(c.offset(0, RANGE, RANGE, 0, DIST_MAX), (0, 300));
    }

    #[test]
    fn negated_tilt_negates_offset() {
        let c = TiltCorrection { mode: TiltCorrectionMode::Tilt, gain: 300.0 };
        assert_eq!(c.offset(-RANGE, 0, RANGE, 0, DIST_MAX), (-300, 0));
    }

    #[test]
    fn axes_are_independent() {
        let c = TiltCorrection { mode: TiltCorrectionMode::Tilt, gain: 1000.0 };
        // 45° on x only: sin(45°) ~= 0.7071 -> 707; y untouched.
        assert_eq!(c.offset(RANGE / 2, 0, RANGE, 0, DIST_MAX), (707, 0));
    }

    #[test]
    fn distance_ramp_full_at_contact_zero_at_max_hover() {
        let c = TiltCorrection { mode: TiltCorrectionMode::TiltDistance, gain: 300.0 };
        // distance 0 (touching) -> full offset.
        assert_eq!(c.offset(RANGE, 0, RANGE, 0, DIST_MAX), (300, 0));
        // distance at max (far hover) -> no offset.
        assert_eq!(c.offset(RANGE, 0, RANGE, DIST_MAX, DIST_MAX), (0, 0));
        // halfway -> half offset.
        assert_eq!(c.offset(RANGE, 0, RANGE, DIST_MAX / 2, DIST_MAX), (150, 0));
    }
}
