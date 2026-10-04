//! What is in an image file, and a reader that yields its raw disk bytes.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::Result;

/// How an image file is compressed, found from its first bytes, not its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Compression {
    /// Not compressed: the file is the disk image.
    None,
    /// `.gz`
    Gzip,
    /// `.xz`, as Raspberry Pi OS and many others ship.
    Xz,
    /// `.zst`
    Zstd,
    /// `.bz2`
    Bzip2,
    /// `.zip` holding one image (stored or deflated).
    Zip,
}

impl Compression {
    /// For people: `None` is "uncompressed", the rest are what users call them.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "uncompressed",
            Self::Gzip => "gzip",
            Self::Xz => "xz",
            Self::Zstd => "zstd",
            Self::Bzip2 => "bzip2",
            Self::Zip => "zip",
        }
    }

    fn sniff(magic: &[u8]) -> Self {
        match magic {
            [0x1f, 0x8b, ..] => Self::Gzip,
            [0xfd, b'7', b'z', b'X', b'Z', 0x00, ..] => Self::Xz,
            [0x28, 0xb5, 0x2f, 0xfd, ..] => Self::Zstd,
            [b'B', b'Z', b'h', ..] => Self::Bzip2,
            [b'P', b'K', 3, 4, ..] => Self::Zip,
            _ => Self::None,
        }
    }
}

/// What the decompressed bytes look like, which decides how they can be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImageKind {
    /// Starts with an MBR or GPT: a disk image. Written byte for byte.
    RawDisk,
    /// An ISO 9660 filesystem that is *also* a disk image (isohybrid): most
    /// Linux distributions. Written byte for byte.
    HybridIso,
    /// An ISO 9660 (or UDF) filesystem with no partition table: Windows
    /// install media and some older ISOs. Written by extract mode
    /// ([`crate::extract`]): a new GPT and FAT32 partition, with the ISO's
    /// files copied onto it.
    PlainIso,
    /// No partition table and no ISO header. May still be valid (a bare
    /// filesystem image, some firmware images), so it is written byte for
    /// byte when asked; but it may as well be a file that is no disk image
    /// at all. Front ends should say so before erasing a drive for it.
    Unknown,
}

impl ImageKind {
    fn classify(head: &[u8]) -> Self {
        let mbr = head.len() >= 512 && head[510] == 0x55 && head[511] == 0xAA;
        let gpt = head.len() >= 520 && &head[512..520] == b"EFI PART";
        let iso9660 = head.len() >= 0x8006 && &head[0x8001..0x8006] == b"CD001";
        // A UDF-only disc image has no CD001, just its recognition sequence
        // (BEA01, NSR02/03, TEA01) in the same place.
        let udf = (16..head.len() / 2048)
            .any(|s| matches!(&head[s * 2048 + 1..s * 2048 + 6], b"NSR02" | b"NSR03"));
        let iso = iso9660 || udf;
        match (iso, mbr || gpt) {
            (true, true) => Self::HybridIso,
            (true, false) => Self::PlainIso,
            (false, true) => Self::RawDisk,
            (false, false) => Self::Unknown,
        }
    }

    /// Whether writing the image byte for byte gives a bootable drive.
    pub fn raw_writable(self) -> bool {
        !matches!(self, Self::PlainIso)
    }

    /// Whether it goes on a drive by extract mode rather than byte for byte.
    pub fn needs_extract(self) -> bool {
        matches!(self, Self::PlainIso)
    }

    /// A short description for people.
    pub fn describe(self) -> &'static str {
        match self {
            Self::RawDisk => "disk image",
            Self::HybridIso => "hybrid ISO (bootable as written)",
            Self::PlainIso => "ISO (its files are copied onto a FAT32 drive)",
            Self::Unknown => "no partition table found",
        }
    }
}

/// What [`inspect`] found out about an image file.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ImageInfo {
    /// The file.
    pub path: PathBuf,
    /// How it is compressed.
    pub compression: Compression,
    /// What its decompressed bytes are.
    pub kind: ImageKind,
    /// Size of the file on disk.
    pub file_size: u64,
    /// Size once decompressed, when it is known exactly without decompressing
    /// (uncompressed, `.xz` and `.zip` images). `None` means unknown, not zero.
    pub disk_size: Option<u64>,
    /// For an archive, the name of the image inside it.
    pub archive_entry: Option<String>,
    /// When the file was last modified, as [`inspect`] saw it (`None` where
    /// the OS does not say). With `file_size`, how [`ImageInfo::open_file`]
    /// tells that the file changed since.
    pub modified: Option<std::time::SystemTime>,
}

