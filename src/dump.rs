use std::io::Read;

use crate::config::Config;
use crate::device::DeviceProfile;
use crate::input::parse_input_event;
use crate::ssh;

pub fn run_touch(
    config: &Config,
    device: &DeviceProfile,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_dump(config, device.input_event_size, &config.touch_device, "touch")
}

pub fn run_pen(
    config: &Config,
    device: &DeviceProfile,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_dump(config, device.input_event_size, &config.pen_device, "pen")
}

fn run_dump(
    config: &Config,
    input_event_size: usize,
    device: &str,
    name: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (_cleanup, mut channel) = ssh::open_input_stream(device, config, false)?;

    eprintln!("Dumping {} events from {} (Ctrl+C to stop)\n", name, device);

    let mut buf = vec![0u8; input_event_size];
    let mut count: u64 = 0;

    loop {
        channel.read_exact(&mut buf)?;

        let Some(ev) = parse_input_event(&buf) else {
            continue;
        };

        count += 1;
        let name = format_event_code(ev.event_type().raw(), ev.raw_code());
        println!("{:6}  {}  value={}", count, name, ev.raw_value());
    }
}

fn format_event_code(ty: u16, code: u16) -> String {
    match ty {
        0 => "SYN_REPORT".into(),
        1 => format!("KEY/{}", code),
        3 => format!("ABS_{}", abs_code_name(code)),
        _ => format!("type{}/code{}", ty, code),
    }
}

fn abs_code_name(code: u16) -> &'static str {
    match code {
        0x00 => "X",
        0x01 => "Y",
        0x18 => "PRESSURE",
        0x19 => "DISTANCE",
        0x1a => "TILT_X",
        0x1b => "TILT_Y",
        0x2f => "MT_SLOT",
        0x30 => "MT_TOUCH_MAJOR",
        0x31 => "MT_TOUCH_MINOR",
        0x34 => "MT_ORIENTATION",
        0x35 => "MT_POSITION_X",
        0x36 => "MT_POSITION_Y",
        0x37 => "MT_TOOL_TYPE",
        0x39 => "MT_TRACKING_ID",
        0x3a => "MT_PRESSURE",
        _ => "?",
    }
}
