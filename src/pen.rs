//! Forward reMarkable pen input to a uinput pen device.

use std::io::Read;
use std::path::Path;

use evdevil::event::{Abs, InputEvent, Key};
use evdevil::uinput::{AbsSetup, UinputDevice};
use evdevil::AbsInfo;
use evdevil::Bus;
use evdevil::InputId;
use evdevil::InputProp;

use crate::config::PEN_DEVICE;
use crate::event::{
    key_event, parse_input_event, ABS_PRESSURE, EV_ABS, EV_SYN, INPUT_EVENT_SIZE, SYN_REPORT,
};
use crate::ssh;

// reMarkable pen axis ranges from dumps: X/Y up to ~16600, tilt ±~7k, pressure 0..4k, distance 0..255
fn create_pen_device() -> Result<UinputDevice, Box<dyn std::error::Error + Send + Sync>> {
    let axes = [
        AbsSetup::new(Abs::X, AbsInfo::new(0, 20000)),
        AbsSetup::new(Abs::Y, AbsInfo::new(0, 20000)),
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

pub fn run(key_path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (_sess, mut channel) = ssh::open_input_stream(PEN_DEVICE, key_path)?;
    log::info!("[pen] creating uinput device…");
    let device = create_pen_device()?;
    if let Ok(name) = device.sysname() {
        log::info!("[pen] uinput device created: /sys/devices/virtual/input/{}", name.to_string_lossy());
    }
    log::info!("[pen] forwarding (move pen on tablet to see events)");

    let mut buf = [0u8; INPUT_EVENT_SIZE];
    let mut batch: Vec<InputEvent> = Vec::with_capacity(32);
    let mut count: u64 = 0;
    loop {
        channel.read_exact(&mut buf)?;
        if let Some(ev) = parse_input_event(&buf) {
            let ty = ev.event_type().raw();
            let code = ev.raw_code();
            batch.push(ev);
            if ty == EV_SYN && code == SYN_REPORT {
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
