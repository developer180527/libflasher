//! Which whole disks the running system lives on, so they are never listed
//! or opened, however they are attached.
//!
//! The device mounted at `/` is not enough. Root is often on a device-mapper
//! volume (LVM, LUKS) or RAID whose real disks are found only through sysfs
//! `slaves/`; a live USB runs from an overlay over a squashfs loop device
//! whose file is on the stick itself; some kernels name the root device
//! `/dev/root`, which has no node. So every mount the system needs, and
//! every swap device, is followed down to the physical disks under it,
//! through partitions, `slaves/` and loop devices' backing files.
//!
//! Only `std`: the logic takes the sysfs root and the mount table as
//! arguments, so tests build a fake sysfs and run on any OS.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Mount points the running system cannot do without.
const SYSTEM_MOUNTS: &[&str] = &["/", "/boot", "/boot/efi", "/efi", "/usr", "/var", "/home"];

/// Where live systems mount the medium they booted from (Debian, Ubuntu,
/// Fedora, Arch, older Debian).
const LIVE_MEDIA: &[&str] = &[
    "/run/live/medium",
    "/cdrom",
    "/run/initramfs/live",
    "/run/archiso/bootmnt",
    "/lib/live/mount/medium",
];

/// How deep to follow devices stacked on devices; real stacks are 2–3 deep.
const MAX_DEPTH: usize = 8;

/// The whole disks (kernel names: `sda`, `nvme0n1`) under the running system.
pub fn disks() -> io::Result<Vec<String>> {
    let mounts = parse_mounts(&fs::read_to_string("/proc/self/mounts")?);
    let swaps = parse_swaps(&fs::read_to_string("/proc/swaps").unwrap_or_default());
    // The device numbers of the system mounts themselves: this finds `/`
    // even when the mount table calls it `/dev/root`.
    let numbers: Vec<(u32, u32)> = SYSTEM_MOUNTS
        .iter()
        .filter_map(|m| fs::metadata(m).ok())
        .map(|m| split_dev(std::os::unix::fs::MetadataExt::dev(&m)))
        .collect();
    Ok(disks_in(Path::new("/sys"), &mounts, &swaps, &numbers))
}

/// [`disks`], given everything it reads: a sysfs root, the mount table
/// (`(source, mountpoint)`), swap devices, and device numbers of the system
/// mounts.
pub(crate) fn disks_in(
    sys: &Path,
    mounts: &[(String, String)],
    swaps: &[String],
    numbers: &[(u32, u32)],
) -> Vec<String> {
    let mut out = Vec::new();
    let system = mounts
        .iter()
        .filter(|(_, m)| SYSTEM_MOUNTS.contains(&m.as_str()) || LIVE_MEDIA.contains(&m.as_str()))
        .map(|(source, _)| source);
    for source in system.chain(swaps) {
        if let Some(name) = kernel_name(source) {
            backing(
                sys,
                &sys.join("class/block").join(name),
                mounts,
                0,
                &mut out,
            );
        }
    }
    for (major, minor) in numbers {
        let node = sys.join("dev/block").join(format!("{major}:{minor}"));
        backing(sys, &node, mounts, 0, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

/// Add the physical whole disks under the block device at sysfs `node`.
fn backing(
    sys: &Path,
    node: &Path,
    mounts: &[(String, String)],
    depth: usize,
    out: &mut Vec<String>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(dev) = fs::canonicalize(node) else {
        return;
    };
    let whole = if dev.join("partition").exists() {
        match dev.parent() {
            Some(p) => p.to_path_buf(),
            None => return,
        }
    } else {
        dev
    };
    let Some(name) = whole
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
    else {
        return;
    };

    // Device-mapper and RAID: the disks are its slaves (or theirs).
    let slaves: Vec<PathBuf> = fs::read_dir(whole.join("slaves"))
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    if !slaves.is_empty() {
        for s in slaves {
            backing(sys, &s, mounts, depth + 1, out);
        }
        return;
    }

    // A loop device: the disk holding its file, found through the mount
    // that contains the file.
    if name.starts_with("loop") {
        let file = read(&whole.join("loop/backing_file"));
        if let Some(source) = containing_mount(mounts, &file) {
            if let Some(n) = kernel_name(source) {
                backing(
                    sys,
                    &sys.join("class/block").join(n),
                    mounts,
                    depth + 1,
                    out,
                );
            }
        }
        return;
    }

    out.push(name);
}

/// The source of the mount with the longest mount point that contains `file`.
fn containing_mount<'a>(mounts: &'a [(String, String)], file: &str) -> Option<&'a str> {
    if !file.starts_with('/') {
        return None;
    }
    mounts
        .iter()
        .filter(|(_, m)| Path::new(file).starts_with(m))
        .max_by_key(|(_, m)| m.len())
        .map(|(source, _)| source.as_str())
}

/// `/dev/sda2` → `sda2`; `/dev/mapper/root` → `dm-0` through its symlink.
/// `None` for sources that are not device nodes (`overlay`, `tmpfs`).
fn kernel_name(source: &str) -> Option<String> {
    if !source.starts_with("/dev/") {
        return None;
    }
    let resolved = fs::canonicalize(source).unwrap_or_else(|_| PathBuf::from(source));
    resolved
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
}

/// `(device, mountpoint)` for every line of `/proc/self/mounts`, octal
/// escapes decoded.
pub(crate) fn parse_mounts(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split(' ');
            Some((f.next()?.to_string(), unescape(f.next()?)))
        })
        .collect()
}

