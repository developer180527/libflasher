//! Linux: sysfs to find disks, `umount2` to release them, `/dev/sdX` to write.
//!
//! Writing needs root. As root, disks are opened directly; otherwise
//! `libflasher-helper`, started through `pkexec`, opens them and hands the
//! open file back (see [`helper`]), so the app itself never runs as root.
//!
//! Empty on every other OS, so `cargo build --workspace` works everywhere.
#![cfg(target_os = "linux")]

pub mod helper;
mod system;
mod watch;

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use libflasher_core::{DeviceInfo, Error, Platform, RawDevice, Result};

pub struct Linux;

impl Platform for Linux {
    fn name(&self) -> &'static str {
        "Linux"
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        let mounts = mounts();
        let system = system::disks()?;
        let mut out = Vec::new();
        for entry in fs::read_dir("/sys/block")? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            let sys = Path::new("/sys/block").join(&name);
            if system.contains(&name) || !is_candidate(&name, &sys) {
                continue;
            }
            let dev = format!("/dev/{name}");
            let model = [
                read(&sys.join("device/vendor")),
                read(&sys.join("device/model")),
            ]
            .join(" ")
            .trim()
            .to_string();
            let mountpoints = mounts
                .iter()
                .filter(|(d, _)| system::is_on_disk(d, &dev))
                .map(|(_, m)| m.clone())
                .collect();
            let size = read(&sys.join("size")).parse::<u64>().unwrap_or(0) * 512;
            let bus = if name.starts_with("mmcblk") {
                "SD"
            } else {
                "USB"
            };
            out.push(DeviceInfo::new(dev, model, size, bus, mountpoints));
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// As root, directly; otherwise through `libflasher-helper` (pkexec),
    /// which opens the disk and hands the open file back.
    fn open_device(&self, device: &DeviceInfo) -> Result<Box<dyn RawDevice>> {
        let (_, sys) = check(device)?;
        let file = if is_root() {
            open_as_root(device)?
        } else {
            helper::open(&device.path)?
        };
        let sector = read(&sys.join("queue/logical_block_size"))
            .parse()
            .unwrap_or(512);
        let size = read(&sys.join("size")).parse::<u64>().unwrap_or(0) * 512;
        // Whatever the kernel cached of this disk before is not what is on
        // it now (it may be a different stick at the same name).
        drop_cache(&file)?;
        Ok(Box::new(Disk { file, size, sector }))
    }

    fn eject(&self, _device: &DeviceInfo) -> Result<()> {
        // Everything is synced by now; powering the port off is UDisks2's job
        // (`udisksctl power-off`), and not worth a D-Bus dependency yet.
        Ok(())
    }

    /// Kernel uevents for the block subsystem, on a thread of their own.
    fn watch(&self, on_change: libflasher_core::OnChange) -> Option<Box<dyn std::any::Any + Send>> {
        watch::Watcher::start(on_change).map(|w| Box::new(w) as Box<dyn std::any::Any + Send>)
    }

