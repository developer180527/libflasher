//! Disk arrival and removal notifications from the Configuration Manager
//! (`CM_Register_Notification`, Windows 8+), for the disk device interface.
//! Windows calls back on a thread pool thread; no window or message loop.

use std::ffi::c_void;

use libflasher_core::OnChange;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Register_Notification, CM_Unregister_Notification, CM_NOTIFY_ACTION, CM_NOTIFY_EVENT_DATA,
    CM_NOTIFY_FILTER, CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CR_SUCCESS, HCMNOTIFICATION,
};
use windows_sys::Win32::System::Ioctl::GUID_DEVINTERFACE_DISK;

unsafe extern "system" fn changed(
    _notify: HCMNOTIFICATION,
    context: *const c_void,
    _action: CM_NOTIFY_ACTION,
    _data: *const CM_NOTIFY_EVENT_DATA,
    _size: u32,
) -> u32 {
    // SAFETY: `context` is the boxed `OnChange` the Watcher keeps alive until
    // after unregistering, which waits for callbacks in flight.
    let on_change = unsafe { &*(context as *const OnChange) };
    on_change();
    0 // ERROR_SUCCESS
}

pub struct Watcher {
    handle: HCMNOTIFICATION,
    context: *mut OnChange,
}

// SAFETY: the registration handle may be unregistered from any thread, and
// the context is only read (through `Fn`, which is Sync) by callbacks.
unsafe impl Send for Watcher {}

impl Watcher {
    pub fn start(on_change: OnChange) -> Option<Self> {
        let context = Box::into_raw(Box::new(on_change));
        // SAFETY: zeroed is a valid CM_NOTIFY_FILTER; the fields set are the
        // ones the device-interface filter type reads.
        let mut filter: CM_NOTIFY_FILTER = unsafe { std::mem::zeroed() };
        filter.cbSize = std::mem::size_of::<CM_NOTIFY_FILTER>() as u32;
        filter.FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE;
        filter.u.DeviceInterface.ClassGuid = GUID_DEVINTERFACE_DISK;
        let mut handle: HCMNOTIFICATION = std::ptr::null_mut();
        // SAFETY: valid filter and out-pointer; the callback and context
        // stay valid until CM_Unregister_Notification returns.
        let r = unsafe {
            CM_Register_Notification(&filter, context.cast(), Some(changed), &mut handle)
        };
        if r != CR_SUCCESS {
            drop(unsafe { Box::from_raw(context) });
            return None;
        }
        Some(Self { handle, context })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // SAFETY: unregistering waits for running callbacks to finish, after
        // which nothing reads the context.
        unsafe {
            CM_Unregister_Notification(self.handle);
            drop(Box::from_raw(self.context));
        }
    }
}
