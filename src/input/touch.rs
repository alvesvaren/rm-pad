use std::io::Read;
use std::time::Duration;

use evdevil::event::{Abs, Key, KeyEvent, KeyState};
use evdevil::uinput::{AbsSetup, UinputDevice};
use evdevil::{AbsInfo, InputProp, Slot};

use crate::config::Config;
use crate::device::DeviceProfile;
use crate::orientation::Orientation;
use crate::palm::SharedPalmState;
use crate::ssh;

use super::event::{
    parse_input_event, ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID,
    EV_ABS, EV_KEY, EV_SYN, INPUT_EVENT_SIZE, SYN_REPORT,
};

const MT_SLOTS: usize = 16;

struct SlotState {
    x: [Option<i32>; MT_SLOTS],
    y: [Option<i32>; MT_SLOTS],
    last_x: [Option<i32>; MT_SLOTS],
    last_y: [Option<i32>; MT_SLOTS],
    active: [bool; MT_SLOTS],
    tracking_id: [Option<i32>; MT_SLOTS],
}

impl SlotState {
    fn new() -> Self {
        Self {
            x: [None; MT_SLOTS],
            y: [None; MT_SLOTS],
            last_x: [None; MT_SLOTS],
            last_y: [None; MT_SLOTS],
            active: [false; MT_SLOTS],
            tracking_id: [None; MT_SLOTS],
        }
    }

    fn clear_slot(&mut self, slot: usize) {
        self.x[slot] = None;
        self.y[slot] = None;
        self.last_x[slot] = None;
        self.last_y[slot] = None;
    }

    fn active_count(&self) -> i32 {
        self.active.iter().filter(|&&a| a).count() as i32
    }

    fn get_position(&self, slot: usize) -> Option<(i32, i32)> {
        match (self.x[slot], self.y[slot]) {
            (Some(x), Some(y)) => Some((x, y)),
            _ => match (self.last_x[slot], self.last_y[slot]) {
                (Some(x), Some(y)) => Some((x, y)),
                _ => None,
            },
        }
    }

    fn get_primary_position(&self, device: &DeviceProfile, orientation: Orientation) -> Option<(i32, i32)> {
        (0..MT_SLOTS)
            .find(|&s| self.active[s])
            .and_then(|s| self.x[s].zip(self.y[s]))
            .map(|(ax, ay)| {
                orientation.transform_touch(
                    ax.clamp(0, device.touch_x_max),
                    ay.clamp(0, device.touch_y_max),
                    device.touch_x_max,
                    device.touch_y_max,
                )
            })
    }
}

struct FrameState {
    current_slot: usize,
    contact_count: i32,
    pending_positions: Vec<(i32, i32)>,
}

impl FrameState {
    fn new() -> Self {
        Self {
            current_slot: 0,
            contact_count: 0,
            pending_positions: Vec::with_capacity(MT_SLOTS),
        }
    }
}

fn create_touchpad_device(device: &DeviceProfile, orientation: Orientation) -> Result<UinputDevice, Box<dyn std::error::Error + Send + Sync>> {
    let (out_x_max, out_y_max) = orientation.touch_output_dimensions(device.touch_x_max, device.touch_y_max);
    let resolution = device.touch_resolution;

    let axes = [
        AbsSetup::new(Abs::X, AbsInfo::new(0, out_x_max).with_resolution(resolution)),
        AbsSetup::new(Abs::Y, AbsInfo::new(0, out_y_max).with_resolution(resolution)),
        AbsSetup::new(Abs::MT_SLOT, AbsInfo::new(0, (MT_SLOTS - 1) as i32)),
        AbsSetup::new(Abs::MT_TRACKING_ID, AbsInfo::new(-1, i32::MAX)),
        AbsSetup::new(Abs::MT_POSITION_X, AbsInfo::new(0, out_x_max).with_resolution(resolution)),
        AbsSetup::new(Abs::MT_POSITION_Y, AbsInfo::new(0, out_y_max).with_resolution(resolution)),
    ];

    let device = UinputDevice::builder()?
        .with_props([InputProp::POINTER, InputProp::BUTTONPAD])?
        .with_abs_axes(axes)?
        .with_keys([
            Key::BTN_LEFT,
            Key::BTN_TOUCH,
            Key::BTN_TOOL_FINGER,
            Key::BTN_TOOL_DOUBLETAP,
            Key::BTN_TOOL_TRIPLETAP,
            Key::BTN_TOOL_QUADTAP,
        ])?
        .build("reMarkable Touch")?;

    Ok(device)
}

pub fn run_touch(
    config: &Config,
    device_profile: &DeviceProfile,
    palm: Option<SharedPalmState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (_cleanup, mut channel) =
        ssh::open_input_stream(&config.touch_device, config, config.grab_input)?;

    log::info!("Creating touch uinput device");
    let uinput = create_touchpad_device(device_profile, config.orientation)?;

    if let Ok(name) = uinput.sysname() {
        log::info!("Touch device ready: /sys/devices/virtual/input/{}", name.to_string_lossy());
    }

    std::thread::sleep(Duration::from_secs(1));
    log::info!("Touch forwarding started");

    run_event_loop(&mut channel, &uinput, device_profile, config.orientation, palm, config.palm_grace_ms)
}

