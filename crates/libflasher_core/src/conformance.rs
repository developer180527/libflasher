//! One test every [`Platform`] must pass against a real (virtual) disk:
//! list it, open it, flash and verify an image, restore it.
//!
//! CI creates a throwaway disk — a loop device on Linux, an `hdiutil` image
//! on macOS, a VHD on Windows — names it in `FLASHER_TEST_DISK`, and runs each
//! backend's ignored `conformance` test built with its `test-virtual-disks`
//! feature (which lets that one virtual disk through the "removable only"
//! filter). Without the variable the test does nothing; and it refuses any
//! disk over [`MAX_TEST_DISK`], so it cannot be pointed at a real drive.

use std::sync::atomic::AtomicBool;

use crate::{image, Error, FlashOptions, Platform, Result};

/// The environment variable naming the throwaway disk.
pub const TEST_DISK_VAR: &str = "FLASHER_TEST_DISK";

/// No real drive is this small; every CI test disk is.
pub const MAX_TEST_DISK: u64 = 512 << 20;

/// The disk named for testing, if any.
pub fn test_disk() -> Option<String> {
    std::env::var(TEST_DISK_VAR).ok().filter(|s| !s.is_empty())
}

/// The full cycle against the disk at `path`. Panics with a description on
/// any failure, for use directly inside a `#[test]`.
pub fn full_cycle(platform: &dyn Platform, path: &str) {
    if let Err(e) = run(platform, path) {
        panic!("conformance failed on {path} ({}): {e}", platform.name());
    }
}

fn run(platform: &dyn Platform, path: &str) -> Result<()> {
    let refuse = |reason: String| Error::Refused {
        device: path.into(),
        reason,
    };
    let listed = platform.list_devices()?;
    let drive = listed
        .iter()
        .find(|d| d.path == path)
        .cloned()
        .ok_or_else(|| refuse(format!("not listed; listed were {listed:?}")))?;
    if drive.size == 0 || drive.size > MAX_TEST_DISK {
        return Err(refuse(format!(
            "{} bytes is not a test disk (must be 1..={MAX_TEST_DISK})",
            drive.size
        )));
    }

    // An image filling a quarter of the disk, with an MBR signature so it is
    // a raw-writable disk image, and an odd length so the last write pads.
    let len = (drive.size / 4) as usize + 777;
    let mut bytes: Vec<u8> = (0..len).map(|i| (i * 131 % 251) as u8).collect();
    bytes[510] = 0x55;
    bytes[511] = 0xAA;
    let file =
        std::env::temp_dir().join(format!("libflasher_conformance_{}.img", std::process::id()));
    std::fs::write(&file, &bytes)?;
    let info = image::inspect(&file)?;

    let result = (|| {
        let mut dev = platform.open_listed(&drive)?;
        let written = crate::flash(
            &info,
            dev.as_mut(),
            &FlashOptions::default().with_verify(true),
            &AtomicBool::new(false),
            &mut |_| {},
        )?;
        if written != len as u64 {
            return Err(refuse(format!("wrote {written} bytes, expected {len}")));
        }
        drop(dev);
        // Writing changed the partition table; the OS may take a moment to
        // settle before the disk is listed the same way again.
        let again = (0..20)
            .find_map(|_| {
                let d = platform
                    .list_devices()
                    .ok()?
                    .into_iter()
                    .find(|d| d.same_disk(&drive));
                if d.is_none() {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                d
            })
            .ok_or_else(|| refuse("disappeared after writing".into()))?;
        platform.restore_listed(&again, "FLASHERTEST")
    })();
    let _ = std::fs::remove_file(&file);
    result
}
