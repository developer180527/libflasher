//! The decompressed size of an `.xz` file, read from its index without
//! decompressing anything.
//!
//! An xz file is one or more streams, each ending in an index (one record per
//! block: its compressed and decompressed size) and a 12-byte footer saying
//! how long the index is. Walking the streams from the end backwards and
//! summing the records gives the exact size in a few small reads — so a
//! 1 GB `.img.xz` can say "5.9 GB" before the first byte is written.
//!
//! Anything unexpected returns `None`: the caller then falls back to an
//! estimate, and says it is one.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const FOOTER: u64 = 12;
const HEADER: u64 = 12;
/// No real index is near this; refuse rather than allocate whatever a broken
/// footer claims.
const MAX_INDEX: u64 = 64 << 20;

pub fn uncompressed_size(path: &Path) -> Option<u64> {
    let mut f = File::open(path).ok()?;
    let mut end = f.metadata().ok()?.len();
    let mut total = 0u64;

    while end > 0 {
        // Stream padding: zero bytes between streams, in multiples of four.
        let mut word = [0u8; 4];
        while end >= 4 {
            read_at(&mut f, end - 4, &mut word)?;
            if word != [0; 4] {
                break;
            }
            end -= 4;
        }
        if end < HEADER + FOOTER {
            return None;
        }

        let mut footer = [0u8; FOOTER as usize];
        read_at(&mut f, end - FOOTER, &mut footer)?;
        if &footer[10..12] != b"YZ" {
            return None;
        }
        let index_size = (u32::from_le_bytes(footer[4..8].try_into().ok()?) as u64 + 1) * 4;
        if index_size > MAX_INDEX || index_size + FOOTER + HEADER > end {
            return None;
        }

        let mut index = vec![0u8; index_size as usize];
        read_at(&mut f, end - FOOTER - index_size, &mut index)?;
        let (blocks_size, uncompressed) = parse_index(&index)?;
        total = total.checked_add(uncompressed)?;

        let stream = HEADER
            .checked_add(blocks_size)?
            .checked_add(index_size)?
            .checked_add(FOOTER)?;
        end = end.checked_sub(stream)?;
    }
    Some(total)
}

/// Returns (bytes the blocks occupy, decompressed bytes) from one index.
fn parse_index(index: &[u8]) -> Option<(u64, u64)> {
    let mut p = Cursor(index, 0);
    if p.byte()? != 0x00 {
        return None;
    }
    let records = p.varint()?;
    let (mut blocks, mut uncompressed) = (0u64, 0u64);
    for _ in 0..records {
        let unpadded = p.varint()?;
        // Blocks are padded to four bytes in the file.
        blocks = blocks.checked_add(unpadded.checked_add(3)? & !3)?;
        uncompressed = uncompressed.checked_add(p.varint()?)?;
    }
    Some((blocks, uncompressed))
}

struct Cursor<'a>(&'a [u8], usize);

impl Cursor<'_> {
    fn byte(&mut self) -> Option<u8> {
        let b = *self.0.get(self.1)?;
        self.1 += 1;
        Some(b)
    }

    /// xz's multibyte integer: 7 bits per byte, little end first, at most 9 bytes.
    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for i in 0..9 {
            let b = self.byte()?;
            v |= ((b & 0x7f) as u64) << (7 * i);
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
}

fn read_at(f: &mut File, at: u64, buf: &mut [u8]) -> Option<()> {
    f.seek(SeekFrom::Start(at)).ok()?;
    f.read_exact(buf).ok()
}

#[cfg(all(test, feature = "xz"))]
mod tests {
    use std::io::Write;

    use super::*;

    fn xz(data: &[u8]) -> Vec<u8> {
        let mut e = xz2::write::XzEncoder::new(Vec::new(), 1);
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("flasher_xz_{}_{name}", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn reads_size_of_one_stream() {
        let data: Vec<u8> = (0..3_000_123u32).map(|i| (i % 7) as u8).collect();
        let p = temp("one.xz", &xz(&data));
        assert_eq!(uncompressed_size(&p), Some(data.len() as u64));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn sums_concatenated_streams_with_padding() {
        let mut file = xz(&[1u8; 100_000]);
        file.extend_from_slice(&[0; 8]);
        file.extend(xz(&[2u8; 54_321]));
        let p = temp("multi.xz", &file);
        assert_eq!(uncompressed_size(&p), Some(154_321));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn refuses_garbage() {
        let p = temp("bad.xz", b"definitely not an xz file at all");
        assert_eq!(uncompressed_size(&p), None);
        let mut truncated = xz(&[3u8; 10_000]);
        truncated.truncate(truncated.len() - 5);
        std::fs::write(&p, truncated).unwrap();
        assert_eq!(uncompressed_size(&p), None);
        std::fs::remove_file(p).ok();
    }
}
