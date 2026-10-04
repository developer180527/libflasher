//! Reading the one disk image inside a `.zip` (APPNOTE.TXT, PKWARE's
//! specification), as some distributions ship them.
//!
//! The central directory at the end of the file says where each entry is,
//! how it is compressed and how big it is uncompressed — so, as with `.xz`,
//! the exact size is known before writing. ZIP64 is supported: images over
//! 4 GB need it. Entries may be stored or deflated; anything else
//! (encrypted, Deflate64, LZMA, …) is refused by name.
//!
//! The archive must hold one image: a single file, or a single `.img`/`.iso`
//! among other files. Otherwise it is refused with the names it holds,
//! rather than a guess.
//!
//! Written from the specification; every size, count and offset read from
//! the file is bounded or checked against the file's length before use, and
//! the entry's CRC-32 is checked as it is read.

use std::io::{self, BufReader, Read, Seek, SeekFrom};

/// End of central directory record: 22 bytes, then up to 64 KiB of comment.
const EOCD_SIG: u32 = 0x0605_4b50;
const EOCD_LEN: u64 = 22;
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
const ZIP64_LOCATOR_LEN: u64 = 20;
const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const LOCAL_SIG: u32 = 0x0403_4b50;
/// No real image archive has a central directory near this.
const MAX_CENTRAL: u64 = 16 << 20;
const MAX_ENTRIES: u64 = 100_000;

const STORED: u16 = 0;
const DEFLATED: u16 = 8;

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("not a readable zip archive: {}", msg.into()),
    )
}

fn unsupported(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, msg.into())
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// The image inside an archive: where its bytes are and what they become.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Its name in the archive.
    pub name: String,
    /// Compression method: stored or deflated.
    method: u16,
    crc32: u32,
    compressed: u64,
    /// Its size uncompressed: the disk image's size.
    pub size: u64,
    /// Offset of its local header.
    header: u64,
}

/// One central directory record, as much of it as is needed.
struct Record {
    name: String,
    flags: u16,
    method: u16,
    crc32: u32,
    compressed: u64,
    size: u64,
    header: u64,
}

fn read_at<R: Read + Seek>(r: &mut R, at: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut b = vec![0u8; len];
    r.seek(SeekFrom::Start(at))?;
    r.read_exact(&mut b)?;
    Ok(b)
}