fn run_event_loop(
    channel: &mut impl Read,
    uinput: &UinputDevice,
    device: &DeviceProfile,
    orientation: Orientation,
    palm: Option<SharedPalmState>,
    grace_ms: u64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut buf = [0u8; INPUT_EVENT_SIZE];
    let mut slots = SlotState::new();
    let mut frame = FrameState::new();
    let mut next_tracking_id: i32 = 0;
    let mut frame_count: u64 = 0;

    loop {
        channel.read_exact(&mut buf)?;

        let Some(ev) = parse_input_event(&buf) else {
            continue;
        };

        let ty = ev.event_type().raw();
        let code = ev.raw_code();
        let value = ev.raw_value();

        if ty == EV_KEY {
            continue;
        }

        if ty == EV_ABS {
            process_abs_event(&mut slots, &mut frame, code, value);
        }

        if ty != EV_SYN || code != SYN_REPORT {
            continue;
        }

        resolve_pending_positions(&mut slots, &frame);
        frame.pending_positions.clear();

        let contact_count = slots.active_count();

        if should_suppress_palm(&palm, grace_ms) {
            emit_palm_suppression(uinput, &mut slots)?;
            log_frame_progress(&mut frame_count, 0, true);
            continue;
        }

        emit_touch_frame(uinput, &mut slots, &mut next_tracking_id, device, orientation)?;
        log_frame_progress(&mut frame_count, contact_count, false);
    }
}

fn process_abs_event(slots: &mut SlotState, frame: &mut FrameState, code: u16, value: i32) {
    match code {
        ABS_MT_SLOT => {
            frame.current_slot = (value.max(0) as usize).min(MT_SLOTS - 1);
        }
        ABS_MT_TRACKING_ID => {
            let slot = frame.current_slot;
            if value >= 0 {
                if !slots.active[slot] {
                    frame.contact_count += 1;
                }
                slots.active[slot] = true;
            } else {
                if slots.active[slot] {
                    frame.contact_count = frame.contact_count.saturating_sub(1);
                }
                slots.active[slot] = false;
                slots.clear_slot(slot);
            }
        }
        ABS_MT_POSITION_X => {
            let slot = frame.current_slot;
            slots.x[slot] = Some(value);
            activate_slot_if_needed(slots, frame, slot);
        }
        ABS_MT_POSITION_Y => {
            let slot = frame.current_slot;
            slots.y[slot] = Some(value);
            activate_slot_if_needed(slots, frame, slot);

            if let Some(x) = slots.x[slot] {
                frame.pending_positions.push((x, value));
            }
        }
        _ => {}
    }
}

fn activate_slot_if_needed(slots: &mut SlotState, frame: &mut FrameState, slot: usize) {
    if slots.active[slot] {
        return;
    }
    slots.active[slot] = true;
    frame.contact_count += 1;
}

fn resolve_pending_positions(slots: &mut SlotState, frame: &FrameState) {
    let active_slots: Vec<usize> = (0..MT_SLOTS).filter(|&s| slots.active[s]).collect();
    let contact_count = slots.active_count() as usize;

    if contact_count == 0 {
        return;
    }
    if active_slots.len() != contact_count {
        return;
    }
    if frame.pending_positions.len() != contact_count {
        return;
    }

    for (i, &slot) in active_slots.iter().enumerate() {
        if let Some(&(x, y)) = frame.pending_positions.get(i) {
            slots.x[slot] = Some(x);
            slots.y[slot] = Some(y);
        }
    }
}

fn should_suppress_palm(palm: &Option<SharedPalmState>, grace_ms: u64) -> bool {
    let Some(palm_state) = palm else { return false };
    let Ok(state) = palm_state.lock() else { return false };

    if state.pen_down {
        return true;
    }

    state
        .last_pen_up
        .map(|t| t.elapsed().as_millis() < grace_ms as u128)
        .unwrap_or(false)
}

fn emit_palm_suppression(
    uinput: &UinputDevice,
    slots: &mut SlotState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut writer = uinput.writer();

    for slot in 0..MT_SLOTS {
        if slots.tracking_id[slot].is_none() {
            continue;
        }

        let slot_writer = writer.slot(Slot::from(slot as u16))?;
        writer = slot_writer
            .write(&[evdevil::event::AbsEvent::new(Abs::MT_TRACKING_ID, -1).into()])?
            .finish_slot()?;
        slots.tracking_id[slot] = None;
    }

    let key_events = release_all_tool_keys();
    writer = writer.write(&key_events)?;
    writer.finish()?;

    Ok(())
}

