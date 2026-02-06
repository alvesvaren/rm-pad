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
    /// 
    /// Device touch coordinates: (0,0) is top-left in portrait mode.
    /// Returns (out_x, out_y) in the rotated coordinate system.
    pub fn transform_touch(&self, x: i32, y: i32, x_max: i32, y_max: i32) -> (i32, i32) {
        match self {
            Orientation::Portrait => (x, y),
            // LandscapeRight: swap X/Y (original working behavior)
            Orientation::LandscapeRight => (y, x),
            // LandscapeLeft: swap X/Y and invert both
            Orientation::LandscapeLeft => (y_max - y, x_max - x),
            // Inverted: invert both axes
            Orientation::Inverted => (x_max - x, y_max - y),
        }
    }

    /// Transform pen coordinates from device space to output space.
    /// 
    /// The pen digitizer on reMarkable is natively landscape-oriented,
    /// so LandscapeRight is the identity transform (no change).
    pub fn transform_pen(&self, x: i32, y: i32, x_max: i32, y_max: i32) -> (i32, i32) {
        match self {
            // LandscapeRight: native pen orientation, no transform
            Orientation::LandscapeRight => (x, y),
            // Portrait: swap X/Y (opposite of touch)
            Orientation::Portrait => (y, x),
            // LandscapeLeft: invert both axes
            Orientation::LandscapeLeft => (x_max - x, y_max - y),
            // Inverted: swap X/Y and invert both
            Orientation::Inverted => (y_max - y, x_max - x),
        }
    }

    /// Transform tilt values to match the orientation.
    /// Tilt follows pen orientation (LandscapeRight is native).
    pub fn transform_tilt(&self, tilt_x: i32, tilt_y: i32) -> (i32, i32) {
        match self {
            // LandscapeRight: native, no transform
            Orientation::LandscapeRight => (tilt_x, tilt_y),
            // Portrait: swap tilt axes
            Orientation::Portrait => (tilt_y, tilt_x),
            // LandscapeLeft: invert both
            Orientation::LandscapeLeft => (-tilt_x, -tilt_y),
            // Inverted: swap and invert
            Orientation::Inverted => (-tilt_y, -tilt_x),
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
        
        assert_eq!(portrait.output_dimensions(100, 200), (100, 200));
        assert_eq!(landscape.output_dimensions(100, 200), (200, 100));
    }

    #[test]
    fn test_from_str() {
        assert_eq!("portrait".parse::<Orientation>().unwrap(), Orientation::Portrait);
        assert_eq!("landscape-right".parse::<Orientation>().unwrap(), Orientation::LandscapeRight);
        assert_eq!("landscape_left".parse::<Orientation>().unwrap(), Orientation::LandscapeLeft);
        assert!("invalid".parse::<Orientation>().is_err());
    }
}
