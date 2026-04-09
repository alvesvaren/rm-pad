use evdevil::event::{EventType, InputEvent};

pub const INPUT_EVENT_SIZE_32: usize = 16;
pub const INPUT_EVENT_SIZE_64: usize = 24;

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;
pub const SYN_REPORT: u16 = 0;

pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
pub const ABS_PRESSURE: u16 = 0x18;

/// Parse a Linux input_event from raw bytes (32-bit or 64-bit format).
pub fn parse_input_event(buf: &[u8]) -> Option<InputEvent> {
    match buf.len() {
        INPUT_EVENT_SIZE_32 => parse_input_event_32(buf),
        INPUT_EVENT_SIZE_64 => parse_input_event_64(buf),
        len if len >= INPUT_EVENT_SIZE_64 => parse_input_event_64(buf),
        len if len >= INPUT_EVENT_SIZE_32 => parse_input_event_32(buf),
        _ => None,
    }
}

fn parse_input_event_32(buf: &[u8]) -> Option<InputEvent> {
    let ty = u16::from_le_bytes([buf[8], buf[9]]);
    let code = u16::from_le_bytes([buf[10], buf[11]]);
    let value = i32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);

    Some(InputEvent::new(EventType::from_raw(ty), code, value))
}

fn parse_input_event_64(buf: &[u8]) -> Option<InputEvent> {
    let ty = u16::from_le_bytes([buf[16], buf[17]]);
    let code = u16::from_le_bytes([buf[18], buf[19]]);
    let value = i32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);

    Some(InputEvent::new(EventType::from_raw(ty), code, value))
}

pub fn key_event(code: u16, value: i32) -> InputEvent {
    InputEvent::new(EventType::from_raw(EV_KEY), code, value)
}
