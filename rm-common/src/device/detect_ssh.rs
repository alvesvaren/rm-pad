use std::io::Read;

use ssh2::Session;

use super::{DeviceProfile, RM2, RMPP};

impl DeviceProfile {
    /// Detect device via SSH connection.
    pub fn detect_via_ssh(session: &Session) -> Result<&'static Self, Box<dyn std::error::Error + Send + Sync>> {
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

        if model.contains("reMarkable Ferrari") {
            log::info!("Detected reMarkable Paper Pro");
            return Ok(&RMPP);
        }

        if model.contains("reMarkable 2.0") {
            log::info!("Detected reMarkable 2");
            return Ok(&RM2);
        }

        Err(format!("Unsupported device model: '{}'", model).into())
    }
}
