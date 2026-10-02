//! Writing a GUID Partition Table with one partition (UEFI spec, chapter 5):
//! protective MBR, primary header and entries at the start, backup entries
//! and header at the end.

use std::io::{self, SeekFrom};

use crate::RawDevice;

const ENTRIES: u64 = 128;
const ENTRY_SIZE: u64 = 128;
/// Partitions start and end on 1 MiB boundaries, as every OS's tools do:
/// aligned to any flash erase block and any sector size.
const ALIGN: u64 = 1 << 20;

/// "Microsoft basic data": mounted by Windows, macOS and Linux, and booted
/// from by UEFI firmware on removable media.
pub(crate) const BASIC_DATA: [u8; 16] = guid(
    0xEBD0A0A2,
    0xB9E5,
    0x4433,
    [0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7],
);

/// A GUID in the mixed-endian byte order GPT stores.
const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let a = a.to_le_bytes();
    let b = b.to_le_bytes();
    let c = c.to_le_bytes();
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6],
        d[7],
    ]
}

/// A random (version 4) GUID, from the clock and process, hashed. GPT needs
/// them unique per disk, not secret.
fn random_guid(salt: u8) -> [u8; 16] {
    use sha2::{Digest, Sha256};
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut h = Sha256::new();
    h.update(t.as_nanos().to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.update([salt]);
    let mut g = [0u8; 16];
    g.copy_from_slice(&h.finalize()[..16]);
    g[7] = (g[7] & 0x0F) | 0x40; // version 4 (stored little-endian in field 3)
    g[8] = (g[8] & 0x3F) | 0x80; // RFC 4122 variant
    g
}

/// Where the one partition goes: byte offset and length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    pub start: u64,
    pub len: u64,
}

/// The partition a disk of `size` bytes with `sector`-byte sectors gets.
pub(crate) fn layout(size: u64, sector: u64) -> Option<Layout> {
    let sectors = size / sector;
    let table = ENTRIES * ENTRY_SIZE / sector;
    let last_usable = sectors.checked_sub(2 + table)?; // backup entries + header
    let start = ALIGN;
    let end = ((last_usable + 1) * sector) / ALIGN * ALIGN; // exclusive, aligned down
    (end > start).then(|| Layout {
        start,
        len: end - start,
    })
}

