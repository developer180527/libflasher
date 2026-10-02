//! Writing an image to a device, and reading it back.

use std::io::SeekFrom;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::image::{read_full, ImageInfo};
use crate::{Error, RawDevice, Result};

/// Bytes per write: a multiple of every sector size in use (512, 4096), and
/// small enough that one write is a short, bounded request for the drive. A
/// cheap USB controller once hung a whole Mac part-way through 4 MiB writes;
/// 1 MiB keeps each request, and each stall, small.
const CHUNK: usize = 1 << 20;

/// How [`flash`] behaves. Start from `FlashOptions::default()` and change
/// what you need with the `with_*` methods.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct FlashOptions {
    /// Read the whole image back from the device and compare it.
    pub verify: bool,
    /// Flush to the medium after this many bytes, so the drive never holds a
    /// large backlog of unwritten data in its own cache.
    pub sync_every: u64,
    /// One write, flush or read taking longer than this means the drive has
    /// stopped responding: stop, rather than send it more.
    pub stall_timeout: Duration,
}

impl FlashOptions {
    /// Read everything back and compare after writing (default: on).
    pub fn with_verify(mut self, verify: bool) -> Self {
        self.verify = verify;
        self
    }

    /// Flush to the medium after this many bytes (default: 32 MiB).
    pub fn with_sync_every(mut self, bytes: u64) -> Self {
        self.sync_every = bytes;
        self
    }

    /// Give up on a drive that takes longer than this for one request
    /// (default: 20 s).
    pub fn with_stall_timeout(mut self, timeout: Duration) -> Self {
        self.stall_timeout = timeout;
        self
    }
}

impl Default for FlashOptions {
    fn default() -> Self {
        Self {
            verify: true,
            sync_every: 32 << 20,
            stall_timeout: Duration::from_secs(20),
        }
    }
}

/// Run one device operation, naming its I/O errors (unplugged, failing) and
/// failing with [`Error::Stalled`] if it took
/// longer than `limit`. It cannot interrupt the call — a request stuck in
/// the kernel stays stuck — but it guarantees nothing more is sent after it.
fn timed<T>(limit: Duration, offset: u64, op: impl FnOnce() -> Result<T>) -> Result<T> {
    let start = Instant::now();
    let out = op().map_err(|e| e.at_device(offset))?;
    let took = start.elapsed();
    if took > limit {
        return Err(Error::Stalled {
            seconds: took.as_secs_f32(),
            offset,
        });
    }
    Ok(out)
}

/// What a long operation is doing, reported as it goes. Feed these to
/// [`crate::rate::StatusLine`] for text a person can read.
///
/// New phases may be added in minor releases; match with a `_` arm.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum Progress {
    /// `total` is the image's decompressed size when it is known exactly. When
    /// it is not, `fraction` is how much of the compressed *file* has been
    /// read: a fair estimate, but an estimate, so say so (`total.is_none()`).
    Writing {
        /// Image bytes written so far.
        written: u64,
        /// The image's decompressed size, if known.
        total: Option<u64>,
        /// 0..1, exact when `total` is known, estimated otherwise.
        fraction: f32,
    },
    /// Waiting for the OS and the drive to put every byte on the medium.
    Syncing,
    /// Reading the drive back and comparing it with the image.
    Verifying {
        /// Bytes compared so far.
        verified: u64,
        /// Bytes to compare.
        total: u64,
    },
    /// Hashing the image file to compare with its published checksum,
    /// before anything touches the drive. Bytes of the file, not the disk.
    Checking {
        /// Bytes of the file hashed so far.
        done: u64,
        /// The file's size.
        total: u64,
    },
}

/// Write `image` to the start of `device`, then optionally verify it.
/// Returns the number of image bytes written.
pub fn flash(
    image: &ImageInfo,
    device: &mut dyn RawDevice,
    options: &FlashOptions,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<u64> {
    let sector = device.sector_size().max(512) as usize;
    let capacity = device.size();
    if let Some(size) = image.disk_size {
        if size > capacity {
            return Err(Error::ImageTooLarge {
                image: size,
                device: capacity,
            });
        }
    }

    let src = image.open()?;
    let mut reader = src.reader;
    let mut buf = vec![0u8; CHUNK];
    let mut written = 0u64;
    device.seek(SeekFrom::Start(0))?;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let n = read_full(&mut reader, &mut buf)?;
        if n == 0 {
            break;
        }
        if written + n as u64 > capacity {
            return Err(Error::ImageTooLarge {
                image: written + n as u64,
                device: capacity,
            });
        }
        // Raw devices accept whole sectors only: pad the final chunk with zeros.
        let padded = n.next_multiple_of(sector);
        buf[n..padded].fill(0);
        timed(options.stall_timeout, written, || {
            Ok(device.write_all(&buf[..padded])?)
        })?;
        written += n as u64;
        if written.is_multiple_of(options.sync_every.max(CHUNK as u64)) {
            timed(options.stall_timeout, written, || device.sync())?;
        }

        let fraction = match image.disk_size {
            Some(total) => written as f64 / total.max(1) as f64,
            None => src.consumed.load(Ordering::Relaxed) as f64 / image.file_size.max(1) as f64,
        }
        .min(1.0) as f32;
        progress(Progress::Writing {
            written,
            total: image.disk_size,
            fraction,
        });
        if n < CHUNK {
            break;
        }
    }

    progress(Progress::Syncing);
    timed(options.stall_timeout, written, || device.sync())?;

    if options.verify {
        verify(
            image,
            device,
            written,
            sector,
            options.stall_timeout,
            cancel,
            progress,
        )?;
    }
    Ok(written)
}