/// Find the image in the archive `r`.
pub fn find<R: Read + Seek>(r: &mut R) -> io::Result<Entry> {
    let len = r.seek(SeekFrom::End(0))?;
    if len < EOCD_LEN {
        return Err(bad("too short"));
    }
    // The end record is the last 22 bytes, unless a comment follows it.
    let tail_len = len.min(EOCD_LEN + u16::MAX as u64);
    let tail = read_at(r, len - tail_len, tail_len as usize)?;
    let eocd_at = (0..=tail.len() - EOCD_LEN as usize)
        .rev()
        .find(|&i| u32_at(&tail, i) == EOCD_SIG)
        .ok_or_else(|| bad("no end of central directory"))?;
    let eocd = &tail[eocd_at..];
    let eocd_pos = len - tail_len + eocd_at as u64;
    if u16_at(eocd, 4) != 0 || u16_at(eocd, 6) != 0 {
        return Err(unsupported(
            "split (multi-part) zip archives are not supported; join them first",
        ));
    }
    let mut count = u16_at(eocd, 10) as u64;
    let mut cd_size = u32_at(eocd, 12) as u64;
    let mut cd_at = u32_at(eocd, 16) as u64;

    // ZIP64: the real values are in a second end record, found through the
    // locator just before the first.
    if (count == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_at == 0xFFFF_FFFF)
        && eocd_pos >= ZIP64_LOCATOR_LEN
    {
        let loc = read_at(r, eocd_pos - ZIP64_LOCATOR_LEN, ZIP64_LOCATOR_LEN as usize)?;
        if u32_at(&loc, 0) == ZIP64_LOCATOR_SIG {
            let at = u64_at(&loc, 8);
            if at.checked_add(56).is_none_or(|end| end > eocd_pos) {
                return Err(bad("ZIP64 end record out of bounds"));
            }
            let z = read_at(r, at, 56)?;
            if u32_at(&z, 0) != ZIP64_EOCD_SIG {
                return Err(bad("no ZIP64 end record"));
            }
            count = u64_at(&z, 32);
            cd_size = u64_at(&z, 40);
            cd_at = u64_at(&z, 48);
        }
    }
    if count > MAX_ENTRIES || cd_size > MAX_CENTRAL {
        return Err(bad("central directory too large"));
    }
    if cd_at.checked_add(cd_size).is_none_or(|end| end > eocd_pos) {
        return Err(bad("central directory out of bounds"));
    }

    let cd = read_at(r, cd_at, cd_size as usize)?;
    let mut records = Vec::new();
    let mut pos = 0usize;
    for _ in 0..count {
        records.push(record(&cd, &mut pos)?);
    }
    let entry = choose(records)?;
    for (bit, what) in [(0x0001, "encrypted"), (0x0040, "encrypted")] {
        if entry.flags & bit != 0 {
            return Err(unsupported(format!("{} in the zip is {what}", entry.name)));
        }
    }
    if entry.method != STORED && entry.method != DEFLATED {
        return Err(unsupported(format!(
            "{} in the zip is compressed with method {}; only stored and deflate are supported",
            entry.name, entry.method
        )));
    }
    if entry.method == STORED && entry.compressed != entry.size {
        return Err(bad("a stored entry's sizes disagree"));
    }
    Ok(Entry {
        name: entry.name,
        method: entry.method,
        crc32: entry.crc32,
        compressed: entry.compressed,
        size: entry.size,
        header: entry.header,
    })
}

/// Parse the central directory record at `*pos`, moving past it.
fn record(cd: &[u8], pos: &mut usize) -> io::Result<Record> {
    let h = cd
        .get(*pos..*pos + 46)
        .ok_or_else(|| bad("central directory truncated"))?;
    if u32_at(h, 0) != CENTRAL_SIG {
        return Err(bad("broken central directory"));
    }
    let (n, e, c) = (
        u16_at(h, 28) as usize,
        u16_at(h, 30) as usize,
        u16_at(h, 32) as usize,
    );
    let name = cd
        .get(*pos + 46..*pos + 46 + n)
        .ok_or_else(|| bad("name out of bounds"))?;
    let extra = cd
        .get(*pos + 46 + n..*pos + 46 + n + e)
        .ok_or_else(|| bad("extra field out of bounds"))?;
    let mut rec = Record {
        name: String::from_utf8_lossy(name).into_owned(),
        flags: u16_at(h, 8),
        method: u16_at(h, 10),
        crc32: u32_at(h, 16),
        compressed: u32_at(h, 20) as u64,
        size: u32_at(h, 24) as u64,
        header: u32_at(h, 42) as u64,
    };
    // ZIP64 extended information: the 64-bit values, for exactly the fields
    // that hold 0xFFFFFFFF, in this order.
    let mut x = extra;
    while x.len() >= 4 {
        let (id, len) = (u16_at(x, 0), u16_at(x, 2) as usize);
        let body = x
            .get(4..4 + len)
            .ok_or_else(|| bad("extra field out of bounds"))?;
        if id == 0x0001 {
            let mut b = body;
            for field in [&mut rec.size, &mut rec.compressed, &mut rec.header] {
                if *field == 0xFFFF_FFFF {
                    let v = b.get(..8).ok_or_else(|| bad("short ZIP64 field"))?;
                    *field = u64_at(v, 0);
                    b = &b[8..];
                }
            }
        }
        x = &x[4 + len..];
    }
    *pos += 46 + n + e + c;
    Ok(rec)
}

/// Files that are not the image: directories, and the metadata macOS's
/// Archive Utility adds.
fn is_clutter(name: &str) -> bool {
    name.ends_with('/')
        || name.starts_with("__MACOSX/")
        || name
            .rsplit('/')
            .next()
            .is_some_and(|f| f.starts_with("._") || f == ".DS_Store")
}

