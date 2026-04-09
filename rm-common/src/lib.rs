//! Shared configuration, optional SSH/evgrab helpers, and screen-mirror protocol types.

pub mod config;
pub mod device;
pub mod orientation;
pub mod protocol;

#[cfg(feature = "ssh")]
pub mod grab;
#[cfg(feature = "ssh")]
pub mod screen_client;
#[cfg(feature = "ssh")]
pub mod ssh;

pub use protocol::{UpdateHeader, HEADER_SIZE};
