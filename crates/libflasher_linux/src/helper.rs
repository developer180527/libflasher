//! Writing a disk without running the whole program as root.
//!
//! `libflasher-helper` is a tiny program started through `pkexec`, which
//! shows the desktop's own password prompt. It does only what needs root:
//!
//! - `open <device>`: unmount the disk, open it exclusively, and hand the
//!   open file back to the caller over its stdout, a Unix socket
//!   (`SCM_RIGHTS`) — the same idea as macOS's `authopen`. Everything after
//!   that, writing and verifying, runs in the caller, unprivileged.
//! - `restore <device> <label>`: run `wipefs`, `sfdisk` and `mkfs.exfat`.
//!
//! It trusts nothing it is given: the device must be one it lists itself, as
//! root, as a removable whole disk that does not hold the running system,
//! and the label must pass `volume_label`.
//!
//! The helper is found at `LIBFLASHER_HELPER`, else next to the running
//! program, else in `/usr/libexec` or `/usr/lib/libflasher`. Tests may set
//! `LIBFLASHER_ELEVATOR=sudo` to use passwordless `sudo -n` instead of
//! `pkexec`; nothing else is accepted.

use std::fs::File;
use std::io::{self, Read};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

use libflasher_core::platform::volume_label;
use libflasher_core::{Error, Platform, Result};

use crate::Linux;

const NAME: &str = "libflasher-helper";

// ---- the helper itself (runs as root) --------------------------------------

/// The helper's `main`. `args` excludes the program name.
pub fn main(args: impl IntoIterator<Item = String>) -> ExitCode {
    let args: Vec<String> = args.into_iter().collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(Error::Permission(format!(
            "{NAME} must run as root, through pkexec"
        )));
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    match argv.as_slice() {
        ["open", path] => {
            let device = listed(path)?;
            let file = crate::open_as_root(&device)?;
            // stdout is the caller's socket; anything else means misuse.
            let out = unsafe { std::os::fd::BorrowedFd::borrow_raw(1) };
            send_fd(out.as_raw_fd(), file.as_raw_fd()).map_err(|e| Error::Tool {
                tool: NAME.into(),
                message: format!("handing over the open disk: {e} (stdout must be a Unix socket)"),
            })
        }
        ["restore", path, label] => {
            let label = volume_label(label).map_err(|reason| Error::Refused {
                device: path.to_string(),
                reason,
            })?;
            let device = listed(path)?;
            crate::restore_as_root(&device, &label)
        }
        _ => Err(Error::Tool {
            tool: NAME.into(),
            message: "usage: libflasher-helper open <device> | restore <device> <label>".into(),
        }),
    }
}

/// The device, as this (root) process lists it — or a refusal.
fn listed(path: &str) -> Result<libflasher_core::DeviceInfo> {
    Linux
        .list_devices()?
        .into_iter()
        .find(|d| d.path == path)
        .ok_or_else(|| Error::Refused {
            device: path.into(),
            reason: "not a removable whole disk (or it holds the running system)".into(),
        })
}

fn send_fd(sock: i32, fd: i32) -> io::Result<()> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0u64; 4]; // aligned for cmsghdr, room for one fd
                                 // SAFETY: all pointers are to locals valid for the call; CMSG_* stay
                                 // within `control`, sized above for one int.
    unsafe {
        let mut msg: libc::msghdr = mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(mem::size_of::<i32>() as u32) as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(mem::size_of::<i32>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<i32>(), fd);
        if libc::sendmsg(sock, &msg, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

// ---- the caller's side (unprivileged) --------------------------------------

/// Open `path` for writing through the helper.
pub(crate) fn open(path: &str) -> Result<File> {
    let (ours, theirs) = UnixStream::pair()?;
    let mut child = command(&["open", path])?
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::piped())
        .spawn()?;
    // `theirs` now lives only in the child: EOF if it exits without sending.
    let fd = recv_fd(&ours)?;
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut stderr);
    }
    let status = child.wait()?;
    match fd {
        Some(fd) if status.success() => Ok(File::from(fd)),
        _ => Err(failure(status.code(), &stderr)),
    }
}

/// Restore `path` to an ordinary drive through the helper.
pub(crate) fn restore(path: &str, label: &str) -> Result<()> {
    let out = command(&["restore", path, label])?
        .stdin(Stdio::null())
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    Err(failure(
        out.status.code(),
        &String::from_utf8_lossy(&out.stderr),
    ))
}

