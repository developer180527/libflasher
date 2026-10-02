//! Disk plug-in and removal notifications from the kernel's uevent netlink
//! socket: the stream udev itself listens to. Needs no root and no libudev.
//!
//! A thread waits on the socket and on a pipe; writing to the pipe stops it.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::thread::JoinHandle;

use libflasher_core::OnChange;

/// The kernel's multicast group for uevents.
const KERNEL_GROUP: u32 = 1;

pub struct Watcher {
    stop: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    pub fn start(on_change: OnChange) -> Option<Self> {
        // SAFETY: plain socket/pipe syscalls; every fd is wrapped in an OwnedFd
        // as soon as it exists, so each is closed exactly once.
        let sock = unsafe {
            let fd = libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                libc::NETLINK_KOBJECT_UEVENT,
            );
            if fd < 0 {
                return None;
            }
            let sock = OwnedFd::from_raw_fd(fd);
            let mut addr: libc::sockaddr_nl = std::mem::zeroed();
            addr.nl_family = libc::AF_NETLINK as u16;
            addr.nl_groups = KERNEL_GROUP;
            let len = std::mem::size_of::<libc::sockaddr_nl>() as u32;
            if libc::bind(fd, (&addr as *const libc::sockaddr_nl).cast(), len) != 0 {
                return None;
            }
            sock
        };
        let (wake_read, wake_write) = unsafe {
            let mut fds = [0; 2];
            if libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
                return None;
            }
            (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
        };
        let thread = std::thread::Builder::new()
            .name("libflasher-uevent".into())
            .spawn(move || run(sock, wake_read, on_change))
            .ok()?;
        Some(Self {
            stop: wake_write,
            thread: Some(thread),
        })
    }
}

fn run(sock: OwnedFd, wake: OwnedFd, on_change: OnChange) {
    let mut buf = vec![0u8; 8192];
    loop {
        let mut fds = [
            libc::pollfd {
                fd: sock.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two valid pollfds, owned for the duration of the call.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if fds[1].revents != 0 {
            return;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            // SAFETY: reading into our own buffer of its true length.
            let len =
                unsafe { libc::recv(sock.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            if len > 0 && is_block_event(&buf[..len as usize]) {
                on_change();
            }
        }
    }
}

/// A uevent is `action@devpath\0KEY=value\0…`; disks are `SUBSYSTEM=block`.
fn is_block_event(msg: &[u8]) -> bool {
    msg.split(|&b| b == 0)
        .any(|field| field == b"SUBSYSTEM=block")
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // SAFETY: one byte to our own pipe.
        unsafe { libc::write(self.stop.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_block_event;

    #[test]
    fn recognises_block_device_events() {
        let add = b"add@/devices/pci0000:00/usb1/1-1/block/sdb\0ACTION=add\0SUBSYSTEM=block\0DEVNAME=sdb\0";
        let usb = b"add@/devices/pci0000:00/usb1/1-1\0ACTION=add\0SUBSYSTEM=usb\0";
        assert!(is_block_event(add));
        assert!(!is_block_event(usb));
        assert!(!is_block_event(b"SUBSYSTEM=blockish\0"));
    }
}
