//! The Windows calls. Handles are `std::fs::File`s opened with the right
//! access and share modes, so they close themselves; `DeviceIoControl` gets
//! their raw handle.

use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem::{size_of, zeroed};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::process::{Command, Stdio};

use libflasher_core::{DeviceInfo, Error, Platform, RawDevice, Result};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, FILE_FLAG_NO_BUFFERING,
    FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ, FILE_SHARE_WRITE,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::Ioctl::{
    PropertyStandardQuery, StorageDeviceProperty, DISK_GEOMETRY_EX, FSCTL_DISMOUNT_VOLUME,
    FSCTL_LOCK_VOLUME, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_UPDATE_PROPERTIES,
    IOCTL_STORAGE_EJECT_MEDIA, IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY,
    STORAGE_DEVICE_DESCRIPTOR, STORAGE_DEVICE_NUMBER, STORAGE_PROPERTY_QUERY,
};
use windows_sys::Win32::System::Power::{
    PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest,
};
use windows_sys::Win32::System::Registry::{
    RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_MULTI_SZ, RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows_sys::Win32::System::Threading::{POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::policy::{self, DiskFacts};

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
/// Physical drives are numbered from 0 with no gaps guaranteed; this is far
/// past any real machine.
const MAX_DISKS: u32 = 64;

pub struct Windows;

impl Platform for Windows {
    fn name(&self) -> &'static str {
        "Windows"
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        let system = system_disks()?;
        let mut out = Vec::new();
        for n in 0..MAX_DISKS {
            // Disks that do not exist fail to open; that is the end of nothing.
            let Ok(handle) = open_for_query(&policy::disk_path(n)) else {
                continue;
            };
            let Some((facts, model)) = describe(&handle, n, &system) else {
                continue;
            };
            if !(policy::is_candidate(&facts) || is_test_disk(&facts)) {
                continue;
            }
            out.push(DeviceInfo::new(
                policy::disk_path(n),
                model,
                facts.size,
                policy::bus_name(facts.bus_type),
                letters_on(n)
                    .into_iter()
                    .map(|l| format!("{l}:\\"))
                    .collect(),
            ));
        }
        Ok(out)
    }

    fn open_device(&self, device: &DeviceInfo) -> Result<Box<dyn RawDevice>> {
        let (n, facts) = checked(device)?;
        // Lock and dismount every volume on the disk, and hold the locks for
        // the whole write: Windows refuses raw writes over a mounted volume,
        // and would remount it the moment we let go.
        let mut locks = Vec::new();
        for path in volumes_on(n) {
            let vol = File::options()
                .access_mode(GENERIC_READ | GENERIC_WRITE)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(&path)
                .map_err(permission)?;
            ioctl(&vol, FSCTL_LOCK_VOLUME, &[], &mut [])
                .map_err(|e| tool(&format!("locking volume {path}"), e))?;
            ioctl(&vol, FSCTL_DISMOUNT_VOLUME, &[], &mut [])
                .map_err(|e| tool(&format!("dismounting volume {path}"), e))?;
            locks.push(vol);
        }
        let file = File::options()
            .access_mode(GENERIC_READ | GENERIC_WRITE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            // No OS cache, so verification reads the drive, not memory.
            .custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH)
            .open(&device.path)
            .map_err(permission)?;
        let sector = geometry(&file)
            .map(|g| g.Geometry.BytesPerSector)
            .unwrap_or(512)
            .max(512);
        Ok(Box::new(Disk {
            file,
            size: facts.size,
            sector,
            bounce: Bounce::new(),
            _locks: locks,
        }))
    }

    fn eject(&self, device: &DeviceInfo) -> Result<()> {
        let (n, _) = checked(device)?;
        let disk = open_for_query(&policy::disk_path(n))?;
        // Card readers honour this; USB sticks mostly report "not supported"
        // and are safe to pull anyway once flushed and closed, which they are.
        let _ = ioctl(&disk, IOCTL_STORAGE_EJECT_MEDIA, &[], &mut []);
        Ok(())
    }

    fn watch(&self, on_change: libflasher_core::OnChange) -> Option<Box<dyn std::any::Any + Send>> {
        crate::watch::Watcher::start(on_change)
            .map(|w| Box::new(w) as Box<dyn std::any::Any + Send>)
    }

    fn keep_awake(&self, reason: &str) -> Option<Box<dyn std::any::Any + Send>> {
        PowerRequest::new(reason).map(|r| Box::new(r) as Box<dyn std::any::Any + Send>)
    }

    /// `diskpart` is in every Windows and is what Disk Management uses.
    fn restore(&self, device: &DeviceInfo, label: &str) -> Result<()> {
        let (n, _) = checked(device)?;
        let script =
            std::env::temp_dir().join(format!("libflasher-restore-{}.txt", std::process::id()));
        std::fs::write(&script, policy::restore_script(n, label))?;
        let out = Command::new("diskpart")
            .arg("/s")
            .arg(&script)
            .stdin(Stdio::null())
            .output();
        let _ = std::fs::remove_file(&script);
        let out = out.map_err(|e| tool("diskpart", e))?;
        if !out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            let last = text
                .lines()
                .map(str::trim)
                .rfind(|l| !l.is_empty())
                .unwrap_or("failed");
            return Err(Error::Tool {
                tool: "diskpart".into(),
                message: last.into(),
            });
        }
        Ok(())
    }
}

