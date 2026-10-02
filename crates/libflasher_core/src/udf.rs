//! Reading files out of a UDF image (ECMA-167 with the OSTA UDF profile),
//! the filesystem Windows install ISOs keep their files in.
//!
//! Supports what read-only disc images use: one partition with a type 1 map,
//! File Entries and Extended File Entries, short and long allocation
//! descriptors, embedded data, continuation extents and holes. Not UDF 2.50
//! metadata partitions or virtual allocation tables (rewritable media),
//! which are refused by name. Written from the standards; every size, depth
//! and count read from the image is bounded.

use std::io::{self, Read, Seek, SeekFrom};

use crate::iso9660::{read_extents, Entry, HOLE};

const SECTOR: u64 = 2048;
const MAX_DIR_BYTES: u64 = 16 << 20;
const MAX_DEPTH: usize = 64;
const MAX_ENTRIES: usize = 1_000_000;
/// Allocation-extent continuations followed for one file.
const MAX_CONTINUATIONS: usize = 4096;

mod tag {
    pub const ANCHOR: u16 = 2;
    pub const PARTITION: u16 = 5;
    pub const LOGICAL_VOLUME: u16 = 6;
    pub const TERMINATING: u16 = 8;
    pub const FILE_SET: u16 = 256;
    pub const FILE_ID: u16 = 257;
    pub const ALLOC_EXTENT: u16 = 258;
    pub const FILE_ENTRY: u16 = 261;
    pub const EXT_FILE_ENTRY: u16 = 266;
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("not a readable UDF image: {}", msg.into()),
    )
}

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// A descriptor tag's identifier, after checking its checksum: the sum of
/// the tag's bytes other than the checksum byte itself.
fn tag_id(b: &[u8]) -> Option<u16> {
    if b.len() < 16 {
        return None;
    }
    let sum = b[..16]
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 4)
        .fold(0u8, |a, (_, &x)| a.wrapping_add(x));
    (sum == b[4]).then(|| u16le(b, 0))
}

/// Whether the image announces UDF: an `NSR02`/`NSR03` descriptor in the
/// volume recognition sequence that starts at sector 16 (after any ISO 9660
/// descriptors, in a "bridge" image).
pub fn is_udf<R: Read + Seek>(r: &mut R) -> io::Result<bool> {
    let mut s = [0u8; 6];
    for lba in 16..16 + 64 {
        r.seek(SeekFrom::Start(lba * SECTOR))?;
        if r.read_exact(&mut s).is_err() {
            return Ok(false);
        }
        match &s[1..6] {
            b"NSR02" | b"NSR03" => return Ok(true),
            b"CD001" | b"BEA01" | b"TEA01" | b"BOOT2" | b"CDW02" => continue,
            _ => return Ok(false),
        }
    }
    Ok(false)
}

/// What a File Entry says: kind, length, and where the bytes are.
struct FileData {
    is_dir: bool,
    size: u64,
    extents: Vec<(u64, u64)>,
}

/// An opened UDF image.
pub struct Udf<R> {
    r: R,
    block: u64,
    /// The partition's first block, in blocks from the start of the image.
    partition: u64,
    root: u32,
    /// The logical volume identifier, e.g. `CCCOMA_X64FRE_EN-US_DV9`.
    pub volume_id: String,
}