/// Bytes the decompressed image needs to look at to classify it: past the ISO
/// primary volume descriptor at 0x8000.
const HEAD: usize = 64 * 1024;

/// Look at an image file: its compression, what kind of image it holds, and
/// its decompressed size when that can be known without decompressing.
/// Reads only the first 64 KiB of image data (and, for `.xz`, its index).
pub fn inspect(path: impl AsRef<Path>) -> Result<ImageInfo> {
    let path = path.as_ref().to_path_buf();
    let mut file = File::open(&path)?;
    let meta = file.metadata()?;
    let file_size = meta.len();
    let modified = meta.modified().ok();

    let mut magic = [0u8; 8];
    let n = read_full(&mut file, &mut magic)?;
    let compression = Compression::sniff(&magic[..n]);

    let mut head = vec![0u8; HEAD];
    let mut reader = decoder(compression, File::open(&path)?)?;
    let n = read_full(&mut reader, &mut head)?;
    head.truncate(n);

    let (disk_size, archive_entry) = match compression {
        Compression::None => (Some(file_size), None),
        // Exact, from the index at the end of the file; no decompression.
        Compression::Xz => (crate::xz_size::uncompressed_size(&path), None),
        // Exact, from the central directory.
        #[cfg(feature = "zip")]
        Compression::Zip => {
            let entry = crate::zip::find(&mut File::open(&path)?).map_err(zip_error)?;
            (Some(entry.size), Some(entry.name))
        }
        // gzip records the size modulo 4 GiB, which is a guess for disk
        // images; zstd only sometimes records it; bzip2 never. Unknown is honest.
        _ => (None, None),
    };
    Ok(ImageInfo {
        path,
        compression,
        kind: ImageKind::classify(&head),
        file_size,
        disk_size,
        archive_entry,
        modified,
    })
}

/// A zip problem as this crate's error: what cannot be read is `Unsupported`.
#[cfg(feature = "zip")]
fn zip_error(e: io::Error) -> crate::Error {
    match e.kind() {
        io::ErrorKind::Unsupported => crate::Error::Unsupported(e.to_string()),
        _ => crate::Error::Io(e),
    }
}

/// An image opened for reading its decompressed bytes.
#[non_exhaustive]
pub struct ImageReader {
    /// The image's decompressed bytes, from the start.
    pub reader: Box<dyn Read + Send>,
    /// Compressed bytes consumed so far; against `file_size`, the progress of
    /// a write whose decompressed length is unknown.
    pub consumed: Arc<AtomicU64>,
}

impl ImageInfo {
    /// Open the image file — but only if it is still the file [`inspect`]
    /// looked at (same size and modification time), so a checksum checked
    /// and an image written are the same bytes. Fails with
    /// [`Error::ImageChanged`](crate::Error::ImageChanged) otherwise.
    pub fn open_file(&self) -> Result<File> {
        let file = File::open(&self.path)?;
        let meta = file.metadata()?;
        if meta.len() != self.file_size || meta.modified().ok() != self.modified {
            return Err(crate::Error::ImageChanged {
                path: self.path.display().to_string(),
            });
        }
        Ok(file)
    }

    /// Open the image for reading its decompressed bytes from the start.
    pub fn open(&self) -> Result<ImageReader> {
        let consumed = Arc::new(AtomicU64::new(0));
        let file = Counting {
            inner: self.open_file()?,
            count: consumed.clone(),
        };
        Ok(ImageReader {
            reader: decoder(self.compression, file)?,
            consumed,
        })
    }
}

