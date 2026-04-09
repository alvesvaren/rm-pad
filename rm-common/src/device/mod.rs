mod rm2;
mod rmpp;

#[cfg(feature = "ssh")]
mod detect_ssh;

pub use rm2::RM2;
pub use rmpp::RMPP;

/// Device-specific parameters for input handling.
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
}
