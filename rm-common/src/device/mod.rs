mod rm2;
mod rmpp;

#[cfg(feature = "ssh")]
mod detect_ssh;

pub use rm2::RM2;
pub use rmpp::RMPP;

/// Device-specific parameters for input handling (`rm-pad` reads pen/touch evdev using these ranges;
/// touch `ABS_MT_POSITION_*` clamps and `transform_touch` / `transform_pen` are in `orientation`).
#[derive(Debug, Clone, Copy)]
pub struct DeviceProfile {
    #[allow(dead_code)]
    pub name: &'static str,

    pub input_event_size: usize,

    pub pen_x_max: i32,
    pub pen_y_max: i32,
    pub pen_pressure_max: i32,
    pub pen_distance_max: i32,
    pub pen_tilt_range: i32,

    pub touch_x_max: i32,
    pub touch_y_max: i32,
    pub touch_resolution: i32,

    pub pen_device: &'static str,
    pub touch_device: &'static str,
}

impl DeviceProfile {
    /// Defaults to RM2. For actual detection with SSH, use `detect_via_ssh()` (requires `ssh` feature).
    pub fn current() -> &'static Self {
        &RM2
    }

    /// Physical framebuffer size in pixels (width × height) for screen mirroring.
    pub fn framebuffer_size(&self) -> (u32, u32) {
        match self.name {
            "reMarkable 2" => (1404, 1872),
            "reMarkable Paper Pro" => (1620, 2160),
            _ => (1404, 1872),
        }
    }
}
