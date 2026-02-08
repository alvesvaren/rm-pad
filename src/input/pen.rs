use std::io::Read;
use std::time::Instant;

use evdevil::event::{Abs, InputEvent, Key};
use evdevil::uinput::{AbsSetup, UinputDevice};
use evdevil::{AbsInfo, Bus, InputId, InputProp};

use crate::config::Config;
use crate::device::DeviceProfile;
use crate::orientation::Orientation;
use crate::palm::SharedPalmState;
use crate::ssh;

use super::event::{key_event, parse_input_event, ABS_PRESSURE, EV_ABS, EV_SYN, INPUT_EVENT_SIZE, SYN_REPORT};

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_TILT_X: u16 = 0x1a;
const ABS_TILT_Y: u16 = 0x1b;

fn create_pen_device(device: &DeviceProfile, orientation: Orientation) -> Result<UinputDevice, Box<dyn std::error::Error + Send + Sync>> {
    let (out_x_max, out_y_max) = orientation.pen_output_dimensions(device.pen_x_max, device.pen_y_max);
    let axes = [
        AbsSetup::new(Abs::X, AbsInfo::new(0, out_x_max).with_resolution(100)),
        AbsSetup::new(Abs::Y, AbsInfo::new(0, out_y_max).with_resolution(100)),
        AbsSetup::new(Abs::PRESSURE, AbsInfo::new(0, device.pen_pressure_max)),
        AbsSetup::new(Abs::DISTANCE, AbsInfo::new(0, device.pen_distance_max)),
        AbsSetup::new(Abs::TILT_X, AbsInfo::new(-device.pen_tilt_range, device.pen_tilt_range)),
        AbsSetup::new(Abs::TILT_Y, AbsInfo::new(-device.pen_tilt_range, device.pen_tilt_range)),
    ];

    let device = UinputDevice::builder()?
        .with_input_id(InputId::new(Bus::from_raw(0x03), 0x2d1f, 0x0001, 0))?
        .with_props([InputProp::DIRECT])?
        .with_abs_axes(axes)?
        .with_keys([Key::BTN_TOOL_PEN, Key::BTN_TOUCH, Key::BTN_STYLUS])?
        .build("reMarkable Pen")?;

    Ok(device)
}

pub fn run_pen(
    config: &Config,
    device_profile: &DeviceProfile,
    palm: Option<SharedPalmState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (_cleanup, mut channel) =
        ssh::open_input_stream(&config.pen_device, config, config.grab_input)?;

    log::info!("Creating pen uinput device");
    let uinput = create_pen_device(device_profile, config.orientation)?;

    if let Ok(name) = uinput.sysname() {
        log::info!("Pen device ready: /sys/devices/virtual/input/{}", name.to_string_lossy());
    }

    std::thread::sleep(std::time::Duration::from_secs(1));
    log::info!("Pen forwarding started");

    let btn_touch_code = Key::BTN_TOUCH.raw();
    let mut buf = [0u8; INPUT_EVENT_SIZE];
    let mut batch: Vec<InputEvent> = Vec::with_capacity(32);
    let mut touch_down = false;
    let mut frame_count: u64 = 0;

    // For collecting X/Y/tilt values within a frame
    let mut pending_x: Option<i32> = None;
    let mut pending_y: Option<i32> = None;
    let mut pending_tilt_x: Option<i32> = None;
    let mut pending_tilt_y: Option<i32> = None;
    let orientation = config.orientation;

    loop {
        channel.read_exact(&mut buf)?;

        let Some(ev) = parse_input_event(&buf) else {
            continue;
        };

        let ty = ev.event_type().raw();
        let code = ev.raw_code();
        let value = ev.raw_value();

        // Collect position and tilt values, defer transformation until SYN_REPORT
        if ty == EV_ABS {
            match code {
                ABS_X => {
                    pending_x = Some(value);
                    continue;
                }
                ABS_Y => {
                    pending_y = Some(value);
                    continue;
                }
                ABS_TILT_X => {
                    pending_tilt_x = Some(value);
                    continue;
                }
                ABS_TILT_Y => {
                    pending_tilt_y = Some(value);
                    continue;
                }
                _ => {}
            }
        }

        batch.push(ev);

        if ty != EV_SYN || code != SYN_REPORT {
            continue;
        }

        // Transform and emit position events
        if let (Some(x), Some(y)) = (pending_x.take(), pending_y.take()) {
            let (out_x, out_y) = orientation.transform_pen(
                x, y,
                device_profile.pen_x_max,
                device_profile.pen_y_max,
            );
            batch.insert(0, InputEvent::new(evdevil::event::EventType::from_raw(EV_ABS), Abs::X.raw(), out_x));
            batch.insert(1, InputEvent::new(evdevil::event::EventType::from_raw(EV_ABS), Abs::Y.raw(), out_y));
        }

        // Transform and emit tilt events
        if let (Some(tx), Some(ty)) = (pending_tilt_x.take(), pending_tilt_y.take()) {
            let (out_tx, out_ty) = orientation.transform_tilt(tx, ty);
            batch.insert(0, InputEvent::new(evdevil::event::EventType::from_raw(EV_ABS), Abs::TILT_X.raw(), out_tx));
            batch.insert(1, InputEvent::new(evdevil::event::EventType::from_raw(EV_ABS), Abs::TILT_Y.raw(), out_ty));
        }

        let pressure = batch
            .iter()
            .rfind(|e| e.event_type().raw() == EV_ABS && e.raw_code() == ABS_PRESSURE)
            .map(|e| e.raw_value())
            .unwrap_or(0);

        let now_touching = pressure > 0;
        update_palm_state(&palm, now_touching);

        if now_touching != touch_down {
            let key_ev = key_event(btn_touch_code, if now_touching { 1 } else { 0 });
            batch.insert(0, key_ev);
        }
        touch_down = now_touching;

        if frame_count == 0 {
            log::info!("Pen events flowing");
        }
        frame_count += 1;

        uinput.write(&batch)?;
        batch.clear();

        if frame_count.is_multiple_of(500) {
            log::debug!("Pen frames forwarded: {}", frame_count);
        }
    }
}

fn update_palm_state(palm: &Option<SharedPalmState>, now_touching: bool) {
    let Some(palm_state) = palm else { return };
    let Ok(mut state) = palm_state.lock() else { return };

    state.pen_down = now_touching;
    if !now_touching {
        state.last_pen_up = Some(Instant::now());
    }
}