impl<R: Read + Seek> Udf<R> {
    /// Read the anchor, the volume descriptors and the file set.
    pub fn open(mut r: R) -> io::Result<Self> {
        if !is_udf(&mut r)? {
            return Err(bad("no NSR02/NSR03 descriptor"));
        }
        let anchor = read_at(&mut r, 256 * SECTOR, SECTOR as usize)?;
        if tag_id(&anchor) != Some(tag::ANCHOR) {
            return Err(bad("no anchor at sector 256"));
        }
        let (vds_len, vds_at) = (u32le(&anchor, 16) as u64, u32le(&anchor, 20) as u64);

        let mut partitions: Vec<(u16, u64)> = Vec::new();
        let mut lvd: Option<Vec<u8>> = None;
        for i in 0..(vds_len / SECTOR).min(256) {
            let d = read_at(&mut r, (vds_at + i) * SECTOR, SECTOR as usize)?;
            match tag_id(&d) {
                Some(tag::PARTITION) => partitions.push((u16le(&d, 22), u32le(&d, 188) as u64)),
                Some(tag::LOGICAL_VOLUME) => lvd = Some(d),
                Some(tag::TERMINATING) => break,
                _ => {}
            }
        }
        let lvd = lvd.ok_or_else(|| bad("no logical volume descriptor"))?;
        let block = u32le(&lvd, 212) as u64;
        if block != SECTOR {
            return Err(bad(format!("{block}-byte blocks")));
        }
        // Partition map 0 must be a type 1 map naming a partition we have.
        let map_len = u32le(&lvd, 264) as usize;
        let maps = lvd
            .get(440..440 + map_len)
            .ok_or_else(|| bad("partition maps out of bounds"))?;
        if maps.first() != Some(&1) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "UDF with a metadata or virtual partition (UDF 2.50 / rewritable media) is not supported",
            ));
        }
        let number = u16le(maps, 4);
        let partition = partitions
            .iter()
            .find(|(n, _)| *n == number)
            .map(|(_, start)| *start)
            .ok_or_else(|| bad(format!("partition {number} not described")))?;
        let volume_id = dstring(&lvd[84..84 + 128]);

        // The file set descriptor: where the root directory is.
        let fsd_lbn = u32le(&lvd, 248 + 4);
        let fsd = read_at(&mut r, (partition + fsd_lbn as u64) * block, block as usize)?;
        if tag_id(&fsd) != Some(tag::FILE_SET) {
            return Err(bad("no file set descriptor"));
        }
        let root = u32le(&fsd, 400 + 4);
        Ok(Udf {
            r,
            block,
            partition,
            root,
            volume_id,
        })
    }

    fn read_block(&mut self, lbn: u32) -> io::Result<Vec<u8>> {
        read_at(
            &mut self.r,
            (self.partition + lbn as u64) * self.block,
            self.block as usize,
        )
    }

    /// Byte offset in the image of a block in the partition.
    fn offset(&self, lbn: u32) -> u64 {
        (self.partition + lbn as u64) * self.block
    }

    /// A file or directory's (is_dir, size, extents), from its (Extended)
    /// File Entry.
    fn file_entry(&mut self, lbn: u32) -> io::Result<FileData> {
        let fe = self.read_block(lbn)?;
        let (ea_at, ad_base) = match tag_id(&fe) {
            Some(tag::FILE_ENTRY) => (168, 176),
            Some(tag::EXT_FILE_ENTRY) => (208, 216),
            other => {
                return Err(bad(format!(
                    "expected a file entry at block {lbn}, found tag {other:?}"
                )))
            }
        };
        let is_dir = fe[16 + 11] == 4;
        let ad_type = u16le(&fe, 16 + 18) & 7;
        let size = u64le(&fe, 56);
        let (l_ea, l_ad) = (u32le(&fe, ea_at) as usize, u32le(&fe, ea_at + 4) as usize);
        let start = ad_base + l_ea;
        let ads = fe
            .get(start..start + l_ad)
            .ok_or_else(|| bad("allocation descriptors out of bounds"))?
            .to_vec();

        if ad_type == 3 {
            // The data is inside the entry itself.
            if size > l_ad as u64 {
                return Err(bad("embedded data longer than its space"));
            }
            return Ok(FileData {
                is_dir,
                size,
                extents: vec![(self.offset(lbn) + start as u64, size)],
            });
        }
        let ad_len = match ad_type {
            0 => 8,  // short_ad
            1 => 16, // long_ad
            _ => return Err(bad(format!("allocation descriptor type {ad_type}"))),
        };
        let mut extents = Vec::new();
        let mut list = ads;
        let mut hops = 0;
        'lists: loop {
            for ad in list.chunks_exact(ad_len) {
                let raw = u32le(ad, 0);
                let (kind, len) = (raw >> 30, (raw & 0x3FFF_FFFF) as u64);
                if len == 0 {
                    break 'lists;
                }
                let pos = u32le(ad, 4);
                match kind {
                    0 => extents.push((self.offset(pos), len)),
                    1 | 2 => extents.push((HOLE, len)),
                    _ => {
                        // The list continues in an Allocation Extent Descriptor.
                        hops += 1;
                        if hops > MAX_CONTINUATIONS {
                            return Err(bad("too many allocation extents"));
                        }
                        let aed = self.read_block(pos)?;
                        if tag_id(&aed) != Some(tag::ALLOC_EXTENT) {
                            return Err(bad("broken allocation extent chain"));
                        }
                        let n = u32le(&aed, 20) as usize;
                        list = aed
                            .get(24..24 + n)
                            .ok_or_else(|| bad("allocation extent out of bounds"))?
                            .to_vec();
                        continue 'lists;
                    }
                }
            }
            break;
        }
        // Extents are block-rounded; the entry's length is the truth.
        let mut left = size;
        for e in extents.iter_mut() {
            e.1 = e.1.min(left);
            left -= e.1;
        }
        extents.retain(|e| e.1 > 0);
        if left > 0 {
            return Err(bad(format!("block {lbn}: extents shorter than the file")));
        }
        Ok(FileData {
            is_dir,
            size,
            extents,
        })
    }

    /// Every entry in the image, directories before their contents.
    pub fn walk(&mut self) -> io::Result<Vec<Entry>> {
        let mut out = Vec::new();
        let mut stack = vec![(String::new(), self.root, 0usize)];
        while let Some((prefix, lbn, depth)) = stack.pop() {
            if depth > MAX_DEPTH {
                return Err(bad("directories nested too deep"));
            }
            let FileData {
                is_dir,
                size,
                extents,
            } = self.file_entry(lbn)?;
            if !is_dir {
                return Err(bad(format!("{prefix:?} is not a directory")));
            }
            if size > MAX_DIR_BYTES {
                return Err(bad(format!("directory of {size} bytes")));
            }
            let mut data = Vec::with_capacity(size as usize);
            read_extents(
                &mut self.r,
                &Entry::new(String::new(), true, size, extents),
                &mut vec![0u8; 64 << 10],
                |b| {
                    data.extend_from_slice(b);
                    Ok(())
                },
            )?;
            let mut pos = 0usize;
            while pos + 38 <= data.len() {
                let fid = &data[pos..];
                if tag_id(fid) != Some(tag::FILE_ID) {
                    return Err(bad("broken directory"));
                }
                let flags = fid[18];
                let l_fi = fid[19] as usize;
                let icb = u32le(fid, 20 + 4);
                let l_iu = u16le(fid, 36) as usize;
                let total = (38 + l_iu + l_fi).div_ceil(4) * 4;
                if pos + 38 + l_iu + l_fi > data.len() {
                    return Err(bad("file identifier out of bounds"));
                }
                let name_bytes = &fid[38 + l_iu..38 + l_iu + l_fi];
                pos += total;
                // Bit 2: deleted; bit 3: the parent ("..").
                if flags & 0x0C != 0 || l_fi == 0 {
                    continue;
                }
                let name = osta_name(name_bytes).ok_or_else(|| bad("undecodable file name"))?;
                if name.is_empty() || name.contains('/') || name == "." || name == ".." {
                    return Err(bad(format!("unusable file name {name:?}")));
                }
                if out.len() >= MAX_ENTRIES {
                    return Err(bad("too many entries"));
                }
                let path = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                if flags & 0x02 != 0 {
                    stack.push((path.clone(), icb, depth + 1));
                    out.push(Entry::new(path, true, 0, Vec::new()));
                } else {
                    let FileData { size, extents, .. } = self.file_entry(icb)?;
                    out.push(Entry::new(path, false, size, extents));
                }
            }
        }
        Ok(out)
    }

    /// The image, for ranged reads of the entries this returned.
    pub(crate) fn reader(&mut self) -> &mut R {
        &mut self.r
    }

    /// Stream a file's bytes to `sink`, in pieces of at most `buf.len()`.
    pub fn read_file(
        &mut self,
        e: &Entry,
        buf: &mut [u8],
        sink: impl FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        read_extents(&mut self.r, e, buf, sink)
    }
}

