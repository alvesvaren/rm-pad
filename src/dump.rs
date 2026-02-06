//! Dump raw input events from reMarkable for debugging.
//! Run: rm-mouse dump touch  (or dump pen) to stream and print events.

use std::io::Read;

use crate::config::Config;
use crate::event::{parse_input_event, INPUT_EVENT_SIZE};
use crate::ssh;

fn code_name(ty: u16, code: u16) -> String {
    if ty == 0 {
        return format!("SYN_REPORT");
    }
    if ty == 1 {
        return format!("KEY/{}", code);
    }
    if ty == 3 {
        let abs = match code {
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
        };
        return format!("ABS_{}({})", abs, code);
    }
    format!("type{} code{}", ty, code)
}

pub fn run_dump_touch(config: &Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (_sess, mut channel, _guard) =
        ssh::open_input_stream(&config.touch_device, config, false, None)?;
    eprintln!("Dumping touch events from {} (Ctrl+C to stop):\n", config.touch_device);
    let mut buf = [0u8; INPUT_EVENT_SIZE];
    let mut n = 0u64;
    loop {
        channel.read_exact(&mut buf)?;
        if let Some(ev) = parse_input_event(&buf) {
            n += 1;
            let ty = ev.event_type().raw();
            let code = ev.raw_code();
            let value = ev.raw_value();
            let name = code_name(ty, code);
            println!("{:6}  {}  value={}", n, name, value);
        }
    }
}

pub fn run_dump_pen(config: &Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (_sess, mut channel, _guard) = ssh::open_input_stream(&config.pen_device, config, false, None)?;
    eprintln!("Dumping pen events from {} (Ctrl+C to stop):\n", config.pen_device);
    let mut buf = [0u8; INPUT_EVENT_SIZE];
    let mut n = 0u64;
    loop {
        channel.read_exact(&mut buf)?;
        if let Some(ev) = parse_input_event(&buf) {
            n += 1;
            let ty = ev.event_type().raw();
            let code = ev.raw_code();
            let value = ev.raw_value();
            let name = code_name(ty, code);
            println!("{:6}  {}  value={}", n, name, value);
        }
    }
}
