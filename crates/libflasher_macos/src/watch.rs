//! Disk plug-in and removal notifications from DiskArbitration, the
//! framework `diskutil` and Finder use.
//!
//! DiskArbitration delivers callbacks on a CFRunLoop, so the watcher owns a
//! thread running one. The loop runs in half-second slices and checks a stop
//! flag between them: stopping a run loop from another thread races with it
//! starting, and a slice costs nothing while idle.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use libflasher_core::OnChange;

type CFTypeRef = *const c_void;
type DASessionRef = *mut c_void;
type DADiskRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type DiskCallback = extern "C" fn(disk: DADiskRef, context: *mut c_void);

#[link(name = "DiskArbitration", kind = "framework")]
extern "C" {
    fn DASessionCreate(allocator: CFTypeRef) -> DASessionRef;
    fn DARegisterDiskAppearedCallback(
        session: DASessionRef,
        matching: CFTypeRef,
        callback: DiskCallback,
        context: *mut c_void,
    );
    fn DARegisterDiskDisappearedCallback(
        session: DASessionRef,
        matching: CFTypeRef,
        callback: DiskCallback,
        context: *mut c_void,
    );
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

extern "C" fn changed(_disk: DADiskRef, context: *mut c_void) {
    // SAFETY: `context` is the `OnChange` the thread below keeps alive for as
    // long as the session can call back.
    let on_change = unsafe { &*(context as *const OnChange) };
    on_change();
}

/// Watching until dropped.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    pub fn start(on_change: OnChange) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("libflasher-diskarbitration".into())
            .spawn(move || {
                // SAFETY: plain CoreFoundation/DiskArbitration calls on this
                // thread's own run loop; the context outlives the session,
                // which is unscheduled and released before it is dropped.
                unsafe {
                    let session = DASessionCreate(std::ptr::null());
                    if session.is_null() {
                        let _ = ready_tx.send(false);
                        return;
                    }
                    let context: *mut OnChange = Box::into_raw(Box::new(on_change));
                    DARegisterDiskAppearedCallback(
                        session,
                        std::ptr::null(),
                        changed,
                        context.cast(),
                    );
                    DARegisterDiskDisappearedCallback(
                        session,
                        std::ptr::null(),
                        changed,
                        context.cast(),
                    );
                    let run_loop = CFRunLoopGetCurrent();
                    DASessionScheduleWithRunLoop(session, run_loop, kCFRunLoopDefaultMode);
                    let _ = ready_tx.send(true);
                    while !flag.load(Ordering::Relaxed) {
                        CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.5, 0);
                    }
                    DASessionUnscheduleFromRunLoop(session, run_loop, kCFRunLoopDefaultMode);
                    CFRelease(session as CFTypeRef);
                    drop(Box::from_raw(context));
                }
            })
            .ok()?;
        if ready_rx.recv().ok()? {
            Some(Self {
                stop,
                thread: Some(thread),
            })
        } else {
            let _ = thread.join();
            None
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
