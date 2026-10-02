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
    fn bus_names() {
        assert_eq!(bus_name(bus::USB), "USB");
        assert_eq!(bus_name(bus::MMC), "SD");
        assert_eq!(bus_name(11), "Removable");
    }
}
