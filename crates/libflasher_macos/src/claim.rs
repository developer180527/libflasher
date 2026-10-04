//! Holding a disk while it is written: a DiskArbitration mount-approval
//! callback that refuses every mount of the disk or its partitions until
//! dropped.
//!
//! Unmounting is not enough on its own. macOS mounts a disk's volumes again
//! when anything asks — Finder, Spotlight, `diskutil mount`, another app —
//! and a write under a mounted volume fails part-way (or worse, the volume
//! writes back over the image). Every such mount goes through
//! DiskArbitration, which asks each session's approval callback first.
//!
//! A second guard backs the refusal: every quarter second the hold reads the
//! kernel's mount table, and any volume of the disk found mounted (by a
//! macOS that ignores the refusal, or a mount that does not go through
//! DiskArbitration) is force-unmounted at once and remembered:
//! [`Claim::remounted`]. A write under way may have been disturbed, so the
//! open disk fails from then on rather than carry on. The mount table, not
//! DiskArbitration's notifications, because on macOS 26 a mount that went
//! through raised no "description changed" callback. (`diskutil mountDisk`
//! reports success there even when the refusal held, so only the mount
//! table says what happened.)
//!
//! Not `DADiskClaim`: on macOS 27 its completion callback never runs, even
//! from a minimal C program, so a claim cannot be confirmed. Refusing mounts
//! is what matters, and it needs no privilege.
//!
//! Like `watch.rs`, the session lives on a thread running its own run loop.
//!
//! A session's callbacks are live only once diskarbitrationd has taken the
//! session in, which happens asynchronously and takes longer on a loaded
//! machine. It then reports every disk present to the session's "appeared"
//! callback, so hearing about our own disk is the sign the refusal is in
//! place; [`Claim::take`] returns only after that.

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

type CFTypeRef = *const c_void;
type DASessionRef = *mut c_void;
type DADiskRef = *mut c_void;
type DADissenterRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type DAReturn = i32;

type ApprovalCallback = extern "C" fn(disk: DADiskRef, context: *mut c_void) -> DADissenterRef;
type DiskCallback = extern "C" fn(disk: DADiskRef, context: *mut c_void);
/// `kDADiskUnmountOptionForce`.
const UNMOUNT_FORCE: u32 = 0x0008_0000;

/// `kDAReturnExclusiveAccess`: "the disk is in exclusive use".
const EXCLUSIVE_ACCESS: DAReturn = 0xF8DA_0004_u32 as i32;
/// How long diskarbitrationd has to take the session in.
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[link(name = "DiskArbitration", kind = "framework")]
extern "C" {
    fn DASessionCreate(allocator: CFTypeRef) -> DASessionRef;
    fn DADiskGetBSDName(disk: DADiskRef) -> *const c_char;
    fn DARegisterDiskMountApprovalCallback(
        session: DASessionRef,
        matching: CFTypeRef,
        callback: ApprovalCallback,
        context: *mut c_void,
    );
    fn DARegisterDiskAppearedCallback(
        session: DASessionRef,
        matching: CFTypeRef,
        callback: DiskCallback,
        context: *mut c_void,
    );
    fn DADiskCreateFromBSDName(
        allocator: CFTypeRef,
        session: DASessionRef,
        name: *const c_char,
    ) -> DADiskRef;
    fn DADiskUnmount(disk: DADiskRef, options: u32, callback: CFTypeRef, context: *mut c_void);
    fn DAUnregisterCallback(session: DASessionRef, callback: *mut c_void, context: *mut c_void);
    fn DAUnregisterApprovalCallback(
        session: DASessionRef,
        callback: *mut c_void,
        context: *mut c_void,
    );
    fn DADissenterCreate(
        allocator: CFTypeRef,
        status: DAReturn,
        string: CFTypeRef,
    ) -> DADissenterRef;
    fn DASessionScheduleWithRunLoop(session: DASessionRef, run_loop: CFRunLoopRef, mode: CFTypeRef);
    fn DASessionUnscheduleFromRunLoop(
        session: DASessionRef,
        run_loop: CFRunLoopRef,
        mode: CFTypeRef,
    );
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRunInMode(mode: CFTypeRef, seconds: f64, return_after_source_handled: u8) -> i32;
    fn CFRelease(cf: CFTypeRef);
    static kCFRunLoopDefaultMode: CFTypeRef;
}