/// Re-reads the disk behind `device.path` and refuses it unless it still
/// qualifies, whatever the caller was told when it was listed.
fn checked(device: &DeviceInfo) -> Result<(u32, DiskFacts)> {
    let refuse = |reason: &str| Error::Refused {
        device: device.path.clone(),
        reason: reason.into(),
    };
    let n = policy::disk_number(&device.path).ok_or_else(|| refuse("not a physical drive path"))?;
    let handle = open_for_query(&device.path)?;
    let (facts, _) = describe(&handle, n, &system_disks()?)
        .ok_or_else(|| refuse("could not read the drive's details"))?;
    if !(policy::is_candidate(&facts) || is_test_disk(&facts)) {
        return Err(refuse(
            "not a removable drive, or the drive Windows runs from",
        ));
    }
    Ok((n, facts))
}

/// CI's throwaway VHD, named in `FLASHER_TEST_DISK`; compiled in only for
/// that test. Never the system disk.
fn is_test_disk(facts: &DiskFacts) -> bool {
    #[cfg(feature = "test-virtual-disks")]
    if !facts.is_system
        && libflasher_core::conformance::test_disk().as_deref()
            == Some(policy::disk_path(facts.number).as_str())
    {
        return true;
    }
    let _ = facts;
    false
}

/// Opened with no access rights: enough for queries, needs no elevation.
fn open_for_query(path: &str) -> Result<File> {
    Ok(File::options()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)?)
}

fn describe(disk: &File, number: u32, system: &[u32]) -> Option<(DiskFacts, String)> {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut buf = vec![0u8; 1024];
    let n = ioctl(
        disk,
        IOCTL_STORAGE_QUERY_PROPERTY,
        as_bytes(&query),
        &mut buf,
    )
    .ok()?;
    if n < size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
        return None;
    }
    // SAFETY: the buffer holds at least one descriptor; read unaligned since a
    // Vec<u8> promises no alignment.
    let desc: STORAGE_DEVICE_DESCRIPTOR = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
    let text = |offset: u32| -> String {
        let o = offset as usize;
        if o == 0 || o >= n {
            return String::new();
        }
        let end = buf[o..n].iter().position(|&b| b == 0).map_or(n, |e| o + e);
        String::from_utf8_lossy(&buf[o..end]).into_owned()
    };
    let size = geometry(disk)
        .map(|g| g.DiskSize.max(0) as u64)
        .unwrap_or(0);
    let facts = DiskFacts {
        number,
        bus_type: desc.BusType,
        removable_media: desc.RemovableMedia != 0,
        size,
        is_system: system.contains(&number),
    };
    Some((
        facts,
        policy::model(&text(desc.VendorIdOffset), &text(desc.ProductIdOffset)),
    ))
}

fn geometry(disk: &File) -> Option<DISK_GEOMETRY_EX> {
    let mut buf = vec![0u8; 256];
    let n = ioctl(disk, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut buf).ok()?;
    if n < size_of::<DISK_GEOMETRY_EX>() {
        return None;
    }
    // SAFETY: as above.
    Some(unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) })
}

/// The physical disk number behind a volume (`\\.\C:`).
fn disk_of_volume(path: &str) -> Option<u32> {
    let vol = open_for_query(path).ok()?;
    let mut out = [0u8; size_of::<STORAGE_DEVICE_NUMBER>()];
    ioctl(&vol, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut out).ok()?;
    // SAFETY: `out` is exactly one STORAGE_DEVICE_NUMBER.
    let num: STORAGE_DEVICE_NUMBER = unsafe { std::ptr::read_unaligned(out.as_ptr().cast()) };
    Some(num.DeviceNumber)
}

