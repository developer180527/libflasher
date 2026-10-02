//! The contract between the core and an operating system.

use std::io::{Read, Seek, Write};

use crate::{Error, Result};

/// A whole physical disk the user could write an image to.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeviceInfo {
    /// The OS's name for the whole disk: `/dev/disk4`, `/dev/sdb`,
    /// `\\.\PhysicalDrive2`. Opaque to the core; only the platform interprets it.
    pub path: String,
    /// Vendor and model, as the OS reports them.
    pub model: String,
    /// Capacity in bytes.
    pub size: u64,
    /// "USB", "SD", "Thunderbolt", … for display only.
    pub bus: String,
    /// Mounted volumes on this disk. They are unmounted before writing.
    pub mountpoints: Vec<String>,
}

impl DeviceInfo {
    /// For platform implementations: everything a listing knows about a disk.
    pub fn new(
        path: impl Into<String>,
        model: impl Into<String>,
        size: u64,
        bus: impl Into<String>,
        mountpoints: Vec<String>,
    ) -> Self {
        Self {
            path: path.into(),
            model: model.into(),
            size,
            bus: bus.into(),
            mountpoints,
        }
    }

    /// Whether two listings describe the same physical disk. Mounts are left
    /// out: they change while a disk stays put, and are unmounted anyway.
    pub fn same_disk(&self, other: &DeviceInfo) -> bool {
        self.path == other.path
            && self.model == other.model
            && self.size == other.size
            && self.bus == other.bus
    }

    /// Short enough for a picker: `SanDisk Ultra · 28.7 GB USB`. The path is
    /// left out because it can be long; show it next to the picker instead.
    pub fn display_name(&self) -> String {
        let model = if self.model.is_empty() {
            "Unknown device"
        } else {
            &self.model
        };
        format!("{model} · {} {}", human_size(self.size), self.bus)
    }
}

/// An open, exclusively-held whole disk.
///
/// Reads and writes must be multiples of [`RawDevice::sector_size`] on some
/// platforms (macOS `/dev/rdisk*`, Windows physical drives); the flash pipeline
/// guarantees that.
pub trait RawDevice: Read + Write + Seek + Send {
    /// The size every read and write must be a multiple of.
    fn sector_size(&self) -> u32;
    /// The device's capacity in bytes.
    fn size(&self) -> u64;
    /// Flush every write to the medium, not just to the OS cache.
    fn sync(&mut self) -> Result<()>;
}

/// One operating system's way of finding, claiming and releasing disks.
///
/// Implementations must only ever return *removable/external* disks from
/// [`Platform::list_devices`] and must refuse to open the disk the running
/// system lives on, whatever path they are handed.
pub trait Platform: Send + Sync {
    /// "macOS", "Linux", "Windows", …
    fn name(&self) -> &'static str;

    /// The removable disks attached now. Never the system disk.
    fn list_devices(&self) -> Result<Vec<DeviceInfo>>;

    /// Unmount every volume on the disk, obtain the privilege to write it (an
    /// OS password prompt, if needed) and open it for raw read/write.
    fn open_device(&self, device: &DeviceInfo) -> Result<Box<dyn RawDevice>>;

    /// Release the disk so the user can pull it out.
    fn eject(&self, device: &DeviceInfo) -> Result<()>;

    /// Keep the computer from sleeping until the returned guard is dropped.
    /// `None` where the OS offers no way, or it failed: the caller carries on,
    /// it just cannot promise the machine stays awake.
    fn keep_awake(&self, _reason: &str) -> Option<Box<dyn std::any::Any + Send>> {
        None
    }

    /// Erase the whole disk back to an ordinary storage drive: one partition,
    /// exFAT, named `label` (already checked by [`volume_label`]).
    fn restore(&self, device: &DeviceInfo, _label: &str) -> Result<()> {
        Err(Error::Unsupported(format!(
            "restoring {} on {}",
            device.path,
            self.name()
        )))
    }

    /// The disk at `device.path` as listed now — but only if it is still the
    /// one the user picked.
    ///
    /// Device paths are reused: unplug a stick and the next disk attached can
    /// get the same `/dev/disk4`. Without this check, a list shown a minute ago
    /// would point at whatever disk holds that name now.
    fn still_listed(&self, device: &DeviceInfo) -> Result<DeviceInfo> {
        let now = self
            .list_devices()?
            .into_iter()
            .find(|d| d.path == device.path);
        match now {
            Some(d) if d.same_disk(device) => Ok(d),
            Some(_) => Err(Error::Refused {
                device: device.path.clone(),
                reason: "a different drive is now at this path; refresh the drive list".into(),
            }),
            None => Err(Error::Refused {
                device: device.path.clone(),
                reason: "the drive is no longer connected; refresh the drive list".into(),
            }),
        }
    }

    /// [`Platform::open_device`] after [`Platform::still_listed`]. Use this,
    /// not `open_device`, for anything a person chose from a list.
    fn open_listed(&self, device: &DeviceInfo) -> Result<Box<dyn RawDevice>> {
        self.open_device(&self.still_listed(device)?)
    }

    /// [`Platform::restore`] after [`Platform::still_listed`].
    fn restore_listed(&self, device: &DeviceInfo, label: &str) -> Result<()> {
        let label = volume_label(label).map_err(|reason| Error::Refused {
            device: device.path.clone(),
            reason,
        })?;
        self.restore(&self.still_listed(device)?, &label)
    }
}

/// A volume name every OS will accept for an exFAT drive: 1–11 characters of
/// letters, digits, space, `-` and `_` (FAT's limit, the strictest in use).
pub fn volume_label(label: &str) -> std::result::Result<String, String> {
    let l = label.trim();
    if l.is_empty() {
        return Err("give the drive a name".into());
    }
    if l.chars().count() > 11 {
        return Err("a drive name can be at most 11 characters".into());
    }
    if let Some(c) = l
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_')))
    {
        return Err(format!(
            "a drive name cannot contain {c:?}; use letters, digits, space, - or _"
        ));
    }
    Ok(l.to_string())
}

/// `28.7 GB`: decimal units, as drive makers and file managers count.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}
