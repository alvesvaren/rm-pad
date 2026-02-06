//! Forward reMarkable touch input to a uinput touchpad.
//! Derives BTN_TOUCH/BTN_LEFT from ABS_MT_TRACKING_ID (Parade TrueTouch / cyttsp5:
//! TRACKING_ID >= 0 = contact, TRACKING_ID == -1 = release; works with or without ABS_MT_SLOT).

use std::io::Read;
use std::path::Path;

use evdevil::event::{Abs, InputEvent, Key};
use evdevil::uinput::{AbsSetup, UinputDevice};
use evdevil::AbsInfo;
use evdevil::InputProp;

use crate::config::TOUCH_DEVICE;
use crate::event::{
    key_event, parse_input_event, ABS_MT_SLOT, ABS_MT_TRACKING_ID, EV_ABS, EV_KEY, EV_SYN,
    INPUT_EVENT_SIZE, SYN_REPORT,
};
use crate::ssh;

fn create_touchpad_device() -> Result<UinputDevice, Box<dyn std::error::Error + Send + Sync>> {
    let axes = [
        AbsSetup::new(Abs::X, AbsInfo::new(0, 4095)),
        AbsSetup::new(Abs::Y, AbsInfo::new(0, 4095)),
        AbsSetup::new(Abs::MT_POSITION_X, AbsInfo::new(0, 4095)),
        AbsSetup::new(Abs::MT_POSITION_Y, AbsInfo::new(0, 4095)),
        AbsSetup::new(Abs::MT_SLOT, AbsInfo::new(0, 9)),
        AbsSetup::new(Abs::MT_TRACKING_ID, AbsInfo::new(-1, 65535)),
    ];
    // INPUT_PROP_POINTER = treat as touchpad (cursor visible), not direct touchscreen.
    let device = UinputDevice::builder()?
        .with_props([InputProp::POINTER])?
        .with_abs_axes(axes)?
        .with_keys([evdevil::event::Key::BTN_LEFT, evdevil::event::Key::BTN_TOUCH])?
        .build("reMarkable Touch")?;
    Ok(device)
}

pub fn run(key_path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (_sess, mut channel) = ssh::open_input_stream(TOUCH_DEVICE, key_path)?;
    log::info!("[touch] creating uinput device…");
    let device = create_touchpad_device()?;
    if let Ok(name) = device.sysname() {
        log::info!("[touch] uinput device created: /sys/devices/virtual/input/{}", name.to_string_lossy());
    }
    log::info!("[touch] forwarding (touch screen to see events)");

    let btn_touch_code = Key::BTN_TOUCH.raw();
    let btn_left_code = Key::BTN_LEFT.raw();

    let mut buf = [0u8; INPUT_EVENT_SIZE];
    let mut batch: Vec<InputEvent> = Vec::with_capacity(32);
    let mut current_slot: usize = 0;
    let mut slot_active: [bool; 16] = [false; 16];
    let mut contact_count: i32 = 0; // for type A (no SLOT): count TRACKING_ID >=0 vs -1
    let mut touch_down = false;
    let mut count: u64 = 0;

    loop {
        channel.read_exact(&mut buf)?;
        if let Some(ev) = parse_input_event(&buf) {
            let ty = ev.event_type().raw();
            let code = ev.raw_code();
            let value = ev.raw_value();

            if ty == EV_KEY {
                continue;
            }
            if ty == EV_ABS {
                if code == ABS_MT_SLOT {
                    current_slot = value.max(0) as usize;
                    if current_slot >= 16 {
                        current_slot = 15;
                    }
                } else if code == ABS_MT_TRACKING_ID {
                    if value >= 0 {
                        contact_count += 1;
                        slot_active[current_slot] = true;
                    } else {
                        contact_count = contact_count.saturating_sub(1);
                        slot_active[current_slot] = false;
                    }
                    let any_active = contact_count > 0;
                    if any_active && !touch_down {
                        batch.push(key_event(btn_touch_code, 1));
                        touch_down = true;
                    } else if !any_active && touch_down {
                        batch.push(key_event(btn_touch_code, 0));
                        batch.push(key_event(btn_left_code, 0));
                        touch_down = false;
                    }
                }
            }
            batch.push(ev);
            if ty == EV_SYN && code == SYN_REPORT {
                if count == 0 {
                    log::info!("[touch] first event batch (events are flowing)");
                }
                count += 1;
                device.write(&batch)?;
                batch.clear();
                if count % 500 == 0 {
                    log::debug!("[touch] batches forwarded: {}", count);
                }
            }
        }
    }
}
