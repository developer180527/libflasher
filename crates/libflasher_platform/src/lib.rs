//! The one place that knows which OS crates exist.
//!
//! To add an OS: create `libflasher_<os>` implementing
//! [`libflasher_core::Platform`], add it under a `[target.'cfg(…)'.dependencies]`
//! in this crate's manifest, and add one arm below.
//!
//! `FLASHER_MOCK_DIR=<dir>` swaps the real OS for [`libflasher_core::mock`]:
//! every `*.disk` file in `<dir>` becomes a drive. That is how the app runs
//! headless — in CI, in tests, or on a machine with no USB port.

use std::sync::Arc;

/// The program libflasher starts through `pkexec` to open disks on Linux.
/// An app ships it as a binary of its own whose `main` calls
/// `linux_helper::main`; see `libflasher_linux::helper`.
#[cfg(target_os = "linux")]
pub use libflasher_linux::helper as linux_helper;

use libflasher_core::Platform;

pub fn current() -> Arc<dyn Platform> {
    if let Some(dir) = std::env::var_os("FLASHER_MOCK_DIR") {
        return Arc::new(libflasher_core::mock::MockPlatform::new(dir));
    }
    #[cfg(target_os = "macos")]
    return Arc::new(libflasher_macos::MacOs);
    #[cfg(target_os = "linux")]
    return Arc::new(libflasher_linux::Linux);
    #[cfg(windows)]
    return Arc::new(libflasher_windows::Windows);
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    return Arc::new(Unsupported);
}

/// Lists nothing and opens nothing, so the app still builds and runs on an OS
/// with no implementation yet.
#[allow(dead_code)]
struct Unsupported;

impl Platform for Unsupported {
    fn name(&self) -> &'static str {
        "unsupported OS"
    }
    fn list_devices(&self) -> libflasher_core::Result<Vec<libflasher_core::DeviceInfo>> {
        Ok(Vec::new())
    }
    fn open_device(
        &self,
        _: &libflasher_core::DeviceInfo,
    ) -> libflasher_core::Result<Box<dyn libflasher_core::RawDevice>> {
        Err(libflasher_core::Error::Unsupported(
            "raw device access".into(),
        ))
    }
    fn eject(&self, _: &libflasher_core::DeviceInfo) -> libflasher_core::Result<()> {
        Ok(())
    }
}
