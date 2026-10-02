//! Extract mode: for ISOs that are not disk images (not "hybrid"), make the
//! drive bootable by copying the ISO's files onto a fresh FAT32 partition.
//!
//! UEFI firmware boots removable media by running `\EFI\BOOT\BOOT<arch>.EFI`
//! from a FAT partition, so an ISO that carries that file boots this way
//! with no bootloader installed by us. That covers most modern installers.
//! Files are read through UDF when the image has it (Windows ISOs) and ISO
//! 9660 otherwise. Not covered yet, and refused with a reason rather than
//! half-done: ISOs with no UEFI bootloader (BIOS-only) and files over FAT32's
//! 4 GiB limit (current Windows ISOs' `install.wim`, until it can be split).

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::blockio::BlockIo;
use crate::iso9660::{Entry, Iso};
use crate::udf::Udf;
use crate::{gpt, Compression, Error, FlashOptions, ImageInfo, Progress, RawDevice, Result};

/// FAT32 cannot hold a file this large or larger.
const FAT32_MAX_FILE: u64 = 1 << 32;
/// fatfs needs at least ~65 k clusters for FAT32; this leaves room.
const MIN_PARTITION: u64 = 64 << 20;
const COPY_CHUNK: usize = 1 << 20;

/// What extracting an ISO would do, worked out without touching a drive.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Plan {
    /// The FAT volume name, from the ISO's volume identifier.
    pub label: String,
    /// Files to copy.
    pub files: usize,
    /// Bytes in those files.
    pub bytes: u64,
    /// Space the copy needs on the drive, with filesystem overhead.
    pub needs: u64,
}

/// Check an ISO can be extracted, and say what that would take.
pub fn plan(image: &ImageInfo) -> Result<Plan> {
    let (entries, label) = read_tree(image)?;
    plan_for(&entries, label)
}

/// The image's files, through whichever filesystem holds them: UDF when the
/// image has one (Windows ISOs, whose ISO 9660 tree is only a readme), ISO
/// 9660 otherwise.
enum Source {
    Iso(Iso<BufReader<File>>),
    Udf(Udf<BufReader<File>>),
}

impl Source {
    fn open(image: &ImageInfo) -> Result<Self> {
        if image.compression != Compression::None {
            return Err(Error::Unsupported(format!(
                "extracting a {}-compressed ISO; decompress it first",
                image.compression.name()
            )));
        }
        let mut f = BufReader::new(File::open(&image.path)?);
        if crate::udf::is_udf(&mut f)? {
            return Udf::open(f).map(Source::Udf).map_err(|e| match e.kind() {
                io::ErrorKind::Unsupported => Error::Unsupported(e.to_string()),
                _ => Error::Io(e),
            });
        }
        Ok(Source::Iso(Iso::open(f)?))
    }

    fn volume_id(&self) -> String {
        match self {
            Source::Iso(i) => i.volume_id.clone(),
            Source::Udf(u) => u.volume_id.clone(),
        }
    }

    fn walk(&mut self) -> io::Result<Vec<Entry>> {
        match self {
            Source::Iso(i) => i.walk(),
            Source::Udf(u) => u.walk(),
        }
    }

    fn read_file(
        &mut self,
        e: &Entry,
        buf: &mut [u8],
        sink: impl FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        match self {
            Source::Iso(i) => i.read_file(e, buf, sink),
            Source::Udf(u) => u.read_file(e, buf, sink),
        }
    }
}

fn read_tree(image: &ImageInfo) -> Result<(Vec<Entry>, String)> {
    let mut src = Source::open(image)?;
    let label = src.volume_id();
    Ok((src.walk()?, label))
}

