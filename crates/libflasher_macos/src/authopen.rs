//! Opening a raw disk as an unprivileged app, the way Disk Utility does.
//!
//! Two steps, because a disk cannot be opened for writing while a volume on
//! it is mounted (macOS 26's FSKit exFAT and FAT drivers hold it: `EBUSY`),
//! and its volumes should not be unmounted until the user has agreed:
//!
//! 1. [`authorize`] asks the user for an administrator password with the
//!    system's own dialog, for the right to open that one path read-write.
//!    Cancelling it changes nothing.
//! 2. With the volumes then unmounted, [`open_rw`] runs
//!    `/usr/libexec/authopen -extauth`, handing it that authorization, so it
//!    opens the path without asking again; with `-stdoutpipe` it passes the
//!    open descriptor back over its stdout — which must therefore be a Unix
//!    socket — as `SCM_RIGHTS` ancillary data.
//!
//! The app never runs as root; only the one descriptor is privileged.

use std::ffi::{c_char, c_void, CString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};

use libflasher_core::{Error, Result};

type AuthorizationRef = *mut c_void;

#[repr(C)]
struct AuthorizationItem {
    name: *const c_char,
    value_length: usize,
    value: *mut c_void,
    flags: u32,
}

#[repr(C)]
struct AuthorizationItemSet {
    count: u32,
    items: *mut AuthorizationItem,
}

/// What `authopen -extauth` reads from its stdin.
const EXTERNAL_FORM_LEN: usize = 32;

const INTERACTION_ALLOWED: u32 = 1 << 0;
const EXTEND_RIGHTS: u32 = 1 << 1;
const DESTROY_RIGHTS: u32 = 1 << 3;
const PRE_AUTHORIZE: u32 = 1 << 4;
const ERR_DENIED: i32 = -60005;
const ERR_CANCELED: i32 = -60006;

#[link(name = "Security", kind = "framework")]
extern "C" {
    fn AuthorizationCreate(
        rights: *const AuthorizationItemSet,
        environment: *const AuthorizationItemSet,
        flags: u32,
        authorization: *mut AuthorizationRef,
    ) -> i32;
    fn AuthorizationMakeExternalForm(
        authorization: AuthorizationRef,
        external: *mut [u8; EXTERNAL_FORM_LEN],
    ) -> i32;
    fn AuthorizationFree(authorization: AuthorizationRef, flags: u32) -> i32;
}

/// The user's consent to open one path read-write, obtained but not yet used.
pub struct Authorization {
    auth: AuthorizationRef,
    external: [u8; EXTERNAL_FORM_LEN],
}

// The reference is an opaque handle that Security.framework accepts from any
// thread; it is only freed in Drop.
unsafe impl Send for Authorization {}

impl Drop for Authorization {
    fn drop(&mut self) {
        // SAFETY: created by AuthorizationCreate and freed once, here.
        unsafe { AuthorizationFree(self.auth, DESTROY_RIGHTS) };
    }
}

