//! The Windows backend's decisions, kept free of Windows calls so every OS
//! can test them.

/// `STORAGE_BUS_TYPE` values (winioctl.h) that matter here.
pub mod bus {
    pub const USB: i32 = 7;
    pub const SD: i32 = 12;
    pub const MMC: i32 = 13;
}

/// What the OS told us about one physical disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskFacts {
    pub number: u32,
    pub bus_type: i32,
    pub removable_media: bool,
    pub size: u64,
    /// The disk holding the running Windows (the system volume's disk).
    pub is_system: bool,
}

/// Whether a disk may be offered for writing: removable or on a USB/SD bus,
/// never the system disk, never empty (a card reader with no card).
pub fn is_candidate(d: &DiskFacts) -> bool {
    let removable = d.removable_media || matches!(d.bus_type, bus::USB | bus::SD | bus::MMC);
    removable && !d.is_system && d.size > 0
}

/// A name for the bus, for the drive picker.
pub fn bus_name(bus_type: i32) -> &'static str {
    match bus_type {
        bus::USB => "USB",
        bus::SD | bus::MMC => "SD",
        _ => "Removable",
    }
}

/// `\\.\PhysicalDrive3` → 3. Refuses anything else, including partitions and
/// volume paths, whatever a caller hands in.
pub fn disk_number(path: &str) -> Option<u32> {
    let n = path.strip_prefix(r"\\.\PhysicalDrive")?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) || (n.len() > 1 && n.starts_with('0'))
    {
        return None;
    }
    n.parse().ok()
}

/// `FindFirstVolumeW` names a volume `\\?\Volume{…}\`; opening it as a
/// device needs the name without the final backslash (with it, the volume's
/// root directory is opened instead).
pub fn volume_open_path(name: &str) -> String {
    name.strip_suffix('\\').unwrap_or(name).to_string()
}

/// The disk numbers in a `VOLUME_DISK_EXTENTS` (from
/// `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`): a count, then 24-byte
/// `DISK_EXTENT`s from offset 8, each starting with its disk number. A
/// volume spanning disks (dynamic disks, Storage Spaces) names them all.
pub fn extent_disks(buf: &[u8]) -> Vec<u32> {
    let Some(count) = buf.get(..4) else {
        return Vec::new();
    };
    let count = u32::from_le_bytes(count.try_into().unwrap()) as usize;
    let mut out: Vec<u32> = (0..count)
        .filter_map(|i| buf.get(8 + i * 24..8 + i * 24 + 4))
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The drive letter of a page file as `ExistingPageFiles` lists it:
/// `\??\C:\pagefile.sys` → `C`.
pub fn pagefile_letter(entry: &str) -> Option<char> {
    let path = entry.trim().strip_prefix(r"\??\").unwrap_or(entry.trim());
    let mut chars = path.chars();
    let letter = chars.next().filter(char::is_ascii_alphabetic)?;
    (chars.next() == Some(':')).then(|| letter.to_ascii_uppercase())
}

/// How long to keep trying to lock a volume, and how often. Explorer,
/// antivirus scanners and the indexer open a drive's volumes for a moment
/// after it is plugged in; a lock refused then is worth asking again.
pub const LOCK_TRIES: u32 = 20;
pub const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// Call `op` until it succeeds or `tries` calls have failed, waiting `wait`
/// between them; the last error if none succeeded.
pub fn retry<T, E>(
    tries: u32,
    wait: std::time::Duration,
    mut op: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let mut left = tries.max(1);
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) if left <= 1 => return Err(e),
            Err(_) => {
                left -= 1;
                std::thread::sleep(wait);
            }
        }
    }
}