fn emit_touch_frame(
    uinput: &UinputDevice,
    slots: &mut SlotState,
    next_tracking_id: &mut i32,
    device: &DeviceProfile,
    orientation: Orientation,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut writer = uinput.writer();
    let contact_count = slots.active_count();
    let (out_x_max, out_y_max) = orientation.touch_output_dimensions(device.touch_x_max, device.touch_y_max);

    for slot in 0..MT_SLOTS {
        if slots.active[slot] {
            let is_new = slots.tracking_id[slot].is_none();
            if is_new {
                *next_tracking_id = next_tracking_id.wrapping_add(1);
                slots.tracking_id[slot] = Some(*next_tracking_id);
            }

            let Some((ax, ay)) = slots.get_position(slot) else {
                continue;
            };

            let (out_x, out_y) = orientation.transform_touch(
                ax.clamp(0, device.touch_x_max),
                ay.clamp(0, device.touch_y_max),
                device.touch_x_max,
                device.touch_y_max,
            );
            let out_x = out_x.clamp(0, out_x_max);
            let out_y = out_y.clamp(0, out_y_max);
            slots.last_x[slot] = Some(ax);
            slots.last_y[slot] = Some(ay);

            let slot_writer = writer.slot(Slot::from(slot as u16))?;
            if is_new {
                let id = slots.tracking_id[slot].unwrap();
                writer = slot_writer
                    .write(&[
                        evdevil::event::AbsEvent::new(Abs::MT_TRACKING_ID, id).into(),
                        evdevil::event::AbsEvent::new(Abs::MT_POSITION_X, out_x).into(),
                        evdevil::event::AbsEvent::new(Abs::MT_POSITION_Y, out_y).into(),
                    ])?
                    .finish_slot()?;
            } else {
                writer = slot_writer
                    .write(&[
                        evdevil::event::AbsEvent::new(Abs::MT_POSITION_X, out_x).into(),
                        evdevil::event::AbsEvent::new(Abs::MT_POSITION_Y, out_y).into(),
                    ])?
                    .finish_slot()?;
            }
        } else if slots.tracking_id[slot].is_some() {
            let slot_writer = writer.slot(Slot::from(slot as u16))?;
            writer = slot_writer
                .write(&[evdevil::event::AbsEvent::new(Abs::MT_TRACKING_ID, -1).into()])?
                .finish_slot()?;
            slots.tracking_id[slot] = None;
        }
    }

    if let Some((out_x, out_y)) = slots.get_primary_position(device, orientation) {
        writer = writer.write(&[
            evdevil::event::AbsEvent::new(Abs::X, out_x).into(),
            evdevil::event::AbsEvent::new(Abs::Y, out_y).into(),
        ])?;
    }

    let key_events = build_tool_key_events(contact_count);
    writer = writer.write(&key_events)?;
    writer.finish()?;

    Ok(())
}

fn build_tool_key_events(contact_count: i32) -> Vec<evdevil::event::InputEvent> {
    let finger_down = contact_count > 0;

    let tool_key = match contact_count {
        0 => None,
        1 => Some(Key::BTN_TOOL_FINGER),
        2 => Some(Key::BTN_TOOL_DOUBLETAP),
        3 => Some(Key::BTN_TOOL_TRIPLETAP),
        _ => Some(Key::BTN_TOOL_QUADTAP),
    };

    let mut events = vec![KeyEvent::new(
        Key::BTN_TOUCH,
        if finger_down { KeyState::PRESSED } else { KeyState::RELEASED },
    )
    .into()];

    for key in [
        Key::BTN_TOOL_FINGER,
        Key::BTN_TOOL_DOUBLETAP,
        Key::BTN_TOOL_TRIPLETAP,
        Key::BTN_TOOL_QUADTAP,
    ] {
        let state = if Some(key) == tool_key {
            KeyState::PRESSED
        } else {
            KeyState::RELEASED
        };
        events.push(KeyEvent::new(key, state).into());
    }

    events
}

fn release_all_tool_keys() -> Vec<evdevil::event::InputEvent> {
    [
        Key::BTN_TOUCH,
        Key::BTN_TOOL_FINGER,
        Key::BTN_TOOL_DOUBLETAP,
        Key::BTN_TOOL_TRIPLETAP,
        Key::BTN_TOOL_QUADTAP,
    ]
    .into_iter()
    .map(|key| KeyEvent::new(key, KeyState::RELEASED).into())
    .collect()
}

fn log_frame_progress(frame_count: &mut u64, contact_count: i32, suppressed: bool) {
    if *frame_count == 0 {
        log::info!("Touch events flowing");
    }
    *frame_count += 1;

    if (*frame_count).is_multiple_of(500) {
        if suppressed {
            log::debug!("Touch frames: {} (palm suppressed)", frame_count);
        } else {
            log::debug!("Touch frames: {}, contacts: {}", frame_count, contact_count);
        }
    }
}