/// Compare a device against an image without writing: for checking a drive
/// written earlier, or by another tool. Returns the number of bytes compared.
pub fn verify_device(
    image: &ImageInfo,
    device: &mut dyn RawDevice,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<u64> {
    let total = image.disk_size.unwrap_or(u64::MAX);
    if total != u64::MAX && total > device.size() {
        return Err(Error::ImageTooLarge {
            image: total,
            device: device.size(),
        });
    }
    let sector = device.sector_size().max(512) as usize;
    verify(
        image,
        device,
        total,
        sector,
        FlashOptions::default().stall_timeout,
        cancel,
        progress,
    )
}

/// Compare up to `total` bytes (or to the image's end); returns how many were.
fn verify(
    image: &ImageInfo,
    device: &mut dyn RawDevice,
    total: u64,
    sector: usize,
    stall_timeout: Duration,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<u64> {
    let mut reader = image.open()?.reader;
    let mut want = vec![0u8; CHUNK];
    let mut got = vec![0u8; CHUNK];
    let mut verified = 0u64;
    device.seek(SeekFrom::Start(0))?;

    while verified < total {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let n = read_full(&mut reader, &mut want)?;
        if n == 0 {
            break;
        }
        let padded = n.next_multiple_of(sector);
        timed(stall_timeout, verified, || {
            Ok(device.read_exact(&mut got[..padded])?)
        })?;
        if let Some(i) = want[..n].iter().zip(&got[..n]).position(|(a, b)| a != b) {
            return Err(Error::VerifyFailed {
                offset: verified + i as u64,
            });
        }
        verified += n as u64;
        // `total` is unknown (MAX) for a compressed image checked on its own.
        progress(Progress::Verifying {
            verified,
            total: if total == u64::MAX { verified } else { total },
        });
    }
    Ok(verified)
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read, Seek, Write};
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::image;

    /// A `RawDevice` in memory, so the pipeline is testable without a disk.
    struct MemDevice(Cursor<Vec<u8>>);

    impl Read for MemDevice {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(b)
        }
    }
    impl Write for MemDevice {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            assert!(
                b.len().is_multiple_of(512),
                "unaligned write of {} bytes",
                b.len()
            );
            let room = self.0.get_ref().len() as u64 - self.0.position();
            assert!(b.len() as u64 <= room, "write past end of device");
            self.0.write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Seek for MemDevice {
        fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(p)
        }
    }
    impl RawDevice for MemDevice {
        fn sector_size(&self) -> u32 {
            512
        }
        fn size(&self) -> u64 {
            self.0.get_ref().len() as u64
        }
        fn sync(&mut self) -> Result<()> {
            Ok(())
        }
    }

    fn temp(name: &str, bytes: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!("flasher_test_{}_{name}", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// An odd length, so the last chunk needs padding, spanning several chunks.
    fn pattern() -> Vec<u8> {
        (0..(CHUNK * 2 + 777))
            .map(|i| (i * 31 % 251) as u8)
            .collect()
    }

    fn run(path: &PathBuf, dev_size: usize) -> (Result<u64>, Vec<u8>) {
        let info = image::inspect(path).unwrap();
        let mut dev = MemDevice(Cursor::new(vec![0xEE; dev_size]));
        let r = flash(
            &info,
            &mut dev,
            &FlashOptions::default(),
            &AtomicBool::new(false),
            &mut |_| {},
        );
        (r, dev.0.into_inner())
    }

    #[test]
    fn writes_and_verifies_raw() {
        let data = pattern();
        let path = temp("raw.img", &data);
        let (r, dev) = run(&path, CHUNK * 3);
        assert_eq!(r.unwrap(), data.len() as u64);
        assert_eq!(&dev[..data.len()], &data[..]);
        std::fs::remove_file(path).ok();
    }

    #[test]
    #[cfg(feature = "gzip")]
    fn writes_gzip() {
        use flate2::write::GzEncoder;
        let data = pattern();
        let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&data).unwrap();
        let path = temp("img.gz", &enc.finish().unwrap());
        let (r, dev) = run(&path, CHUNK * 3);
        assert_eq!(r.unwrap(), data.len() as u64);
        assert_eq!(&dev[..data.len()], &data[..]);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn refuses_image_larger_than_device() {
        let path = temp("big.img", &pattern());
        let (r, _) = run(&path, CHUNK);
        assert!(matches!(r, Err(Error::ImageTooLarge { .. })));
        std::fs::remove_file(path).ok();
    }

    /// Counts what reaches the drive, and can make one write slow.
    struct Instrumented {
        inner: MemDevice,
        writes: usize,
        syncs: usize,
        slow_write: Option<(usize, Duration)>,
        /// From this write on, fail as an unplugged device does.
        gone_from: Option<usize>,
    }
    impl Read for Instrumented {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(b)
        }
    }
    impl Write for Instrumented {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            if self.gone_from.is_some_and(|n| self.writes >= n) {
                return Err(crate::mock::device_gone_for_tests());
            }
            if let Some((n, d)) = self.slow_write {
                if self.writes == n {
                    std::thread::sleep(d);
                }
            }
            self.inner.write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Seek for Instrumented {
        fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(p)
        }
    }
    impl RawDevice for Instrumented {
        fn sector_size(&self) -> u32 {
            512
        }
        fn size(&self) -> u64 {
            self.inner.size()
        }
        fn sync(&mut self) -> Result<()> {
            self.syncs += 1;
            Ok(())
        }
    }

    fn instrumented(size: usize, slow_write: Option<(usize, Duration)>) -> Instrumented {
        Instrumented {
            inner: MemDevice(Cursor::new(vec![0; size])),
            writes: 0,
            syncs: 0,
            slow_write,
            gone_from: None,
        }
    }

    #[test]
    fn flushes_periodically() {
        let data: Vec<u8> = (0..CHUNK * 8 + 777).map(|i| (i % 251) as u8).collect();
        let path = temp("periodic.img", &data);
        let info = image::inspect(&path).unwrap();
        let mut dev = instrumented(CHUNK * 12, None);
        let opts = FlashOptions {
            verify: false,
            sync_every: 2 * CHUNK as u64,
            ..FlashOptions::default()
        };
        flash(&info, &mut dev, &opts, &AtomicBool::new(false), &mut |_| {}).unwrap();
        // 8 whole chunks → 4 periodic flushes, plus the final one.
        assert_eq!(dev.syncs, 5);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn stops_sending_after_a_stall() {
        let path = temp("stall.img", &pattern());
        let info = image::inspect(&path).unwrap();
        let mut dev = instrumented(CHUNK * 12, Some((3, Duration::from_millis(80))));
        let opts = FlashOptions {
            stall_timeout: Duration::from_millis(20),
            ..FlashOptions::default()
        };
        let r = flash(&info, &mut dev, &opts, &AtomicBool::new(false), &mut |_| {});
        assert!(
            matches!(r, Err(Error::Stalled { offset, .. }) if offset == 2 * CHUNK as u64),
            "{r:?}"
        );
        assert_eq!(dev.writes, 3, "data was sent to a drive after it stalled");
        assert_eq!(dev.syncs, 0, "a stalled drive was asked to flush");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn names_an_unplugged_drive_and_stops() {
        let path = temp("gone.img", &pattern());
        let info = image::inspect(&path).unwrap();
        let mut dev = instrumented(CHUNK * 12, None);
        dev.gone_from = Some(2);
        let r = flash(
            &info,
            &mut dev,
            &FlashOptions::default(),
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert!(
            matches!(r, Err(Error::DeviceGone { offset }) if offset == CHUNK as u64),
            "{r:?}"
        );
        assert_eq!(dev.writes, 2, "kept writing to a drive that was gone");
        assert_eq!(dev.syncs, 0, "tried to flush a drive that was gone");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn other_device_errors_say_where() {
        // Not a "device gone" code on any OS (EIO on unix, ERROR_ACCESS_DENIED on Windows).
        let e = Error::Io(std::io::Error::from_raw_os_error(5)).at_device(42);
        assert!(matches!(e, Error::DeviceIo { offset: 42, .. }), "{e:?}");
        let e = Error::Cancelled.at_device(42);
        assert!(matches!(e, Error::Cancelled));
    }
}
