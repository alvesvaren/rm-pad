//! Configuration for SSH and device paths.

pub const HOST: &str = "10.11.99.1";
pub const USER: &str = "root";
pub const KEY_PATH: &str = "rm-key";

/// reMarkable: event1 = pen, event2 = touch (adjust if your device differs).
pub const PEN_DEVICE: &str = "/dev/input/event1";
pub const TOUCH_DEVICE: &str = "/dev/input/event2";

/// When using --grab (default): path to rm-mouse-grabber on the reMarkable.
pub const GRABBER_PATH: &str = "/home/root/rm-mouse-grabber";
/// File touched by the host every few seconds. Grabber exits if this is older than STALE_SEC.
pub const ALIVE_FILE: &str = "/tmp/rm-mouse-alive";
/// Seconds after which the grabber considers the host gone and exits (self-check).
pub const STALE_SEC: u32 = 10;
/// Pidfiles written by the grabber (for optional external watchdog).
pub const PEN_PIDFILE: &str = "/tmp/rm-mouse-pen.pid";
pub const TOUCH_PIDFILE: &str = "/tmp/rm-mouse-touch.pid";
