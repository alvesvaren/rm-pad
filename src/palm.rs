//! Shared state for time-based palm rejection: pen down / last pen up time.
//! Used so the touch thread can suppress touch while the pen is down or recently lifted.

use std::sync::{Arc, Mutex};
use std::time::Instant;

/// State shared between pen and touch threads for palm rejection.
/// Held in `Arc<Mutex<PalmState>>` and passed to both threads when palm rejection is enabled.
#[derive(Default)]
pub struct PalmState {
    /// True when the pen is currently touching the screen (pressure > 0).
    pub pen_down: bool,
    /// When the pen was last lifted (pressure went to 0). None if never lifted this session.
    pub last_pen_up: Option<Instant>,
}

impl PalmState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Type alias for the shared palm state passed to pen and touch threads.
pub type SharedPalmState = Arc<Mutex<PalmState>>;
