//! The OS-independent half of the flasher.
//!
//! Nothing in this crate opens a device or knows what a drive letter, a
//! `/dev/rdisk` or a block-device ioctl is. Everything OS-specific sits behind
//! [`Platform`] and [`RawDevice`]; supporting a new OS means implementing those
//! two traits in a new `libflasher_<os>` crate and adding one `cfg` line
//! to `libflasher_platform`.

#![warn(missing_docs)]

pub mod checksum;
pub mod conformance;
mod error;
mod flash;
pub mod image;
pub mod mock;
pub mod platform;
pub mod rate;
mod xz_size;

pub use error::{Error, Result};
pub use flash::{flash, verify_device, FlashOptions, Progress};
pub use image::{Compression, ImageInfo, ImageKind};
pub use platform::{DeviceInfo, Platform, RawDevice};