/// Whether `name` (a BSD name such as `disk4s1`) is the disk `whole` or one
/// of its partitions.
fn is_on(name: &str, whole: &str) -> bool {
    name.strip_prefix(whole).is_some_and(|rest| {
        rest.is_empty()
            || rest
                .strip_prefix('s')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// What the callbacks share with the claim: the disk, whether
/// DiskArbitration has reported it to this session, and how many mounts of
/// it were refused.
struct Shared {
    whole: CString,
    seen: AtomicBool,
    refused: AtomicUsize,
    /// A volume of the disk was mounted while held (and unmounted again).
    remounted: AtomicBool,
    /// Whether to refuse mounts; off only in tests, to stand in for a macOS
    /// that ignores the refusal.
    refuse: bool,
}

impl Shared {
    /// Whether `disk` is ours: the whole disk or one of its partitions.
    fn ours(&self, disk: DADiskRef) -> bool {
        // SAFETY: DiskArbitration hands us a valid disk; its BSD name, when
        // there is one, is a C string it owns for the call.
        let name = unsafe { DADiskGetBSDName(disk) };
        if name.is_null() {
            return false;
        }
        let name = unsafe { CStr::from_ptr(name) };
        matches!((name.to_str(), self.whole.to_str()), (Ok(n), Ok(w)) if is_on(n, w))
    }
}

extern "C" fn refuse_mount(disk: DADiskRef, context: *mut c_void) -> DADissenterRef {
    // SAFETY: `context` is the claim's `Shared`, alive while registered.
    let shared = unsafe { &*(context as *const Shared) };
    if !shared.ours(disk) {
        return std::ptr::null_mut();
    }
    shared.refused.fetch_add(1, Ordering::Relaxed);
    if !shared.refuse {
        return std::ptr::null_mut();
    }
    // DiskArbitration releases the dissenter it is given.
    unsafe { DADissenterCreate(std::ptr::null(), EXCLUSIVE_ACCESS, std::ptr::null()) }
}

/// BSD names (`disk4s1`) of the volumes of `whole` the kernel has mounted.
fn mounted_volumes(whole: &str) -> Vec<String> {
    let mut list: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo points `list` at its own buffer of `n` entries,
    // valid until the next call on this thread.
    let n = unsafe { libc::getmntinfo(&mut list, libc::MNT_NOWAIT) };
    if n <= 0 || list.is_null() {
        return Vec::new();
    }
    let mounts = unsafe { std::slice::from_raw_parts(list, n as usize) };
    mounts
        .iter()
        .filter_map(|m| {
            let from = unsafe { CStr::from_ptr(m.f_mntfromname.as_ptr()) };
            let name = from.to_str().ok()?.strip_prefix("/dev/")?;
            is_on(name, whole).then(|| name.to_string())
        })
        .collect()
}

extern "C" fn appeared(disk: DADiskRef, context: *mut c_void) {
    // SAFETY: as in `refuse_mount`.
    let shared = unsafe { &*(context as *const Shared) };
    if shared.ours(disk) {
        shared.seen.store(true, Ordering::Relaxed);
    }
}

/// The disk is held until this is dropped.
pub struct Claim {
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Claim {
    /// Refuse mounts of the whole disk `bsd_name` (`disk4`) and its
    /// partitions until dropped. Returns once the refusal is in place; `Err`
    /// if DiskArbitration is unavailable or never reports the disk.
    pub fn take(bsd_name: &str) -> Result<Self, String> {
        Self::with(bsd_name, true)
    }

    fn with(bsd_name: &str, refuse: bool) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            whole: CString::new(bsd_name).map_err(|e| e.to_string())?,
            seen: AtomicBool::new(false),
            refused: AtomicUsize::new(0),
            remounted: AtomicBool::new(false),
            refuse,
        });
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let ours = shared.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let thread = std::thread::Builder::new()
            .name("libflasher-claim".into())
            .spawn(move || {
                // SAFETY: CoreFoundation/DiskArbitration calls on this
                // thread's own run loop. `ours` outlives both callbacks,
                // which are unregistered before it is dropped.
                unsafe {
                    let session = DASessionCreate(std::ptr::null());
                    if session.is_null() {
                        let _ = ready_tx.send(Err("no DiskArbitration session".into()));
                        return;
                    }
                    let context = Arc::as_ptr(&ours) as *mut c_void;
                    DARegisterDiskMountApprovalCallback(
                        session,
                        std::ptr::null(),
                        refuse_mount,
                        context,
                    );
                    DARegisterDiskAppearedCallback(session, std::ptr::null(), appeared, context);
                    let run_loop = CFRunLoopGetCurrent();
                    DASessionScheduleWithRunLoop(session, run_loop, kCFRunLoopDefaultMode);
                    let end = std::time::Instant::now() + READY_TIMEOUT;
                    while !ours.seen.load(Ordering::Relaxed) && std::time::Instant::now() < end {
                        CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.1, 1);
                    }
                    let ready = ours.seen.load(Ordering::Relaxed);
                    let _ = ready_tx.send(if ready {
                        Ok(())
                    } else {
                        Err("macOS did not report the drive; it may have been unplugged".into())
                    });
                    let whole = ours.whole.to_str().unwrap_or_default().to_string();
                    while ready && !flag.load(Ordering::Relaxed) {
                        CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.25, 0);
                        for volume in mounted_volumes(&whole) {
                            ours.remounted.store(true, Ordering::Relaxed);
                            let Ok(name) = CString::new(volume) else {
                                continue;
                            };
                            let disk =
                                DADiskCreateFromBSDName(std::ptr::null(), session, name.as_ptr());
                            if !disk.is_null() {
                                DADiskUnmount(
                                    disk,
                                    UNMOUNT_FORCE,
                                    std::ptr::null(),
                                    std::ptr::null_mut(),
                                );
                                CFRelease(disk as CFTypeRef);
                            }
                        }
                    }
                    DAUnregisterCallback(session, appeared as *mut c_void, context);
                    DAUnregisterApprovalCallback(session, refuse_mount as *mut c_void, context);
                    DASessionUnscheduleFromRunLoop(session, run_loop, kCFRunLoopDefaultMode);
                    CFRelease(session as CFTypeRef);
                }
            })
            .map_err(|e| e.to_string())?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                shared,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err("the claim thread stopped".into())
            }
        }
    }

    /// Whether macOS mounted a volume of the disk while it was held. It was
    /// unmounted again, but anything written meanwhile may be damaged.
    pub fn remounted(&self) -> bool {
        self.shared.remounted.load(Ordering::Relaxed)
    }

    /// How many mounts of the disk have been refused so far.
    #[cfg(test)]
    fn refused(&self) -> usize {
        self.shared.refused.load(Ordering::Relaxed)
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn command_text(tool: &str, args: &[&str]) -> String {
        Command::new(tool)
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    }

    /// Whether any volume of `/dev/diskN` is mounted.
    fn mounted_at(whole: &str) -> bool {
        command_text("mount", &[])
            .lines()
            .any(|l| l.starts_with(&format!("{whole}s")) || l.starts_with(&format!("{whole} ")))
    }

    #[test]
    fn names_on_a_disk() {
        assert!(is_on("disk4", "disk4"));
        assert!(is_on("disk4s1", "disk4"));
        assert!(is_on("disk4s12", "disk4"));
        assert!(!is_on("disk41", "disk4"));
        assert!(!is_on("disk41s1", "disk4"));
        assert!(!is_on("disk4s", "disk4"));
        assert!(!is_on("disk4s1x", "disk4"));
    }

    /// A disk image with a FAT volume, attached unmounted: while held, a
    /// `diskutil mountDisk` is refused, or undone within moments and
    /// recorded; once the hold is dropped, mounting works.
    /// Needs no root.
    #[test]
    fn nothing_stays_mounted_while_held() {
        held_disk_test(true);
    }

    /// As macOS 26 behaves: the refusal is ignored and the mount goes
    /// through. It must be undone and recorded, on any macOS.
    #[test]
    fn a_mount_that_gets_through_is_undone() {
        held_disk_test(false);
    }

    fn held_disk_test(refuse: bool) {
        let dir =
            std::env::temp_dir().join(format!("libflasher_claim_{}_{refuse}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("c.dmg");
        let ok = Command::new("hdiutil")
            .args([
                "create",
                "-size",
                "40m",
                "-fs",
                "MS-DOS",
                "-volname",
                "CLAIMTEST",
            ])
            .arg(&img)
            .output()
            .unwrap();
        assert!(
            ok.status.success(),
            "{}",
            String::from_utf8_lossy(&ok.stderr)
        );
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
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let whole = text.split_whitespace().next().unwrap().to_string(); // /dev/diskN
        let bsd = whole.trim_start_matches("/dev/").to_string();

        let mount = || {
            Command::new("diskutil")
                .args(["mountDisk", &whole])
                .output()
                .unwrap()
                .status
                .success()
        };
        let result = std::panic::catch_unwind(|| {
            let claim = Claim::with(&bsd, refuse).expect("claim");
            // diskutil's exit status says nothing: macOS 26 reports success
            // even when the refusal held. The mount table is the truth.
            let _ = mount();
            assert!(claim.refused() > 0, "the refusal was never asked");
            let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while mounted_at(&whole) && std::time::Instant::now() < end {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let version = command_text("sw_vers", &["-productVersion"]);
            assert!(
                !mounted_at(&whole),
                "still mounted 5 s after a mount while held (macOS {version}, refusal {refuse})"
            );
            if !refuse {
                // The mount went through, so it must have been noticed.
                assert!(claim.remounted(), "a mount while held went unrecorded");
            }
            eprintln!(
                "macOS {version}, refusal {refuse}: {}",
                if claim.remounted() {
                    "mounted, then undone"
                } else {
                    "never mounted"
                }
            );
            drop(claim);
            assert!(mount(), "not mountable once released");
        });
        let _ = Command::new("hdiutil")
            .args(["detach", "-force", &whole])
            .output();
        let _ = std::fs::remove_dir_all(&dir);
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }
}
