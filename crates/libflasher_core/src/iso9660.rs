//! Reading files out of an ISO 9660 image (ECMA-119), with the two common
//! long-name extensions: Rock Ridge (POSIX names, what most Linux ISOs use)
//! and Joliet (UCS-2 names, what Windows tools write).
//!
//! Only what extract mode needs: walk the tree, list files with their sizes
//! and where their bytes are, and read them. Written from the standards; it
//! trusts nothing in the image — sizes, depths and entry counts are bounded,
//! so a malformed or hostile ISO is an error, not a hang.
//!
//! Windows install ISOs keep their files in UDF, not here: their ISO 9660
//! tree holds only a "this disc contains a UDF file system" readme.

use std::io::{self, Read, Seek, SeekFrom};

const SECTOR: u64 = 2048;
/// No real directory is this large; refuse rather than allocate it.
const MAX_DIR_BYTES: u64 = 16 << 20;
const MAX_DEPTH: usize = 64;
const MAX_ENTRIES: usize = 1_000_000;

/// One file or directory.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Entry {
    /// Path from the root, `/`-separated, no leading slash: `EFI/BOOT/BOOTX64.EFI`.
    pub path: String,
    /// A directory, not a file.
    pub is_dir: bool,
    /// Total size in bytes (all extents).
    pub size: u64,
    /// Where the bytes are: (byte offset in the image, length). Files over
    /// 4 GiB are stored as several extents.
    extents: Vec<(u64, u64)>,
}

/// An extent with this offset is a hole: it reads as zeros (UDF sparse
/// extents, "allocated but not recorded").
pub(crate) const HOLE: u64 = u64::MAX;

impl Entry {
    pub(crate) fn new(path: String, is_dir: bool, size: u64, extents: Vec<(u64, u64)>) -> Self {
        Entry {
            path,
            is_dir,
            size,
            extents,
        }
    }
}

/// Stream an entry's bytes from the image `r` to `sink`, in pieces of at
/// most `buf.len()`. Shared by the ISO 9660 and UDF readers.
pub(crate) fn read_extents<R: Read + Seek>(
    r: &mut R,
    e: &Entry,
    buf: &mut [u8],
    mut sink: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    for &(offset, len) in &e.extents {
        let hole = offset == HOLE;
        if !hole {
            r.seek(SeekFrom::Start(offset))?;
        }
        let mut left = len;
        while left > 0 {
            let n = (buf.len() as u64).min(left) as usize;
            if hole {
                buf[..n].fill(0);
            } else {
                r.read_exact(&mut buf[..n])?;
            }
            sink(&buf[..n])?;
            left -= n as u64;
        }
    }
    Ok(())
}

/// Like [`read_extents`], for the bytes `[start, start + len)` of the entry.
pub(crate) fn read_extents_range<R: Read + Seek>(
    r: &mut R,
    e: &Entry,
    start: u64,
    len: u64,
    buf: &mut [u8],
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    if start.checked_add(len).is_none_or(|end| end > e.size) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "read past the end of a file in the image",
        ));
    }
    let (mut skip, mut left) = (start, len);
    for &(offset, ext_len) in &e.extents {
        if left == 0 {
            break;
        }
        if skip >= ext_len {
            skip -= ext_len;
            continue;
        }
        let hole = offset == HOLE;
        if !hole {
            r.seek(SeekFrom::Start(offset + skip))?;
        }
        let mut here = (ext_len - skip).min(left);
        skip = 0;
        while here > 0 {
            let n = (buf.len() as u64).min(here) as usize;
            if hole {
                buf[..n].fill(0);
            } else {
                r.read_exact(&mut buf[..n])?;
            }
            sink(&buf[..n])?;
            here -= n as u64;
            left -= n as u64;
        }
    }
    Ok(())
}

#[cfg(test)]
impl Entry {
    pub(crate) fn for_tests(path: &str, size: u64) -> Self {
        Entry {
            path: path.into(),
            is_dir: false,
            size,
            extents: vec![(0, size)],
        }
    }
}

/// Which names the tree was read with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Names {
    /// Rock Ridge `NM` names: the original POSIX names.
    RockRidge,
    /// Joliet: UCS-2 names up to 64 characters.
    Joliet,
    /// Plain ISO 9660: upper case, 8.3-ish, version suffixes removed.
    Iso9660,
}

