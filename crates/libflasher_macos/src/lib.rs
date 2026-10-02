//! macOS: `diskutil` to find and release disks, `authopen` to get a writable
//! file descriptor without running the whole app as root.
//!
//! `diskutil` is the supported command-line face of DiskArbitration. Moving to
//! the DiskArbitration framework directly would add hot-plug notifications;
//! nothing else here would change.

//!
//! Empty on every other OS, so `cargo build --workspace` works everywhere.
#![cfg(target_os = "macos")]

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};

use libflasher_core::{DeviceInfo, Error, Platform, RawDevice, Result};
use plist::{Dictionary, Value};

mod authopen;
mod watch;

pub struct MacOs;

impl Platform for MacOs {
    fn name(&self) -> &'static str {
        "macOS"
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        let list = diskutil_plist(&["list", "-plist", "external", "physical"])?;
        // CI's throwaway disk image is "external, virtual": add just that one.
        #[cfg(feature = "test-virtual-disks")]
        let list = with_test_disk(list)?;
        let mut out = Vec::new();
        for entry in array(&list, "AllDisksAndPartitions") {
            let Some(d) = entry.as_dictionary() else {
                continue;
            };
            let Some(id) = string(d, "DeviceIdentifier") else {
                continue;
            };
            let info = disk_info(&id)?;
            if bool(&info, "Internal") {
                continue;
            }
            let mut mountpoints: Vec<String> = string(d, "MountPoint").into_iter().collect();
            for p in d
                .get("Partitions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(m) = p.as_dictionary().and_then(|p| string(p, "MountPoint")) {
                    mountpoints.push(m);
                }
            }
            out.push(DeviceInfo::new(
                format!("/dev/{id}"),
                string(&info, "MediaName").unwrap_or_default().trim(),
                integer(&info, "TotalSize")
                    .or_else(|| integer(&info, "Size"))
                    .unwrap_or(0),
                string(&info, "BusProtocol").unwrap_or_default(),
                mountpoints,
            ));
        }
        Ok(out)
    }

    fn open_device(&self, device: &DeviceInfo) -> Result<Box<dyn RawDevice>> {
        let (id, info) = external_whole_disk(device)?;
        let size = integer(&info, "TotalSize").unwrap_or(0);
        let sector = integer(&info, "DeviceBlockSize").unwrap_or(512) as u32;

        run("diskutil", &["unmountDisk", &device.path])?;

        // The raw node bypasses the buffer cache: several times faster than /dev/diskN.
        let raw = format!("/dev/r{id}");
        let file = if unsafe { libc::geteuid() } == 0 {
            File::options().read(true).write(true).open(&raw)?
        } else {
            authopen::open_rw(&raw)?
        };
        Ok(Box::new(Disk { file, size, sector }))
    }

    fn eject(&self, device: &DeviceInfo) -> Result<()> {
        whole_disk_id(&device.path)?;
        run("diskutil", &["eject", &device.path]).map(|_| ())
    }

    /// DiskArbitration callbacks, on a thread of their own.
    fn watch(&self, on_change: libflasher_core::OnChange) -> Option<Box<dyn std::any::Any + Send>> {
        watch::Watcher::start(on_change).map(|w| Box::new(w) as Box<dyn std::any::Any + Send>)
    }

    /// `caffeinate` is Apple's own tool for this. `-i` holds off idle sleep
    /// and `-s` system sleep while on power; `-w` ties it to this process, so
    /// even if we crash it does not keep the Mac awake forever.
    fn keep_awake(&self, _reason: &str) -> Option<Box<dyn std::any::Any + Send>> {
        let child = Command::new("/usr/bin/caffeinate")
            .args(["-i", "-s", "-w", &std::process::id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        Some(Box::new(KillOnDrop(child)))
    }

    /// Disk Utility's erase, from the command line: a GUID partition map and
    /// one exFAT volume, which macOS, Windows and Linux all read and write.
    fn restore(&self, device: &DeviceInfo, label: &str) -> Result<()> {
        external_whole_disk(device)?;
        run(
            "diskutil",
            &["eraseDisk", "ExFAT", label, "GPT", &device.path],
        )
        .map(|_| ())
    }
}

#[cfg(feature = "test-virtual-disks")]
fn with_test_disk(mut list: Value) -> Result<Value> {
    let Some(path) = libflasher_core::conformance::test_disk() else {
        return Ok(list);
    };
    let all = diskutil_plist(&["list", "-plist", "external"])?;
    let extra: Vec<Value> = array(&all, "AllDisksAndPartitions")
        .iter()
        .filter(|e| {
            e.as_dictionary()
                .and_then(|d| string(d, "DeviceIdentifier"))
                .map(|id| format!("/dev/{id}"))
                .as_deref()
                == Some(path.as_str())
        })
        .cloned()
        .collect();
    if let Some(Value::Array(a)) = list
        .as_dictionary_mut()
        .and_then(|d| d.get_mut("AllDisksAndPartitions"))
    {
        a.extend(extra);
    }
    Ok(list)
}

/// Re-reads the disk at `device.path` and refuses anything that is not an
/// external whole disk, whatever the caller was told when it listed it.
fn external_whole_disk(device: &DeviceInfo) -> Result<(&str, Dictionary)> {
    let id = whole_disk_id(&device.path)?;
    let info = disk_info(id)?;
    if bool(&info, "Internal") || !bool(&info, "WholeDisk") {
        return Err(Error::Refused {
            device: device.path.clone(),
            reason: "not an external whole disk".into(),
        });
    }
    Ok((id, info))
}

struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Disk {
    file: File,
    size: u64,
    sector: u32,
}

impl Read for Disk {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}
impl Write for Disk {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
impl Seek for Disk {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}
impl RawDevice for Disk {
    fn sector_size(&self) -> u32 {
        self.sector
    }
    fn size(&self) -> u64 {
        self.size
    }
    fn sync(&mut self) -> Result<()> {
        // Not `sync_all`: on macOS that is `F_FULLFSYNC`, which only
        // filesystems implement, and a raw disk answers ENOTTY. The raw node
        // has no OS cache to flush; what can hold data is the drive's own
        // write cache, which DKIOCSYNCHRONIZECACHE asks it to empty.
        const DKIOCSYNCHRONIZECACHE: libc::c_ulong = 0x2000_6416; // _IO('d', 22)
        let fd = self.file.as_raw_fd();
        if unsafe { libc::fsync(fd) } != 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::ENOTTY) {
                return Err(e.into());
            }
        }
        // Some USB bridges do not implement the cache command. That is not a
        // failure: they write through, and verification reads the medium.
        unsafe { libc::ioctl(fd, DKIOCSYNCHRONIZECACHE) };
        Ok(())
    }
}