fn plan_for(entries: &[Entry], volume_id: String) -> Result<Plan> {
    let uefi = entries.iter().any(|e| {
        let p = e.path.to_ascii_lowercase();
        !e.is_dir && p.starts_with("efi/boot/boot") && p.ends_with(".efi")
    });
    if !uefi {
        return Err(Error::Unsupported(
            "this ISO has no UEFI bootloader (EFI/BOOT/BOOTX64.EFI), so it can only boot in legacy BIOS mode, \
             which is not supported yet"
                .into(),
        ));
    }
    if let Some(big) = entries
        .iter()
        .find(|e| !e.is_dir && e.size >= FAT32_MAX_FILE)
    {
        return Err(Error::Unsupported(format!(
            "{} is {}, over FAT32's 4 GB limit for one file; splitting it is not supported yet",
            big.path,
            crate::platform::human_size(big.size)
        )));
    }
    // FAT compares names without case: two names differing only in case
    // would silently become one file.
    let mut seen = HashSet::new();
    for e in entries {
        if !seen.insert(fat_path(&e.path).to_lowercase()) {
            return Err(Error::Unsupported(format!(
                "{} differs from another file only in letter case, which FAT cannot hold",
                e.path
            )));
        }
    }
    let files: Vec<&Entry> = entries.iter().filter(|e| !e.is_dir).collect();
    let bytes: u64 = files.iter().map(|e| e.size).sum();
    // Clusters are at most 32 KiB on the sizes we format; round every file
    // up to one, add the FATs and directories generously.
    let needs = files
        .iter()
        .map(|e| e.size.div_ceil(32 << 10) * (32 << 10))
        .sum::<u64>()
        + bytes / 64
        + (16 << 20);
    Ok(Plan {
        label: fat_label(&volume_id),
        files: files.len(),
        bytes,
        needs,
    })
}

