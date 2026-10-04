//! The OS-independent half of the flasher.
//!
//! Nothing in this crate opens a device or knows what a drive letter, a
//! `/dev/rdisk` or a block-device ioctl is. Everything OS-specific sits behind
//! [`Platform`] and [`RawDevice`]; supporting a new OS means implementing those
//! two traits in a new `libflasher_<os>` crate and adding one `cfg` line
//! to `libflasher_platform`.

#![warn(missing_docs)]

mod blockio;
pub mod checksum;
pub mod conformance;
mod error;
pub mod extract;
mod flash;
mod gpt;
#[cfg(test)]
mod hostile;
pub mod image;
pub mod iso9660;
pub mod mock;
pub mod platform;
pub mod rate;
pub mod udf;
mod wim;
mod xz_size;
#[cfg(feature = "zip")]
mod zip;

pub use error::{Error, Result};
pub use flash::{flash, verify_device, write_image, FlashOptions, Progress};
pub use image::{Compression, ImageInfo, ImageKind};
pub use platform::{DeviceInfo, OnChange, Platform, RawDevice};
