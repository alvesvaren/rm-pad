use super::DeviceProfile;

pub const RM2: DeviceProfile = DeviceProfile {
    name: "reMarkable 2",

    // Pen digitizer ranges (from device dumps)
    pen_x_max: 20967,
    pen_y_max: 15725,
    pen_pressure_max: 4095,
    pen_distance_max: 255,
    pen_tilt_range: 6400,

    // Touch screen: 1872×1404 display, ~210×158 mm → ~9 units/mm
    touch_x_max: 1403,
    touch_y_max: 1871,
    touch_resolution: 9,

    // Default device paths
    pen_device: "/dev/input/event1",
    touch_device: "/dev/input/event2",
};
