use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Shared state for palm rejection between pen and touch threads.
#[derive(Default)]
pub struct PalmState {
    pub pen_down: bool,
    pub last_pen_up: Option<Instant>,
}

impl PalmState {
    pub fn new() -> Self {
        Self::default()
    }
}

pub type SharedPalmState = Arc<Mutex<PalmState>>;
