//! Splitting a Windows Imaging (WIM) file into `.swm` parts small enough
//! for FAT32, which Windows Setup reads in place of `install.wim`.
//!
//! A WIM is a header, compressed "resources" (file contents and per-image
//! metadata), a lookup table naming every resource, and XML describing the
//! images. A split set is the same resources shared out among several WIMs
//! — each with the same GUID, its part number and the total, and a lookup
//! table of only its own resources — with every image's metadata in part 1.
//! Nothing is decompressed or recompressed: resources are copied byte for
//! byte, so a split cannot corrupt the data, only move it.
//!
//! Written from Microsoft's description of the format. Solid resources (the
//! ESD-style LZMS archives) cannot be shared out and are refused.

use std::io;

const HEADER: usize = 208;
const ENTRY: usize = 50;
const MAGIC: &[u8; 8] = b"MSWIM\0\0\0";
/// Header flag: one part of a split set.
const HDR_SPANNED: u32 = 0x0000_0008;
/// Resource flags.
const RES_METADATA: u8 = 0x02;
const RES_COMPRESSED: u8 = 0x04;
const RES_SOLID: u8 = 0x10;
/// Largest lookup table read: over a million resources, far past any real
/// WIM. The header's claim is not trusted with an allocation.
const MAX_LOOKUP: u64 = 64 << 20;

/// Offsets of the header's resource headers.
mod at {
    pub const FLAGS: usize = 16;
    pub const PART: usize = 40;
    pub const TOTAL: usize = 42;
    pub const LOOKUP: usize = 48;
    pub const XML: usize = 72;
    pub const BOOT: usize = 96;
    pub const BOOT_INDEX: usize = 120;
    pub const INTEGRITY: usize = 124;
}

/// A resource header: size on disk (7 bytes), flags, offset, original size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResHdr {
    size: u64,
    flags: u8,
    offset: u64,
    original: u64,
}

impl ResHdr {
    fn read(b: &[u8]) -> Self {
        let mut size = [0u8; 8];
        size[..7].copy_from_slice(&b[..7]);
        ResHdr {
            size: u64::from_le_bytes(size),
            flags: b[7],
            offset: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            original: u64::from_le_bytes(b[16..24].try_into().unwrap()),
        }
    }

    fn write(&self, b: &mut [u8]) {
        b[..7].copy_from_slice(&self.size.to_le_bytes()[..7]);
        b[7] = self.flags;
        b[8..16].copy_from_slice(&self.offset.to_le_bytes());
        b[16..24].copy_from_slice(&self.original.to_le_bytes());
    }
}

/// Reads byte ranges of the WIM wherever it lives (inside an ISO, here).
pub(crate) trait RangeSource {
    fn read_range(
        &mut self,
        offset: u64,
        len: u64,
        buf: &mut [u8],
        sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()>;

    fn read_vec(&mut self, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len as usize);
        self.read_range(offset, len, &mut vec![0u8; 1 << 16], &mut |b| {
            out.extend_from_slice(b);
            Ok(())
        })?;
        Ok(out)
    }
}

/// One `.swm` file: its name and how to produce its bytes.
#[derive(Clone, Debug)]
pub(crate) struct Part {
    /// `install.swm`, `install2.swm`, … (the names Windows Setup looks for).
    pub name: String,
    pub size: u64,
    pieces: Vec<Piece>,
}

#[derive(Clone, Debug)]
enum Piece {
    Bytes(Vec<u8>),
    /// A range of the original WIM.
    Copy {
        offset: u64,
        len: u64,
    },
}

impl Part {
    /// Produce the part's bytes, in order, to `sink`.
    pub fn stream(
        &self,
        src: &mut dyn RangeSource,
        buf: &mut [u8],
        sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        for p in &self.pieces {
            match p {
                Piece::Bytes(b) => {
                    for chunk in b.chunks(buf.len().max(1)) {
                        sink(chunk)?;
                    }
                }
                Piece::Copy { offset, len } => src.read_range(*offset, *len, buf, sink)?,
            }
        }
        Ok(())
    }
}

struct Resource {
    hdr: ResHdr,
    /// The lookup table entry after the resource header: part number,
    /// reference count, SHA-1.
    rest: [u8; ENTRY - 24],
}