    /// systemd's sleep inhibitor, held by a child that sleeps until killed.
    /// `systemd-inhibit --list` shows it under the running program's name.
    fn keep_awake(&self, reason: &str) -> Option<Box<dyn std::any::Any + Send>> {
        let child = Command::new("systemd-inhibit")
            .args([
                "--what=sleep:idle",
                &format!("--who={}", program_name()),
                &format!("--why={reason}"),
                "--mode=block",
                "sleep",
                "infinity",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        Some(Box::new(KillOnDrop(child)))
    }

    /// A GPT with one "Microsoft basic data" partition, formatted exFAT —
    /// what macOS's and Windows' own "erase" produce. As root directly;
    /// otherwise through `libflasher-helper`.
    fn restore(&self, device: &DeviceInfo, label: &str) -> Result<()> {
        check(device)?;
        if is_root() {
            restore_as_root(device, label)
        } else {
            helper::restore(&device.path, label)
        }
    }
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Open the disk for writing; needs root. Used directly when running as
/// root, and by the helper.
pub(crate) fn open_as_root(device: &DeviceInfo) -> Result<File> {
    prepare(device)?;
    // O_EXCL on a block device fails if anything else still holds it mounted.
    Ok(File::options()
        .read(true)
        .write(true)
        .custom_flags(libc::O_EXCL)
        .open(&device.path)?)
}

/// Needs `sfdisk` (util-linux) and `mkfs.exfat` (exfatprogs), and root.
pub(crate) fn restore_as_root(device: &DeviceInfo, label: &str) -> Result<()> {
    let (name, _) = prepare(device)?;
    run("wipefs", &["--all", &device.path], None)?;
    run(
        "sfdisk",
        &["--quiet", &device.path],
        Some("label: gpt\n,,EBD0A0A2-B9E5-4433-87C0-68B6B72699C7\n"),
    )?;
    // Wait for the kernel and udev to create the new partition's node.
    let _ = run("udevadm", &["settle"], None);
    // sdb → sdb1; mmcblk0, nvme0n1 (names ending in a digit) → mmcblk0p1.
    let sep = if name.ends_with(|c: char| c.is_ascii_digit()) {
        "p"
    } else {
        ""
    };
    let part = format!("/dev/{name}{sep}1");
    run("mkfs.exfat", &["-n", label, &part], None)?;
    Ok(())
}

/// The device must be a removable whole disk that the running system does
/// not live on; returns its kernel name and sysfs directory. Needs no
/// privileges.
fn check(device: &DeviceInfo) -> Result<(String, std::path::PathBuf)> {
    let name = device.path.strip_prefix("/dev/").unwrap_or("");
    let sys = Path::new("/sys/block").join(name);
    if name.is_empty() || name.contains('/') || !is_candidate(name, &sys) {
        return Err(Error::Refused {
            device: device.path.clone(),
            reason: "not a removable whole disk".into(),
        });
    }
    if system::disks()?.iter().any(|d| d == name) {
        return Err(Error::Refused {
            device: device.path.clone(),
            reason: "the running system is on this disk".into(),
        });
    }
    Ok((name.to_string(), sys))
}

/// Checks `device` is a removable whole disk and that we are root, then
/// unmounts everything on it. Returns its kernel name and sysfs directory.
fn prepare(device: &DeviceInfo) -> Result<(String, std::path::PathBuf)> {
    let checked = check(device)?;
    if !is_root() {
        return Err(Error::Permission("changing a disk needs root".into()));
    }
    for (dev, mountpoint) in mounts() {
        if system::is_on_disk(&dev, &device.path) {
            let m = CString::new(mountpoint.clone()).map_err(io::Error::other)?;
            if unsafe { libc::umount2(m.as_ptr(), 0) } != 0 {
                let e = io::Error::last_os_error();
                return Err(Error::Tool {
                    tool: "umount".into(),
                    message: format!("{mountpoint}: {e}"),
                });
            }
        }
    }
    Ok(checked)
}

fn run(tool: &str, args: &[&str], stdin: Option<&str>) -> Result<()> {
    let mut child = Command::new(tool)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Tool {
            tool: tool.into(),
            message: format!("could not run it ({e}); is it installed?"),
        })?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(Error::Tool {
            tool: tool.into(),
            message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(())
}

struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A removable or USB-attached whole disk, not a loop/ram/virtual device.
fn is_candidate(name: &str, sys: &Path) -> bool {
    // CI's throwaway loop device; compiled in only for that test.
    #[cfg(feature = "test-virtual-disks")]
    if libflasher_core::conformance::test_disk().as_deref() == Some(&format!("/dev/{name}")) {
        return sys.exists();
    }
    let virtual_dev = ["loop", "ram", "zram", "dm-", "md", "sr", "nbd"]
        .iter()
        .any(|p| name.starts_with(p));
    if virtual_dev || !sys.exists() {
        return false;
    }
    let removable = read(&sys.join("removable")) == "1";
    let usb = fs::canonicalize(sys)
        .map(|p| p.to_string_lossy().contains("/usb"))
        .unwrap_or(false);
    removable || usb || name.starts_with("mmcblk")
}

/// `(device, mountpoint)` for every mount, octal escapes decoded.
fn mounts() -> Vec<(String, String)> {
    system::parse_mounts(&fs::read_to_string("/proc/self/mounts").unwrap_or_default())
}

fn read(p: &Path) -> String {
    fs::read_to_string(p)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
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
    /// Also drops the kernel's cached copy of the disk, so a read after
    /// this — verification — comes from the drive, not from memory.
    fn sync(&mut self) -> Result<()> {
        self.file.sync_all()?;
        drop_cache(&self.file)
    }
}

/// Evict the disk's pages from the page cache. The disk is opened buffered,
/// and without this, reading back what was just written is answered from
/// memory: verification would pass whatever the drive stored. Only clean
/// pages are dropped, so call it after `sync_all`. Unlike the `BLKFLSBUF`
/// ioctl it needs no privilege, which matters when the helper opened the disk.
fn drop_cache(file: &File) -> Result<()> {
    use std::os::fd::AsRawFd;
    // posix_fadvise returns the error number rather than setting errno.
    match unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) } {
        0 => Ok(()),
        e => Err(io::Error::from_raw_os_error(e).into()),
    }
}

/// The running program's file name, for naming it to the OS.
fn program_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "libflasher".into())
}

#[cfg(test)]
mod tests {
    /// See `libflasher_core::conformance`: runs in CI as root, against a
    /// loop device named in `FLASHER_TEST_DISK`.
    /// Attaching a loop device is a block "add" uevent. Needs root for
    /// `losetup`, so it runs in CI's conformance job.
    #[test]
    #[ignore = "needs root to attach a loop device"]
    fn watch_notices_a_loop_device_being_attached_and_removed() {
        use libflasher_core::Platform;
        use std::process::Command;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let img = std::env::temp_dir().join(format!("libflasher_watch_{}.img", std::process::id()));
        std::fs::write(&img, vec![0u8; 1 << 20]).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let guard = super::Linux.watch(Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        assert!(guard.is_some(), "uevent socket should open");
        let wait_for_more = |than: usize| {
            let end = Instant::now() + Duration::from_secs(10);
            while count.load(Ordering::SeqCst) <= than {
                assert!(Instant::now() < end, "no notification within 10 s");
                std::thread::sleep(Duration::from_millis(20));
            }
        };

        let before = count.load(Ordering::SeqCst);
        let out = Command::new("losetup")
            .args(["--find", "--show"])
            .arg(&img)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
        wait_for_more(before);

        let before = count.load(Ordering::SeqCst);
        assert!(Command::new("losetup")
            .args(["-d", &dev])
            .status()
            .unwrap()
            .success());
        wait_for_more(before);

        drop(guard);
        let _ = std::fs::remove_file(img);
    }

    #[test]
    #[ignore = "writes to the disk named in FLASHER_TEST_DISK"]
    #[cfg(feature = "test-virtual-disks")]
    fn conformance() {
        if let Some(path) = libflasher_core::conformance::test_disk() {
            libflasher_core::conformance::full_cycle(&super::Linux, &path);
        }
    }
}
