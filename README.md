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
| Images | Disk image / hybrid ISO / plain ISO detected from content, not name. gzip, xz, zstd, bzip2. An `.xz`'s exact size is read from its index, so progress is exact and a too-small drive is refused before writing. |
| Filesystems read | ISO 9660 with Rock Ridge and Joliet (continuation areas, split names); UDF 1.02–2.01 (Windows ISOs). |
| Checksums | SHA-256 of the download, checked before the drive is touched; found automatically in `SHA256SUMS` / `<image>.sha256` next to it. |
| Safety | Only removable disks are listed. The disks the running system is on never are, however they are attached, and are refused if named: macOS follows `/` to its APFS physical stores (a Mac started from an external SSD); Linux follows the system mounts and swap through partitions, LVM/LUKS/RAID and loop devices (a live USB, `/dev/root`). A drive is re-checked (model, size, bus) before opening, because device paths are reused after unplugging. |
| Faults | 1 MiB requests, a flush every 32 MiB; a request over 20 s is a stall and nothing more is sent; an unplugged drive is reported as such, mid-write or mid-verify. |
| Progress | Speed and time left over a sliding window, labelled when estimated (`rate::StatusLine`, the same words in every front end). |
| Platform services | Hot-plug notifications, keep-awake, eject, restore a flashed drive to plain exFAT. |

## Architecture

```
libflasher                the crate apps depend on: re-exports everything below,
│                         plus current_platform() and, on Linux, linux_helper
├── libflasher_core       OS-independent; never opens a device itself
│   ├── image             what an image is; decompressing reader; xz index size
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
Windows has no such mechanism, so the app runs elevated.

**Untrusted input.** ISO, UDF and WIM structures come from downloaded files;
every size, depth, count and offset read from them is bounded or checked
before use, and a malformed image is an error, never a hang or a panic.

## How it is tested

| Layer | What | Where |
|---|---|---|
| Unit | Every module, including hostile-input bounds, injected faults (stall, unplug, corruption), GPT CRCs, WIM split invariants | every OS, every push |
| Backends, read-only | Real disks on each runner: listing, refusing the system disk, keep-awake, hot-plug (attach and detach a virtual disk) | macOS, Linux, Windows |
| **Conformance** | List → open → flash → verify → restore through the real backend, on a throwaway virtual disk (loop device, `hdiutil` image, VHD). Linux runs it twice: as root, and unprivileged through the helper. Refuses any disk over 512 MiB. | macOS, Linux, Windows |
| Independent tools | Apple's `hdiutil` builds Joliet, UDF and bridge ISOs; libflasher extracts them; **macOS mounts the result** and every file is compared. **wimlib** verifies and applies our `.swm` split sets. | macOS tests; CI `wim-split` |
| **Boot** | A GRUB UEFI ISO (built by `xorriso`, and by `genisoimage` with UDF) is extracted, then **booted in QEMU on OVMF** | CI `extract-boots` |

Real hardware so far: a Raspberry Pi image and an Ubuntu ISO written and
verified to a USB-C stick on macOS. Nothing else has touched a physical drive.

## State

Done: everything above. About 7,900 lines; 52 tests and 9 CI jobs, all green.

**Known gaps**, most important first:

- **Real-hardware testing** on Linux and Windows, and of extract mode anywhere.
- **Windows Setup from split `.swm` files**: the split is verified by wimlib,
  not yet by installing Windows from it.
- **Legacy BIOS boot, and files over 4 GB other than `install.wim`**: refused
  today. Both need GPL components (Syslinux/GRUB; NTFS + UEFI:NTFS), which
  will ship as a separate, optional package so libflasher stays MIT.
- Extracted Linux ISOs that find their files by volume label (Fedora's) may
  not boot, as FAT labels are 11 characters. Those ISOs are hybrid, so they
  take the raw path anyway.
- Ejecting is a no-op on Linux (needs UDisks2 power-off).
- A Linux root on a multi-device btrfs or a ZFS pool is traced to one disk at most; such roots on USB disks are rare, and a mounted disk is refused when opened anyway.
- Not yet on crates.io. The API is frozen at 0.1 but has had no outside users.

## Licence

MIT. libflasher contains no GPL code. Rufus (GPLv3) was read for behaviour,
never copied. wimlib, GRUB and xorriso are used only in CI, as independent
test oracles, and are not distributed.