/// Plan the split of the WIM of `len` bytes into parts of at most `limit`
/// bytes each. `stem` names them: `install` → `install.swm`, `install2.swm`.
pub(crate) fn split(
    src: &mut dyn RangeSource,
    len: u64,
    limit: u64,
    stem: &str,
) -> Result<Vec<Part>, String> {
    let io = |e: io::Error| format!("reading install.wim: {e}");
    let header = src.read_vec(0, HEADER as u64).map_err(io)?;
    if &header[..8] != MAGIC
        || u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize != HEADER
    {
        return Err("install.wim is not a WIM file".into());
    }
    let part = u16::from_le_bytes([header[at::PART], header[at::PART + 1]]);
    let total = u16::from_le_bytes([header[at::TOTAL], header[at::TOTAL + 1]]);
    if part != 1 || total != 1 {
        return Err("install.wim is already part of a split set".into());
    }
    let lookup = ResHdr::read(&header[at::LOOKUP..]);
    let xml = ResHdr::read(&header[at::XML..]);
    let boot = ResHdr::read(&header[at::BOOT..]);
    if lookup.flags & RES_COMPRESSED != 0
        || !lookup.size.is_multiple_of(ENTRY as u64)
        || lookup.size > MAX_LOOKUP
    {
        return Err("install.wim has a lookup table this cannot read".into());
    }
    for r in [lookup, xml] {
        if r.offset.checked_add(r.size).is_none_or(|end| end > len) {
            return Err("install.wim is damaged: a table points past its end".into());
        }
    }
    let table = src.read_vec(lookup.offset, lookup.size).map_err(io)?;

    let mut metadata = Vec::new();
    let mut files = Vec::new();
    for e in table.as_chunks::<ENTRY>().0 {
        let hdr = ResHdr::read(e);
        if hdr.flags & RES_SOLID != 0 {
            return Err(
                "install.wim uses solid (ESD-style) compression, which cannot be split".into(),
            );
        }
        if hdr.offset.checked_add(hdr.size).is_none_or(|end| end > len) {
            return Err("install.wim is damaged: a resource lies past its end".into());
        }
        let r = Resource {
            hdr,
            rest: e[24..].try_into().unwrap(),
        };
        if hdr.flags & RES_METADATA != 0 {
            metadata.push(r)
        } else {
            files.push(r)
        }
    }
    if metadata.is_empty() {
        return Err("install.wim has no images".into());
    }
    // In the order they are stored, so each part is read sequentially.
    files.sort_by_key(|r| r.hdr.offset);

    // Share out: part 1 starts with every image's metadata; each part takes
    // resources until the next would push it over the limit.
    let overhead = |n: usize| (HEADER + n * ENTRY) as u64 + xml.size;
    let mut groups: Vec<Vec<Resource>> = vec![metadata];
    let mut used: u64 = groups[0].iter().map(|r| r.hdr.size).sum();
    if used + overhead(groups[0].len()) > limit {
        return Err("install.wim's image metadata alone is larger than a part may be".into());
    }
    for r in files {
        if r.hdr.size + overhead(1) > limit {
            return Err(format!(
                "install.wim holds one file of {} compressed, which no part can hold",
                crate::platform::human_size(r.hdr.size)
            ));
        }
        let g = groups.last_mut().unwrap();
        if used + r.hdr.size + overhead(g.len() + 1) > limit {
            groups.push(Vec::new());
            used = 0;
        }
        used += r.hdr.size;
        groups.last_mut().unwrap().push(r);
    }

    let total = groups.len();
    if total > u16::MAX as usize {
        return Err("install.wim would need too many parts".into());
    }
    let boot_index = u32::from_le_bytes(
        header[at::BOOT_INDEX..at::BOOT_INDEX + 4]
            .try_into()
            .unwrap(),
    );
    let mut parts = Vec::with_capacity(total);
    for (i, group) in groups.into_iter().enumerate() {
        let number = i + 1;
        let mut pieces = vec![Piece::Bytes(Vec::new())]; // the header, filled in last
        let mut offset = HEADER as u64;
        let mut table = Vec::with_capacity(group.len() * ENTRY);
        let mut new_boot = None;
        for r in &group {
            pieces.push(Piece::Copy {
                offset: r.hdr.offset,
                len: r.hdr.size,
            });
            let moved = ResHdr { offset, ..r.hdr };
            if number == 1 && boot.size != 0 && r.hdr.offset == boot.offset {
                new_boot = Some(moved);
            }
            let mut e = [0u8; ENTRY];
            moved.write(&mut e);
            e[24..].copy_from_slice(&r.rest);
            e[24..26].copy_from_slice(&(number as u16).to_le_bytes());
            table.extend_from_slice(&e);
            offset += r.hdr.size;
        }
        let table_hdr = ResHdr {
            size: table.len() as u64,
            flags: lookup.flags,
            offset,
            original: table.len() as u64,
        };
        offset += table.len() as u64;
        let xml_hdr = ResHdr { offset, ..xml };
        offset += xml.size;
        pieces.push(Piece::Bytes(table));
        pieces.push(Piece::Copy {
            offset: xml.offset,
            len: xml.size,
        });

        let mut h = header.clone();
        let flags =
            u32::from_le_bytes(h[at::FLAGS..at::FLAGS + 4].try_into().unwrap()) | HDR_SPANNED;
        h[at::FLAGS..at::FLAGS + 4].copy_from_slice(&flags.to_le_bytes());
        h[at::PART..at::PART + 2].copy_from_slice(&(number as u16).to_le_bytes());
        h[at::TOTAL..at::TOTAL + 2].copy_from_slice(&(total as u16).to_le_bytes());
        table_hdr.write(&mut h[at::LOOKUP..]);
        xml_hdr.write(&mut h[at::XML..]);
        // The boot image's metadata is in part 1; other parts point nowhere.
        match new_boot {
            Some(b) => b.write(&mut h[at::BOOT..]),
            None => {
                h[at::BOOT..at::BOOT + 24].fill(0);
                if number != 1 {
                    h[at::BOOT_INDEX..at::BOOT_INDEX + 4].fill(0);
                }
            }
        }
        if number == 1 {
            h[at::BOOT_INDEX..at::BOOT_INDEX + 4].copy_from_slice(&boot_index.to_le_bytes());
        }
        // No integrity table: it covered the old layout.
        h[at::INTEGRITY..at::INTEGRITY + 24].fill(0);
        pieces[0] = Piece::Bytes(h);

        let name = if number == 1 {
            format!("{stem}.swm")
        } else {
            format!("{stem}{number}.swm")
        };
        parts.push(Part {
            name,
            size: offset,
            pieces,
        });
    }
    Ok(parts)
}

