//! Wire format for compressed RGB565 screen updates.
//!
//! The host sends one or more `UpdateHeader + payload` parts per logical batch. The tablet ACKs at
//! batch granularity with `BatchAck`, which lets the host keep a few batches in flight instead of
//! blocking after every sparse tile.
//!
//! `UpdateHeader.host_unix_ms` is set by the host when the packet is finished (before TCP write).
//! With `RM_MIRROR_LATENCY_LOG=1`, the tablet logs `client_unix - host_unix` as a rough
//! end-to-end hint (clock skew affects the number).

/// `UpdateHeader.waveform` when `x,y,width,height` are in **host capture** coordinates
/// and the LZ4 payload is RGB565 at that resolution (tablet scales to FB).
pub const UPDATE_COORDS_CAPTURE: u8 = 1;

/// `UpdateHeader.waveform` when `x,y,width,height` are in **tablet framebuffer**
/// coordinates and the payload is RGB565 at that resolution (host pre-scaled).
pub const UPDATE_COORDS_FRAMEBUFFER: u8 = 2;

/// Header sent before each LZ4 payload (little-endian on wire, 29 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdateHeader {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub waveform: u8,
    pub payload_size: u32,
    /// Milliseconds since UNIX epoch on the host when this packet was finalized (see `unix_time_millis`).
    pub host_unix_ms: u64,
    /// Logical batch/frame identifier shared by all parts that belong to the same update batch.
    pub batch_id: u32,
    /// Zero-based index of this part inside `part_count`.
    pub part_index: u16,
    /// Number of parts in this batch (1 for a single merged region).
    pub part_count: u16,
}

pub const HEADER_SIZE: usize = 29;
pub const ACK_OK: u8 = 0x06;
pub const ACK_SIZE: usize = 5;

/// Batch-level ACK returned by the tablet after it finishes handling a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchAck {
    pub batch_id: u32,
    pub status: u8,
}

#[inline]
pub fn unix_time_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl UpdateHeader {
    pub fn to_bytes(self) -> [u8; HEADER_SIZE] {
        let mut b = [0u8; HEADER_SIZE];
        b[0..2].copy_from_slice(&self.x.to_le_bytes());
        b[2..4].copy_from_slice(&self.y.to_le_bytes());
        b[4..6].copy_from_slice(&self.width.to_le_bytes());
        b[6..8].copy_from_slice(&self.height.to_le_bytes());
        b[8] = self.waveform;
        b[9..13].copy_from_slice(&self.payload_size.to_le_bytes());
        b[13..21].copy_from_slice(&self.host_unix_ms.to_le_bytes());
        b[21..25].copy_from_slice(&self.batch_id.to_le_bytes());
        b[25..27].copy_from_slice(&self.part_index.to_le_bytes());
        b[27..29].copy_from_slice(&self.part_count.to_le_bytes());
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
            host_unix_ms: u64::from_le_bytes(buf[13..21].try_into().ok()?),
            batch_id: u32::from_le_bytes(buf[21..25].try_into().ok()?),
            part_index: u16::from_le_bytes(buf[25..27].try_into().ok()?),
            part_count: u16::from_le_bytes(buf[27..29].try_into().ok()?),
        })
    }

    #[inline]
    pub fn is_last_part(&self) -> bool {
        self.part_index.saturating_add(1) >= self.part_count
    }
}

impl BatchAck {
    pub fn ok(batch_id: u32) -> Self {
        Self {
            batch_id,
            status: ACK_OK,
        }
    }

    pub fn to_bytes(self) -> [u8; ACK_SIZE] {
        let mut b = [0u8; ACK_SIZE];
        b[0..4].copy_from_slice(&self.batch_id.to_le_bytes());
        b[4] = self.status;
        b
    }

    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < ACK_SIZE {
            return None;
        }
        Some(Self {
            batch_id: u32::from_le_bytes(buf[0..4].try_into().ok()?),
            status: buf[4],
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
            host_unix_ms: 0,
            batch_id: 7,
            part_index: 2,
            part_count: 4,
        };
        let b = h.to_bytes();
        let d = UpdateHeader::from_bytes(&b).unwrap();
        assert_eq!(d, h);
    }

    #[test]
    fn batch_ack_roundtrip() {
        let ack = BatchAck::ok(42);
        let bytes = ack.to_bytes();
        let decoded = BatchAck::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, ack);
    }
}