/// An opened ISO image.
pub struct Iso<R> {
    r: R,
    root: (u64, u64),
    names: Names,
    /// The volume identifier from the primary descriptor, trimmed.
    pub volume_id: String,
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("not a readable ISO 9660 image: {}", msg.into()),
    )
}

fn le32(b: &[u8]) -> u64 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64
}

impl<R: Read + Seek> Iso<R> {
    /// Read the volume descriptors and pick the best names available.
    pub fn open(mut r: R) -> io::Result<Self> {
        let mut primary = None;
        let mut joliet = None;
        let mut volume_id = String::new();
        let mut sector = [0u8; SECTOR as usize];
        for lba in 16..16 + 64 {
            r.seek(SeekFrom::Start(lba * SECTOR))?;
            r.read_exact(&mut sector)?;
            if &sector[1..6] != b"CD001" {
                return Err(bad("missing CD001 volume descriptor"));
            }
            let root = (le32(&sector[156 + 2..]) * SECTOR, le32(&sector[156 + 10..]));
            match sector[0] {
                1 if primary.is_none() => {
                    primary = Some(root);
                    volume_id = pad_trim(&String::from_utf8_lossy(&sector[40..72]));
                }
                // A supplementary descriptor whose escape sequence names a
                // UCS-2 level is Joliet.
                2 if [b"%/@", b"%/C", b"%/E"]
                    .iter()
                    .any(|e| sector[88..120].windows(3).any(|w| w == *e)) =>
                {
                    // Its volume id is UCS-2 too: the real name, where the
                    // primary's is limited to A-Z, 0-9 and _ (and padded).
                    joliet = Some((root, pad_trim(&joliet_name(&sector[40..72]))));
                }
                255 => break,
                _ => {}
            }
        }
        let primary = primary.ok_or_else(|| bad("no primary volume descriptor"))?;
        let mut iso = Iso {
            r,
            root: primary,
            names: Names::Iso9660,
            volume_id,
        };
        if iso.has_rock_ridge()? {
            iso.names = Names::RockRidge;
        } else if let Some((j, name)) = joliet {
            iso.root = j;
            iso.names = Names::Joliet;
            if !name.is_empty() {
                iso.volume_id = name;
            }
        }
        Ok(iso)
    }

    /// Which names [`Iso::walk`] reports.
    pub fn names(&self) -> Names {
        self.names
    }

    /// Rock Ridge announces itself with an `SP` entry in the root's "."
    /// record (SUSP), and names files with `NM`.
    fn has_rock_ridge(&mut self) -> io::Result<bool> {
        let data = self.read_extent(self.root.0, self.root.1.min(SECTOR))?;
        let len = data[0] as usize;
        if len < 34 || len > data.len() {
            return Ok(false);
        }
        let name_len = data[32] as usize;
        let su_start = 33 + name_len + (1 - name_len % 2);
        if !(su_start + 4 <= len && &data[su_start..su_start + 2] == b"SP") {
            return Ok(false);
        }
        // Rock Ridge is announced, but it only beats Joliet if it actually
        // carries names: some writers (macOS's `hdiutil`) record permissions
        // and times with no `NM`, which leaves the plain ISO 9660 names —
        // upper case, accents dropped — where Joliet has the real ones.
        let mut pos = 0;
        while pos < data.len() {
            let rec_len = data[pos] as usize;
            if rec_len == 0 {
                break;
            }
            if rec_len < 34 || pos + rec_len > data.len() {
                return Ok(false);
            }
            let rec = &data[pos..pos + rec_len];
            pos += rec_len;
            let nl = rec[32] as usize;
            if 33 + nl > rec.len() || rec[33..33 + nl] == [0] || rec[33..33 + nl] == [1] {
                continue;
            }
            let inline = rec[(33 + nl + (1 - nl % 2)).min(rec.len())..].to_vec();
            let su = self.system_use(&inline)?;
            return Ok(rr_name(&su).is_some());
        }
        Ok(true) // an empty root: names do not matter
    }