/// The one image among the archive's records, or a refusal naming them.
fn choose(records: Vec<Record>) -> io::Result<Record> {
    let files: Vec<Record> = records
        .into_iter()
        .filter(|r| !is_clutter(&r.name))
        .collect();
    if files.len() == 1 {
        return Ok(files.into_iter().next().unwrap());
    }
    let image = |r: &Record| {
        let n = r.name.to_ascii_lowercase();
        n.ends_with(".img") || n.ends_with(".iso")
    };
    if files.iter().filter(|r| image(r)).count() == 1 {
        return Ok(files.into_iter().find(image).unwrap());
    }
    if files.is_empty() {
        return Err(unsupported("the zip archive holds no files"));
    }
    let mut names: Vec<&str> = files.iter().take(10).map(|r| r.name.as_str()).collect();
    if files.len() > 10 {
        names.push("…");
    }
    Err(unsupported(format!(
        "the zip archive holds {} files and it is not clear which is the image ({}); extract it first",
        files.len(),
        names.join(", ")
    )))
}

/// The entry's uncompressed bytes, checked against its size and CRC-32.
pub fn reader<R: Read + Seek + Send + 'static>(
    mut r: R,
    entry: &Entry,
) -> io::Result<Box<dyn Read + Send>> {
    let file_len = r.seek(SeekFrom::End(0))?;
    let local = read_at(&mut r, entry.header, 30)?;
    if u32_at(&local, 0) != LOCAL_SIG {
        return Err(bad("broken local header"));
    }
    let data = entry.header + 30 + u16_at(&local, 26) as u64 + u16_at(&local, 28) as u64;
    if data
        .checked_add(entry.compressed)
        .is_none_or(|end| end > file_len)
    {
        return Err(bad("entry data out of bounds"));
    }
    r.seek(SeekFrom::Start(data))?;
    let raw = BufReader::with_capacity(1 << 20, r).take(entry.compressed);
    let inner: Box<dyn Read + Send> = match entry.method {
        DEFLATED => Box::new(flate2::bufread::DeflateDecoder::new(raw)),
        _ => Box::new(raw),
    };
    Ok(Box::new(Checked {
        inner,
        name: entry.name.clone(),
        want_len: entry.size,
        want_crc: entry.crc32,
        len: 0,
        crc: crc32fast::Hasher::new(),
    }))
}

/// Passes bytes through; at the end, they must be as many as the archive
/// says, with its CRC-32.
struct Checked {
    inner: Box<dyn Read + Send>,
    name: String,
    want_len: u64,
    want_crc: u32,
    len: u64,
    crc: crc32fast::Hasher,
}

impl Read for Checked {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.len += n as u64;
        if self.len > self.want_len {
            return Err(bad(format!(
                "{} is longer than the archive says",
                self.name
            )));
        }
        self.crc.update(&buf[..n]);
        if n == 0 && !buf.is_empty() {
            if self.len != self.want_len {
                return Err(bad(format!(
                    "{} is shorter than the archive says",
                    self.name
                )));
            }
            if self.crc.clone().finalize() != self.want_crc {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} in the zip is damaged (its CRC-32 does not match); download it again",
                        self.name
                    ),
                ));
            }
        }
        Ok(n)
    }
}

/// A small zip writer, for tests: stored or deflated entries, optionally
/// with ZIP64 records or data descriptors, as other tools write them.
#[cfg(test)]
pub(crate) mod build {
    use std::io::Write;

    pub struct Options {
        pub deflate: bool,
        pub zip64: bool,
        /// Sizes and CRC after the data, zeros in the local header (bit 3),
        /// as streaming writers and macOS's Archive Utility do.
        pub descriptor: bool,
    }