/// Drive letters of volumes on disk `n`: what to show as its mount points.
fn letters_on(n: u32) -> Vec<char> {
    ('A'..='Z')
        .filter(|l| disk_of_volume(&format!(r"\\.\{l}:")) == Some(n))
        .collect()
}

/// Every mounted volume on disk `n`, as a path to open it by
/// (`\\?\Volume{…}`) — with a drive letter or without one. Windows refuses
/// raw writes over any mounted volume, lettered or not, so all of them are
/// locked. (A volume spanning disks reports an error for the disk-number
/// query and is left out; it is never removable.)
fn volumes_on(n: u32) -> Vec<String> {
    let mut out = Vec::new();
    let mut name = [0u16; 260];
    // SAFETY: `name` is valid for its length on every call; the find handle
    // is closed below.
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if find == INVALID_HANDLE_VALUE {
        return out;
    }
    loop {
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let path = policy::volume_open_path(&String::from_utf16_lossy(&name[..len]));
        if disk_of_volume(&path) == Some(n) {
            out.push(path);
        }
        // SAFETY: as above.
        if unsafe { FindNextVolumeW(find, name.as_mut_ptr(), name.len() as u32) } == 0 {
            break;
        }
    }
    // SAFETY: `find` came from FindFirstVolumeW and is closed once.
    unsafe { FindVolumeClose(find) };
    out
}