    fn read_extent(&mut self, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        if len > MAX_DIR_BYTES {
            return Err(bad(format!("directory of {len} bytes")));
        }
        let mut buf = vec![0u8; len as usize];
        self.r.seek(SeekFrom::Start(offset))?;
        self.r.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Every entry in the image, directories before their contents.
    pub fn walk(&mut self) -> io::Result<Vec<Entry>> {
        let mut out = Vec::new();
        let mut stack = vec![(String::new(), self.root, 0usize)];
        while let Some((prefix, (off, len), depth)) = stack.pop() {
            if depth > MAX_DEPTH {
                return Err(bad("directories nested too deep"));
            }
            for e in self.read_dir(&prefix, off, len)? {
                if out.len() >= MAX_ENTRIES {
                    return Err(bad("too many entries"));
                }
                if e.is_dir {
                    let (o, l) = e.extents[0];
                    stack.push((e.path.clone(), (o, l), depth + 1));
                }
                out.push(e);
            }
        }
        Ok(out)
    }

    fn read_dir(&mut self, prefix: &str, offset: u64, len: u64) -> io::Result<Vec<Entry>> {
        let data = self.read_extent(offset, len)?;
        let mut out: Vec<Entry> = Vec::new();
        let mut pos = 0usize;
        // A file continued in another extent (multi-extent, for > 4 GiB).
        let mut continuing = false;
        while pos < data.len() {
            let rec_len = data[pos] as usize;
            if rec_len == 0 {
                // Records never span sectors: the rest of this one is padding.
                pos = (pos / SECTOR as usize + 1) * SECTOR as usize;
                continue;
            }
            if rec_len < 34 || pos + rec_len > data.len() {
                return Err(bad("directory record out of bounds"));
            }
            let rec = &data[pos..pos + rec_len];
            pos += rec_len;

            let flags = rec[25];
            let name_len = rec[32] as usize;
            if 33 + name_len > rec.len() {
                return Err(bad("name out of bounds"));
            }
            let raw = &rec[33..33 + name_len];
            if raw == [0] || raw == [1] {
                continue; // "." and ".."
            }
            let is_dir = flags & 0x02 != 0;
            let extent = (le32(&rec[2..]) * SECTOR, le32(&rec[10..]));

            if continuing {
                if let Some(last) = out.last_mut() {
                    last.extents.push(extent);
                    last.size += extent.1;
                }
                continuing = flags & 0x80 != 0;
                continue;
            }
            let inline = &rec[(33 + name_len + (1 - name_len % 2)).min(rec.len())..];
            let su = if self.names == Names::RockRidge {
                self.system_use(inline)?
            } else {
                Vec::new()
            };
            let su = su.as_slice();
            if self.names == Names::RockRidge && susp(su, b"SL").is_some() {
                // A symlink: nothing a FAT drive can hold.
                continuing = flags & 0x80 != 0;
                continue;
            }
            let name = match self.names {
                Names::RockRidge => rr_name(su).unwrap_or_else(|| plain_name(raw)),
                Names::Joliet => joliet_name(raw),
                Names::Iso9660 => plain_name(raw),
            };
            if name.is_empty() || name.contains('/') || name == "." || name == ".." {
                return Err(bad(format!("unusable file name {name:?}")));
            }
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            out.push(Entry {
                path,
                is_dir,
                size: extent.1,
                extents: vec![extent],
            });
            continuing = flags & 0x80 != 0;
        }
        Ok(out)
    }

    /// A record's System Use entries: the ones inline, then those in any
    /// continuation areas (`CE`) they point to, where Rock Ridge puts what
    /// does not fit — often the name.
    fn system_use(&mut self, inline: &[u8]) -> io::Result<Vec<u8>> {
        let mut all = inline.to_vec();
        let mut next = susp(inline, b"CE").map(<[u8]>::to_vec);
        for _ in 0..8 {
            let Some(ce) = next.take() else { break };
            if ce.len() < 24 {
                return Err(bad("short CE entry"));
            }
            let (block, offset, len) = (le32(&ce[0..]), le32(&ce[8..]), le32(&ce[16..]));
            if offset + len > SECTOR {
                return Err(bad("CE area out of bounds"));
            }
            let area = self.read_extent(block * SECTOR + offset, len)?;
            next = susp(&area, b"CE").map(<[u8]>::to_vec);
            all.extend_from_slice(&area);
        }
        Ok(all)
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

/// The payload of the first System Use entry with this signature.
fn susp<'a>(mut su: &'a [u8], sig: &[u8; 2]) -> Option<&'a [u8]> {
    while su.len() >= 4 {
        let len = su[2] as usize;
        if len < 4 || len > su.len() {
            return None;
        }
        if &su[..2] == sig {
            return Some(&su[4..len]);
        }
        su = &su[len..];
    }
    None
}

/// The Rock Ridge name: every `NM` entry's text, joined, since a long name
/// may be split across several (each but the last flagged CONTINUE).
fn rr_name(mut su: &[u8]) -> Option<String> {
    let mut name = Vec::new();
    let mut found = false;
    while su.len() >= 4 {
        let len = su[2] as usize;
        if len < 4 || len > su.len() {
            break;
        }
        if &su[..2] == b"NM" && len >= 5 {
            let flags = su[4];
            if flags & 0b110 != 0 {
                return None; // "." or ".." by flag: not a name
            }
            name.extend_from_slice(&su[5..len]);
            found = true;
            if flags & 1 == 0 {
                break;
            }
        }
        su = &su[len..];
    }
    (found && !name.is_empty()).then(|| String::from_utf8_lossy(&name).into_owned())
}

/// Identifiers are padded with spaces by the standard and with NULs by
/// some writers (macOS's `hdiutil`); strip both.
fn pad_trim(s: &str) -> String {
    s.trim_matches(|c: char| c == ' ' || c == '\0').to_string()
}

/// `README.TXT;1` → `README.TXT`; `DIR.` → `DIR`.
fn plain_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    let s = s.split(';').next().unwrap_or("");
    s.strip_suffix('.').unwrap_or(s).to_string()
}

