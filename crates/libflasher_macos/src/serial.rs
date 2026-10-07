//! A disk's serial number, from the I/O Registry: `diskutil` does not report
//! one. The disk's IOMedia object is found by its BSD name, and the search
//! walks up its parents to the USB (or other) device that carries the serial.
//! Read-only, and needs no privileges.

use std::ffi::{c_char, c_void, CString};

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type IoObject = u32;

const MAIN_PORT_DEFAULT: u32 = 0;
const ITERATE_RECURSIVELY_AND_PARENTS: u32 = 1 | 2;
const UTF8: u32 = 0x0800_0100;
const SERVICE_PLANE: &[u8] = b"IOService\0";
/// Where each kind of device keeps it: USB, then anything else.
const KEYS: [&str; 3] = [
    "USB Serial Number",
    "kUSBSerialNumberString",
    "Serial Number",
];

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOBSDNameMatching(main_port: u32, options: u32, bsd_name: *const c_char) -> CFTypeRef;
    /// Consumes `matching`.
    fn IOServiceGetMatchingService(main_port: u32, matching: CFTypeRef) -> IoObject;
    fn IORegistryEntrySearchCFProperty(
        entry: IoObject,
        plane: *const c_char,
        key: CFStringRef,
        allocator: CFTypeRef,
        options: u32,
    ) -> CFTypeRef;
    fn IOObjectRelease(object: IoObject) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const c_char, encoding: u32) -> CFStringRef;
    fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, len: isize, encoding: u32) -> u8;
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFRelease(cf: CFTypeRef);
}

/// The serial number of the whole disk `id` (`disk4`), if anything above it
/// in the registry reports one.
pub(crate) fn of(id: &str) -> Option<String> {
    let name = CString::new(id).ok()?;
    // SAFETY: each CF object created here is released once; the registry
    // object is released; strings are read into a buffer of their stated size.
    unsafe {
        let matching = IOBSDNameMatching(MAIN_PORT_DEFAULT, 0, name.as_ptr());
        if matching.is_null() {
            return None;
        }
        let media = IOServiceGetMatchingService(MAIN_PORT_DEFAULT, matching);
        if media == 0 {
            return None;
        }
        let found = KEYS.iter().find_map(|k| string_property(media, k));
        IOObjectRelease(media);
        found
    }
}

unsafe fn string_property(entry: IoObject, key: &str) -> Option<String> {
    let k = CString::new(key).ok()?;
    let cf_key = CFStringCreateWithCString(std::ptr::null(), k.as_ptr(), UTF8);
    if cf_key.is_null() {
        return None;
    }
    let value = IORegistryEntrySearchCFProperty(
        entry,
        SERVICE_PLANE.as_ptr().cast(),
        cf_key,
        std::ptr::null(),
        ITERATE_RECURSIVELY_AND_PARENTS,
    );
    CFRelease(cf_key);
    if value.is_null() {
        return None;
    }
    let mut out = None;
    if CFGetTypeID(value) == CFStringGetTypeID() {
        let mut buf = [0 as c_char; 256];
        if CFStringGetCString(value, buf.as_mut_ptr(), buf.len() as isize, UTF8) != 0 {
            let s = std::ffi::CStr::from_ptr(buf.as_ptr())
                .to_string_lossy()
                .trim()
                .to_string();
            out = (!s.is_empty()).then_some(s);
        }
    }
    CFRelease(value);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_without_failing_and_finds_nothing_for_a_missing_disk() {
        // Whatever the internal disk reports (some Macs give a serial, some
        // not), asking must not crash or hang.
        let _ = of("disk0");
        assert_eq!(of("disk999"), None);
        assert_eq!(of("not a disk"), None);
        assert_eq!(of("dis\0k0"), None);
    }

    /// Prints what the lookup finds for `FLASHER_SERIAL_DISK` (e.g. disk4),
    /// to compare by hand with `ioreg`.
    #[test]
    #[ignore = "manual check against ioreg"]
    fn print_serial() {
        let id = std::env::var("FLASHER_SERIAL_DISK").unwrap_or_else(|_| "disk0".into());
        println!("{id}: {:?}", of(&id));
    }
}
