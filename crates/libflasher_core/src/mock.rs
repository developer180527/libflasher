//! Drives that are files: the whole app, end to end, with no hardware.
//!
//! Every `*.disk` file in a directory is one removable drive, as large as the
//! file. Create one with `truncate -s 8G stick.disk` (sparse, so it costs no
//! space). `libflasher_platform::current()` uses this when `FLASHER_MOCK_DIR` is
//! set, so the CLI and the GUI both run against it unchanged.
//!
//! Faults are injected by name, so a test can reach every error path:
//! a drive whose file name contains `denied` refuses to open (the user
//! cancelling the password prompt), one containing `corrupt` flips a byte
//! on write (a failing stick, which verification must catch), and one
//! containing `unplug` is pulled out after 2 MiB: its file disappears and the
//! write fails with the OS's "device not configured" error, as a real unplug does.
//! A drive named `slow` takes 20 ms more per write.
//!
//! Restoring writes `MOCKFS <label>` at the start of the drive, and keeping
//! the computer awake creates `.awake` in the directory, so tests can see both.
//! A `<name>.serial` file beside a drive gives it that serial number.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::{DeviceInfo, Error, Platform, RawDevice, Result};

/// A [`Platform`] made of files: see the module docs.
pub struct MockPlatform {
    dir: PathBuf,
}

impl MockPlatform {
    /// A platform whose drives are the `*.disk` files in `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn file_for(&self, device: &DeviceInfo) -> Result<PathBuf> {
        let path = PathBuf::from(&device.path);
        // Only files directly inside the mock directory are drives.
        if path.parent() != Some(self.dir.as_path()) || path.extension().is_none_or(|e| e != "disk")
        {
            return Err(Error::Refused {
                device: device.path.clone(),
                reason: "not a mock drive".into(),
            });
        }
        Ok(path)
    }
}

impl Platform for MockPlatform {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "disk") && path.is_file() {
                let stem = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                // A serial, if `<name>.serial` sits beside the drive file:
                // lets a test swap in an identical-looking drive.
                let serial = fs::read_to_string(path.with_extension("serial")).ok();
                out.push(
                    DeviceInfo::new(
                        path.display().to_string(),
                        format!("Mock {stem}"),
                        fs::metadata(&path)?.len(),
                        "Mock",
                        Vec::new(),
                    )
                    .with_serial(serial),
                );
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    fn open_device(&self, device: &DeviceInfo) -> Result<Box<dyn RawDevice>> {
        let path = self.file_for(device)?;
        if device.path.contains("denied") {
            return Err(Error::Permission(
                "authorization was cancelled or denied".into(),
            ));
        }
        let file = File::options().read(true).write(true).open(&path)?;
        let size = file.metadata()?.len();
        Ok(Box::new(FileDevice {
            file,
            size,
            corrupt: device.path.contains("corrupt"),
            slow: device.path.contains("slow"),
            unplug: device
                .path
                .contains("unplug")
                .then(|| (path.clone(), UNPLUG_AT)),
        }))
    }

    fn eject(&self, device: &DeviceInfo) -> Result<()> {
        self.file_for(device).map(|_| ())
    }

    /// Polls the directory every 20 ms: mock drives are files, which have no
    /// plug-in events of their own.
    fn watch(&self, on_change: crate::platform::OnChange) -> Option<Box<dyn std::any::Any + Send>> {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let stop = Arc::new(AtomicBool::new(false));
        let (dir, flag) = (self.dir.clone(), stop.clone());
        std::thread::spawn(move || {
            let me = MockPlatform::new(dir);
            let mut last = me.list_devices().ok();
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(20));
                let now = me.list_devices().ok();
                if now != last {
                    on_change();
                    last = now;
                }
            }
        });
        Some(Box::new(StopOnDrop(stop)))
    }

    /// Held for as long as the computer must stay awake; while it is, the
    /// file `.awake` exists in the mock directory and holds the reason.
    fn keep_awake(&self, reason: &str) -> Option<Box<dyn std::any::Any + Send>> {
        let path = self.dir.join(".awake");
        fs::write(&path, reason).ok()?;
        Some(Box::new(AwakeMarker(path)))
    }

    /// Zeroes the first MiB and writes `MOCKFS <label>` at the start, where a
    /// test can read it back. Drives named `denied` refuse, as for writing.
    fn restore(&self, device: &DeviceInfo, label: &str) -> Result<()> {
        let path = self.file_for(device)?;
        if device.path.contains("denied") {
            return Err(Error::Permission(
                "authorization was cancelled or denied".into(),
            ));
        }
        let mut f = File::options().write(true).open(&path)?;
        let len = f.metadata()?.len().min(1 << 20) as usize;
        let mut head = vec![0u8; len];
        let tag = format!("{RESTORED_TAG}{label}");
        head[..tag.len().min(len)].copy_from_slice(&tag.as_bytes()[..tag.len().min(len)]);
        f.write_all(&head)?;
        f.sync_all()?;
        Ok(())
    }
}

/// What [`MockPlatform::restore`] writes at the start of a drive.
pub const RESTORED_TAG: &str = "MOCKFS ";

struct AwakeMarker(PathBuf);

impl Drop for AwakeMarker {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// A file standing in for a disk: whole-sector I/O enforced, like a raw device.
pub struct FileDevice {
    file: File,
    size: u64,
    corrupt: bool,
    /// Each write takes 20 ms longer, so a test can look at the app mid-write.
    slow: bool,
    /// The file to delete, and the offset at which the "cable is pulled".
    unplug: Option<(PathBuf, u64)>,
}

const UNPLUG_AT: u64 = 2 << 20;

#[cfg(test)]
pub(crate) fn device_gone_for_tests() -> io::Error {
    device_gone()
}

/// What a raw write or read to a device that has gone away fails with.
fn device_gone() -> io::Error {
    #[cfg(windows)]
    return io::Error::from_raw_os_error(1167); // ERROR_DEVICE_NOT_CONNECTED
    #[cfg(not(windows))]
    return io::Error::from_raw_os_error(6); // ENXIO, "Device not configured"
}

impl FileDevice {
    /// Open a file as a device, with no faults injected.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::options().read(true).write(true).open(path)?;
        Ok(Self {
            size: file.metadata()?.len(),
            file,
            corrupt: false,
            slow: false,
            unplug: None,
        })
    }
}

impl Read for FileDevice {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Write for FileDevice {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !buf.len().is_multiple_of(512) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "write is not a whole number of sectors",
            ));
        }
        let pos = self.file.stream_position()?;
        if let Some((path, at)) = &self.unplug {
            if pos + buf.len() as u64 > *at {
                let _ = fs::remove_file(path);
                return Err(device_gone());
            }
        }
        if pos + buf.len() as u64 > self.size {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write past the end of the device",
            ));
        }
        if self.slow {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if self.corrupt && pos == 0 && !buf.is_empty() {
            let mut bad = buf.to_vec();
            bad[0] ^= 0xFF;
            self.file.write_all(&bad)?;
            return Ok(buf.len());
        }
        self.file.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for FileDevice {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

impl RawDevice for FileDevice {
    fn sector_size(&self) -> u32 {
        512
    }
    fn size(&self) -> u64 {
        self.size
    }
    fn sync(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }
}

struct StopOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