/// Write the table. `name` is the partition's GPT name (up to 36 characters).
pub(crate) fn write(dev: &mut dyn RawDevice, name: &str) -> io::Result<Layout> {
    let sector = dev.sector_size().max(512) as u64;
    let size = dev.size();
    let l = layout(size, sector).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "disk too small for a partition table",
        )
    })?;
    let sectors = size / sector;
    let table_sectors = ENTRIES * ENTRY_SIZE / sector;
    let first_usable = 2 + table_sectors;
    let last_usable = sectors - 2 - table_sectors;
    let backup_entries = sectors - 1 - table_sectors;

    let mut entries = vec![0u8; (ENTRIES * ENTRY_SIZE) as usize];
    entries[0..16].copy_from_slice(&BASIC_DATA);
    entries[16..32].copy_from_slice(&random_guid(1));
    entries[32..40].copy_from_slice(&(l.start / sector).to_le_bytes());
    entries[40..48].copy_from_slice(&((l.start + l.len) / sector - 1).to_le_bytes());
    for (i, u) in name.encode_utf16().take(36).enumerate() {
        entries[56 + 2 * i..58 + 2 * i].copy_from_slice(&u.to_le_bytes());
    }
    let entries_crc = crc32fast::hash(&entries);
    let disk_guid = random_guid(2);

    let header = |me: u64, other: u64, entries_lba: u64| {
        let mut h = vec![0u8; sector as usize];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&me.to_le_bytes());
        h[32..40].copy_from_slice(&other.to_le_bytes());
        h[40..48].copy_from_slice(&first_usable.to_le_bytes());
        h[48..56].copy_from_slice(&last_usable.to_le_bytes());
        h[56..72].copy_from_slice(&disk_guid);
        h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        h[80..84].copy_from_slice(&(ENTRIES as u32).to_le_bytes());
        h[84..88].copy_from_slice(&(ENTRY_SIZE as u32).to_le_bytes());
        h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let crc = crc32fast::hash(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    };

    // Protective MBR: one partition of type 0xEE covering the disk, so
    // MBR-only tools see it as in use rather than empty.
    let mut mbr = vec![0u8; sector as usize];
    let p = 446;
    mbr[p + 1..p + 4].copy_from_slice(&[0x00, 0x02, 0x00]);
    mbr[p + 4] = 0xEE;
    mbr[p + 5..p + 8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    mbr[p + 8..p + 12].copy_from_slice(&1u32.to_le_bytes());
    mbr[p + 12..p + 16].copy_from_slice(&((sectors - 1).min(u32::MAX as u64) as u32).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    // Clear the first and last MiB first: old partition tables, an ISO's
    // volume descriptors at 32 KiB, a previous backup GPT.
    let zero = vec![0u8; ALIGN as usize];
    put(dev, 0, &zero)?;
    put(
        dev,
        size / sector * sector - ALIGN.min(size / sector * sector),
        &zero[..ALIGN.min(size / sector * sector) as usize],
    )?;

    put(dev, 0, &mbr)?;
    put(dev, sector, &header(1, sectors - 1, 2))?;
    put(dev, 2 * sector, &entries)?;
    put(dev, backup_entries * sector, &entries)?;
    put(
        dev,
        (sectors - 1) * sector,
        &header(sectors - 1, 1, backup_entries),
    )?;
    Ok(l)
}

fn put(dev: &mut dyn RawDevice, at: u64, data: &[u8]) -> io::Result<()> {
    dev.seek(SeekFrom::Start(at))?;
    dev.write_all(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_aligned_and_inside_the_usable_area() {
        for (size, sector) in [
            (64u64 << 20, 512u64),
            (8 << 30, 512),
            (16 << 30, 4096),
            ((64 << 20) + 12345 * 512, 512),
        ] {
            let l = layout(size, sector).unwrap();
            assert_eq!(l.start, 1 << 20);
            assert_eq!(l.start % ALIGN, 0);
            assert_eq!((l.start + l.len) % ALIGN, 0);
            let backup = size / sector * sector - (1 + ENTRIES * ENTRY_SIZE / sector) * sector;
            assert!(
                l.start + l.len <= backup,
                "partition overlaps the backup table: {size} {sector}"
            );
        }
        assert_eq!(layout(1 << 20, 512), None, "too small");
    }

    #[test]
    fn known_guid_encoding() {
        // EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, as it appears on disk.
        assert_eq!(&BASIC_DATA[..4], &[0xA2, 0xA0, 0xD0, 0xEB]);
        assert_eq!(&BASIC_DATA[4..8], &[0xE5, 0xB9, 0x33, 0x44]);
        assert_eq!(
            &BASIC_DATA[8..],
            &[0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7]
        );
    }

    #[test]
    fn writes_valid_headers_and_crcs() {
        let path = std::env::temp_dir().join(format!("libflasher_gpt_{}", std::process::id()));
        std::fs::write(&path, vec![0xFFu8; 64 << 20]).unwrap();
        let mut dev = crate::mock::FileDevice::open(&path).unwrap();
        let l = write(&mut dev, "TEST").unwrap();
        drop(dev);
        let d = std::fs::read(&path).unwrap();
        let check = |at: usize| {
            let h = &d[at..at + 512];
            assert_eq!(&h[..8], b"EFI PART");
            let mut z = h[..92].to_vec();
            z[16..20].fill(0);
            assert_eq!(crc32fast::hash(&z).to_le_bytes(), h[16..20], "header CRC");
            let lba = u64::from_le_bytes(h[72..80].try_into().unwrap()) as usize;
            let entries = &d[lba * 512..lba * 512 + 16384];
            assert_eq!(
                crc32fast::hash(entries).to_le_bytes(),
                h[88..92],
                "entries CRC"
            );
            assert_eq!(&entries[..16], &BASIC_DATA);
            let first = u64::from_le_bytes(entries[32..40].try_into().unwrap());
            assert_eq!(first * 512, l.start);
        };
        check(512);
        check(d.len() - 512);
        assert_eq!(d[446 + 4], 0xEE);
        assert_eq!(&d[510..512], &[0x55, 0xAA]);
        assert!(
            d[512 * 34..1 << 20].iter().all(|&b| b == 0),
            "old data cleared before the partition"
        );
        std::fs::remove_file(path).ok();
    }
}
