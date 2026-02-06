//! Forward reMarkable touch as a libinput MT touchpad (absolute multi-touch).
//! Axes swapped: device X -> output Y, device Y -> output X.
//! Cursor moves with finger like a laptop touchpad (libinput turns absolute positions into pointer motion).

use std::io::Read;
use std::time::Duration;

use evdevil::event::{Abs, Key, KeyEvent, KeyState};
use evdevil::uinput::{AbsSetup, UinputDevice};
use evdevil::{AbsInfo, InputProp, Slot};

use std::sync::Arc;

use crate::config::Config;
use crate::event::{
    parse_input_event, ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID,
    EV_ABS, EV_KEY, EV_SYN, INPUT_EVENT_SIZE, SYN_REPORT,
};
use crate::palm::SharedPalmState;
use crate::ssh;

const MT_SLOTS: usize = 16;

// reMarkable 2: 1872×1404 display, ~210×158 mm → ~8.9 units/mm. Use 9 for resolution.
const TOUCH_X_MAX: i32 = 1872;
const TOUCH_Y_MAX: i32 = 1404;
const TOUCH_RESOLUTION: i32 = 9; // units/mm (libinput uses for size: range/resolution = mm)

fn create_touchpad_device() -> Result<UinputDevice, Box<dyn std::error::Error + Send + Sync>> {
    // libinput touchpad sanity checks require:
    // - ABS_X, ABS_Y (legacy) and ABS_MT_POSITION_X/Y with matching ranges and resolution (both axes).
    // - Axis ranges must match the values we emit. We swap device X/Y when writing (device X→output Y,
    //   device Y→output X), so output X range is 0..TOUCH_Y_MAX and output Y range is 0..TOUCH_X_MAX.
    // - INPUT_PROP_BUTTONPAD: clickpad (no separate physical buttons).
    // - BTN_TOOL_FINGER: required by libinput tp_pass_sanity_check() so the device is accepted.
    let out_x_max = TOUCH_Y_MAX; // logical X = device Y
    let out_y_max = TOUCH_X_MAX; // logical Y = device X
    let axes: [AbsSetup; 6] = [
        AbsSetup::new(
            Abs::X,
            AbsInfo::new(0, out_x_max).with_resolution(TOUCH_RESOLUTION),
        ),
        AbsSetup::new(
            Abs::Y,
            AbsInfo::new(0, out_y_max).with_resolution(TOUCH_RESOLUTION),
        ),
        AbsSetup::new(Abs::MT_SLOT, AbsInfo::new(0, (MT_SLOTS - 1) as i32)),
        AbsSetup::new(Abs::MT_TRACKING_ID, AbsInfo::new(-1, i32::MAX)),
        AbsSetup::new(
            Abs::MT_POSITION_X,
            AbsInfo::new(0, out_x_max).with_resolution(TOUCH_RESOLUTION),
        ),
        AbsSetup::new(
            Abs::MT_POSITION_Y,
            AbsInfo::new(0, out_y_max).with_resolution(TOUCH_RESOLUTION),
        ),
    ];
    let device = UinputDevice::builder()?
        .with_props([InputProp::POINTER, InputProp::BUTTONPAD])?
        .with_abs_axes(axes)?
        .with_keys([
            evdevil::event::Key::BTN_LEFT,
            evdevil::event::Key::BTN_TOUCH,
            evdevil::event::Key::BTN_TOOL_FINGER,
            evdevil::event::Key::BTN_TOOL_DOUBLETAP,
            evdevil::event::Key::BTN_TOOL_TRIPLETAP,
            evdevil::event::Key::BTN_TOOL_QUADTAP,
        ])?
        .build("reMarkable Touch")?;
    Ok(device)
}

pub fn run(
    config: &Config,
    palm: Option<SharedPalmState>,
    grace_ms: u64,
    pause_refcount: Option<Arc<std::sync::atomic::AtomicUsize>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let use_grab = !config.no_grab;
    let (_sess, mut channel, _pause_guard) =
        ssh::open_input_stream(&config.touch_device, config, use_grab, pause_refcount)?;
    run_mt(&mut channel, palm, grace_ms)
}

