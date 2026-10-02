//! Opening a raw disk as an unprivileged app, the way Disk Utility does.
//!
//! `/usr/libexec/authopen` asks the user for an administrator password with
//! the system's own dialog, opens the path, and with `-stdoutpipe` hands the
//! open descriptor back over its stdout — which must therefore be a Unix
//! socket — as `SCM_RIGHTS` ancillary data. The app never runs as root; only
//! the one descriptor is privileged.

use std::fs::File;
use std::io::{self, Read};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::ptr;

use libflasher_core::{Error, Result};

pub fn open_rw(path: &str) -> Result<File> {
    let (ours, theirs) = UnixStream::pair()?;
    let mut child = Command::new("/usr/libexec/authopen")
        .args(["-stdoutpipe", "-o", &libc::O_RDWR.to_string(), path])
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::piped())
        .spawn()?;
    // `theirs` now lives only in the child, so the read below sees EOF if
    // authopen exits without sending anything (the user pressed Cancel).

    let fd = recv_fd(&ours)?;
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut stderr);
    }
    let status = child.wait()?;

    match fd {
        Some(fd) if status.success() => Ok(File::from(fd)),
        _ => Err(Error::Permission(if stderr.trim().is_empty() {
            "authorization was cancelled or denied".into()
        } else {
            stderr.trim().to_string()
        })),
    }
}

/// Receive one file descriptor sent with `SCM_RIGHTS`; `None` on EOF.
fn recv_fd(sock: &UnixStream) -> io::Result<Option<OwnedFd>> {
    let mut data = [0u8; 256];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    // u64s, so the control buffer is aligned for `cmsghdr`.
    let mut control = [0u64; 8];

    // SAFETY: every pointer in `msg` refers to a local that outlives the call,
    // with its true length; the cmsg walk stays within `msg_controllen`.
    unsafe {
        let mut msg: libc::msghdr = mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = mem::size_of_val(&control) as _;

        let n = loop {
            let n = libc::recvmsg(sock.as_raw_fd(), &mut msg, 0);
            if n >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break n;
            }
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let fd: libc::c_int = ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast());
                return Ok(Some(OwnedFd::from_raw_fd(fd)));
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom, Write};

    /// authopen opens a file the user already owns without asking, so the
    /// descriptor-passing path is testable without a password or a disk.
    #[test]
    fn passes_a_descriptor() {
        let p = std::env::temp_dir().join(format!("flasher_authopen_{}", std::process::id()));
        std::fs::write(&p, b"before").unwrap();
        let mut f = super::open_rw(p.to_str().unwrap()).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(b"AFTER!").unwrap();
        drop(f);
        assert_eq!(std::fs::read(&p).unwrap(), b"AFTER!");
        std::fs::remove_file(p).ok();
    }
}
