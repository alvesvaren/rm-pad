use std::io::Read;
use std::time::Instant;

use evdevil::event::{Abs, InputEvent, Key};
use evdevil::uinput::{AbsSetup, UinputDevice};
use evdevil::{AbsInfo, Bus, InputId, InputProp};

use crate::config::Config;
use crate::device::DeviceProfile;
use crate::display::SizeData;
use crate::fit;
use crate::palm::SharedPalmState;
use crate::pen_map::{PenInputMap, PenInputPipeline};
use crate::ssh;

use super::event::{key_event, parse_input_event, ABS_PRESSURE, EV_ABS, EV_KEY, EV_SYN, SYN_REPORT};

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_DISTANCE: u16 = 0x19;
const ABS_TILT_X: u16 = 0x1a;
const ABS_TILT_Y: u16 = 0x1b;

fn create_pen_device(
    device: &DeviceProfile,
    pipeline: &PenInputPipeline,
) -> Result<UinputDevice, Box<dyn std::error::Error + Send + Sync>> {
    // The pipeline's final axis range is the uinput ABS_X/ABS_Y space: the pen
    // digitizer range when empty (compositor stretches it across the desktop),
    // or whatever a mapping stage advertises.
    //
    // Resolution must be non-zero or libinput refuses to create a tablet tool
    // (Hyprland/wlroots then never sees the device and the pen is dead). The
    // value is arbitrary for absolute->output mapping.
    let axes = [
        AbsSetup::new(Abs::X, AbsInfo::new(0, pipeline.axis_x_max).with_resolution(100)),
        AbsSetup::new(Abs::Y, AbsInfo::new(0, pipeline.axis_y_max).with_resolution(100)),
        AbsSetup::new(Abs::PRESSURE, AbsInfo::new(0, device.pen_pressure_max)),
        AbsSetup::new(Abs::DISTANCE, AbsInfo::new(0, device.pen_distance_max)),
        AbsSetup::new(Abs::TILT_X, AbsInfo::new(-device.pen_tilt_range, device.pen_tilt_range)),
        AbsSetup::new(Abs::TILT_Y, AbsInfo::new(-device.pen_tilt_range, device.pen_tilt_range)),
    ];

    log::info!(
        "Pen axis range 0..{} x 0..{} [{}]",
        pipeline.axis_x_max, pipeline.axis_y_max, pipeline.describe()
    );

    let device = UinputDevice::builder()?
        .with_input_id(InputId::new(Bus::from_raw(0x03), 0x2d1f, 0x0001, 0))?
        .with_props([InputProp::DIRECT])?
        .with_abs_axes(axes)?
        .with_keys([Key::BTN_TOOL_PEN, Key::BTN_TOUCH, Key::BTN_STYLUS])?
        .build("reMarkable Pen")?;

    Ok(device)
}

fn resolve_pen_inputs_maps(config: &Config) -> Vec<Box<dyn PenInputMap>> {
    let mut maps: Vec<Box<dyn PenInputMap>> = Vec::new();

    let size_data = config.aspect_ratio
        .map(SizeData::AspectRatio)
        .or(config.resolution
            .map(SizeData::Resolution));
    if let Some(fit_map) = fit::resolve(config.fit, size_data) {
        maps.push(Box::new(fit_map));
    }

    // Add more strategies (PenInputMap) when needed.
    maps
}

pub fn run_pen(
    config: &Config,
    device_profile: &DeviceProfile,
    palm: Option<SharedPalmState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (_cleanup, mut channel) =
        ssh::open_input_stream(&config.pen_device, config, config.grab_input)?;

    // Build the fixed-order pen-coordinate pipeline. Displays are enumerated
    // once here and shared by every stage. A screen-selection stage will be
    // pushed here too once fit and multi-screen are merged.
    let orientation = config.orientation;
    let (seed_x_max, seed_y_max) =
        orientation.pen_output_dimensions(device_profile.pen_x_max, device_profile.pen_y_max);

    let pipeline = PenInputPipeline::new(seed_x_max, seed_y_max, resolve_pen_inputs_maps(config));
    log::info!("Pen input pipeline: {}", pipeline.describe());

    log::info!("Creating pen uinput device");
    let uinput = create_pen_device(device_profile, &pipeline)?;

    if let Ok(name) = uinput.sysname() {
        log::info!("Pen device ready: /sys/devices/virtual/input/{}", name.to_string_lossy());
    }

    std::thread::sleep(std::time::Duration::from_secs(1));
    log::info!("Pen forwarding started");

    let btn_touch_code = Key::BTN_TOUCH.raw();
    let mut buf = vec![0u8; device_profile.input_event_size];
    let mut batch: Vec<InputEvent> = Vec::with_capacity(32);
    let mut touch_down = false;
    // evdev only reports changed values, so pressure persists across frames
    let mut pressure: i32 = 0;
    let mut frame_count: u64 = 0;

    // For collecting X/Y/tilt values within a frame
    let mut pending_x: Option<i32> = None;
    let mut pending_y: Option<i32> = None;
    let mut pending_tilt_x: Option<i32> = None;
    let mut pending_tilt_y: Option<i32> = None;

    loop {
        channel.read_exact(&mut buf)?;

        let Some(ev) = parse_input_event(&buf) else {
            continue;
        };

        let ty = ev.event_type().raw();
        let code = ev.raw_code();
        let value = ev.raw_value();

        // BTN_TOUCH is derived from pressure below; drop the device's own
        if ty == EV_KEY && code == btn_touch_code {
            continue;
        }

        if ty == EV_ABS && code == ABS_DISTANCE && device_profile.pen_distance_inverted {
            let distance = device_profile.pen_distance_max - value.clamp(0, device_profile.pen_distance_max);
            batch.push(InputEvent::new(ev.event_type(), code, distance));
            continue;
        }

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
            let (mapped_x, mapped_y) = pipeline.map(out_x, out_y);
            log::debug!(
                "pen raw=({x},{y}) oriented=({out_x},{out_y}) mapped=({mapped_x},{mapped_y})"
            );
            let (out_x, out_y) = (mapped_x, mapped_y);
            batch.insert(0, InputEvent::new(evdevil::event::EventType::from_raw(EV_ABS), Abs::X.raw(), out_x));
            batch.insert(1, InputEvent::new(evdevil::event::EventType::from_raw(EV_ABS), Abs::Y.raw(), out_y));
        }

        // Transform and emit tilt events
        if let (Some(tx), Some(ty)) = (pending_tilt_x.take(), pending_tilt_y.take()) {
            let (out_tx, out_ty) = orientation.transform_tilt(tx, ty);
            batch.insert(0, InputEvent::new(evdevil::event::EventType::from_raw(EV_ABS), Abs::TILT_X.raw(), out_tx));
            batch.insert(1, InputEvent::new(evdevil::event::EventType::from_raw(EV_ABS), Abs::TILT_Y.raw(), out_ty));
        }

        if let Some(p) = batch
            .iter()
            .rfind(|e| e.event_type().raw() == EV_ABS && e.raw_code() == ABS_PRESSURE)
        {
            pressure = p.raw_value();
        }

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
