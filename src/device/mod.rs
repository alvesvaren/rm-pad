mod rm2;
mod rmpp;

use std::io::Read;

pub use rm2::RM2;
pub use rmpp::RMPP;

/// Device-specific parameters for input handling.
#[derive(Debug, Clone, Copy)]
pub struct DeviceProfile {
    #[allow(dead_code)]
    pub name: &'static str,

    // Raw input_event size on the device (bytes)
    pub input_event_size: usize,

    // Pen digitizer ranges
    pub pen_x_max: i32,
    pub pen_y_max: i32,
    pub pen_pressure_max: i32,
    pub pen_distance_max: i32,
    pub pen_tilt_range: i32,

    // Touch screen dimensions
    pub touch_x_max: i32,
    pub touch_y_max: i32,
    pub touch_resolution: i32,

    // Default device paths
    pub pen_device: &'static str,
    pub touch_device: &'static str,
}

impl DeviceProfile {
    /// Get profile for the current device.
    /// 
    /// Defaults to RM2. For actual detection, use `detect_via_ssh()`.
    pub fn current() -> &'static Self {
        &RM2
    }

    /// Detect device via SSH connection.
    /// 
    /// Reads the device model from /proc/device-tree/model on the remote device.
    /// Returns an error if the model cannot be detected or is unsupported.
    pub fn detect_via_ssh(session: &ssh2::Session) -> Result<&'static Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut channel = session.channel_session()?;
        channel.exec("cat /proc/device-tree/model")?;

        let mut output = String::new();
        channel.read_to_string(&mut output)?;
        channel.close()?;
        channel.wait_close()?;

        let status = channel.exit_status()?;
        if status != 0 {
            return Err(format!("Failed to read device model (exit status {})", status).into());
        }

        let model = output.trim();
        if model.is_empty() {
            return Err("Device model is empty".into());
        }

        log::debug!("Detected remote device model: {}", model);

        // Check for rMPP first (more specific)
        if model.contains("reMarkable Ferrari") {
            log::info!("Detected reMarkable Paper Pro");
            return Ok(&RMPP);
        }

        // Check for RM2 (matches "reMarkable 2.0", "reMarkable 2", etc.)
        if model.contains("reMarkable 2.0") {
            log::info!("Detected reMarkable 2");
            return Ok(&RM2);
        }

        Err(format!("Unsupported device model: '{}'", model).into())
    }
}
