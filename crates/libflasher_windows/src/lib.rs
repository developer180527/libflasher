//! Windows: `\\.\PhysicalDriveN` for the disk, `FSCTL_LOCK_VOLUME` /
//! `FSCTL_DISMOUNT_VOLUME` to release its volumes, a power request to stay
//! awake, and `diskpart` to restore a drive.
//!
//! Writing a physical drive needs an elevated (administrator) process; the
//! app's manifest asks for it. Without it, opening the disk fails with
//! "access denied", which is reported as a permission error.
//!
//! The decisions that do not need Windows to make — which disks qualify, what
//! the `diskpart` script says — live in [`policy`] and are tested on every OS.
//! Everything that calls Windows lives in `sys` and is tested on Windows (CI).
//!
//! How Rufus does the same things, for reference (Rufus is GPLv3; nothing here
//! is copied from it): `src/dev.c` (`GetDevices`), `src/drive.c`
//! (`GetLogicalHandle`, `UnmountVolume`).

pub mod policy;

#[cfg(windows)]
mod sys;
#[cfg(windows)]
mod watch;

#[cfg(windows)]
pub use sys::Windows;