fn decoder<R: Read + Seek + Send + 'static>(c: Compression, r: R) -> Result<Box<dyn Read + Send>> {
    #[cfg(feature = "zip")]
    if c == Compression::Zip {
        let mut r = r;
        let entry = crate::zip::find(&mut r).map_err(zip_error)?;
        return crate::zip::reader(r, &entry).map_err(zip_error);
    }
    let r = BufReader::with_capacity(1 << 20, r);
    Ok(match c {
        Compression::None => Box::new(r),
        #[cfg(feature = "gzip")]
        Compression::Gzip => Box::new(flate2::bufread::MultiGzDecoder::new(r)),
        #[cfg(feature = "xz")]
        Compression::Xz => Box::new(xz2::bufread::XzDecoder::new_multi_decoder(r)),
        #[cfg(feature = "zstd")]
        Compression::Zstd => Box::new(zstd::stream::read::Decoder::with_buffer(r)?),
        #[cfg(feature = "bzip2")]
        Compression::Bzip2 => Box::new(bzip2::bufread::MultiBzDecoder::new(r)),
        #[allow(unreachable_patterns)]
        other => {
            return Err(crate::Error::Unsupported(format!(
                "{other:?} images (built without that decompressor)"
            )))
        }
    })
}

struct Counting<R> {
    inner: R,
    count: Arc<AtomicU64>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// Seeking (a zip's central directory) is not consuming: only reads count.
impl<R: Seek> Seek for Counting<R> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.inner.seek(to)
    }
}

/// Fill `buf` unless the stream ends first; returns how much was read.
pub(crate) fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn iso_head(mbr: bool) -> Vec<u8> {
        let mut h = vec![0u8; HEAD];
        h[0x8001..0x8006].copy_from_slice(b"CD001");
        if mbr {
            h[510] = 0x55;
            h[511] = 0xAA;
        }
        h
    }

    #[test]
    fn classifies() {
        assert_eq!(ImageKind::classify(&iso_head(true)), ImageKind::HybridIso);
        assert_eq!(ImageKind::classify(&iso_head(false)), ImageKind::PlainIso);
        let mut gpt = vec![0u8; 1024];
        gpt[512..520].copy_from_slice(b"EFI PART");
        assert_eq!(ImageKind::classify(&gpt), ImageKind::RawDisk);
        assert_eq!(ImageKind::classify(&[0u8; 100]), ImageKind::Unknown);
        let mut udf = vec![0u8; HEAD];
        udf[0x8001..0x8006].copy_from_slice(b"BEA01");
        udf[0x8801..0x8806].copy_from_slice(b"NSR02");
        assert_eq!(
            ImageKind::classify(&udf),
            ImageKind::PlainIso,
            "UDF-only image"
        );
    }

    #[test]
    fn refuses_an_image_that_changed_after_inspecting() {
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, SystemTime};

        let path =
            std::env::temp_dir().join(format!("libflasher_changed_{}.img", std::process::id()));
        std::fs::write(&path, iso_head(true)).unwrap();
        let info = inspect(&path).unwrap();
        assert!(info.open().is_ok(), "unchanged: opens");

        // Rewritten in place, same size: only the modification time moves.
        let f = File::options().write(true).open(&path).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(60))
            .unwrap();
        drop(f);
        assert!(
            matches!(info.open(), Err(crate::Error::ImageChanged { .. })),
            "same size, newer"
        );

        // Grown (a download still arriving): the size moves.
        let info = inspect(&path).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"more")
            .unwrap();
        assert!(matches!(
            info.open_file(),
            Err(crate::Error::ImageChanged { .. })
        ));
        let hash = "0".repeat(64);
        let checked =
            crate::checksum::verify_image(&info, &hash, &AtomicBool::new(false), &mut |_| {});
        assert!(
            matches!(checked, Err(crate::Error::ImageChanged { .. })),
            "{checked:?}"
        );
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn sniffs_compression() {
        assert_eq!(Compression::sniff(&[0x1f, 0x8b, 8]), Compression::Gzip);
        assert_eq!(Compression::sniff(b"\xfd7zXZ\x00"), Compression::Xz);
        assert_eq!(
            Compression::sniff(&[0x28, 0xb5, 0x2f, 0xfd]),
            Compression::Zstd
        );
        assert_eq!(Compression::sniff(b"BZh9"), Compression::Bzip2);
        assert_eq!(Compression::sniff(b"PK\x03\x04"), Compression::Zip);
        assert_eq!(Compression::sniff(b"\x00\x00"), Compression::None);
    }
}
