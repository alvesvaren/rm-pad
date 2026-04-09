//! Wire format for compressed 4-bit grayscale screen updates.

/// Header sent before each LZ4 payload (little-endian on wire, 13 bytes).
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdateHeader {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub waveform: u8,
    pub payload_size: u32,
}

pub const HEADER_SIZE: usize = 13;

impl UpdateHeader {
    pub fn to_bytes(self) -> [u8; HEADER_SIZE] {
        let mut b = [0u8; HEADER_SIZE];
        b[0..2].copy_from_slice(&self.x.to_le_bytes());
        b[2..4].copy_from_slice(&self.y.to_le_bytes());
        b[4..6].copy_from_slice(&self.width.to_le_bytes());
        b[6..8].copy_from_slice(&self.height.to_le_bytes());
        b[8] = self.waveform;
        b[9..13].copy_from_slice(&self.payload_size.to_le_bytes());
        b
    }

    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_SIZE {
            return None;
        }
        Some(Self {
            x: u16::from_le_bytes(buf[0..2].try_into().ok()?),
            y: u16::from_le_bytes(buf[2..4].try_into().ok()?),
            width: u16::from_le_bytes(buf[4..6].try_into().ok()?),
            height: u16::from_le_bytes(buf[6..8].try_into().ok()?),
            waveform: buf[8],
            payload_size: u32::from_le_bytes(buf[9..13].try_into().ok()?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = UpdateHeader {
            x: 10,
            y: 20,
            width: 100,
            height: 50,
            waveform: 1,
            payload_size: 1234,
        };
        let b = h.to_bytes();
        let d = UpdateHeader::from_bytes(&b).unwrap();
        assert_eq!(d, h);
    }
}