impl RangeSource for &[u8] {
    fn read_range(
        &mut self,
        offset: u64,
        len: u64,
        buf: &mut [u8],
        sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        let end = offset.checked_add(len).filter(|&e| e <= self.len() as u64);
        let end = end.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "past the end"))?
            as usize;
        for c in self[offset as usize..end].chunks(buf.len().max(1)) {
            sink(c)?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small but well-formed WIM: `images` metadata resources, `files`
    /// file resources of the given sizes, a lookup table and XML. Contents
    /// are arbitrary bytes; each resource's "hash" is its index, so a split
    /// can be checked resource by resource.
    pub(crate) fn wim(images: usize, files: &[usize]) -> Vec<u8> {
        let mut out = vec![0u8; HEADER];
        let mut table = Vec::new();
        let mut boot = None;
        let all: Vec<(bool, usize)> = (0..images)
            .map(|_| (true, 300))
            .chain(files.iter().map(|&n| (false, n)))
            .collect();
        for (i, &(meta, n)) in all.iter().enumerate() {
            let offset = out.len() as u64;
            out.extend((0..n).map(|j| (j as u8) ^ (i as u8)));
            let hdr = ResHdr {
                size: n as u64,
                flags: RES_COMPRESSED | if meta { RES_METADATA } else { 0 },
                offset,
                original: n as u64 * 2,
            };
            if meta && boot.is_none() {
                boot = Some(hdr);
            }
            let mut e = [0u8; ENTRY];
            hdr.write(&mut e);
            e[24..26].copy_from_slice(&1u16.to_le_bytes());
            e[26..30].copy_from_slice(&1u32.to_le_bytes());
            e[30..34].copy_from_slice(&(i as u32).to_le_bytes());
            table.extend_from_slice(&e);
        }
        let lookup = ResHdr {
            size: table.len() as u64,
            flags: 0,
            offset: out.len() as u64,
            original: table.len() as u64,
        };
        out.extend_from_slice(&table);
        let xml: Vec<u8> = "\u{feff}<WIM><IMAGE INDEX=\"1\"><NAME>Test</NAME></IMAGE></WIM>"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let xml_hdr = ResHdr {
            size: xml.len() as u64,
            flags: 0,
            offset: out.len() as u64,
            original: xml.len() as u64,
        };
        out.extend_from_slice(&xml);
        out[..8].copy_from_slice(MAGIC);
        out[8..12].copy_from_slice(&(HEADER as u32).to_le_bytes());
        out[12..16].copy_from_slice(&0x10d00u32.to_le_bytes());
        out[at::FLAGS..at::FLAGS + 4].copy_from_slice(&0x0004_0002u32.to_le_bytes()); // compressed, LZX
        out[20..24].copy_from_slice(&32768u32.to_le_bytes());
        out[24..40].copy_from_slice(&[0xA5; 16]);
        out[at::PART..at::PART + 2].copy_from_slice(&1u16.to_le_bytes());
        out[at::TOTAL..at::TOTAL + 2].copy_from_slice(&1u16.to_le_bytes());
        out[44..48].copy_from_slice(&(images as u32).to_le_bytes());
        lookup.write(&mut out[at::LOOKUP..]);
        xml_hdr.write(&mut out[at::XML..]);
        boot.unwrap().write(&mut out[at::BOOT..]);
        out[at::BOOT_INDEX..at::BOOT_INDEX + 4].copy_from_slice(&1u32.to_le_bytes());
        out
    }

    fn materialise(src: &[u8], parts: &[Part]) -> Vec<Vec<u8>> {
        parts
            .iter()
            .map(|p| {
                let mut out = Vec::new();
                let mut s = src;
                p.stream(&mut s, &mut [0u8; 1000], &mut |b| {
                    out.extend_from_slice(b);
                    Ok(())
                })
                .unwrap();
                assert_eq!(out.len() as u64, p.size, "{} size", p.name);
                out
            })
            .collect()
    }

    #[test]
    fn every_resource_lands_in_exactly_one_part_intact() {
        let files = [4000, 9000, 2500, 7000, 3000, 6000, 100, 8000];
        let src = wim(2, &files);
        let limit = 16_000;
        let parts = split(&mut &src[..], src.len() as u64, limit, "install").unwrap();
        assert!(parts.len() > 1, "test should actually split");
        assert_eq!(parts[0].name, "install.swm");
        assert_eq!(parts[1].name, "install2.swm");
        let bytes = materialise(&src, &parts);

        let orig_table = ResHdr::read(&src[at::LOOKUP..]);
        let orig: Vec<&[u8]> = src[orig_table.offset as usize..][..orig_table.size as usize]
            .as_chunks::<ENTRY>()
            .0
            .iter()
            .map(|e| &e[..])
            .collect();
        let mut seen = vec![0; orig.len()];
        for (i, p) in bytes.iter().enumerate() {
            let n = i + 1;
            assert!(p.len() as u64 <= limit, "part {n} is {} bytes", p.len());
            assert_eq!(&p[..8], MAGIC);
            assert_eq!(&p[24..40], &src[24..40], "same GUID in every part");
            assert_eq!(
                u16::from_le_bytes([p[at::PART], p[at::PART + 1]]) as usize,
                n
            );
            assert_eq!(
                u16::from_le_bytes([p[at::TOTAL], p[at::TOTAL + 1]]) as usize,
                bytes.len()
            );
            assert_ne!(
                u32::from_le_bytes(p[at::FLAGS..at::FLAGS + 4].try_into().unwrap()) & HDR_SPANNED,
                0
            );
            let t = ResHdr::read(&p[at::LOOKUP..]);
            for e in p[t.offset as usize..][..t.size as usize]
                .as_chunks::<ENTRY>()
                .0
            {
                let h = ResHdr::read(e);
                let idx = u32::from_le_bytes(e[30..34].try_into().unwrap()) as usize;
                let o = ResHdr::read(orig[idx]);
                assert_eq!(
                    &p[h.offset as usize..][..h.size as usize],
                    &src[o.offset as usize..][..o.size as usize],
                    "resource {idx}"
                );
                assert_eq!((h.size, h.flags, h.original), (o.size, o.flags, o.original));
                assert_eq!(
                    u16::from_le_bytes([e[24], e[25]]) as usize,
                    n,
                    "entry's part number"
                );
                if h.flags & RES_METADATA != 0 {
                    assert_eq!(n, 1, "metadata must be in part 1");
                }
                seen[idx] += 1;
            }
            let x = ResHdr::read(&p[at::XML..]);
            let xo = ResHdr::read(&src[at::XML..]);
            assert_eq!(
                &p[x.offset as usize..][..x.size as usize],
                &src[xo.offset as usize..][..xo.size as usize]
            );
        }
        assert!(
            seen.iter().all(|&c| c == 1),
            "each resource exactly once: {seen:?}"
        );
        let b = ResHdr::read(&bytes[0][at::BOOT..]);
        assert_eq!(
            &bytes[0][b.offset as usize..][..b.size as usize],
            &src[ResHdr::read(&src[at::BOOT..]).offset as usize..][..b.size as usize]
        );
    }

    #[test]
    fn refuses_what_cannot_be_split() {
        let src = wim(1, &[50_000]);
        assert!(split(&mut &src[..], src.len() as u64, 20_000, "install")
            .unwrap_err()
            .contains("no part can hold"));
        let mut solid = wim(1, &[1000]);
        let t = ResHdr::read(&solid[at::LOOKUP..]);
        solid[t.offset as usize + ENTRY + 7] |= RES_SOLID;
        assert!(
            split(&mut &solid[..], solid.len() as u64, 1 << 20, "install")
                .unwrap_err()
                .contains("solid")
        );
        assert!(split(&mut &b"not a wim at all, not even close to it......................................................................................................................................................................................................"[..], 220, 1 << 20, "install").is_err());
    }

    /// Split the WIM in `FLASHER_TEST_WIM` into `FLASHER_TEST_SWM_DIR` with
    /// parts of `FLASHER_TEST_SWM_PART` bytes. CI then has wimlib, an
    /// independent implementation, verify and apply the split set.
    /// A header claiming a huge lookup table is refused before anything
    /// that size is allocated.
    #[test]
    fn refuses_an_oversized_lookup_table() {
        let mut w = wim(1, &[1000]);
        let size = (MAX_LOOKUP / ENTRY as u64 + 1) * ENTRY as u64;
        w[at::LOOKUP..at::LOOKUP + 7].copy_from_slice(&size.to_le_bytes()[..7]);
        w[at::LOOKUP + 8..at::LOOKUP + 16].copy_from_slice(&(HEADER as u64).to_le_bytes());
        let e = split(&mut &w[..], 4 << 30, 1 << 20, "install").unwrap_err();
        assert!(e.contains("lookup table"), "{e}");
    }

    #[test]
    #[ignore = "needs FLASHER_TEST_WIM, FLASHER_TEST_SWM_DIR, FLASHER_TEST_SWM_PART"]
    fn split_wim_to_dir() {
        let (Ok(wim), Ok(dir), Ok(part)) = (
            std::env::var("FLASHER_TEST_WIM"),
            std::env::var("FLASHER_TEST_SWM_DIR"),
            std::env::var("FLASHER_TEST_SWM_PART"),
        ) else {
            return;
        };
        let src = std::fs::read(&wim).unwrap();
        let parts = split(
            &mut &src[..],
            src.len() as u64,
            part.parse().unwrap(),
            "install",
        )
        .unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        for p in &parts {
            let mut out = Vec::new();
            p.stream(&mut &src[..], &mut [0u8; 1 << 16], &mut |b| {
                out.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
            std::fs::write(std::path::Path::new(&dir).join(&p.name), out).unwrap();
            println!("{} {} bytes", p.name, p.size);
        }
    }
}
