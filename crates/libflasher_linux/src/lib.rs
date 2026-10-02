//! Linux: sysfs to find disks, `umount2` to release them, `/dev/sdX` to write.
//!
//! Writing needs root. Until a polkit helper exists, run the app with `sudo`
//! or `pkexec`; `open_device` says so rather than failing with EACCES.
//!
//! Empty on every other OS, so `cargo build --workspace` works everywhere.
#![cfg(target_os = "linux")]

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
        let root_disk = mounts
            .iter()
            .find(|(_, m)| m == "/")
            .and_then(|(dev, _)| disk_of(dev));
        let mut out = Vec::new();
        for entry in fs::read_dir("/sys/block")? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            let sys = Path::new("/sys/block").join(&name);
            if Some(&name) == root_disk.as_ref() || !is_candidate(&name, &sys) {
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
                .filter(|(d, _)| d.starts_with(&dev))
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

    fn open_device(&self, device: &DeviceInfo) -> Result<Box<dyn RawDevice>> {
        let (_, sys) = prepare(device)?;
        // O_EXCL on a block device fails if anything else still holds it mounted.
        let file = File::options()
            .read(true)
            .write(true)
            .custom_flags(libc::O_EXCL)
            .open(&device.path)?;
        let sector = read(&sys.join("queue/logical_block_size"))
            .parse()
            .unwrap_or(512);
        let size = read(&sys.join("size")).parse::<u64>().unwrap_or(0) * 512;
        Ok(Box::new(Disk { file, size, sector }))
    }

    fn eject(&self, _device: &DeviceInfo) -> Result<()> {
        // Everything is synced by now; powering the port off is UDisks2's job
        // (`udisksctl power-off`), and not worth a D-Bus dependency yet.
        Ok(())
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
    /// what macOS's and Windows' own "erase" produce. Needs `sfdisk`
    /// (util-linux) and `mkfs.exfat` (exfatprogs).
    fn restore(&self, device: &DeviceInfo, label: &str) -> Result<()> {
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
}

/// Checks `device` is a removable whole disk and that we are root, then
/// unmounts everything on it. Returns its kernel name and sysfs directory.
fn prepare(device: &DeviceInfo) -> Result<(String, std::path::PathBuf)> {
    let name = device.path.strip_prefix("/dev/").unwrap_or("");
    let sys = Path::new("/sys/block").join(name);
    if name.is_empty() || name.contains('/') || !is_candidate(name, &sys) {
        return Err(Error::Refused {
            device: device.path.clone(),
            reason: "not a removable whole disk".into(),
        });
    }
    if unsafe { libc::geteuid() } != 0 {
        return Err(Error::Permission(
            "changing a disk needs root on Linux: run with sudo or pkexec".into(),
        ));
    }
    for (dev, mountpoint) in mounts() {
        if dev.starts_with(&device.path) {
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
    Ok((name.to_string(), sys))
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
    fs::read_to_string("/proc/self/mounts")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut f = l.split(' ');
            Some((f.next()?.to_string(), unescape(f.next()?)))
        })
        .collect()
}

fn unescape(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// `/dev/sda2` → `sda`, `/dev/nvme0n1p2` → `nvme0n1`, via sysfs rather than string rules.
fn disk_of(dev: &str) -> Option<String> {
    let part = dev.strip_prefix("/dev/")?;
    let p = fs::canonicalize(Path::new("/sys/class/block").join(part)).ok()?;
    let whole = if p.join("partition").exists() {
        p.parent()?
    } else {
        &p
    };
    Some(whole.file_name()?.to_string_lossy().into_owned())
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
    fn sync(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
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
    #[test]
    #[ignore = "writes to the disk named in FLASHER_TEST_DISK"]
    #[cfg(feature = "test-virtual-disks")]
    fn conformance() {
        if let Some(path) = libflasher_core::conformance::test_disk() {
            libflasher_core::conformance::full_cycle(&super::Linux, &path);
        }
    }
}
