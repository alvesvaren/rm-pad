//! Forward reMarkable pen input to a uinput pen device.

use std::io::Read;
use std::path::Path;
use std::time::Instant;

use evdevil::event::{Abs, InputEvent, Key};
use evdevil::uinput::{AbsSetup, UinputDevice};
use evdevil::AbsInfo;
use evdevil::Bus;
use evdevil::InputId;
use evdevil::InputProp;

use crate::config::{PEN_DEVICE, PEN_PIDFILE};
use crate::event::{
    key_event, parse_input_event, ABS_PRESSURE, EV_ABS, EV_SYN, INPUT_EVENT_SIZE, SYN_REPORT,
};
use crate::palm::SharedPalmState;
use crate::ssh;

// reMarkable digitizer ranges from pen_bounds dump: X 39..20892, Y 164..15725.
// libinput requires resolution on X/Y to accept the device as a tablet (needed for Krita etc.).
fn create_pen_device() -> Result<UinputDevice, Box<dyn std::error::Error + Send + Sync>> {
    let axes = [
        AbsSetup::new(
            Abs::X,
            AbsInfo::new(0, 21000).with_resolution(100), // units/mm, libinput requirement
        ),
        AbsSetup::new(
            Abs::Y,
            AbsInfo::new(0, 16000).with_resolution(100),
        ),
        AbsSetup::new(Abs::PRESSURE, AbsInfo::new(0, 4095)),
        AbsSetup::new(Abs::DISTANCE, AbsInfo::new(0, 255)),
        AbsSetup::new(Abs::TILT_X, AbsInfo::new(-8192, 8192)),
        AbsSetup::new(Abs::TILT_Y, AbsInfo::new(-8192, 8192)),
    ];
    // USB bus + vendor 0x2d1f so libwacom can match via DeviceMatch=usb:2d1f:0001 (see data/ in repo).
    let device = UinputDevice::builder()?
        .with_input_id(InputId::new(Bus::from_raw(0x03), 0x2d1f, 0x0001, 0))?
        .with_props([InputProp::DIRECT])?
        .with_abs_axes(axes)?
        .with_keys([Key::BTN_TOOL_PEN, Key::BTN_TOUCH, Key::BTN_STYLUS])?
        .build("reMarkable Pen")?;
    Ok(device)
}

pub fn run(
    key_path: &Path,
    palm: Option<SharedPalmState>,
    use_grab: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (_sess, mut channel) =
        ssh::open_input_stream(PEN_DEVICE, key_path, use_grab, Some(PEN_PIDFILE))?;
    log::info!("[pen] creating uinput device…");
    let device = create_pen_device()?;
    if let Ok(name) = device.sysname() {
        log::info!("[pen] uinput device created: /sys/devices/virtual/input/{}", name.to_string_lossy());
    }
    // Give udev/libinput time to attach before sending events (kernel uinput docs).
    std::thread::sleep(std::time::Duration::from_secs(1));
    log::info!("[pen] forwarding (move pen on tablet to see events)");

    let btn_touch_code = Key::BTN_TOUCH.raw();
    let mut buf = [0u8; INPUT_EVENT_SIZE];
    let mut batch: Vec<InputEvent> = Vec::with_capacity(32);
    let mut touch_down = false;
    let mut count: u64 = 0;

    loop {
        channel.read_exact(&mut buf)?;
        if let Some(ev) = parse_input_event(&buf) {
            let ty = ev.event_type().raw();
            let code = ev.raw_code();
            batch.push(ev);
            if ty == EV_SYN && code == SYN_REPORT {
                // reMarkable doesn't send BTN_TOUCH; derive from ABS_PRESSURE (dump: pressure only when touching).
                let pressure = batch
                    .iter()
                    .filter(|e| e.event_type().raw() == EV_ABS && e.raw_code() == ABS_PRESSURE)
                    .last()
                    .map(|e| e.raw_value())
                    .unwrap_or(0);
                let now_touching = pressure > 0;
                if let Some(ref palm_state) = palm {
                    if let Ok(mut state) = palm_state.lock() {
                        state.pen_down = now_touching;
                        if !now_touching {
                            state.last_pen_up = Some(Instant::now());
                        }
                    }
                }
                if now_touching != touch_down {
                    let key_ev = key_event(btn_touch_code, if now_touching { 1 } else { 0 });
                    batch.insert(0, key_ev);
                }
                touch_down = now_touching;

                if count == 0 {
                    log::info!("[pen] first event batch (events are flowing)");
                }
                count += 1;
                device.write(&batch)?;
                batch.clear();
                if count % 500 == 0 {
                    log::debug!("[pen] batches forwarded: {}", count);
                }
            }
        }
    }
}