/// What to tell someone whose drive could not be locked: which drive, by
/// letter where it has any, and what to do.
pub fn busy_message(letters: &[char]) -> String {
    let which = if letters.is_empty() {
        "the drive".to_string()
    } else {
        letters
            .iter()
            .map(|l| format!("{l}:"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "another program is using {which}; close any windows or programs using it and try again"
    )
}

pub fn disk_path(number: u32) -> String {
    format!(r"\\.\PhysicalDrive{number}")
}

/// Trim the space-padded strings `STORAGE_DEVICE_DESCRIPTOR` returns and join
/// vendor and product: `"SanDisk", "Ultra   "` → `"SanDisk Ultra"`.
pub fn model(vendor: &str, product: &str) -> String {
    [vendor.trim(), product.trim()]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `diskpart` script that turns disk `number` back into an ordinary
/// drive: GPT, one exFAT partition named `label`, given a drive letter.
/// `label` must already have passed `libflasher_core::platform::volume_label`,
/// which leaves nothing that could escape the quotes.
pub fn restore_script(number: u32, label: &str) -> String {
    format!(
        "select disk {number}\r\n\
         clean\r\n\
         convert gpt\r\n\
         create partition primary\r\n\
         format fs=exfat label=\"{label}\" quick\r\n\
         assign\r\n\
         exit\r\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(bus_type: i32, removable_media: bool, is_system: bool, size: u64) -> DiskFacts {
        DiskFacts {
            number: 1,
            bus_type,
            removable_media,
            size,
            is_system,
        }
    }

    #[test]
    fn offers_only_removable_non_system_disks() {
        assert!(
            is_candidate(&disk(bus::USB, false, false, 32 << 30)),
            "USB stick"
        );
        assert!(
            is_candidate(&disk(bus::SD, false, false, 32 << 30)),
            "SD card"
        );
        assert!(
            is_candidate(&disk(11 /* SATA */, true, false, 32 << 30)),
            "removable media on any bus"
        );
        assert!(
            !is_candidate(&disk(11, false, false, 512 << 30)),
            "internal SATA disk"
        );
        assert!(
            !is_candidate(&disk(17 /* NVMe */, false, false, 512 << 30)),
            "internal NVMe disk"
        );
        assert!(
            !is_candidate(&disk(bus::USB, false, true, 512 << 30)),
            "Windows running from USB"
        );
        assert!(
            !is_candidate(&disk(bus::SD, true, false, 0)),
            "card reader with no card"
        );
    }

    #[test]
    fn disk_paths_round_trip_and_reject_lookalikes() {
        assert_eq!(disk_number(&disk_path(3)), Some(3));
        assert_eq!(disk_number(r"\\.\PhysicalDrive12"), Some(12));
        for bad in [
            r"\\.\PhysicalDrive",
            r"\\.\PhysicalDrive01",
            r"\\.\PhysicalDrive1x",
            r"\\.\C:",
            r"C:\",
            "/dev/sdb",
            r"\\.\PhysicalDrive-1",
        ] {
            assert_eq!(disk_number(bad), None, "{bad}");
        }
    }

    #[test]
    fn volume_names_open_the_volume_not_its_root() {
        let guid = r"\\?\Volume{3f2504e0-4f89-11d3-9a0c-0305e82c3301}";
        assert_eq!(volume_open_path(&format!("{guid}\\")), guid);
        assert_eq!(volume_open_path(guid), guid);
    }

    #[test]
    fn models_are_trimmed_and_joined() {
        assert_eq!(model("SanDisk ", "Ultra     "), "SanDisk Ultra");
        assert_eq!(model("   ", "ProductCode"), "ProductCode");
        assert_eq!(model("", ""), "");
    }

    #[test]
    fn restore_script_targets_only_the_given_disk() {
        let s = restore_script(4, "MY STICK");
        assert!(s.starts_with("select disk 4\r\n"));
        assert!(s.contains("format fs=exfat label=\"MY STICK\" quick"));
        assert_eq!(s.matches("select disk").count(), 1);
        // The label validator is what keeps quotes out; check it holds.
        assert!(libflasher_core::platform::volume_label("A\"B").is_err());
    }

    #[test]
    fn reads_disk_extents() {
        let mut buf = vec![0u8; 8 + 3 * 24];
        buf[0] = 3;
        for (i, disk) in [2u32, 0, 2].iter().enumerate() {
            buf[8 + i * 24..12 + i * 24].copy_from_slice(&disk.to_le_bytes());
        }
        assert_eq!(extent_disks(&buf), [0, 2]);
        // A count larger than the buffer holds reads only what is there.
        buf[0] = 9;
        assert_eq!(extent_disks(&buf), [0, 2]);
        assert!(extent_disks(&[1, 0]).is_empty());
    }

    #[test]
    fn page_file_letters() {
        assert_eq!(pagefile_letter(r"\??\C:\pagefile.sys"), Some('C'));
        assert_eq!(pagefile_letter(r"d:\pagefile.sys"), Some('D'));
        assert_eq!(pagefile_letter(r"\??\Volume{x}\pagefile.sys"), None);
        assert_eq!(pagefile_letter(""), None);
    }

    #[test]
    fn retries_until_it_works_or_gives_up() {
        let zero = std::time::Duration::ZERO;
        let mut calls = 0;
        let r: Result<u32, &str> = retry(5, zero, || {
            calls += 1;
            if calls < 3 {
                Err("busy")
            } else {
                Ok(calls)
            }
        });
        assert_eq!(r, Ok(3));
        let mut calls = 0;
        let r: Result<(), &str> = retry(4, zero, || {
            calls += 1;
            Err("busy")
        });
        assert_eq!((r, calls), (Err("busy"), 4));
        assert_eq!(
            LOCK_TRIES as u128 * LOCK_WAIT.as_millis(),
            5000,
            "about 5 s"
        );
    }

    #[test]
    fn busy_messages_name_the_drive() {
        assert!(busy_message(&['E', 'F']).contains("E:, F:"));
        assert!(busy_message(&[]).contains("the drive"));
    }

    #[test]
    fn bus_names() {
        assert_eq!(bus_name(bus::USB), "USB");
        assert_eq!(bus_name(bus::MMC), "SD");
        assert_eq!(bus_name(11), "Removable");
    }
}