fn joliet_name(raw: &[u8]) -> String {
    let units: Vec<u16> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    let s = String::from_utf16_lossy(&units);
    s.split(';').next().unwrap_or("").to_string()
}

/// A tiny ISO 9660 writer, for tests: plain names, Joliet or Rock Ridge.
#[cfg(test)]
pub(crate) mod build {
    use super::SECTOR;

    pub struct File<'a> {
        pub path: &'a str,
        pub data: Vec<u8>,
    }

    /// An ISO holding `files` (directories are implied by paths).
    pub fn iso(volume_id: &str, files: &[File], rock_ridge: bool) -> Vec<u8> {
        use std::collections::BTreeMap;
        // Directory tree: dir path -> (subdirs, files)
        let mut dirs: BTreeMap<String, (Vec<String>, Vec<usize>)> = BTreeMap::new();
        dirs.insert(String::new(), (vec![], vec![]));
        for (i, f) in files.iter().enumerate() {
            let parts: Vec<&str> = f.path.split('/').collect();
            let mut cur = String::new();
            for p in &parts[..parts.len() - 1] {
                let next = if cur.is_empty() {
                    p.to_string()
                } else {
                    format!("{cur}/{p}")
                };
                if !dirs.contains_key(&next) {
                    dirs.insert(next.clone(), (vec![], vec![]));
                    dirs.get_mut(&cur).unwrap().0.push(next.clone());
                }
                cur = next;
            }
            dirs.get_mut(&cur).unwrap().1.push(i);
        }
        // Layout: sectors 16 PVD, 17 terminator, then one sector per dir, then files.
        let dir_list: Vec<String> = dirs.keys().cloned().collect();
        let dir_lba = |d: &str| 18 + dir_list.iter().position(|x| x == d).unwrap() as u32;
        let mut next = 18 + dir_list.len() as u32;
        let mut file_lba = vec![0u32; files.len()];
        for (i, f) in files.iter().enumerate() {
            file_lba[i] = next;
            next += (f.data.len() as u64).div_ceil(SECTOR).max(1) as u32;
        }
        let mut img = vec![0u8; next as usize * SECTOR as usize];

        let record = |name: &[u8], lba: u32, len: u32, dir: bool, rr: Option<&[u8]>| {
            let mut su = Vec::new();
            if let Some(nm) = rr {
                su.extend_from_slice(b"NM");
                su.push((5 + nm.len()) as u8);
                su.push(1);
                su.push(0);
                su.extend_from_slice(nm);
            }
            let pad = 1 - name.len() % 2;
            let len_total = 33 + name.len() + pad + su.len();
            let mut r = vec![0u8; len_total + len_total % 2];
            r[0] = r.len() as u8;
            r[2..6].copy_from_slice(&lba.to_le_bytes());
            r[6..10].copy_from_slice(&lba.to_be_bytes());
            r[10..14].copy_from_slice(&len.to_le_bytes());
            r[14..18].copy_from_slice(&len.to_be_bytes());
            r[25] = if dir { 2 } else { 0 };
            r[32] = name.len() as u8;
            r[33..33 + name.len()].copy_from_slice(name);
            let s = 33 + name.len() + pad;
            r[s..s + su.len()].copy_from_slice(&su);
            r
        };
        for d in &dir_list {
            let (subs, fs) = &dirs[d];
            let mut body = Vec::new();
            // "." carries SUSP "SP" when Rock Ridge is on.
            let mut dot = record(&[0], dir_lba(d), SECTOR as u32, true, None);
            if rock_ridge {
                dot = {
                    let mut r = vec![0u8; 34 + 8];
                    r[..34].copy_from_slice(&dot[..34]);
                    r[0] = r.len() as u8;
                    r[34..41].copy_from_slice(&[b'S', b'P', 7, 1, 0xBE, 0xEF, 0]);
                    r
                };
            }
            body.extend(dot);
            body.extend(record(&[1], dir_lba(d), SECTOR as u32, true, None));
            for s in subs {
                let leaf = s.rsplit('/').next().unwrap();
                let iso_name = leaf.to_uppercase();
                body.extend(record(
                    iso_name.as_bytes(),
                    dir_lba(s),
                    SECTOR as u32,
                    true,
                    rock_ridge.then_some(leaf.as_bytes()),
                ));
            }
            for &i in fs {
                let leaf = files[i].path.rsplit('/').next().unwrap();
                let iso_name = format!("{};1", leaf.to_uppercase());
                body.extend(record(
                    iso_name.as_bytes(),
                    file_lba[i],
                    files[i].data.len() as u32,
                    false,
                    rock_ridge.then_some(leaf.as_bytes()),
                ));
            }
            assert!(
                body.len() <= SECTOR as usize,
                "test builder: directory too big"
            );
            let at = dir_lba(d) as usize * SECTOR as usize;
            img[at..at + body.len()].copy_from_slice(&body);
        }
        for (i, f) in files.iter().enumerate() {
            let at = file_lba[i] as usize * SECTOR as usize;
            img[at..at + f.data.len()].copy_from_slice(&f.data);
        }
        let pvd = 16 * SECTOR as usize;
        img[pvd] = 1;
        img[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
        img[pvd + 6] = 1;
        let vid = format!("{volume_id:<32}");
        img[pvd + 40..pvd + 72].copy_from_slice(&vid.as_bytes()[..32]);
        let root = record(&[0], dir_lba(""), SECTOR as u32, true, None);
        img[pvd + 156..pvd + 156 + 34].copy_from_slice(&root[..34]);
        let term = 17 * SECTOR as usize;
        img[term] = 255;
        img[term + 1..term + 6].copy_from_slice(b"CD001");
        img
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::build::{iso, File};
    use super::*;

    fn files() -> Vec<File<'static>> {
        vec![
            File {
                path: "EFI/BOOT/BOOTX64.EFI",
                data: vec![0xAB; 5000],
            },
            File {
                path: "readme.txt",
                data: b"hello".to_vec(),
            },
            File {
                path: "casper/vmlinuz",
                data: (0..70_000u32).map(|i| i as u8).collect(),
            },
        ]
    }

    fn read_all(iso: &mut Iso<Cursor<Vec<u8>>>, e: &Entry) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 4096];
        iso.read_file(e, &mut buf, |b| {
            out.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        out
    }

    #[test]
    fn reads_rock_ridge_names_and_contents() {
        let fs = files();
        let mut iso = Iso::open(Cursor::new(iso("UBUNTU_24", &fs, true))).unwrap();
        assert_eq!(iso.names(), Names::RockRidge);
        assert_eq!(iso.volume_id, "UBUNTU_24");
        let all = iso.walk().unwrap();
        for f in &fs {
            let e = all
                .iter()
                .find(|e| e.path == f.path)
                .unwrap_or_else(|| panic!("{} missing: {all:?}", f.path));
            assert_eq!(e.size, f.data.len() as u64);
            assert_eq!(read_all(&mut iso, e), f.data);
        }
        assert!(all.iter().any(|e| e.path == "EFI/BOOT" && e.is_dir));
    }

    #[test]
    fn falls_back_to_plain_names() {
        let fs = files();
        let mut iso = Iso::open(Cursor::new(iso("X", &fs, false))).unwrap();
        assert_eq!(iso.names(), Names::Iso9660);
        let all = iso.walk().unwrap();
        assert!(all.iter().any(|e| e.path == "README.TXT"), "{all:?}");
        assert!(all.iter().any(|e| e.path == "EFI/BOOT/BOOTX64.EFI"));
    }

    #[test]
    fn refuses_what_is_not_an_iso() {
        assert!(Iso::open(Cursor::new(vec![0u8; 64 * 2048])).is_err());
        assert!(Iso::open(Cursor::new(vec![0u8; 100])).is_err());
    }

    #[test]
    fn reads_ranges_across_extents_and_holes() {
        // Bytes 0..10 at offset 100, a 5-byte hole, then 0..10 at offset 200.
        let mut img = vec![0u8; 300];
        img[100..110].copy_from_slice(b"ABCDEFGHIJ");
        img[200..210].copy_from_slice(b"KLMNOPQRST");
        let e = Entry::new("f".into(), false, 25, vec![(100, 10), (HOLE, 5), (200, 10)]);
        let mut r = Cursor::new(img);
        let mut got = Vec::new();
        read_extents_range(&mut r, &e, 7, 11, &mut [0u8; 4], &mut |b| {
            got.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        assert_eq!(got, b"HIJ\0\0\0\0\0KLM");
        assert!(read_extents_range(&mut r, &e, 20, 6, &mut [0u8; 4], &mut |_| Ok(())).is_err());
    }

    #[test]
    fn decodes_joliet_names() {
        let raw: Vec<u8> = "Résumé.txt;1"
            .encode_utf16()
            .flat_map(|u| u.to_be_bytes())
            .collect();
        assert_eq!(joliet_name(&raw), "Résumé.txt");
        assert_eq!(plain_name(b"DIR."), "DIR");
        // A name split across two NM entries, the first flagged CONTINUE,
        // with another entry between them.
        let su = [
            &b"NM\x0a\x01\x01Long "[..],
            &b"PX\x04\x01"[..],
            &b"NM\x09\x01\x00Name"[..],
        ]
        .concat();
        assert_eq!(rr_name(&su).as_deref(), Some("Long Name"));
        assert_eq!(rr_name(b"PX\x04\x01"), None);
        assert_eq!(pad_trim("DEMO_LIVE\0\0\0  "), "DEMO_LIVE");
    }

    /// Reads a real ISO named in `FLASHER_TEST_ISO` (read-only), to check
    /// this reader against images it did not build itself.
    #[test]
    #[ignore = "needs FLASHER_TEST_ISO"]
    fn reads_a_real_iso() {
        let Ok(path) = std::env::var("FLASHER_TEST_ISO") else {
            return;
        };
        let mut iso = Iso::open(std::fs::File::open(&path).unwrap()).unwrap();
        let all = iso.walk().unwrap();
        let total: u64 = all.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
        println!(
            "{path}: {:?} names, volume {:?}, {} entries, {total} bytes",
            iso.names(),
            iso.volume_id,
            all.len()
        );
        for e in all
            .iter()
            .filter(|e| e.path.to_ascii_uppercase().starts_with("EFI/BOOT/"))
        {
            println!("  {} {}", e.path, e.size);
        }
        let biggest = all
            .iter()
            .filter(|e| !e.is_dir)
            .max_by_key(|e| e.size)
            .unwrap();
        println!("  largest: {} ({} bytes)", biggest.path, biggest.size);
    }
}
