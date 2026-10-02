//! Write disk images to USB drives and SD cards.
//!
//! One crate to depend on: the OS-independent core (image detection,
//! decompression, checksums, the write-and-verify pipeline) and the backend
//! for the OS you build for, chosen by [`current_platform`].
//!
//! ```
//! use std::sync::atomic::AtomicBool;
//! use libflasher::{image, mock::MockPlatform, rate::StatusLine, FlashOptions, Platform};
//!
//! # fn main() -> libflasher::Result<()> {
//! # let dir = std::env::temp_dir().join(format!("libflasher_doc_{}", std::process::id()));
//! # std::fs::create_dir_all(&dir)?;
//! # std::fs::write(dir.join("stick.disk"), vec![0u8; 4 << 20])?;
//! # let mut img = vec![7u8; 1 << 20];
//! # img[510] = 0x55; img[511] = 0xAA;
//! # std::fs::write(dir.join("disk.img"), &img)?;
//! // A real program uses `libflasher::current_platform()`; this example
//! // uses drives that are files, so it can run anywhere.
//! let platform = MockPlatform::new(&dir);
//! let drive = platform.list_devices()?.remove(0);
//!
//! let image = image::inspect(dir.join("disk.img"))?;
//! assert!(image.kind.raw_writable());
//!
//! // `open_listed` re-checks that the drive is still the one listed.
//! let mut device = platform.open_listed(&drive)?;
//! let mut status = StatusLine::new();
//! let written = libflasher::flash(
//!     &image,
//!     device.as_mut(),
//!     &FlashOptions::default().with_verify(true),
//!     &AtomicBool::new(false), // set it from another thread to cancel
//!     &mut |p| {
//!         status.update(p);
//!         println!("{}", status.text());
//!     },
//! )?;
//! assert_eq!(written, 1 << 20);
//! # std::fs::remove_dir_all(&dir)?;
//! # Ok(())
//! # }
//! ```
//!
//! # Stability
//!
//! Public enums and structs are `#[non_exhaustive]`: new variants, fields and
//! [`Platform`] methods (with defaults) can arrive in minor releases without
//! breaking you. Build values with their constructors (`DeviceInfo::new`,
//! `FlashOptions::default().with_*`), and match enums with a `_` arm.

pub use libflasher_core::*;
pub use libflasher_platform::current as current_platform;