    pub fn zip(files: &[(&str, &[u8])], o: &Options) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in files {
            let body = if o.deflate {
                let mut e =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
                e.write_all(data).unwrap();
                e.finish().unwrap()
            } else {
                data.to_vec()
            };
            let crc = crc32fast::hash(data);
            let method: u16 = if o.deflate { 8 } else { 0 };
            let flags: u16 = if o.descriptor { 8 } else { 0 };
            let header = out.len() as u64;
            let small = |v: u64| if o.zip64 { 0xFFFF_FFFF } else { v as u32 };

            out.extend(0x0403_4b50u32.to_le_bytes());
            out.extend(45u16.to_le_bytes());
            out.extend(flags.to_le_bytes());
            out.extend(method.to_le_bytes());
            out.extend([0u8; 4]); // time, date
            if o.descriptor {
                out.extend([0u8; 12]);
            } else {
                out.extend(crc.to_le_bytes());
                out.extend(small(body.len() as u64).to_le_bytes());
                out.extend(small(data.len() as u64).to_le_bytes());
            }
            out.extend((name.len() as u16).to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(name.as_bytes());
            out.extend(&body);
            if o.descriptor {
                out.extend(0x0807_4b50u32.to_le_bytes());
                out.extend(crc.to_le_bytes());
                out.extend((body.len() as u32).to_le_bytes());
                out.extend((data.len() as u32).to_le_bytes());
            }

            let mut extra = Vec::new();
            if o.zip64 {
                extra.extend(1u16.to_le_bytes());
                extra.extend(24u16.to_le_bytes());
                extra.extend((data.len() as u64).to_le_bytes());
                extra.extend((body.len() as u64).to_le_bytes());
                extra.extend(header.to_le_bytes());
            }
            central.extend(0x0201_4b50u32.to_le_bytes());
            central.extend(45u16.to_le_bytes());
            central.extend(45u16.to_le_bytes());
            central.extend(flags.to_le_bytes());
            central.extend(method.to_le_bytes());
            central.extend([0u8; 4]);
            central.extend(crc.to_le_bytes());
            central.extend(small(body.len() as u64).to_le_bytes());
            central.extend(small(data.len() as u64).to_le_bytes());
            central.extend((name.len() as u16).to_le_bytes());
            central.extend((extra.len() as u16).to_le_bytes());
            central.extend([0u8; 6]); // comment length, disk, internal attributes
            central.extend([0u8; 4]); // external attributes
            central.extend(small(header).to_le_bytes());
            central.extend(name.as_bytes());
            central.extend(&extra);
        }
        let cd_at = out.len() as u64;
        out.extend(&central);
        let count = files.len() as u64;
        if o.zip64 {
            let z64 = out.len() as u64;
            out.extend(0x0606_4b50u32.to_le_bytes());
            out.extend(44u64.to_le_bytes());
            out.extend([45, 0, 45, 0]);
            out.extend([0u8; 8]); // disk numbers
            out.extend(count.to_le_bytes());
            out.extend(count.to_le_bytes());
            out.extend((central.len() as u64).to_le_bytes());
            out.extend(cd_at.to_le_bytes());
            out.extend(0x0706_4b50u32.to_le_bytes());
            out.extend(0u32.to_le_bytes());
            out.extend(z64.to_le_bytes());
            out.extend(1u32.to_le_bytes());
        }
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend([0u8; 4]);
        let c16 = if o.zip64 { 0xFFFF } else { count as u16 };
        out.extend(c16.to_le_bytes());
        out.extend(c16.to_le_bytes());
        let small = |v: u64| if o.zip64 { 0xFFFF_FFFF } else { v as u32 };
        out.extend(small(central.len() as u64).to_le_bytes());
        out.extend(small(cd_at).to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::build::{zip, Options};
    use super::*;
    use std::io::Cursor;

    fn image() -> Vec<u8> {
        (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect()
    }

    fn read_all(z: Vec<u8>) -> io::Result<(Entry, Vec<u8>)> {
        let mut c = Cursor::new(z);
        let e = find(&mut c)?;
        let mut out = Vec::new();
        reader(c, &e)?.read_to_end(&mut out)?;
        Ok((e, out))
    }

    #[test]
    fn reads_every_kind_of_entry() {
        let data = image();
        for deflate in [false, true] {
            for zip64 in [false, true] {
                for descriptor in [false, true] {
                    let o = Options {
                        deflate,
                        zip64,
                        descriptor,
                    };
                    let (e, got) = read_all(zip(&[("pi.img", &data)], &o)).unwrap();
                    let what = format!("deflate {deflate}, zip64 {zip64}, descriptor {descriptor}");
                    assert_eq!(e.size, data.len() as u64, "{what}");
                    assert!(got == data, "{what}");
                }
            }
        }
    }

    #[test]
    fn picks_the_image_among_other_files() {
        let data = image();
        let o = Options {
            deflate: true,
            zip64: false,
            descriptor: false,
        };
        let z = zip(
            &[
                ("disk/", b""),
                ("README.txt", b"read me"),
                ("__MACOSX/._disk.img", b"junk"),
                ("disk/disk.img", &data),
            ],
            &o,
        );
        let (e, got) = read_all(z).unwrap();
        assert_eq!(e.name, "disk/disk.img");
        assert!(got == data);
    }

    #[test]
    fn refuses_an_archive_without_one_clear_image() {
        let o = Options {
            deflate: false,
            zip64: false,
            descriptor: false,
        };
        let z = zip(&[("a.img", b"1"), ("b.img", b"2")], &o);
        let e = find(&mut Cursor::new(z)).unwrap_err();
        assert!(e.to_string().contains("a.img, b.img"), "{e}");
        let z = zip(&[("notes.txt", b"1"), ("data.bin", b"2")], &o);
        assert!(find(&mut Cursor::new(z)).is_err());
    }

    #[test]
    fn damage_is_caught_by_the_crc() {
        let data = image();
        let o = Options {
            deflate: false,
            zip64: false,
            descriptor: false,
        };
        let mut z = zip(&[("pi.img", &data)], &o);
        z[1000] ^= 0xFF; // inside the stored data
        let e = read_all(z).unwrap_err();
        assert!(e.to_string().contains("CRC-32"), "{e}");
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        let o = Options {
            deflate: false,
            zip64: false,
            descriptor: false,
        };
        let z = zip(&[("pi.img", b"data")], &o);
        // Method 14 (LZMA), in the central directory record.
        let mut lzma = z.clone();
        let cd = lzma
            .windows(4)
            .position(|w| w == CENTRAL_SIG.to_le_bytes())
            .unwrap();
        lzma[cd + 10] = 14;
        assert!(find(&mut Cursor::new(lzma))
            .unwrap_err()
            .to_string()
            .contains("method 14"));
        // Encrypted.
        let mut enc = z;
        enc[cd + 8] |= 1;
        assert!(find(&mut Cursor::new(enc))
            .unwrap_err()
            .to_string()
            .contains("encrypted"));
    }

    /// Archives written by tools that are not ours: Info-ZIP's `zip`, and
    /// macOS's `ditto` (Finder's "Compress"), where present.
    #[test]
    fn reads_archives_other_tools_write() {
        let dir = std::env::temp_dir().join(format!("libflasher_zip_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let data = image();
        std::fs::write(dir.join("disk.img"), &data).unwrap();
        let tools: [(&str, &[&str]); 2] = [
            ("zip", &["-q", "zip.zip", "disk.img"]),
            (
                "ditto",
                &["-c", "-k", "--sequesterRsrc", "disk.img", "ditto.zip"],
            ),
        ];
        for (tool, args) in tools {
            let made = std::process::Command::new(tool)
                .args(args)
                .current_dir(&dir)
                .output()
                .is_ok_and(|o| o.status.success());
            if !made {
                continue; // not installed here
            }
            let z = std::fs::read(dir.join(format!("{tool}.zip"))).unwrap();
            let (e, got) = read_all(z).unwrap_or_else(|e| panic!("{tool}: {e}"));
            assert_eq!(e.name, "disk.img", "{tool}");
            assert!(got == data, "{tool}: contents differ");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
