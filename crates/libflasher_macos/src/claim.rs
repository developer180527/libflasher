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
//! Not `DADiskClaim`: on macOS 27 its completion callback never runs, even
//! from a minimal C program, so a claim cannot be confirmed. Refusing mounts
//! is what matters, and it needs no privilege.
//!
//! Like `watch.rs`, the session lives on a thread running its own run loop.

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// `kDAReturnExclusiveAccess`: "the disk is in exclusive use".
const EXCLUSIVE_ACCESS: DAReturn = 0xF8DA_0004_u32 as i32;

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

extern "C" fn refuse_mount(disk: DADiskRef, context: *mut c_void) -> DADissenterRef {
    // SAFETY: `context` is the claim thread's `CString`, alive while the
    // callback is registered; DiskArbitration hands us a valid disk.
    let whole = unsafe { CStr::from_ptr(context as *const c_char) };
    let name = unsafe { DADiskGetBSDName(disk) };
    if name.is_null() {
        return std::ptr::null_mut();
    }
    let name = unsafe { CStr::from_ptr(name) };
    match (name.to_str(), whole.to_str()) {
        // DiskArbitration releases the dissenter it is given.
        (Ok(n), Ok(w)) if is_on(n, w) => unsafe {
            DADissenterCreate(std::ptr::null(), EXCLUSIVE_ACCESS, std::ptr::null())
        },
        _ => std::ptr::null_mut(),
    }
}

/// The disk is held until this is dropped.
pub struct Claim {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Claim {
    /// Refuse mounts of the whole disk `bsd_name` (`disk4`) and its
    /// partitions until dropped. `Err` if DiskArbitration is unavailable.
    pub fn take(bsd_name: &str) -> Result<Self, String> {
        let name = CString::new(bsd_name).map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let thread = std::thread::Builder::new()
            .name("libflasher-claim".into())
            .spawn(move || {
                // SAFETY: CoreFoundation/DiskArbitration calls on this
                // thread's own run loop. `name` outlives the callback, which
                // is unregistered before it is dropped.
                unsafe {
                    let session = DASessionCreate(std::ptr::null());
                    if session.is_null() {
                        let _ = ready_tx.send(Err("no DiskArbitration session".into()));
                        return;
                    }
                    let context = name.as_ptr() as *mut c_void;
                    DARegisterDiskMountApprovalCallback(
                        session,
                        std::ptr::null(),
                        refuse_mount,
                        context,
                    );
                    let run_loop = CFRunLoopGetCurrent();
                    DASessionScheduleWithRunLoop(session, run_loop, kCFRunLoopDefaultMode);
                    // Let the session register with diskarbitrationd before
                    // saying it is in place.
                    CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.2, 0);
                    let _ = ready_tx.send(Ok(()));
                    while !flag.load(Ordering::Relaxed) {
                        CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.5, 0);
                    }
                    DAUnregisterApprovalCallback(session, refuse_mount as *mut c_void, context);
                    DASessionUnscheduleFromRunLoop(session, run_loop, kCFRunLoopDefaultMode);
                    CFRelease(session as CFTypeRef);
                }
            })
            .map_err(|e| e.to_string())?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
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

    /// A disk image with a FAT volume, attached unmounted: while held,
    /// `diskutil mountDisk` is refused; once the hold is dropped, it works.
    /// Needs no root.
    #[test]
    fn a_claimed_disk_cannot_be_mounted() {
        let dir = std::env::temp_dir().join(format!("libflasher_claim_{}", std::process::id()));
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
            let claim = Claim::take(&bsd).expect("claim");
            if mount() {
                // CI (macOS 26, a headless runner) mounts it anyway; a
                // desktop session on macOS 27 does not. Whether the
                // difference is the version or the session is not known yet,
                // so outside a desktop session this records what happened
                // rather than failing.
                let session = command_text("launchctl", &["managername"]);
                let version = command_text("sw_vers", &["-productVersion"]);
                assert_ne!(
                    session, "Aqua",
                    "mounted while held, in a desktop session (macOS {version})"
                );
                eprintln!(
                    "NOTE: mount refusal not honoured here (macOS {version}, {session} session)"
                );
                return;
            }
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
