//! Shared configuration, optional SSH/evgrab helpers, and screen-mirror protocol types.

pub mod config;
pub mod device;
pub mod epdc_align;
pub mod orientation;
pub mod protocol;

#[cfg(feature = "ssh")]
pub mod grab;
#[cfg(feature = "ssh")]
pub mod screen_client;
#[cfg(feature = "ssh")]
pub mod ssh;

pub use epdc_align::expand_rect_to_epdc_grid;
pub use protocol::{
    UpdateHeader, HEADER_SIZE, UPDATE_COORDS_CAPTURE, UPDATE_COORDS_FRAMEBUFFER,
};
