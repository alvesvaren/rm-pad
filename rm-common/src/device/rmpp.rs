use super::DeviceProfile;

/// reMarkable Paper Pro device profile.
/// 
/// Specifications:
/// - Display: 1620×2160 pixels (11.8", 229 dpi)
/// - Architecture: aarch64
/// 
/// Values based on actual device evdev settings from remouse project.
pub const RMPP: DeviceProfile = DeviceProfile {
    name: "reMarkable Paper Pro",

    // 64-bit ARM input_event struct size
    input_event_size: 24,

    pen_x_max: 11180,
    pen_y_max: 15340,
    pen_pressure_max: 4096,
    pen_distance_max: 65535,
    pen_tilt_range: 9000,

    touch_x_max: 2064,
    touch_y_max: 2832,
    touch_resolution: 9,

    pen_device: "/dev/input/event2",
    touch_device: "/dev/input/event3",
};
