//! Decisions about one disk, made from sysfs: whether to offer it, what
//! else is using it, and whether an open file is the disk that was checked.
//!
//! Only `std`, and every function takes the sysfs root, so tests build a
//! fake one and run on any OS — as in `system.rs`.

use std::fs;
use std::path::{Path, PathBuf};

/// Kernel name prefixes of devices that are never a drive someone plugged in.
const VIRTUAL: &[&str] = &["loop", "ram", "zram", "dm-", "md", "sr", "nbd"];

/// Whether `/sys/block/<name>` (under `sys`) is a disk to offer: removable
/// or USB-attached, or an SD card; not virtual, not a partition, not empty.
///
/// MMC is the hard case. `mmcblk*` covers SD cards *and* soldered-in eMMC
/// (`device/type` is `MMC`), and an eMMC's hardware boot and RPMB
/// partitions show up as disks of their own (`mmcblk0boot0`). Only a device
/// whose type is `SD` is a card.
pub(crate) fn is_candidate_in(sys: &Path, name: &str) -> bool {
    let dir = sys.join("block").join(name);
    if VIRTUAL.iter().any(|p| name.starts_with(p)) || !dir.exists() {
        return false;
    }
    if dir.join("partition").exists() || is_mmc_hardware_partition(name) {
        return false;
    }
    // An empty card reader slot is a disk of size 0.
    if read(&dir.join("size")).parse::<u64>().unwrap_or(0) == 0 {
        return false;
    }
    if name.starts_with("mmcblk") {
        return read(&dir.join("device/type")) == "SD";
    }
    let removable = read(&dir.join("removable")) == "1";
    let usb = fs::canonicalize(&dir)
        .map(|p| p.to_string_lossy().contains("/usb"))
        .unwrap_or(false);
    removable || usb
}

/// `mmcblk0boot0`, `mmcblk0boot1`, `mmcblk0rpmb`: parts of an eMMC chip.
fn is_mmc_hardware_partition(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("mmcblk") else {
        return false;
    };
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    rest.starts_with("boot") || rest.starts_with("rpmb")
}

/// What a disk is made of and what sits on it: its partitions, and the
/// device-mapper and RAID devices built on it or on them (LUKS, LVM, md),
/// followed up through `holders/` — the reverse of `system.rs`'s `slaves/`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Users {
    /// Kernel names of the disk's partitions.
    pub partitions: Vec<String>,
    /// Kernel names of everything built on the disk, nearest first.
    pub holders: Vec<String>,
}

/// How deep to follow devices stacked on devices; real stacks are 2–3 deep.
const MAX_DEPTH: usize = 8;

pub(crate) fn users_in(sys: &Path, disk: &str) -> Users {
    let dir = sys.join("block").join(disk);
    let mut users = Users::default();
    let mut tops = vec![dir.clone()];
    for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
        if entry.path().join("partition").exists() {
            users
                .partitions
                .push(entry.file_name().to_string_lossy().into_owned());
            tops.push(entry.path());
        }
    }
    users.partitions.sort();
    for t in tops {
        holders(&t, 0, &mut users.holders);
    }
    users
}