fn read_at<R: Read + Seek>(r: &mut R, at: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut b = vec![0u8; len];
    r.seek(SeekFrom::Start(at))?;
    r.read_exact(&mut b)?;
    Ok(b)
}

/// OSTA compressed Unicode: a compression id (8: one byte per character,
/// 16: UTF-16 big-endian) and then the characters.
fn osta_name(b: &[u8]) -> Option<String> {
    let (&id, rest) = b.split_first()?;
    match id {
        8 => Some(rest.iter().map(|&c| c as char).collect()),
        16 if rest.len() % 2 == 0 => {
            let units: Vec<u16> = rest
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            Some(String::from_utf16_lossy(&units))
        }
        _ => None,
    }
}

/// A fixed-size dstring field: OSTA characters, with the used length in the
/// last byte.
fn dstring(field: &[u8]) -> String {
    let used = *field.last().unwrap_or(&0) as usize;
    if used == 0 || used >= field.len() {
        return String::new();
    }
    osta_name(&field[..used])
        .unwrap_or_default()
        .trim_matches(|c: char| c == ' ' || c == '\0')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_osta_names() {
        assert_eq!(osta_name(b"\x08setup.exe").unwrap(), "setup.exe");
        let utf16: Vec<u8> = std::iter::once(16)
            .chain("Ünïcode.txt".encode_utf16().flat_map(|u| u.to_be_bytes()))
            .collect();
        assert_eq!(osta_name(&utf16).unwrap(), "Ünïcode.txt");
        assert_eq!(osta_name(b"\x07x"), None);
        let mut field = [0u8; 32];
        field[..7].copy_from_slice(b"\x08CCCOMA");
        field[31] = 7;
        assert_eq!(dstring(&field), "CCCOMA");
    }

    #[test]
    fn checks_tag_checksums() {
        let mut t = [0u8; 16];
        t[0] = 2;
        t[4] = 2; // checksum = sum of other tag bytes
        assert_eq!(tag_id(&t), Some(2));
        t[4] = 3;
        assert_eq!(tag_id(&t), None);
    }

    #[test]
    fn plain_iso_is_not_udf() {
        let iso = crate::iso9660::build::iso("X", &[], false);
        assert!(!is_udf(&mut std::io::Cursor::new(iso)).unwrap());
    }

    /// Reads UDF images named in `FLASHER_TEST_UDF` (`:`-separated),
    /// checking a few paths the test scripts put there.
    #[test]
    #[ignore = "needs FLASHER_TEST_UDF"]
    fn reads_real_udf_images() {
        let Ok(list) = std::env::var("FLASHER_TEST_UDF") else {
            return;
        };
        for path in list.split(':') {
            let mut udf = Udf::open(std::fs::File::open(path).unwrap())
                .unwrap_or_else(|e| panic!("{path}: {e}"));
            let all = udf.walk().unwrap();
            println!("{path}: volume {:?}, {} entries", udf.volume_id, all.len());
            for e in &all {
                let mut n = 0u64;
                udf.read_file(e, &mut vec![0u8; 1 << 16], |b| {
                    n += b.len() as u64;
                    Ok(())
                })
                .unwrap();
                assert_eq!(n, e.size, "{}", e.path);
                println!(
                    "  {} {} {}",
                    if e.is_dir { "d" } else { "f" },
                    e.size,
                    e.path
                );
            }
        }
    }
}
