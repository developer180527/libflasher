//! Extract mode: for ISOs that are not disk images (not "hybrid"), make the
//! drive bootable by copying the ISO's files onto a fresh FAT32 partition.
//!
//! UEFI firmware boots removable media by running `\EFI\BOOT\BOOT<arch>.EFI`
//! from a FAT partition, so an ISO that carries that file boots this way
//! with no bootloader installed by us. That covers most modern installers.
//! Files are read through UDF when the image has it (Windows ISOs) and ISO
//! 9660 otherwise. A Windows `sources/install.wim` too big for FAT32 is
//! split into `install.swm`, `install2.swm`, … which Windows Setup reads in
//! its place. Refused with a reason rather than half-done: ISOs with no UEFI
//! bootloader (BIOS-only) and any other file over FAT32's 4 GiB limit.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::blockio::BlockIo;
use crate::iso9660::{read_extents_range, Entry, Iso};
use crate::udf::Udf;
use crate::wim::{self, RangeSource};
use crate::{gpt, Compression, Error, FlashOptions, ImageInfo, Progress, RawDevice, Result};

/// fatfs needs at least ~65 k clusters for FAT32; this leaves room.
const MIN_PARTITION: u64 = 64 << 20;
const COPY_CHUNK: usize = 1 << 20;

/// The size rules, separate so tests can use small ones.
#[derive(Clone, Copy, Debug)]
struct Limits {
    /// FAT32 cannot hold a file this large or larger.
    fat_max_file: u64,
    /// Largest `.swm` part to make: under the FAT32 limit with room to spare,
    /// as Microsoft's own tools do.
    swm_part: u64,
}

const LIMITS: Limits = Limits {
    fat_max_file: 1 << 32,
    swm_part: 4000 << 20,
};

/// The Windows image that is split when too big for FAT32.
const INSTALL_WIM: &str = "sources/install.wim";

/// What extracting an ISO would do, worked out without touching a drive.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Plan {
    /// The FAT volume name, from the ISO's volume identifier.
    pub label: String,
    /// Files to write.
    pub files: usize,
    /// Bytes in those files.
    pub bytes: u64,
    /// Space the copy needs on the drive, with filesystem overhead.
    pub needs: u64,
    /// If `install.wim` is split, into how many `.swm` parts.
    pub wim_parts: Option<usize>,
}

/// Check an ISO can be extracted, and say what that would take.
pub fn plan(image: &ImageInfo) -> Result<Plan> {
    let mut src = Source::open(image)?;
    let items = items(&mut src, LIMITS)?;
    plan_for(&items, &src.volume_id())
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
        let mut f = BufReader::new(image.open_file()?);
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

    fn read_range(
        &mut self,
        e: &Entry,
        start: u64,
        len: u64,
        buf: &mut [u8],
        sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        match self {
            Source::Iso(i) => read_extents_range(i.reader(), e, start, len, buf, sink),
            Source::Udf(u) => read_extents_range(u.reader(), e, start, len, buf, sink),
        }
    }
}

/// One file inside the image, read as a whole: what the WIM splitter reads from.
struct InImage<'a> {
    src: &'a mut Source,
    file: &'a Entry,
}

impl RangeSource for InImage<'_> {
    fn read_range(
        &mut self,
        offset: u64,
        len: u64,
        buf: &mut [u8],
        sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        self.src.read_range(self.file, offset, len, buf, sink)
    }
}

/// Something to put on the drive.
enum Item {
    Dir(String),
    /// A file copied as it is.
    File(Entry),
    /// One part of a split `install.wim`, generated from it.
    Swm {
        path: String,
        wim: Entry,
        part: wim::Part,
    },
}

impl Item {
    fn path(&self) -> &str {
        match self {
            Item::Dir(p) | Item::Swm { path: p, .. } => p,
            Item::File(e) => &e.path,
        }
    }