/// Every disk the running Windows needs: the one holding the Windows
/// directory (all of them, if that volume spans disks), the one holding the
/// EFI system partition, and those holding page files.
///
/// Fails rather than guessing: without the Windows disk, nothing can be
/// told apart from it, so nothing is listed or opened. The other two are
/// added when they can be found; they are on the Windows disk on nearly
/// every machine anyway.
fn system_disks() -> Result<Vec<u32>> {
    let fail = |why: String| Error::Tool {
        tool: "finding the Windows disk".into(),
        message: format!("could not tell which disk Windows runs from ({why})"),
    };
    let letter = windows_letter().ok_or_else(|| fail("no Windows directory".into()))?;
    let mut out = disks_of_volume(&format!(r"\\.\{letter}:")).map_err(|e| fail(e.to_string()))?;
    if out.is_empty() {
        return Err(fail("its volume reports no disk".into()));
    }
    const SETUP: &str = r"SYSTEM\Setup";
    if let Some(esp) =
        registry(SETUP, "SystemPartition", RRF_RT_REG_SZ).and_then(|v| v.into_iter().next())
    {
        // `\Device\HarddiskVolume1`, reachable through the global namespace.
        out.extend(disks_of_volume(&format!(r"\\?\GLOBALROOT{esp}")).unwrap_or_default());
    }
    const MEMORY: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management";
    for entry in registry(MEMORY, "ExistingPageFiles", RRF_RT_REG_MULTI_SZ).unwrap_or_default() {
        if let Some(l) = policy::pagefile_letter(&entry) {
            out.extend(disks_of_volume(&format!(r"\\.\{l}:")).unwrap_or_default());
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// The drive letter of the Windows directory.
fn windows_letter() -> Option<char> {
    let mut buf = [0u16; 260];
    let len = unsafe { GetSystemWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    let dir = String::from_utf16_lossy(&buf[..len.min(buf.len())]);
    dir.chars().next().filter(char::is_ascii_alphabetic)
}

/// Every disk a volume lies on.
fn disks_of_volume(path: &str) -> io::Result<Vec<u32>> {
    let vol = open_for_query(path).map_err(|e| match e {
        Error::Io(e) => e,
        other => io::Error::other(other.to_string()),
    })?;
    let mut out = vec![0u8; 8 + 32 * 24];
    let n = ioctl(&vol, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut out)?;
    Ok(policy::extent_disks(&out[..n]))
}

/// A string (`RRF_RT_REG_SZ`) or string list (`RRF_RT_REG_MULTI_SZ`) value
/// under `HKEY_LOCAL_MACHINE`, as its strings.
fn registry(key: &str, value: &str, kind: u32) -> Option<Vec<String>> {
    let key: Vec<u16> = key.encode_utf16().chain(Some(0)).collect();
    let value: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    let mut buf = vec![0u16; 4096];
    let mut bytes = (buf.len() * 2) as u32;
    // SAFETY: the names are NUL-terminated; `buf` is valid for `bytes`.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            kind,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != 0 {
        return None;
    }
    buf.truncate(bytes as usize / 2);
    Some(
        buf.split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect(),
    )
}

/// One `DeviceIoControl`; returns the bytes written to `out`.
fn ioctl(f: &File, code: u32, input: &[u8], out: &mut [u8]) -> io::Result<usize> {
    let mut returned = 0u32;
    // SAFETY: the buffers are valid for their lengths for the whole call, and
    // the call is synchronous (no OVERLAPPED).
    let ok = unsafe {
        DeviceIoControl(
            f.as_raw_handle() as HANDLE,
            code,
            if input.is_empty() {
                std::ptr::null()
            } else {
                input.as_ptr().cast::<c_void>()
            },
            input.len() as u32,
            if out.is_empty() {
                std::ptr::null_mut()
            } else {
                out.as_mut_ptr().cast::<c_void>()
            },
            out.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(returned as usize)
}

fn as_bytes<T>(v: &T) -> &[u8] {
    // SAFETY: reading the bytes of a plain-old-data FFI struct.
    unsafe { std::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>()) }
}

fn permission(e: io::Error) -> Error {
    if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
        Error::Permission(
            "writing a drive needs administrator rights: run this program as administrator".into(),
        )
    } else {
        Error::Io(e)
    }
}

fn tool(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Tool {
        tool: what.into(),
        message: e.to_string(),
    }
}

struct Disk {
    file: File,
    size: u64,
    sector: u32,
    /// Unbuffered I/O must come from aligned memory; callers' buffers are
    /// ordinary `Vec`s, so every request is copied through this.
    bounce: Bounce,
    /// Locked, dismounted volumes; dropping them unlocks, after the disk.
    _locks: Vec<File>,
}

impl Drop for Disk {
    fn drop(&mut self) {
        // Make Windows re-read the partition table we just wrote, so the
        // new volumes appear without replugging.
        let _ = ioctl(&self.file, IOCTL_DISK_UPDATE_PROPERTIES, &[], &mut []);
    }
}

impl Read for Disk {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min(BOUNCE);
        let got = self.file.read(&mut self.bounce.as_mut()[..n])?;
        buf[..got].copy_from_slice(&self.bounce.as_mut()[..got]);
        Ok(got)
    }
}
impl Write for Disk {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(BOUNCE);
        self.bounce.as_mut()[..n].copy_from_slice(&buf[..n]);
        self.file.write(&self.bounce.as_mut()[..n])
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
        // FlushFileBuffers: on a physical drive, also flushes its write cache.
        self.file.sync_all()?;
        Ok(())
    }
}

/// Bytes per unbuffered request: what the flash pipeline sends at once.
const BOUNCE: usize = 1 << 20;
/// Page alignment: more than any storage driver's alignment requirement
/// (`STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR` masks are at most a sector).
const ALIGN: usize = 4096;

/// `BOUNCE` bytes of page-aligned memory.
struct Bounce(std::ptr::NonNull<u8>);

// SAFETY: plain owned memory, used by one `Disk` at a time.
unsafe impl Send for Bounce {}

impl Bounce {
    fn layout() -> std::alloc::Layout {
        std::alloc::Layout::from_size_align(BOUNCE, ALIGN).expect("valid layout")
    }

    fn new() -> Self {
        // SAFETY: the layout has a non-zero size.
        let p = unsafe { std::alloc::alloc_zeroed(Self::layout()) };
        Bounce(
            std::ptr::NonNull::new(p)
                .unwrap_or_else(|| std::alloc::handle_alloc_error(Self::layout())),
        )
    }

    fn as_mut(&mut self) -> &mut [u8] {
        // SAFETY: `BOUNCE` initialized bytes, owned by `self`.
        unsafe { std::slice::from_raw_parts_mut(self.0.as_ptr(), BOUNCE) }
    }
}

impl Drop for Bounce {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { std::alloc::dealloc(self.0.as_ptr(), Self::layout()) };
    }
}

/// A "system required" power request: Windows does not sleep while it is
/// set. Unlike `SetThreadExecutionState` it belongs to a handle, not a
/// thread, so it can be dropped anywhere.
struct PowerRequest {
    handle: HANDLE,
    /// The reason string must outlive the request.
    _reason: Vec<u16>,
}

// SAFETY: a power request handle is a kernel object usable from any thread.
unsafe impl Send for PowerRequest {}