fn run_mt(
    channel: &mut impl Read,
    palm: Option<SharedPalmState>,
    grace_ms: u64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    log::info!("[touch] creating uinput MT touchpad (reMarkable 2 size)…");
    let device = create_touchpad_device()?;
    if let Ok(name) = device.sysname() {
        log::info!(
            "[touch] uinput device created: /sys/devices/virtual/input/{}",
            name.to_string_lossy()
        );
    }
    log::info!("[touch] waiting 1s for udev/libinput to attach…");
    std::thread::sleep(Duration::from_secs(1));
    log::info!("[touch] absolute MT touchpad (pointer + gestures, axes swapped).");

    let mut buf = [0u8; INPUT_EVENT_SIZE];
    let mut count: u64 = 0;
    let mut slot_x: [Option<i32>; MT_SLOTS] = [None; MT_SLOTS];
    let mut slot_y: [Option<i32>; MT_SLOTS] = [None; MT_SLOTS];
    // Last position emitted per slot so we can resend when device doesn't repeat position this frame (multi-finger).
    let mut slot_last_x: [Option<i32>; MT_SLOTS] = [None; MT_SLOTS];
    let mut slot_last_y: [Option<i32>; MT_SLOTS] = [None; MT_SLOTS];
    let mut frame_slot_active = [false; MT_SLOTS];
    #[allow(unused_assignments)]
    let mut slot_active = frame_slot_active;
    let mut slot_tracking_id: [Option<i32>; MT_SLOTS] = [None; MT_SLOTS];
    let mut next_tracking_id: i32 = 0;
    let mut frame_contact_count = 0i32;
    let mut frame_current_slot: usize = 0;
    // Positions received this frame in order (handles devices that send all slots then all X,Y pairs).
    let mut pending_positions: Vec<(i32, i32)> = Vec::with_capacity(MT_SLOTS);

    log::info!("[touch] waiting for events (touch the reMarkable screen)…");

    loop {
        if let Err(e) = channel.read_exact(&mut buf) {
            return Err(e.into());
        }
        if let Some(ev) = parse_input_event(&buf) {
            let ty = ev.event_type().raw();
            let code = ev.raw_code();
            let value = ev.raw_value();
            if ty == EV_KEY {
                continue;
            }
            if ty == EV_ABS {
                if code == ABS_MT_SLOT {
                    frame_current_slot = value.max(0) as usize;
                    if frame_current_slot >= MT_SLOTS {
                        frame_current_slot = MT_SLOTS - 1;
                    }
                } else if code == ABS_MT_TRACKING_ID {
                    if value >= 0 {
                        if !frame_slot_active[frame_current_slot] {
                            frame_contact_count += 1;
                        }
                        frame_slot_active[frame_current_slot] = true;
                    } else {
                        if frame_slot_active[frame_current_slot] {
                            frame_contact_count = frame_contact_count.saturating_sub(1);
                        }
                        frame_slot_active[frame_current_slot] = false;
                        slot_x[frame_current_slot] = None;
                        slot_y[frame_current_slot] = None;
                        slot_last_x[frame_current_slot] = None;
                        slot_last_y[frame_current_slot] = None;
                    }
                } else if code == ABS_MT_POSITION_X {
                    slot_x[frame_current_slot] = Some(value);
                    // Some devices send position without TRACKING_ID for a new finger; treat as active.
                    if !frame_slot_active[frame_current_slot] {
                        frame_slot_active[frame_current_slot] = true;
                        frame_contact_count += 1;
                    }
                } else if code == ABS_MT_POSITION_Y {
                    slot_y[frame_current_slot] = Some(value);
                    if !frame_slot_active[frame_current_slot] {
                        frame_slot_active[frame_current_slot] = true;
                        frame_contact_count += 1;
                    }
                    // Collect (x,y) in order for reassignment if device sent all IDs then all positions.
                    if let Some(x) = slot_x[frame_current_slot] {
                        pending_positions.push((x, value));
                    }
                }
            }
            if ty == EV_SYN && code == SYN_REPORT {
                let contact_count = frame_contact_count;
                slot_active = frame_slot_active;
                // If device sent positions in slot order without SLOT before each pair, assign by order.
                let active_slots: Vec<usize> = (0..MT_SLOTS).filter(|&s| slot_active[s]).collect();
                if contact_count > 0
                    && active_slots.len() == contact_count as usize
                    && pending_positions.len() == contact_count as usize
                {
                    for (i, &s) in active_slots.iter().enumerate() {
                        if let Some(&(ax, ay)) = pending_positions.get(i) {
                            slot_x[s] = Some(ax);
                            slot_y[s] = Some(ay);
                        }
                    }
                }
                pending_positions.clear();
                let suppress = if let Some(ref palm_state) = palm {
                    palm_state
                        .lock()
                        .map(|state| {
                            state.pen_down
                                || state
                                    .last_pen_up
                                    .map(|t| t.elapsed().as_millis() < grace_ms as u128)
                                    .unwrap_or(false)
                        })
                        .unwrap_or(false)
                } else {
                    false
                };
                if suppress {
                    let mut w = device.writer();
                    for s in 0..MT_SLOTS {
                        if slot_tracking_id[s].is_some() {
                            let slot_w = w.slot(Slot::from(s as u16))?;
                            w = slot_w
                                .write(&[evdevil::event::AbsEvent::new(Abs::MT_TRACKING_ID, -1).into()])?
                                .finish_slot()?;
                            slot_tracking_id[s] = None;
                        }
                    }
                    let key_events: Vec<evdevil::event::InputEvent> = [
                        KeyEvent::new(Key::BTN_TOUCH, KeyState::RELEASED),
                        KeyEvent::new(Key::BTN_TOOL_FINGER, KeyState::RELEASED),
                        KeyEvent::new(Key::BTN_TOOL_DOUBLETAP, KeyState::RELEASED),
                        KeyEvent::new(Key::BTN_TOOL_TRIPLETAP, KeyState::RELEASED),
                        KeyEvent::new(Key::BTN_TOOL_QUADTAP, KeyState::RELEASED),
                    ]
                    .into_iter()
                    .map(Into::into)
                    .collect();
                    w = w.write(&key_events)?;
                    w.finish()?;
                    if count == 0 {
                        log::info!("[touch] first event batch (events are flowing)");
                    }
                    frame_slot_active = slot_active;
                    count += 1;
                    log::debug!("[touch] frame #{} contacts=0 (palm suppressed)", count);
                    continue;
                }
                let mut w = device.writer();
                let finger_down = contact_count > 0;
                for s in 0..MT_SLOTS {
                    let active = slot_active[s];
                    let (x, y) = (slot_x[s], slot_y[s]);
                    if active {
                        let is_new = slot_tracking_id[s].is_none();
                        if is_new {
                            next_tracking_id = next_tracking_id.wrapping_add(1);
                            slot_tracking_id[s] = Some(next_tracking_id);
                        }
                        // Use current frame position or last-known so every active finger is emitted every frame (required for gestures).
                        let (ax, ay) = match (x, y) {
                            (Some(a), Some(b)) => (a, b),
                            _ => match (slot_last_x[s], slot_last_y[s]) {
                                (Some(a), Some(b)) => (a, b),
                                _ => continue, // no position yet for this slot, skip
                            },
                        };
                        let out_x = ay.clamp(0, TOUCH_Y_MAX);
                        let out_y = ax.clamp(0, TOUCH_X_MAX);
                        slot_last_x[s] = Some(ax);
                        slot_last_y[s] = Some(ay);
                        let slot_w = w.slot(Slot::from(s as u16))?;
                        if is_new {
                            let id = slot_tracking_id[s].unwrap();
                            w = slot_w
                                .write(&[
                                    evdevil::event::AbsEvent::new(Abs::MT_TRACKING_ID, id).into(),
                                    evdevil::event::AbsEvent::new(Abs::MT_POSITION_X, out_x).into(),
                                    evdevil::event::AbsEvent::new(Abs::MT_POSITION_Y, out_y).into(),
                                ])?
                                .finish_slot()?;
                        } else {
                            w = slot_w
                                .write(&[
                                    evdevil::event::AbsEvent::new(Abs::MT_POSITION_X, out_x).into(),
                                    evdevil::event::AbsEvent::new(Abs::MT_POSITION_Y, out_y).into(),
                                ])?
                                .finish_slot()?;
                        }
                    } else if slot_tracking_id[s].is_some() {
                        let slot_w = w.slot(Slot::from(s as u16))?;
                        w = slot_w
                            .write(&[evdevil::event::AbsEvent::new(Abs::MT_TRACKING_ID, -1).into()])?
                            .finish_slot()?;
                        slot_tracking_id[s] = None;
                    }
                }
                if let Some((out_x, out_y)) = (0..MT_SLOTS)
                    .find(|&s| slot_active[s])
                    .and_then(|s| slot_x[s].zip(slot_y[s]))
                    .map(|(ax, ay)| (ay.clamp(0, TOUCH_Y_MAX), ax.clamp(0, TOUCH_X_MAX)))
                {
                    w = w.write(&[
                        evdevil::event::AbsEvent::new(Abs::X, out_y).into(),
                        evdevil::event::AbsEvent::new(Abs::Y, out_x).into(),
                    ])?;
                }
                // Emit after slot/ABS so libinput sees positions then touch state.
                // BTN_TOOL_*: only one may be PRESSED at a time (libinput "Invalid fake finger state" else).
                // 1 finger = FINGER, 2 = DOUBLETAP, 3 = TRIPLETAP, 4+ = QUADTAP. BTN_TOUCH = any finger down.
                let tool_key = match contact_count {
                    0 => None,
                    1 => Some(Key::BTN_TOOL_FINGER),
                    2 => Some(Key::BTN_TOOL_DOUBLETAP),
                    3 => Some(Key::BTN_TOOL_TRIPLETAP),
                    _ => Some(Key::BTN_TOOL_QUADTAP),
                };
                let mut key_events: Vec<evdevil::event::InputEvent> = vec![
                    KeyEvent::new(
                        Key::BTN_TOUCH,
                        if finger_down {
                            KeyState::PRESSED
                        } else {
                            KeyState::RELEASED
                        },
                    )
                    .into(),
                ];
                for key in [
                    Key::BTN_TOOL_FINGER,
                    Key::BTN_TOOL_DOUBLETAP,
                    Key::BTN_TOOL_TRIPLETAP,
                    Key::BTN_TOOL_QUADTAP,
                ] {
                    key_events.push(
                        KeyEvent::new(key, if Some(key) == tool_key { KeyState::PRESSED } else { KeyState::RELEASED }).into(),
                    );
                }
                w = w.write(&key_events)?;
                w.finish()?;
                if count == 0 {
                    log::info!("[touch] first event batch (events are flowing)");
                }
                frame_slot_active = slot_active;
                count += 1;
                log::debug!("[touch] frame #{} contacts={}", count, contact_count);
            }
        }
    }
}