/// Swap devices from `/proc/swaps` (swap files are on mounted filesystems,
/// which are covered already if they matter).
fn parse_swaps(text: &str) -> Vec<String> {
    text.lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next())
        .filter(|s| s.starts_with("/dev/"))
        .map(unescape)
        .collect()
}

pub(crate) fn unescape(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// glibc's `major()`/`minor()` of a `dev_t`.
fn split_dev(dev: u64) -> (u32, u32) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major as u32, minor as u32)
}

fn read(p: &Path) -> String {
    fs::read_to_string(p)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A fake sysfs: disks and partitions under `devices/`, with the
    /// `class/block` and `dev/block` symlinks the kernel makes.
    struct FakeSys(PathBuf);

    impl FakeSys {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("libflasher_sysfs_{}_{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("class/block")).unwrap();
            fs::create_dir_all(root.join("dev/block")).unwrap();
            Self(root)
        }

        fn link(&self, name: &str, target: &Path, devnum: Option<&str>) {
            symlink(target, self.0.join("class/block").join(name)).unwrap();
            // The first disk made gets the number; tests that use numbers
            // make only one.
            let num = devnum.map(|n| self.0.join("dev/block").join(n));
            if let Some(n) = num.filter(|n| n.symlink_metadata().is_err()) {
                symlink(target, n).unwrap();
            }
        }

        /// A disk with partitions; partition `i` gets device number `8:{i}`.
        fn disk(&self, name: &str, parts: &[&str]) {
            let d = self.0.join("devices/pci/usb/block").join(name);
            fs::create_dir_all(&d).unwrap();
            self.link(name, &d, None);
            for (i, p) in parts.iter().enumerate() {
                let pd = d.join(p);
                fs::create_dir_all(&pd).unwrap();
                fs::write(pd.join("partition"), format!("{}\n", i + 1)).unwrap();
                self.link(p, &pd, Some(&format!("8:{}", i + 1)));
            }
        }

        /// A device-mapper or RAID device on top of `slaves`.
        fn stacked(&self, name: &str, slaves: &[&str]) {
            let d = self.0.join("devices/virtual/block").join(name);
            fs::create_dir_all(d.join("slaves")).unwrap();
            for s in slaves {
                let target = fs::canonicalize(self.0.join("class/block").join(s)).unwrap();
                symlink(target, d.join("slaves").join(s)).unwrap();
            }
            self.link(name, &d, None);
        }

        fn loop_dev(&self, name: &str, file: &str) {
            let d = self.0.join("devices/virtual/block").join(name);
            fs::create_dir_all(d.join("loop")).unwrap();
            fs::write(d.join("loop/backing_file"), format!("{file}\n")).unwrap();
            self.link(name, &d, None);
        }

        fn disks(
            &self,
            mounts: &[(&str, &str)],
            swaps: &[&str],
            numbers: &[(u32, u32)],
        ) -> Vec<String> {
            let mounts: Vec<(String, String)> = mounts
                .iter()
                .map(|(s, m)| (s.to_string(), m.to_string()))
                .collect();
            let swaps: Vec<String> = swaps.iter().map(|s| s.to_string()).collect();
            disks_in(&self.0, &mounts, &swaps, numbers)
        }
    }

    impl Drop for FakeSys {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn root_on_a_partition() {
        let s = FakeSys::new("plain");
        s.disk("sda", &["sda1", "sda2"]);
        s.disk("sdb", &["sdb1"]);
        let got = s.disks(
            &[
                ("/dev/sda2", "/"),
                ("/dev/sda1", "/boot/efi"),
                ("/dev/sdb1", "/media/stick"),
            ],
            &[],
            &[],
        );
        assert_eq!(got, ["sda"], "a stick mounted elsewhere is not the system");
    }

    #[test]
    fn root_on_lvm_or_luks_is_traced_to_its_disks() {
        let s = FakeSys::new("dm");
        s.disk("sda", &["sda1"]);
        s.disk("sdb", &["sdb1"]);
        s.stacked("dm-0", &["sda1"]);
        // A RAID under device-mapper: two levels of slaves.
        s.stacked("md0", &["sdb1"]);
        s.stacked("dm-1", &["md0"]);
        assert_eq!(s.disks(&[("/dev/dm-0", "/")], &[], &[]), ["sda"]);
        assert_eq!(s.disks(&[("/dev/dm-1", "/home")], &[], &[]), ["sdb"]);
    }

    #[test]
    fn a_live_usb_is_the_system() {
        let s = FakeSys::new("live");
        s.disk("sdb", &["sdb1"]);
        s.disk("sdc", &["sdc1"]);
        s.loop_dev("loop0", "/run/live/medium/live/filesystem.squashfs");
        let mounts = [
            ("overlay", "/"),
            ("/dev/sdb1", "/run/live/medium"),
            ("/dev/loop0", "/run/live/rootfs/filesystem.squashfs"),
            ("/dev/sdc1", "/media/user/OTHER"),
        ];
        assert_eq!(s.disks(&mounts, &[], &[]), ["sdb"]);
    }

    #[test]
    fn a_loop_device_is_followed_to_the_disk_holding_its_file() {
        let s = FakeSys::new("loop");
        s.disk("sdb", &["sdb1"]);
        s.loop_dev("loop3", "/mnt/images/root.img");
        let mounts = [("/dev/sdb1", "/mnt"), ("/dev/loop3", "/")];
        assert_eq!(s.disks(&mounts, &[], &[]), ["sdb"]);
    }

    #[test]
    fn dev_root_is_found_by_device_number() {
        let s = FakeSys::new("devroot");
        s.disk("mmcblk0", &["mmcblk0p1", "mmcblk0p2"]);
        // The mount table says only "/dev/root", which has no node.
        assert_eq!(s.disks(&[("/dev/root", "/")], &[], &[(8, 2)]), ["mmcblk0"]);
    }

    #[test]
    fn swap_disks_count() {
        let s = FakeSys::new("swap");
        s.disk("sda", &["sda1"]);
        s.disk("sdc", &["sdc1"]);
        assert_eq!(
            s.disks(&[("/dev/sda1", "/")], &["/dev/sdc1"], &[]),
            ["sda", "sdc"]
        );
    }

    #[test]
    fn stacks_that_loop_back_on_themselves_end() {
        let s = FakeSys::new("cycle");
        s.loop_dev("loop0", "/data/x.img");
        // The loop's file is on the loop itself: nonsense, but must not hang.
        assert!(s
            .disks(&[("/dev/loop0", "/data"), ("/dev/loop0", "/")], &[], &[])
            .is_empty());
    }

    #[test]
    fn parses_the_kernel_tables() {
        let m =
            parse_mounts("/dev/sda2 / ext4 rw 0 0\n/dev/sdb1 /media/My\\040Stick vfat rw 0 0\n");
        assert_eq!(
            m[1],
            ("/dev/sdb1".to_string(), "/media/My Stick".to_string())
        );
        let s = parse_swaps(
            "Filename\tType\tSize\tUsed\tPriority\n/dev/sda3 partition 1 0 -2\n/swapfile file 1 0 -3\n",
        );
        assert_eq!(s, ["/dev/sda3"]);
        assert_eq!(split_dev(0x0802), (8, 2));
        assert_eq!(split_dev((259u64 << 8) | 1), (259, 1));
    }
}