fn holders(dev: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    for entry in fs::read_dir(dev.join("holders"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if out.contains(&name) {
            continue;
        }
        out.push(name);
        let next: PathBuf = fs::canonicalize(entry.path()).unwrap_or_else(|_| entry.path());
        holders(&next, depth + 1, out);
    }
}

/// How to name a holder to a person: a device-mapper device by its mapper
/// name (`luks-3f2a…`, `vg0-home`), anything else by its kernel name.
pub(crate) fn holder_label(sys: &Path, holder: &str) -> String {
    let dm = read(&sys.join("block").join(holder).join("dm/name"));
    if dm.is_empty() {
        format!("/dev/{holder}")
    } else {
        format!("/dev/mapper/{dm}")
    }
}

/// `"8:16\n"` from a sysfs `dev` file → `(8, 16)`.
pub(crate) fn parse_dev(text: &str) -> Option<(u32, u32)> {
    let (major, minor) = text.trim().split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// The `(major, minor)` sysfs gives `/sys/block/<name>`.
pub(crate) fn dev_number_in(sys: &Path, name: &str) -> Option<(u32, u32)> {
    parse_dev(&read(&sys.join("block").join(name).join("dev")))
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

    /// A fake sysfs: device directories under `devices/`, linked from
    /// `block/` as the kernel does.
    struct FakeSys(PathBuf);

    impl FakeSys {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("libflasher_disk_{}_{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("block")).unwrap();
            Self(root)
        }

        /// A disk at `devices/<bus>/<name>`, with these sysfs attributes.
        fn disk(&self, bus: &str, name: &str, attrs: &[(&str, &str)]) -> PathBuf {
            let d = self.0.join("devices").join(bus).join(name);
            fs::create_dir_all(&d).unwrap();
            for (k, v) in attrs {
                let p = d.join(k);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(p, format!("{v}\n")).unwrap();
            }
            symlink(&d, self.0.join("block").join(name)).unwrap();
            d
        }

        fn partition(&self, disk: &Path, name: &str) -> PathBuf {
            let p = disk.join(name);
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("partition"), "1\n").unwrap();
            p
        }

        /// A dm or md device at `devices/virtual/<name>` built on `under`.
        fn holder(&self, under: &Path, name: &str, dm_name: Option<&str>) -> PathBuf {
            let h = self.0.join("devices/virtual").join(name);
            fs::create_dir_all(&h).unwrap();
            if let Some(n) = dm_name {
                fs::create_dir_all(h.join("dm")).unwrap();
                fs::write(h.join("dm/name"), format!("{n}\n")).unwrap();
            }
            fs::create_dir_all(under.join("holders")).unwrap();
            symlink(&h, under.join("holders").join(name)).unwrap();
            if !self.0.join("block").join(name).exists() {
                symlink(&h, self.0.join("block").join(name)).unwrap();
            }
            h
        }
    }

    impl Drop for FakeSys {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const CARD: &[(&str, &str)] = &[("size", "62521344"), ("device/type", "SD")];

    #[test]
    fn offers_cards_and_sticks_never_emmc_or_empty_slots() {
        let s = FakeSys::new("candidates");
        s.disk(
            "mmc0",
            "mmcblk0",
            &[("size", "61071360"), ("device/type", "MMC")],
        );
        s.disk(
            "mmc0",
            "mmcblk0boot0",
            &[("size", "8192"), ("device/type", "MMC")],
        );
        s.disk("mmc0", "mmcblk0rpmb", &[("size", "8192")]);
        s.disk("mmc1", "mmcblk1", CARD);
        s.disk("pci/usb1/1-1", "sdb", &[("size", "0"), ("removable", "1")]);
        s.disk(
            "pci/usb1/1-2",
            "sdc",
            &[("size", "60437492"), ("removable", "0")],
        );
        s.disk(
            "pci/ata1",
            "sda",
            &[("size", "976773168"), ("removable", "0")],
        );
        s.disk(
            "pci/ata2",
            "sdd",
            &[("size", "2097152"), ("removable", "1")],
        );
        s.disk("virtual", "loop0", &[("size", "2048"), ("removable", "1")]);

        let mut offered: Vec<&str> = [
            "mmcblk0",
            "mmcblk0boot0",
            "mmcblk0rpmb",
            "mmcblk1",
            "sda",
            "sdb",
            "sdc",
            "sdd",
            "loop0",
        ]
        .into_iter()
        .filter(|n| is_candidate_in(&s.0, n))
        .collect();
        offered.sort();
        assert_eq!(offered, ["mmcblk1", "sdc", "sdd"]);
    }

    #[test]
    fn a_partition_is_never_a_candidate() {
        let s = FakeSys::new("part");
        let d = s.disk("mmc1", "mmcblk1", CARD);
        let p = s.partition(&d, "mmcblk1p1");
        fs::write(p.join("size"), "1000\n").unwrap();
        symlink(&p, s.0.join("block/mmcblk1p1")).unwrap();
        assert!(!is_candidate_in(&s.0, "mmcblk1p1"));
        assert!(!is_candidate_in(&s.0, "nothere"));
    }

    #[test]
    fn finds_what_is_built_on_a_disk() {
        let s = FakeSys::new("users");
        let d = s.disk("pci/usb1/1-1", "sdb", &[("size", "1000")]);
        let p1 = s.partition(&d, "sdb1");
        s.partition(&d, "sdb2");
        // LUKS on sdb1, LVM on top of it.
        let luks = s.holder(&p1, "dm-0", Some("luks-3f2a"));
        s.holder(&luks, "dm-1", Some("vg0-home"));
        let users = users_in(&s.0, "sdb");
        assert_eq!(users.partitions, ["sdb1", "sdb2"]);
        assert_eq!(users.holders, ["dm-0", "dm-1"]);
        assert_eq!(holder_label(&s.0, "dm-1"), "/dev/mapper/vg0-home");
        assert_eq!(holder_label(&s.0, "md0"), "/dev/md0");

        let plain = s.disk("pci/usb1/1-2", "sdc", &[("size", "1000")]);
        s.partition(&plain, "sdc1");
        assert_eq!(
            users_in(&s.0, "sdc"),
            Users {
                partitions: vec!["sdc1".into()],
                holders: vec![]
            }
        );
    }

    #[test]
    fn holders_that_loop_end() {
        let s = FakeSys::new("loop");
        let d = s.disk("pci/usb1/1-1", "sdb", &[("size", "1000")]);
        let h = s.holder(&d, "dm-0", None);
        // Nonsense, but must not recurse forever.
        s.holder(&h, "dm-0", None);
        assert_eq!(users_in(&s.0, "sdb").holders, ["dm-0"]);
    }

    #[test]
    fn device_numbers() {
        assert_eq!(parse_dev("8:16\n"), Some((8, 16)));
        assert_eq!(parse_dev("259:0"), Some((259, 0)));
        assert_eq!(parse_dev("8"), None);
        assert_eq!(parse_dev("x:1"), None);
        let s = FakeSys::new("devnum");
        s.disk("pci/usb1/1-1", "sdb", &[("dev", "8:16")]);
        assert_eq!(dev_number_in(&s.0, "sdb"), Some((8, 16)));
        assert_eq!(dev_number_in(&s.0, "sdz"), None);
    }
}
