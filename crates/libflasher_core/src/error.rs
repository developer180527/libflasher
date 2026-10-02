use std::io;

/// Everything that can go wrong. Each message is written for the person
/// using the app: it says what happened and, where there is one, what to do.
///
/// New variants may be added in minor releases; match with a `_` arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An I/O error not tied to the drive (reading the image file, say).
    #[error("{0}")]
    Io(#[from] io::Error),

    /// The OS refused, or the user declined, the privilege needed to write a raw device.
    #[error("permission denied: {0}")]
    Permission(String),

    /// The device is not one we are willing to write to (internal, system disk, …).
    #[error("refusing to write to {device}: {reason}")]
    Refused {
        /// The device path that was refused.
        device: String,
        /// Why, in words for the user.
        reason: String,
    },

    /// The image does not fit on the drive. Found before writing when the
    /// image's size is known, or when the drive runs out otherwise.
    #[error("image is {image} bytes but the device holds only {device}")]
    ImageTooLarge {
        /// Bytes the image needs (at least).
        image: u64,
        /// Bytes the drive holds.
        device: u64,
    },

    /// Reading the drive back did not give the image: a failing or fake drive.
    #[error("verification failed: device differs from image at byte {offset}")]
    VerifyFailed {
        /// The first byte that differs.
        offset: u64,
    },

    /// The drive took longer than `FlashOptions::stall_timeout` to answer one
    /// request. Nothing more was sent to it.
    #[error("the drive stopped responding ({seconds:.0} s for one request at byte {offset}); it may be failing. Unplug it and try another drive")]
    Stalled {
        /// How long the one request took.
        seconds: f32,
        /// Where on the drive it was.
        offset: u64,
    },

    /// The drive went away mid-operation: unplugged, or its USB connection reset.
    #[error("the drive was disconnected after {} had been written. It now holds an incomplete image and will not boot; plug it back in and flash it again", crate::platform::human_size(*offset))]
    DeviceGone {
        /// Bytes written before it went.
        offset: u64,
    },

    /// The drive answered a read or write with an I/O error.
    #[error("the drive reported an I/O error at byte {offset} ({source}); it may be failing or loosely connected")]
    DeviceIo {
        /// Where on the drive.
        offset: u64,
        /// What the OS reported.
        source: io::Error,
    },

    /// The image file is not the one its publisher released: a broken or
    /// tampered download. Nothing was written.
    #[error("the image's SHA-256 does not match the published one, so it is damaged or not the file you meant; nothing was written. Download it again.\n  expected {expected}\n  actual   {actual}")]
    ChecksumMismatch {
        /// The published SHA-256.
        expected: String,
        /// The image file's SHA-256.
        actual: String,
    },

    /// The cancel flag was set, or the user said no at a confirmation.
    #[error("cancelled")]
    Cancelled,

    /// The platform cannot do this (yet).
    #[error("not supported on this platform yet: {0}")]
    Unsupported(String),

    /// A platform helper (diskutil, authopen, …) failed.
    #[error("{tool} failed: {message}")]
    Tool {
        /// The tool, or the step that failed.
        tool: String,
        /// What it said.
        message: String,
    },
}

/// `Result` with this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// The errno values meaning "this device no longer exists", per OS.
fn is_gone(e: &io::Error) -> bool {
    let Some(code) = e.raw_os_error() else {
        return false;
    };
    #[cfg(target_os = "linux")]
    const GONE: &[i32] = &[
        6,   /* ENXIO */
        19,  /* ENODEV */
        123, /* ENOMEDIUM */
    ];
    #[cfg(all(unix, not(target_os = "linux")))]
    const GONE: &[i32] = &[6 /* ENXIO */, 19 /* ENODEV */];
    #[cfg(windows)]
    const GONE: &[i32] = &[
        433,  /* ERROR_NO_SUCH_DEVICE */
        1167, /* ERROR_DEVICE_NOT_CONNECTED */
        21,   /* ERROR_NOT_READY */
    ];
    #[cfg(not(any(unix, windows)))]
    const GONE: &[i32] = &[];
    GONE.contains(&code)
}

impl Error {
    /// Re-label an I/O error that came from the *device* (never use it for
    /// image-file errors) with where it happened and what it means.
    pub(crate) fn at_device(self, offset: u64) -> Self {
        match self {
            Error::Io(e) if is_gone(&e) => Error::DeviceGone { offset },
            Error::Io(e) => Error::DeviceIo { offset, source: e },
            other => other,
        }
    }
}
