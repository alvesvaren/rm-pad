//! Configuration for SSH and device paths.

pub const HOST: &str = "192.168.1.69";
pub const USER: &str = "root";
pub const KEY_PATH: &str = "rm-key";

/// reMarkable: event1 = pen, event2 = touch (adjust if your device differs).
pub const PEN_DEVICE: &str = "/dev/input/event1";
pub const TOUCH_DEVICE: &str = "/dev/input/event2";