    fn size(&self) -> u64 {
        match self {
            Item::Dir(_) => 0,
            Item::File(e) => e.size,
            Item::Swm { part, .. } => part.size,
        }
    }

    /// Produce the item's bytes, as they go on the drive.
    fn produce(
        &self,
        src: &mut Source,
        buf: &mut [u8],
        sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        match self {
            Item::Dir(_) => Ok(()),
            Item::File(e) => src.read_range(e, 0, e.size, buf, sink),
            Item::Swm { wim, part, .. } => part.stream(&mut InImage { src, file: wim }, buf, sink),
        }
    }
}

/// The image's files as items, with an oversized `install.wim` split.
fn items(src: &mut Source, limits: Limits) -> Result<Vec<Item>> {
    let entries = src.walk()?;
    let mut out = Vec::with_capacity(entries.len() + 2);
    for e in entries {
        if e.is_dir {
            out.push(Item::Dir(e.path));
        } else if e.size >= limits.fat_max_file && e.path.eq_ignore_ascii_case(INSTALL_WIM) {
            let dir = &e.path[..e.path.len() - "install.wim".len()];
            let parts = wim::split(
                &mut InImage { src, file: &e },
                e.size,
                limits.swm_part,
                "install",
            )
            .map_err(Error::Unsupported)?;
            for part in parts {
                out.push(Item::Swm {
                    path: format!("{dir}{}", part.name),
                    wim: e.clone(),
                    part,
                });
            }
        } else {
            out.push(Item::File(e));
        }
    }
    Ok(out)
}