/// Characters FAT long names cannot hold, replaced.
fn fat_path(path: &str) -> String {
    path.chars()
        .map(|c| {
            if matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c < ' ' {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// A FAT volume label from an ISO volume id: 11 characters, upper case.
fn fat_label(volume_id: &str) -> String {
    let l: String = volume_id
        .chars()
        .map(|c| c.to_ascii_uppercase())
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ' ') {
                c
            } else {
                '_'
            }
        })
        .take(11)
        .collect();
    let l = l.trim().to_string();
    if l.is_empty() {
        "BOOT".into()
    } else {
        l
    }
}

/// Make the drive a FAT32 copy of the ISO's files. Returns the bytes copied.
pub fn extract(
    image: &ImageInfo,
    device: &mut dyn RawDevice,
    options: &FlashOptions,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<u64> {
    let (entries, volume_id) = read_tree(image)?;
    let plan = plan_for(&entries, volume_id)?;
    let sector = device.sector_size().max(512) as u64;
    let layout = gpt::layout(device.size(), sector).filter(|l| l.len >= MIN_PARTITION);
    let Some(layout) = layout.filter(|l| l.len >= plan.needs) else {
        return Err(Error::ImageTooLarge {
            image: plan.needs.max(MIN_PARTITION),
            device: device.size(),
        });
    };

    progress(Progress::Formatting);
    gpt::write(device, &plan.label).map_err(|e| Error::Io(e).at_device(0))?;
    let mut iso = Source::open(image)?;
    let mut io = BlockIo::new(
        device,
        layout.start,
        layout.len,
        options.stall_timeout,
        options.sync_every,
    );
    let device_err = |e: io::Error, at: u64| match e.kind() {
        io::ErrorKind::TimedOut => Error::Stalled {
            seconds: options.stall_timeout.as_secs_f32(),
            offset: at,
        },
        _ => Error::Io(e).at_device(at),
    };
    let mut label = [b' '; 11];
    label[..plan.label.len()].copy_from_slice(plan.label.as_bytes());
    fatfs::format_volume(
        &mut io,
        fatfs::FormatVolumeOptions::new()
            .fat_type(fatfs::FatType::Fat32)
            .bytes_per_sector(sector as u16)
            .volume_label(label),
    )
    .map_err(|e| device_err(e, 0))?;

    let mut done = 0u64;
    let mut buf = vec![0u8; COPY_CHUNK];
    {
        // fatfs reads the boot sector from wherever the stream is.
        io.seek(std::io::SeekFrom::Start(0))
            .map_err(|e| device_err(e, 0))?;
        let fs = fatfs::FileSystem::new(&mut io, fatfs::FsOptions::new())
            .map_err(|e| device_err(e, 0))?;
        let root = fs.root_dir();
        progress(Progress::Copying {
            done: 0,
            total: plan.bytes,
        });
        for e in &entries {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let path = fat_path(&e.path);
            if e.is_dir {
                root.create_dir(&path)
                    .map_err(|err| device_err(err, done))?;
                continue;
            }
            let mut f = root
                .create_file(&path)
                .map_err(|err| device_err(err, done))?;
            let mut write_err = None;
            let read = iso.read_file(e, &mut buf, |chunk| {
                if cancel.load(Ordering::Relaxed) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                if let Err(err) = f.write_all(chunk) {
                    write_err = Some(err);
                    return Err(io::Error::other("write failed"));
                }
                done += chunk.len() as u64;
                progress(Progress::Copying {
                    done,
                    total: plan.bytes,
                });
                Ok(())
            });
            if let Some(err) = write_err {
                return Err(device_err(err, done));
            }
            match read {
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                    return Err(Error::Cancelled)
                }
                Err(err) => return Err(Error::Io(err)),
                Ok(()) => {}
            }
        }
        drop(root);
        fs.unmount().map_err(|e| device_err(e, done))?;
    }
    progress(Progress::Syncing);
    io.sync().map_err(|e| device_err(e, done))?;

    if options.verify {
        io.drop_cache();
        verify(
            &mut io,
            &mut iso,
            &entries,
            plan.bytes,
            cancel,
            progress,
            &device_err,
        )?;
    }
    Ok(done)
}

/// Read every file back from the drive and compare it with the ISO.
fn verify(
    io: &mut BlockIo,
    iso: &mut Source,
    entries: &[Entry],
    total: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
    device_err: &dyn Fn(io::Error, u64) -> Error,
) -> Result<()> {
    io.seek(std::io::SeekFrom::Start(0))
        .map_err(|e| device_err(e, 0))?;
    let fs = fatfs::FileSystem::new(io, fatfs::FsOptions::new()).map_err(|e| device_err(e, 0))?;
    let root = fs.root_dir();
    let mut verified = 0u64;
    let mut want = vec![0u8; COPY_CHUNK];
    let mut got = vec![0u8; COPY_CHUNK];
    for e in entries.iter().filter(|e| !e.is_dir) {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let mut f = root
            .open_file(&fat_path(&e.path))
            .map_err(|err| device_err(err, verified))?;
        let mut mismatch = None;
        let mut read_err = None;
        iso.read_file(e, &mut want, |chunk| {
            let g = &mut got[..chunk.len()];
            if let Err(err) = f.read_exact(g) {
                read_err = Some(err);
                return Err(io::Error::other("read failed"));
            }
            if let Some(i) = chunk.iter().zip(g.iter()).position(|(a, b)| a != b) {
                mismatch = Some(verified + i as u64);
                return Err(io::Error::other("mismatch"));
            }
            verified += chunk.len() as u64;
            progress(Progress::Verifying { verified, total });
            Ok(())
        })
        .or_else(|err| {
            if mismatch.is_some() || read_err.is_some() {
                Ok(())
            } else {
                Err(err)
            }
        })?;
        if let Some(err) = read_err {
            return Err(device_err(err, verified));
        }
        if let Some(offset) = mismatch {
            return Err(Error::VerifyFailed { offset });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iso9660::build::{iso, File as IsoFile};
    use crate::mock::FileDevice;

    fn temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p =
            std::env::temp_dir().join(format!("libflasher_extract_{}_{name}", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn files() -> Vec<IsoFile<'static>> {
        vec![
            IsoFile {
                path: "EFI/BOOT/BOOTX64.EFI",
                data: vec![0xEF; 70_000],
            },
            IsoFile {
                path: "boot/grub/grub.cfg",
                data: b"menuentry 'x' {}\n".to_vec(),
            },
            IsoFile {
                path: "casper/filesystem.squashfs",
                data: (0..3_000_000u32).map(|i| (i % 253) as u8).collect(),
            },
            IsoFile {
                path: "README.diskdefines",
                data: b"#define DISKNAME Test\n".to_vec(),
            },
        ]
    }

    fn run(iso_bytes: &[u8], disk: u64, name: &str) -> (Result<u64>, std::path::PathBuf) {
        let img = temp(&format!("{name}.iso"), iso_bytes);
        let info = crate::image::inspect(&img).unwrap();
        let disk_path = temp(&format!("{name}.disk"), &vec![0xEEu8; disk as usize]);
        let mut dev = FileDevice::open(&disk_path).unwrap();
        let r = extract(
            &info,
            &mut dev,
            &FlashOptions::default(),
            &AtomicBool::new(false),
            &mut |_| {},
        );
        std::fs::remove_file(img).ok();
        (r, disk_path)
    }

    #[test]
    fn extracts_and_verifies_an_iso_onto_fat32() {
        let fs_ = files();
        let (r, disk) = run(&iso("TEST_LIVE_1", &fs_, true), 96 << 20, "ok");
        let total: u64 = fs_.iter().map(|f| f.data.len() as u64).sum();
        assert_eq!(r.unwrap(), total);

        // Read it back independently: the GPT says where the partition is,
        // and fatfs reads it from a plain file.
        let raw = std::fs::read(&disk).unwrap();
        let entries_lba = u64::from_le_bytes(raw[512 + 72..512 + 80].try_into().unwrap()) as usize;
        let first = u64::from_le_bytes(
            raw[entries_lba * 512 + 32..entries_lba * 512 + 40]
                .try_into()
                .unwrap(),
        ) as usize;
        let part = std::io::Cursor::new(raw[first * 512..].to_vec());
        let fat = fatfs::FileSystem::new(part, fatfs::FsOptions::new()).unwrap();
        assert_eq!(fat.fat_type(), fatfs::FatType::Fat32);
        assert_eq!(fat.volume_label(), "TEST_LIVE_1");
        for f in &fs_ {
            let mut got = Vec::new();
            fat.root_dir()
                .open_file(f.path)
                .unwrap()
                .read_to_end(&mut got)
                .unwrap();
            assert_eq!(got, f.data, "{}", f.path);
        }
        std::fs::remove_file(disk).ok();
    }

    #[test]
    fn refuses_an_iso_without_a_uefi_bootloader() {
        let fs_ = vec![IsoFile {
            path: "isolinux/isolinux.bin",
            data: vec![1; 100],
        }];
        let (r, disk) = run(&iso("BIOS", &fs_, true), 96 << 20, "bios");
        assert!(
            matches!(&r, Err(Error::Unsupported(m)) if m.contains("UEFI")),
            "{r:?}"
        );
        assert!(
            std::fs::read(&disk).unwrap().iter().all(|&b| b == 0xEE),
            "drive was touched"
        );
        std::fs::remove_file(disk).ok();
    }

    #[test]
    fn refuses_a_drive_too_small_before_touching_it() {
        let (r, disk) = run(&iso("X", &files(), true), 32 << 20, "small");
        assert!(matches!(r, Err(Error::ImageTooLarge { .. })), "{r:?}");
        assert!(
            std::fs::read(&disk).unwrap().iter().all(|&b| b == 0xEE),
            "drive was touched"
        );
        std::fs::remove_file(disk).ok();
    }

    #[test]
    fn plans_reject_case_clashes_and_huge_files() {
        let e = |path: &str, size: u64| Entry::for_tests(path, size);
        let boot = e("EFI/BOOT/BOOTX64.EFI", 10);
        assert!(plan_for(
            &[boot.clone(), e("a/Readme", 1), e("a/README", 1)],
            "X".into()
        )
        .is_err());
        assert!(plan_for(
            &[boot.clone(), e("sources/install.wim", 5 << 30)],
            "X".into()
        )
        .is_err());
        let p = plan_for(&[boot, e("x", 100)], "Ubuntu 26.04 LTS amd64".into()).unwrap();
        assert_eq!(p.label, "UBUNTU 26_0");
        assert_eq!(p.files, 2);
    }

    /// Extract the ISO in `FLASHER_TEST_ISO` into the file `FLASHER_TEST_OUT`
    /// (which must exist, at the disk size wanted). CI boots the result in
    /// QEMU with UEFI firmware to prove it starts.
    #[test]
    #[ignore = "needs FLASHER_TEST_ISO and FLASHER_TEST_OUT"]
    fn extract_to_file() {
        let (Ok(iso), Ok(out)) = (
            std::env::var("FLASHER_TEST_ISO"),
            std::env::var("FLASHER_TEST_OUT"),
        ) else {
            return;
        };
        let info = crate::image::inspect(&iso).unwrap();
        assert!(
            info.kind.needs_extract(),
            "{iso} is {:?}, not a plain ISO",
            info.kind
        );
        let mut dev = FileDevice::open(std::path::Path::new(&out)).unwrap();
        let n = extract(
            &info,
            &mut dev,
            &FlashOptions::default(),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        let through = if crate::udf::is_udf(&mut std::fs::File::open(&iso).unwrap()).unwrap() {
            "UDF"
        } else {
            "ISO 9660"
        };
        println!("extracted {n} bytes from {iso} into {out}, read through {through}");
    }
}