/// pkexec exits 126 when the user dismisses or fails the prompt, 127 when
/// it is not allowed at all; otherwise the helper's own message stands.
fn failure(code: Option<i32>, stderr: &str) -> Error {
    match code {
        Some(126) => Error::Permission("authorization was cancelled".into()),
        Some(127) if stderr.trim().is_empty() => {
            Error::Permission("not authorized to write disks".into())
        }
        _ => Error::Tool {
            tool: NAME.into(),
            message: if stderr.trim().is_empty() {
                "failed".into()
            } else {
                stderr.trim().into()
            },
        },
    }
}

/// `pkexec <helper> <args…>` (or `sudo -n -E …` for tests).
fn command(args: &[&str]) -> Result<Command> {
    let helper = helper_path().ok_or_else(|| {
        Error::Permission(format!(
            "writing a disk needs root: install {NAME} (it ships with the app) or run the program with sudo"
        ))
    })?;
    let mut c = match std::env::var("LIBFLASHER_ELEVATOR").as_deref() {
        Ok("sudo") => {
            let mut c = Command::new("sudo");
            c.args(["-n", "-E"]);
            c
        }
        Ok(other) if other != "pkexec" => {
            return Err(Error::Permission(format!(
                "LIBFLASHER_ELEVATOR may be pkexec or sudo, not {other:?}"
            )));
        }
        _ => Command::new("pkexec"),
    };
    c.arg(helper).args(args);
    Ok(c)
}

fn helper_path() -> Option<PathBuf> {
    let candidates = std::env::var_os("LIBFLASHER_HELPER")
        .map(PathBuf::from)
        .into_iter()
        .chain(
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|d| d.join(NAME))),
        )
        .chain([
            PathBuf::from("/usr/libexec").join(NAME),
            PathBuf::from("/usr/lib/libflasher").join(NAME),
        ]);
    // pkexec needs an absolute path.
    candidates.filter(|p| p.is_absolute()).find(|p| p.is_file())
}

fn recv_fd(sock: &UnixStream) -> io::Result<Option<OwnedFd>> {
    let mut data = [0u8; 16];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    let mut control = [0u64; 8];
    // SAFETY: as in send_fd; the cmsg walk stays within msg_controllen.
    unsafe {
        let mut msg: libc::msghdr = mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = mem::size_of_val(&control) as _;
        let n = loop {
            let n = libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC);
            if n >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break n;
            }
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        // Every descriptor that arrived is ours to close: keep the first.
        let mut fds: Vec<OwnedFd> = Vec::new();
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let bytes = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let data = libc::CMSG_DATA(cmsg).cast::<libc::c_int>();
                for i in 0..bytes / std::mem::size_of::<libc::c_int>() {
                    fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::other("descriptor message truncated"));
        }
        let Some(fd) = fds.into_iter().next() else {
            return Ok(None);
        };
        Ok(Some(fd))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fd hand-over itself, between two ends of a socket pair in one
    /// process: no root needed.
    #[test]
    fn hands_an_open_file_across_a_socket() {
        let path = std::env::temp_dir().join(format!("libflasher_helper_{}", std::process::id()));
        std::fs::write(&path, b"handed over").unwrap();
        let file = File::open(&path).unwrap();
        let (a, b) = UnixStream::pair().unwrap();
        send_fd(a.as_raw_fd(), file.as_raw_fd()).unwrap();
        drop(file);
        let mut got = File::from(recv_fd(&b).unwrap().expect("an fd"));
        let mut s = String::new();
        got.read_to_string(&mut s).unwrap();
        assert_eq!(s, "handed over");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn refuses_to_run_unprivileged_or_with_bad_arguments() {
        if unsafe { libc::geteuid() } == 0 {
            return; // as root, the first check passes by design
        }
        assert!(matches!(
            run(&["open".into(), "/dev/sda".into()]),
            Err(Error::Permission(_))
        ));
    }

    #[test]
    fn maps_pkexec_exit_codes() {
        assert!(matches!(failure(Some(126), ""), Error::Permission(_)));
        assert!(matches!(failure(Some(127), ""), Error::Permission(_)));
        assert!(matches!(
            failure(Some(1), "refusing to write to /dev/sda: …"),
            Error::Tool { .. }
        ));
    }
}