fn plan_for(items: &[Item], volume_id: &str) -> Result<Plan> {
    let uefi = items.iter().any(|i| {
        let p = i.path().to_ascii_lowercase();
        matches!(i, Item::File(_)) && p.starts_with("efi/boot/boot") && p.ends_with(".efi")
    });
    if !uefi {
        return Err(Error::Unsupported(
            "this ISO has no UEFI bootloader (EFI/BOOT/BOOTX64.EFI), so it can only boot in legacy BIOS mode, \
             which is not supported yet"
                .into(),
        ));
    }
    if let Some(big) = items.iter().find(|i| i.size() >= LIMITS.fat_max_file) {
        return Err(Error::Unsupported(format!(
            "{} is {}, over FAT32's 4 GB limit for one file; only Windows' install.wim can be split",
            big.path(),
            crate::platform::human_size(big.size())
        )));
    }
    // FAT compares names without case: two names differing only in case
    // would silently become one file.
    let mut seen = HashSet::new();
    for i in items {
        if !seen.insert(fat_path(i.path()).to_lowercase()) {
            return Err(Error::Unsupported(format!(
                "{} differs from another file only in letter case, which FAT cannot hold",
                i.path()
            )));
        }
    }
    let files: Vec<&Item> = items
        .iter()
        .filter(|i| !matches!(i, Item::Dir(_)))
        .collect();
    let bytes: u64 = files.iter().map(|i| i.size()).sum();
    // Clusters are at most 32 KiB on the sizes we format; round every file
    // up to one, add the FATs and directories generously.
    let needs = files
        .iter()
        .map(|i| i.size().div_ceil(32 << 10) * (32 << 10))
        .sum::<u64>()
        + bytes / 64
        + (16 << 20);
    let swm = items
        .iter()
        .filter(|i| matches!(i, Item::Swm { .. }))
        .count();
    Ok(Plan {
        label: fat_label(volume_id),
        files: files.len(),
        bytes,
        needs,
        wim_parts: (swm > 0).then_some(swm),
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

/// Make the drive a FAT32 copy of the ISO's files. Returns the bytes written.
pub fn extract(
    image: &ImageInfo,
    device: &mut dyn RawDevice,
    options: &FlashOptions,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<u64> {
    extract_with(image, device, options, cancel, progress, LIMITS)
}

fn extract_with(
    image: &ImageInfo,
    device: &mut dyn RawDevice,
    options: &FlashOptions,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
    limits: Limits,
) -> Result<u64> {
    let mut src = Source::open(image)?;
    let items = items(&mut src, limits)?;
    let plan = plan_for(&items, &src.volume_id())?;
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
        for item in &items {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let path = fat_path(item.path());
            if let Item::Dir(_) = item {
                root.create_dir(&path)
                    .map_err(|err| device_err(err, done))?;
                continue;
            }
            let mut f = root
                .create_file(&path)
                .map_err(|err| device_err(err, done))?;
            let mut write_err = None;
            let read = item.produce(&mut src, &mut buf, &mut |chunk| {
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
            &mut src,
            &items,
            plan.bytes,
            cancel,
            progress,
            &device_err,
        )?;
    }
    Ok(done)
}

/// Read every file back from the drive and compare it with what was meant
/// to be written: the ISO's bytes, or the generated `.swm` parts.
fn verify(
    io: &mut BlockIo,
    src: &mut Source,
    items: &[Item],
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
    for item in items.iter().filter(|i| !matches!(i, Item::Dir(_))) {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let mut f = root
            .open_file(&fat_path(item.path()))
            .map_err(|err| device_err(err, verified))?;
        let mut mismatch = None;
        let mut read_err = None;
        item.produce(src, &mut want, &mut |chunk| {
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
        // Every byte meant to be there is; nothing else may be. A damaged
        // directory entry can claim more.
        let len = f
            .seek(std::io::SeekFrom::End(0))
            .map_err(|err| device_err(err, verified))?;
        if len != item.size() {
            return Err(Error::VerifyFailed { offset: verified });
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

    /// A file on the drive longer than the one in the ISO fails
    /// verification, though every byte the ISO has matches.
    #[test]
    fn verify_notices_a_file_of_the_wrong_length() {
        let img = temp("len.iso", &iso("LEN", &files(), true));
        let info = crate::image::inspect(&img).unwrap();
        let disk = temp("len.disk", &vec![0u8; 96 << 20]);
        let mut dev = FileDevice::open(&disk).unwrap();
        let no = AtomicBool::new(false);
        extract(&info, &mut dev, &FlashOptions::default(), &no, &mut |_| {}).unwrap();

        let layout = gpt::layout(dev.size(), 512).unwrap();
        let stall = std::time::Duration::from_secs(20);
        fn open(
            dev: &mut FileDevice,
            layout: gpt::Layout,
            stall: std::time::Duration,
        ) -> BlockIo<'_> {
            BlockIo::new(dev, layout.start, layout.len, stall, 32 << 20)
        }
        {
            let mut io = open(&mut dev, layout, stall);
            let fs = fatfs::FileSystem::new(&mut io, fatfs::FsOptions::new()).unwrap();
            let mut f = fs.root_dir().open_file("README.diskdefines").unwrap();
            f.seek(std::io::SeekFrom::End(0)).unwrap();
            f.write_all(b"extra").unwrap();
            drop(f);
            fs.unmount().unwrap();
            io.sync().unwrap();
        }
        let mut src = Source::open(&info).unwrap();
        let items = items(&mut src, LIMITS).unwrap();
        let mut io = open(&mut dev, layout, stall);
        let r = verify(&mut io, &mut src, &items, 0, &no, &mut |_| {}, &|e, at| {
            Error::Io(e).at_device(at)
        });
        assert!(matches!(r, Err(Error::VerifyFailed { .. })), "{r:?}");
        std::fs::remove_file(img).ok();
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
        let f = |path: &str, size: u64| Item::File(Entry::for_tests(path, size));
        let boot = || f("EFI/BOOT/BOOTX64.EFI", 10);
        assert!(plan_for(&[boot(), f("a/Readme", 1), f("a/README", 1)], "X").is_err());
        let e = plan_for(&[boot(), f("data/huge.img", 5 << 30)], "X").unwrap_err();
        assert!(
            e.to_string()
                .contains("only Windows' install.wim can be split"),
            "{e}"
        );
        let p = plan_for(&[boot(), f("x", 100)], "Ubuntu 26.04 LTS amd64").unwrap();
        assert_eq!(p.label, "UBUNTU 26_0");
        assert_eq!(p.files, 2);
        assert_eq!(p.wim_parts, None);
    }

    /// A Windows-shaped ISO whose install.wim is "too big" under small test
    /// limits: it must arrive as install.swm + install2.swm + …, each
    /// exactly what the splitter makes, and no install.wim.
    #[test]
    fn splits_an_oversized_install_wim_while_extracting() {
        let wim = crate::wim::tests::wim(2, &[600_000, 500_000, 700_000, 400_000, 650_000]);
        let fs_ = vec![
            IsoFile {
                path: "EFI/BOOT/BOOTX64.EFI",
                data: vec![0xEF; 10_000],
            },
            IsoFile {
                path: "setup.exe",
                data: b"MZ".to_vec(),
            },
            IsoFile {
                path: "sources/install.wim",
                data: wim.clone(),
            },
        ];
        let img = temp("win.iso", &iso("CCCOMA_X64", &fs_, true));
        let info = crate::image::inspect(&img).unwrap();
        let disk = temp("win.disk", &vec![0u8; 96 << 20]);
        let mut dev = FileDevice::open(&disk).unwrap();
        let limits = Limits {
            fat_max_file: 1 << 20,
            swm_part: 1_200_000,
        };
        extract_with(
            &info,
            &mut dev,
            &FlashOptions::default(),
            &AtomicBool::new(false),
            &mut |_| {},
            limits,
        )
        .unwrap();
        drop(dev);

        let expected =
            crate::wim::split(&mut &wim[..], wim.len() as u64, limits.swm_part, "install").unwrap();
        assert!(expected.len() >= 2);
        let raw = std::fs::read(&disk).unwrap();
        let fat = fatfs::FileSystem::new(
            std::io::Cursor::new(raw[1 << 20..].to_vec()),
            fatfs::FsOptions::new(),
        )
        .unwrap();
        let sources = fat.root_dir().open_dir("sources").unwrap();
        let names: Vec<String> = sources
            .iter()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "." && n != "..")
            .collect();
        assert!(
            !names.iter().any(|n| n.eq_ignore_ascii_case("install.wim")),
            "{names:?}"
        );
        for part in &expected {
            let mut want = Vec::new();
            part.stream(&mut &wim[..], &mut [0u8; 4096], &mut |b| {
                want.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
            let mut got = Vec::new();
            sources
                .open_file(&part.name)
                .unwrap()
                .read_to_end(&mut got)
                .unwrap();
            assert!(got == want, "{} differs", part.name);
        }
        std::fs::remove_file(img).ok();
        std::fs::remove_file(disk).ok();
    }

    /// A 3 TiB disk: the partition stops at FAT32's 2 TiB and extraction
    /// succeeds (without the cap: "Volume has too many sectors"). The disk is
    /// a sparse file, which macOS and Linux make instantly; on Windows it
    /// would really take 3 TiB.
    #[test]
    #[cfg(unix)]
    fn extracts_onto_a_disk_larger_than_fat32_can_address() {
        let img = temp("big.iso", &iso("BIG", &files(), true));
        let info = crate::image::inspect(&img).unwrap();
        let disk = std::env::temp_dir().join(format!("libflasher_3t_{}", std::process::id()));
        std::fs::File::create(&disk)
            .unwrap()
            .set_len(3 << 40)
            .unwrap();
        let mut dev = FileDevice::open(&disk).unwrap();
        let r = extract(
            &info,
            &mut dev,
            &FlashOptions::default(),
            &AtomicBool::new(false),
            &mut |_| {},
        );
        std::fs::remove_file(img).ok();
        std::fs::remove_file(disk).ok();
        r.unwrap();
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