impl PowerRequest {
    fn new(reason: &str) -> Option<Self> {
        let mut text: Vec<u16> = reason.encode_utf16().chain(Some(0)).collect();
        let mut ctx: REASON_CONTEXT = unsafe { zeroed() };
        ctx.Version = 0; // POWER_REQUEST_CONTEXT_VERSION
        ctx.Flags = POWER_REQUEST_CONTEXT_SIMPLE_STRING;
        ctx.Reason.SimpleReasonString = text.as_mut_ptr();
        // SAFETY: `ctx` and the string it points to are valid for the call.
        let handle = unsafe { PowerCreateRequest(&ctx) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return None;
        }
        if unsafe { PowerSetRequest(handle, PowerRequestSystemRequired) } == 0 {
            unsafe { CloseHandle(handle) };
            return None;
        }
        Some(Self {
            handle,
            _reason: text,
        })
    }
}

impl Drop for PowerRequest {
    fn drop(&mut self) {
        unsafe {
            PowerClearRequest(self.handle, PowerRequestSystemRequired);
            CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs on Windows CI: whatever disks the runner has, listing works and
    /// never offers the disk Windows runs from.
    #[test]
    fn lists_without_offering_the_system_disk() {
        let devices = Windows.list_devices().unwrap();
        let system = system_disks().expect("the Windows directory's disk should be known");
        assert!(!system.is_empty());
        for d in &devices {
            let n = policy::disk_number(&d.path).unwrap();
            assert!(!system.contains(&n), "offered a system disk: {d:?}");
        }
    }

    #[test]
    fn refuses_to_open_the_system_disk() {
        let system = system_disks().expect("system disk")[0];
        let fake = DeviceInfo::new(
            policy::disk_path(system),
            "pretend stick",
            1 << 30,
            "USB",
            Vec::new(),
        );
        assert!(matches!(
            Windows.open_device(&fake),
            Err(Error::Refused { .. })
        ));
        assert!(matches!(
            Windows.restore(&fake, "X"),
            Err(Error::Refused { .. })
        ));
    }

    #[test]
    fn refuses_paths_that_are_not_physical_drives() {
        for path in [r"\\.\C:", r"C:\", r"\\.\PhysicalDrive", "/dev/sdb"] {
            let d = DeviceInfo::new(path, "", 1 << 30, "USB", Vec::new());
            assert!(
                matches!(Windows.open_device(&d), Err(Error::Refused { .. })),
                "{path}"
            );
        }
    }

    #[test]
    fn power_request_is_taken_and_released() {
        let guard = Windows
            .keep_awake("test")
            .expect("PowerCreateRequest should work");
        drop(guard);
    }

    /// See `libflasher_core::conformance`: runs in CI (elevated) against a
    /// VHD named in `FLASHER_TEST_DISK`.
    /// Attaching a VHD is a disk arrival. Needs an elevated process for
    /// `diskpart`, so it runs in CI's conformance job.
    #[test]
    #[ignore = "needs administrator rights to attach a VHD"]
    fn watch_notices_a_vhd_being_attached_and_removed() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir().join(format!("libflasher_watch_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let vhd = dir.join("w.vhdx");
        // diskpart only creates the file: a VHD it attaches belongs to its
        // session and can be detached when diskpart exits. Mount-DiskImage
        // keeps it attached until Dismount-DiskImage.
        let script = dir.join("dp.txt");
        std::fs::write(
            &script,
            format!(
                "create vdisk file=\"{}\" maximum=8 type=expandable\r\n",
                vhd.display()
            ),
        )
        .unwrap();
        let out = Command::new("diskpart")
            .arg("/s")
            .arg(&script)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        let powershell = |cmdlet: &str| {
            let cmd = format!("{cmdlet} -ImagePath '{}' | Out-Null", vhd.display());
            let out = Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", &cmd])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{cmdlet}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };

        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let guard = Windows.watch(Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        assert!(guard.is_some(), "CM_Register_Notification should work");
        let wait_for_more = |than: usize| {
            let end = Instant::now() + Duration::from_secs(15);
            while count.load(Ordering::SeqCst) <= than {
                assert!(Instant::now() < end, "no notification within 15 s");
                std::thread::sleep(Duration::from_millis(20));
            }
        };

        let before = count.load(Ordering::SeqCst);
        powershell("Mount-DiskImage");
        wait_for_more(before);
        let before = count.load(Ordering::SeqCst);
        powershell("Dismount-DiskImage");
        wait_for_more(before);

        drop(guard);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    #[ignore = "writes to the disk named in FLASHER_TEST_DISK"]
    #[cfg(feature = "test-virtual-disks")]
    fn conformance() {
        if let Some(path) = libflasher_core::conformance::test_disk() {
            libflasher_core::conformance::full_cycle(&Windows, &path);
        }
    }
}
