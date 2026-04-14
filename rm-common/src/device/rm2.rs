use super::DeviceProfile;

/// reMarkable 2 profile.
///
/// Stock UI logs (Qt `evdevtouch` on OS3.26) report touch `max X: 1403`, `max Y: 1871` on
/// `/dev/input/event2`, matching the fields below. AppLoad qtfb-shim exposes a **1404×1872** client
/// buffer with **2808** byte lines (16bpp), same shape as `DeviceProfile::framebuffer_size`.
///
/// **Coordinate frames** (`rm-pad` touch/pen forwarding):
///
/// - **Touch** (`/dev/input/event2`): `ABS_MT_POSITION_X` is 0..=`touch_x_max` (1404 logical px along
///   the **short** panel edge), `ABS_MT_POSITION_Y` is 0..=`touch_y_max` (1872 along the **long**
///   edge). This is the **portrait** “wire” frame: same numbering as `DeviceProfile::framebuffer_size`
///   `(1404, 1872)` = width × height in mmap for that mode (see `rm-pad` `input/touch.rs`).
///
/// - **Pen** (`/dev/input/event1`): raw `ABS_X` / `ABS_Y` use **`pen_x_max` > `pen_y_max`** — the
///   stylus digitizer’s wide axis matches the **long** physical edge (touch Y direction), narrow axis
///   the **short** (touch X). `Orientation::transform_pen` maps that **landscape-native** pen space
///   into the host’s logical orientation; it is **not** the same as framebuffer row order for touch.
///
/// Screen mirroring must place pixels in the **touch / framebuffer** frame, not pen-native space.
pub const RM2: DeviceProfile = DeviceProfile {
    name: "reMarkable 2",

    // 32-bit ARM input_event struct size
    input_event_size: 16,

    // Pen digitizer ranges (from device dumps); note x_max > y_max vs touch (see module comment).
    pen_x_max: 20967,
    pen_y_max: 15725,
    pen_pressure_max: 4095,
    pen_distance_max: 255,
    pen_tilt_range: 6400,

    // Touch: short × long = 1404 × 1872 logical pixels (~210×158 mm → ~9 units/mm)
    touch_x_max: 1403,
    touch_y_max: 1871,
    touch_resolution: 9,

    // Default device paths
    pen_device: "/dev/input/event1",
    touch_device: "/dev/input/event2",
};