/// Ask for the right authopen needs to open `path` read-write, showing the
/// password (or Touch ID) dialog. `prompt` is shown in it, below the
/// system's own first line.
pub fn authorize(path: &str, prompt: &str) -> Result<Authorization> {
    let right = CString::new(format!("sys.openfile.readwrite.{path}"))
        .map_err(|_| Error::Permission("a path with a NUL in it".into()))?;
    let prompt_name = c"prompt";
    let prompt = CString::new(prompt).unwrap_or_default();
    let mut item = AuthorizationItem {
        name: right.as_ptr(),
        value_length: 0,
        value: std::ptr::null_mut(),
        flags: 0,
    };
    let mut env_item = AuthorizationItem {
        name: prompt_name.as_ptr(),
        value_length: prompt.as_bytes().len(),
        value: prompt.as_ptr() as *mut c_void,
        flags: 0,
    };
    let rights = AuthorizationItemSet {
        count: 1,
        items: &mut item,
    };
    let env = AuthorizationItemSet {
        count: 1,
        items: &mut env_item,
    };
    let mut auth: AuthorizationRef = std::ptr::null_mut();
    // SAFETY: every pointer refers to a local that outlives the calls; the
    // reference is owned by the returned value, or freed here on failure.
    unsafe {
        let status = AuthorizationCreate(
            &rights,
            &env,
            INTERACTION_ALLOWED | EXTEND_RIGHTS | PRE_AUTHORIZE,
            &mut auth,
        );
        if status != 0 {
            if !auth.is_null() {
                AuthorizationFree(auth, DESTROY_RIGHTS);
            }
            return Err(Error::Permission(match status {
                ERR_CANCELED => "authorization was cancelled".into(),
                ERR_DENIED => "authorization was denied".into(),
                s => format!("authorization failed (Security error {s})"),
            }));
        }
        let mut external = [0u8; EXTERNAL_FORM_LEN];
        if AuthorizationMakeExternalForm(auth, &mut external) != 0 {
            AuthorizationFree(auth, DESTROY_RIGHTS);
            return Err(Error::Permission(
                "the authorization could not be handed to authopen".into(),
            ));
        }
        Ok(Authorization { auth, external })
    }
}

/// Open `path` read-write through authopen, using `auth` (see the module
/// docs). By then the user has agreed, so a failure here is the disk
/// refusing to open, not the user saying no.
pub fn open_rw(path: &str, auth: &Authorization) -> Result<File> {
    spawn(path, Some(&auth.external)).map_err(|detail| {
        Error::Io(io::Error::other(format!(
            "{path} could not be opened for writing although permission was given \
             (something still has it in use{})",
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        )))
    })
}

/// Run authopen and receive the descriptor; `Err` carries its stderr.
fn spawn(
    path: &str,
    external: Option<&[u8; EXTERNAL_FORM_LEN]>,
) -> std::result::Result<File, String> {
    let (ours, theirs) = UnixStream::pair().map_err(|e| e.to_string())?;
    let mut cmd = Command::new("/usr/libexec/authopen");
    cmd.arg("-stdoutpipe");
    if external.is_some() {
        cmd.arg("-extauth");
    }
    let mut child = cmd
        .args(["-o", &libc::O_RDWR.to_string(), path])
        .stdin(if external.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    // `theirs` now lives only in the child, so the read below sees EOF if
    // authopen exits without sending anything.
    if let (Some(ext), Some(mut stdin)) = (external, child.stdin.take()) {
        // Dropped straight after: authopen reads exactly this, then stdin ends.
        let _ = stdin.write_all(ext);
    }

    let fd = recv_fd(&ours);
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut stderr);
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    match fd {
        Ok(Some(fd)) if status.success() => Ok(File::from(fd)),
        Err(e) => Err(e.to_string()),
        _ => Err(stderr.trim().to_string()),
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
        // macOS has no MSG_CMSG_CLOEXEC: without this, every program
        // started while the disk is open (diskutil, caffeinate) inherits a
        // writable descriptor to it.
        if libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(fd))
    }
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
        let mut f = super::spawn(p.to_str().unwrap(), None).unwrap();
        // Programs started later must not inherit it.
        let flags = unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(&f), libc::F_GETFD) };
        assert!(
            flags & libc::FD_CLOEXEC != 0,
            "descriptor is inherited by children"
        );
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(b"AFTER!").unwrap();
        drop(f);
        assert_eq!(std::fs::read(&p).unwrap(), b"AFTER!");
        std::fs::remove_file(p).ok();
    }

    /// A path authopen cannot open comes back as an error with its reason,
    /// not a hang.
    #[test]
    fn a_missing_path_is_an_error() {
        let e = super::spawn("/nonexistent/flasher_authopen", None).unwrap_err();
        eprintln!("authopen said: {e:?}");
    }
}
