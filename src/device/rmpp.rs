use super::DeviceProfile;

/// reMarkable Paper Pro device profile.
/// 
/// Specifications:
/// - Display: 1620×2160 pixels (11.8", 229 dpi)
/// - Architecture: aarch64
/// 
/// Note: Pen digitizer ranges are estimated based on similar scaling to RM2.
/// Actual values may need adjustment based on device dumps.
pub const RMPP: DeviceProfile = DeviceProfile {
    name: "reMarkable Paper Pro",

    // Pen digitizer ranges (estimated - may need adjustment from actual device dumps)
    // Based on resolution 1620×2160, assuming similar scaling to RM2
    // RM2: 20966×15725 for 1872×1404 display → ~11.2x scaling
    // rMPP: 1620×2160 display → estimated ~18144×24192 (11.2x scaling)
    pen_x_max: 24192,
    pen_y_max: 18144,
    pen_pressure_max: 4095,
    pen_distance_max: 255,
    pen_tilt_range: 5900,

    // Touch screen: 1620×2160 display (portrait orientation)
    // Assuming similar resolution scaling as RM2 (~9 units/mm)
    touch_x_max: 1619,
    touch_y_max: 2159,
    touch_resolution: 9,

    // Default device paths (may need adjustment - typically same as RM2)
    pen_device: "/dev/input/event1",
    touch_device: "/dev/input/event2",
};