/// `/dev/disk4` → `disk4`, refusing anything that is not a whole-disk node.
fn whole_disk_id(path: &str) -> Result<&str> {
    let id = path.strip_prefix("/dev/").unwrap_or("");
    match id.strip_prefix("disk") {
        Some(n) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => Ok(id),
        _ => Err(Error::Refused {
            device: path.into(),
            reason: "not a whole-disk node".into(),
        }),
    }
}

fn disk_info(id: &str) -> Result<Dictionary> {
    match diskutil_plist(&["info", "-plist", id])? {
        Value::Dictionary(d) => Ok(d),
        _ => Err(tool_err("diskutil", "unexpected output")),
    }
}

fn diskutil_plist(args: &[&str]) -> Result<Value> {
    let out = run("diskutil", args)?;
    Value::from_reader_xml(out.as_slice()).map_err(|e| tool_err("diskutil", e))
}

fn run(tool: &str, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(if out.stderr.is_empty() {
            &out.stdout
        } else {
            &out.stderr
        });
        return Err(tool_err(tool, msg.trim()));
    }
    Ok(out.stdout)
}

fn tool_err(tool: &str, message: impl ToString) -> Error {
    Error::Tool {
        tool: tool.into(),
        message: message.to_string(),
    }
}

fn array<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.as_dictionary()
        .and_then(|d| d.get(key))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn string(d: &Dictionary, key: &str) -> Option<String> {
    d.get(key).and_then(Value::as_string).map(str::to_string)
}
fn integer(d: &Dictionary, key: &str) -> Option<u64> {
    d.get(key).and_then(Value::as_unsigned_integer)
}
fn bool(d: &Dictionary, key: &str) -> bool {
    d.get(key).and_then(Value::as_boolean).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_disk_ids() {
        assert_eq!(whole_disk_id("/dev/disk4").unwrap(), "disk4");
        assert!(whole_disk_id("/dev/disk4s1").is_err());
        assert!(whole_disk_id("/dev/rdisk4").is_err());
        assert!(whole_disk_id("disk4").is_err());
        assert!(whole_disk_id("/dev/disk").is_err());
    }

    /// Lists whatever is plugged in; never fails just because nothing is.
    #[test]
    fn lists_without_error() {
        let devices = MacOs.list_devices().unwrap();
        assert!(devices.iter().all(|d| d.path.starts_with("/dev/disk")));
    }

    #[test]
    fn keep_awake_holds_caffeinate_until_dropped() {
        let guard = MacOs.keep_awake("test").expect("caffeinate should start");
        let pid = guard.downcast_ref::<KillOnDrop>().unwrap().0.id() as i32;
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "caffeinate is not running"
        );
        drop(guard);
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "caffeinate outlived its guard"
        );
    }

    /// See `libflasher_core::conformance`: runs in CI as root, against an
    /// `hdiutil` image named in `FLASHER_TEST_DISK`.
    #[test]
    #[ignore = "writes to the disk named in FLASHER_TEST_DISK"]
    #[cfg(feature = "test-virtual-disks")]
    fn conformance() {
        if let Some(path) = libflasher_core::conformance::test_disk() {
            libflasher_core::conformance::full_cycle(&MacOs, &path);
        }
    }

    /// Attaching a disk image is a plug-in as far as DiskArbitration is
    /// concerned, and needs no root: this runs everywhere, CI included.
    #[test]
    fn watch_notices_a_disk_being_attached_and_removed() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir().join(format!("libflasher_watch_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("w.dmg");
        let ok = Command::new("hdiutil")
            .args(["create", "-size", "1m", "-layout", "NONE", "-type", "UDIF"])
            .arg(&img)
            .output()
            .unwrap();
        assert!(
            ok.status.success(),
            "{}",
            String::from_utf8_lossy(&ok.stderr)
        );

        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let guard = MacOs.watch(Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        assert!(guard.is_some(), "DiskArbitration session should start");
        // Registering reports the disks already present; let that settle.
        std::thread::sleep(Duration::from_millis(500));
        let wait_for_more = |than: usize| {
            let end = Instant::now() + Duration::from_secs(10);
            while count.load(Ordering::SeqCst) <= than {
                assert!(Instant::now() < end, "no notification within 10 s");
                std::thread::sleep(Duration::from_millis(20));
            }
        };

        let before = count.load(Ordering::SeqCst);
        let out = Command::new("hdiutil")
            .args(["attach", "-nomount"])
            .arg(&img)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let dev = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();
        wait_for_more(before);

        let before = count.load(Ordering::SeqCst);
        let out = Command::new("hdiutil")
            .args(["detach", &dev])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        wait_for_more(before);

        drop(guard);
        let _ = std::fs::remove_dir_all(dir);
    }
}
