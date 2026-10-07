# libflasher

Write disk images to USB drives and SD cards, from Rust, on macOS, Linux and
Windows. The engine behind [Flasher](https://github.com/developer180527/Flasher).
MIT licensed, pure Rust except for the optional decompressors.

```toml
[dependencies]
libflasher = { git = "https://github.com/developer180527/libflasher", rev = "…" }  # crates.io later
```

```rust
use std::sync::atomic::AtomicBool;
use libflasher::{image, rate::StatusLine, FlashOptions, Platform};

let platform = libflasher::current_platform();
let drive = platform.list_devices()?.remove(0);           // removable disks only
let image = image::inspect("pi.img.xz")?;                  // kind, compression, exact size
let mut dev = platform.open_listed(&drive)?;               // re-checks it is still that disk
let mut status = StatusLine::new();
libflasher::write_image(&image, dev.as_mut(), &FlashOptions::default(),
    &AtomicBool::new(false), &mut |p| { status.update(p); println!("{}", status.text()) })?;
platform.eject(&drive)?;
```

The crate docs carry a complete example that runs as a test against
file-backed mock drives, so it needs no hardware.

## What it does

**Two ways to put an image on a drive**, chosen by `write_image`:

- **Raw write** for disk images and hybrid ISOs (Raspberry Pi OS, most Linux
  distributions): byte for byte, then read back and compared.
- **Extract mode** for ISOs that are not disk images (Windows installers,
  some Linux ISOs): a fresh GPT + FAT32 partition, the ISO's files copied
  onto it, every file read back and compared. Boots on UEFI firmware. A
  Windows `install.wim` over FAT32's 4 GB limit is split into `install.swm`,
  `install2.swm`, … which Windows Setup reads in its place.

**Around that:**

| | |
|---|---|
| Images | Disk image / hybrid ISO / plain ISO detected from content, not name. gzip, xz, zstd, bzip2, and `.zip` holding one image (stored or deflate, ZIP64, CRC-32 checked). An `.xz`'s or `.zip`'s exact size is read from its index, so progress is exact and a too-small drive is refused before writing. A file of unknown kind is written as asked, and front ends are told to warn. |
| Filesystems read | ISO 9660 with Rock Ridge and Joliet (continuation areas, split names); UDF 1.02–2.01 (Windows ISOs). |
| Checksums | SHA-256 of the download, checked before the drive is touched; found automatically in `SHA256SUMS` / `<image>.sha256` next to it. A checksum file from beside the image proves the download is intact, not that it is genuine; the API says which kind of file it was so front ends can say so. |
| Safety | Only removable disks are listed. The disks the running system is on never are, however they are attached, and are refused if named: macOS follows `/` to its APFS physical stores (a Mac started from an external SSD); Linux follows the system mounts and swap through partitions, LVM/LUKS/RAID and loop devices (a live USB, `/dev/root`); Windows takes the Windows volume's disks, the EFI partition's and the page files', and lists nothing if it cannot tell. Linux offers SD cards but never soldered-in eMMC, its boot partitions or empty card slots. A drive is re-checked (model, size, bus) before opening, because device paths are reused after unplugging; on Linux the opened file is also checked to be that disk. While a disk is open nothing can mount it: Linux opens it `O_EXCL` and names any LUKS/LVM/RAID volume still holding it, Windows locks every volume (retrying for 5 s while Explorer or a scanner lets go), macOS refuses mounts through DiskArbitration, and checks the mount table four times a second: a volume mounted anyway is unmounted at once and the write fails with the reason. |
| Faults | 1 MiB requests, a flush every 32 MiB; a request over 20 s is a stall and nothing more is sent; an unplugged drive is reported as such, mid-write, mid-flush or mid-verify. Verification reads the drive, never an OS cache (Linux drops it, Windows writes unbuffered). Extract mode checks every file's contents and length. |
| Progress | Speed and time left over a sliding window, labelled when estimated (`rate::StatusLine`, the same words in every front end). |
| Platform services | Hot-plug notifications, keep-awake, eject, restore a flashed drive to plain exFAT. |

## Architecture

```
libflasher                the crate apps depend on: re-exports everything below,
│                         plus current_platform() and, on Linux, linux_helper
├── libflasher_core       OS-independent; never opens a device itself
│   ├── image, zip        what an image is; decompressing reader; xz index, zip directory
│   ├── flash             raw write → flush → verify; the write_image dispatcher
│   ├── extract           plan + extract over "items": dirs, files, generated .swm parts
│   ├── iso9660, udf      read-only filesystem readers, bounded against hostile input
│   ├── wim               install.wim → .swm split (resources copied, never recompressed)
│   ├── gpt, blockio      partition table writer; sector-aligned write-back cache for fatfs
│   ├── checksum, rate    SHA-256 and published-sum lookup; speed, time left, status text
│   ├── platform          the Platform and RawDevice traits, DeviceInfo
│   ├── mock              drives that are files, with injected faults (tests, CI, demos)
│   └── conformance       the cycle every backend must pass on a real (virtual) disk
├── libflasher_platform   picks the backend at compile time; FLASHER_MOCK_DIR swaps in mocks
├── libflasher_macos      diskutil, /dev/rdiskN, authopen fd passing, DiskArbitration, caffeinate
├── libflasher_linux      sysfs, umount2, O_EXCL, uevent netlink, systemd-inhibit,
│                         and libflasher-helper (pkexec) so apps need not run as root
└── libflasher_windows    \\.\PhysicalDriveN, FSCTL lock/dismount, CM_Register_Notification,
                          power requests, diskpart; policy.rs holds the testable decisions
```

**The seam that matters** is `Platform` (find, open, release disks; watch,
keep awake, restore) and `RawDevice` (a whole disk that takes whole-sector
reads and writes). Everything else is OS-independent and tested once. A new
OS is a new crate implementing those two traits, one line in
`libflasher_platform`, and passing `conformance::full_cycle`. Trait methods
added later come with defaults, and public types are `#[non_exhaustive]`, so
neither breaks existing users.

**Privilege** never extends to a whole program on macOS or Linux: the OS opens
the disk with the user's consent (`authopen`, or `pkexec libflasher-helper`)
and passes the open file descriptor back, and writing happens unprivileged.
That descriptor is close-on-exec, so no program started meanwhile inherits
it. Windows has no such mechanism, so the app runs elevated; the one script
it hands an elevated tool (`diskpart`) is created fresh in `Windows\Temp` and
locked until it has been read.

**Untrusted input.** ISO, UDF, WIM and zip structures come from downloaded
files; every size, depth, count and offset read from them is bounded or
checked before use, as is the total work (a directory reached from two
parents is refused), and a malformed image is an error, never a hang or a
panic. A mutation test holds this: 120,000 seeded, damaged images per run.

## How it is tested

| Layer | What | Where |
|---|---|---|
| Unit | Every module, including hostile-input bounds, injected faults (stall, unplug, corruption), GPT CRCs, WIM split invariants | every OS, every push |
| **Mutation** | Valid ISO 9660, Rock Ridge, UDF, WIM and zip images, damaged 120,000 seeded ways, read as extract mode would: no panic, nothing over 2 s | every OS, every push |
| Backends, read-only | Real disks on each runner: listing, refusing the system disk, keep-awake, hot-plug (attach and detach a virtual disk) | macOS, Linux, Windows |
| **Conformance** | List → open → flash → verify → restore through the real backend, on a throwaway virtual disk (loop device, `hdiutil` image, VHD). Linux runs it twice: as root, and unprivileged through the helper. Refuses any disk over 512 MiB. | macOS, Linux, Windows |
| Independent tools | Apple's `hdiutil` builds Joliet, UDF and bridge ISOs; libflasher extracts them; **macOS mounts the result** and every file is compared. **wimlib** verifies and applies our `.swm` split sets. Info-ZIP `zip` and macOS `ditto` make the zips read back. | macOS tests; CI `wim-split` |
| **Boot** | A GRUB UEFI ISO (built by `xorriso`, and by `genisoimage` with UDF) is extracted, then **booted in QEMU on OVMF** | CI `extract-boots` |

Real hardware so far, all written from macOS 27 to a USB stick and verified:
a Raspberry Pi image, an Ubuntu ISO, and a Windows 10 22H2 ISO in extract mode
(906 files, `install.wim` split into two `.swm` parts). That Windows stick
booted an MSI desktop in UEFI mode into Windows Setup, which listed every
edition, so it read the split set (October 2026; not installed past that).
Nothing has been written from Linux or Windows yet.

## State

Done: everything above. About 11,100 lines; 115 tests and 10 CI jobs.

**Known gaps**, most important first:

- **Real-hardware testing** on Linux and Windows.
- **A full Windows install from split `.swm` files**: Setup boots from an
  extracted stick and lists the editions from the split set (on real UEFI
  hardware), and wimlib verifies and applies the set; an install has not yet
  been carried through to the desktop.
- **Legacy BIOS boot, and files over 4 GB other than `install.wim`**: refused
  today. Both need GPL components (Syslinux/GRUB; NTFS + UEFI:NTFS), which
  will ship as a separate, optional package so libflasher stays MIT.
- Extracted Linux ISOs that find their files by volume label (Fedora's) may
  not boot, as FAT labels are 11 characters. Those ISOs are hybrid, so they
  take the raw path anyway.
- **Signatures.** Checksums are checked; signatures (`SHA256SUMS.gpg`,
  minisign) are not. That needs an OpenPGP verifier and a store of trusted
  keys, and will be its own piece of work.
- Extract mode makes one FAT32 partition of at most 2 TiB (FAT32's limit with
  512-byte sectors); the rest of a larger disk is left unpartitioned.
- Ejecting is a no-op on Linux (needs UDisks2 power-off).
- A Linux root on a multi-device btrfs or a ZFS pool is traced to one disk at most; such roots on USB disks are rare, and a mounted disk is refused when opened anyway.
- Not yet on crates.io. The API is frozen at 0.1 but has had no outside users.

## Licence

MIT. libflasher contains no GPL code. Rufus (GPLv3) was read for behaviour,
never copied. wimlib, GRUB and xorriso are used only in CI, as independent
test oracles, and are not distributed.
