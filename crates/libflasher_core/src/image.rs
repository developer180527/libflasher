//! What is in an image file, and a reader that yields its raw disk bytes.

use std::fs::File;
use std::io::{self, BufReader, Read};
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
        }
    }

    fn sniff(magic: &[u8]) -> Self {
        match magic {
            [0x1f, 0x8b, ..] => Self::Gzip,
            [0xfd, b'7', b'z', b'X', b'Z', 0x00, ..] => Self::Xz,
            [0x28, 0xb5, 0x2f, 0xfd, ..] => Self::Zstd,
            [b'B', b'Z', b'h', ..] => Self::Bzip2,
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
    /// An ISO 9660 filesystem with no partition table: Windows install media
    /// and some older ISOs. Booting it from USB needs "extract" mode — a new
    /// partition table, a filesystem and a bootloader — which is not built yet.
    PlainIso,
    /// No partition table and no ISO header. May still be valid (a bare
    /// filesystem image); written byte for byte, with a warning.
    Unknown,
}

impl ImageKind {
    fn classify(head: &[u8]) -> Self {
        let mbr = head.len() >= 512 && head[510] == 0x55 && head[511] == 0xAA;
        let gpt = head.len() >= 520 && &head[512..520] == b"EFI PART";
        let iso = head.len() >= 0x8006 && &head[0x8001..0x8006] == b"CD001";
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
    /// (uncompressed and `.xz` images). `None` means unknown, not zero.
    pub disk_size: Option<u64>,
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
    let file_size = file.metadata()?.len();

    let mut magic = [0u8; 8];
    let n = read_full(&mut file, &mut magic)?;
    let compression = Compression::sniff(&magic[..n]);

    let mut head = vec![0u8; HEAD];
    let mut reader = decoder(compression, File::open(&path)?)?;
    let n = read_full(&mut reader, &mut head)?;
    head.truncate(n);

    let disk_size = match compression {
        Compression::None => Some(file_size),
        // Exact, from the index at the end of the file; no decompression.
        Compression::Xz => crate::xz_size::uncompressed_size(&path),
        // gzip records the size modulo 4 GiB, which is a guess for disk
        // images; zstd only sometimes records it; bzip2 never. Unknown is honest.
        _ => None,
    };
    Ok(ImageInfo {
        path,
        compression,
        kind: ImageKind::classify(&head),
        file_size,
        disk_size,
    })
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
    /// Open the image for reading its decompressed bytes from the start.
    pub fn open(&self) -> Result<ImageReader> {
        let consumed = Arc::new(AtomicU64::new(0));
        let file = Counting {
            inner: File::open(&self.path)?,
            count: consumed.clone(),
        };
        Ok(ImageReader {
            reader: decoder(self.compression, file)?,
            consumed,
        })
    }
}

fn decoder<R: Read + Send + 'static>(c: Compression, r: R) -> Result<Box<dyn Read + Send>> {
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
        assert_eq!(Compression::sniff(b"\x00\x00"), Compression::None);
    }
}
