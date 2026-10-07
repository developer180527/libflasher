# Work order: review fixes (libflasher + Flasher)

> **Status: done (code).** All 27 work items are implemented, tested and
> released in Flasher 0.1.3 (libflasher `0a9cea8`, Flasher `b47ab0f`), with
> CI green on macOS, Linux and Windows. Still open: the real-hardware
> checklist in WO-27, which only a person with the devices can do. Kept as
> the record of what the reviews found and how each finding was fixed.

Covers every finding from both reviews (October 2026). Each finding maps to
one work item (WO). The traceability table at the end shows that every
finding is covered.

**Repos:** `libflasher` (L) and `Flasher` (F). Flasher picks up libflasher
changes only through its pin (`scripts/pin-libflasher.sh`). For development,
point Flasher at the local checkout with `.cargo/config.toml`, and relock
before committing.

**Rules for every item**

- Each WO ends with a test that fails before the fix and passes after it,
  wherever the platform allows one. Where no automated test can tell the
  difference (WO-11), say so in the commit.
- No public API breaks. New `Platform` methods get defaults, and new types and
  fields are `#[non_exhaustive]`.
- `cargo clippy --workspace --all-targets` is clean and `cargo test --workspace` is green on macOS.
  CI is green on all three OSes before the pin moves.
- One commit per WO, or per phase for the small ones, in the repo's existing
  commit style.

---

## Phase 0: crashes on untrusted input (L)

The README says a malformed image is "never a hang or a panic". Phase 0 makes
that true and adds a test that keeps it true.

### WO-01 · ISO 9660: panic on an empty root directory
- **Where:** `crates/libflasher_core/src/iso9660.rs`, `has_rock_ridge` (`data[0]`, ~l.228).
- **Fix:** `let Some(&len) = data.first() else { return Ok(false) };`. Also
  audit `open()`: a root extent of length 0 must be an error (`bad("empty root
  directory")`), not a silent empty walk.
- **Test:** a 20-sector ISO with a zero-length root extent (the reviewer's
  case, built in the test) returns `Err`/`Ok(false)` and does not panic.

### WO-02 · UDF: out-of-bounds read on a short partition map
- **Where:** `udf.rs`, `Udf::open`, `u16le(maps, 4)` after only checking
  `maps.first() == Some(&1)`.
- **Fix:** require `maps.len() >= 6` and `maps[1] == 6` (a type 1 map is
  6 bytes long) before reading the partition number. Otherwise return
  `bad("partition map too short")`.
- **Test:** an LVD with `map_len` set to 1..=5 returns `Err` for each value.

### WO-03 · ISO/UDF: unbounded total work on shared directories
- **Where:** `walk()` in `iso9660.rs` and `udf.rs`.
- **Problem:** `MAX_ENTRIES` caps the number of entries, not the number of
  bytes read. Many directories pointing at one 16 MiB subdirectory are
  re-read once per parent: up to about 1M × 16 MiB, which never finishes.
- **Fix:**
  - Keep a `HashSet` of directory extents already visited (the ISO extent
    offset; the UDF ICB block). A real image never reaches one directory from
    two parents. A repeat fails with `bad("directory reached twice")`.
  - Add a second backstop: a total budget for directory bytes read,
    `MAX_TOTAL_DIR_BYTES = 256 MiB`.
- **Test:** 50 directories that all point at one subdirectory error out
  quickly, in ISO and in UDF.

### WO-04 · WIM: lookup-table allocation sized by the file
- **Where:** `wim.rs:167` (`read_vec(lookup.offset, lookup.size)`).
- **Fix:** add `MAX_LOOKUP = 64 MiB`, which is over 1M entries, far past any
  real WIM. Refuse a larger table with "install.wim has a lookup table this
  cannot read". Apply the same cap to the XML resource if it is ever read.
- **Test:** a WIM header claiming a 1 GiB lookup table is refused without
  allocating it.

### WO-05 · Hostile-input regression net
- **What:** a deterministic mutation test in `libflasher_core`. It builds the
  test ISO (`iso9660::build`), a UDF image and a WIM, applies about 2,000
  seeded mutations (truncate, flip bytes, set length fields to 0, 1, max) and
  runs `inspect` → `extract::plan` under `catch_unwind`. It asserts no panic
  and a finish within a time limit.
- **Optional:** a `fuzz/` directory with cargo-fuzz targets for
  `Iso::open+walk`, `Udf::open+walk`, `wim::split` and `xz_size`. Not in CI.
- **Done when:** the test catches WO-01 and WO-02 with those fixes undone.
  WO-03 and WO-04 cost time or memory only at scale, so a small damaged
  image cannot show them; their own targeted tests guard them instead.
- **Status: done.** 20,000 seeds per format (80,000 images) in about 0.8 s.
  With the fixes undone, it catches the ISO panic on 774 seeds and the UDF
  panic on 10.

---

## Phase 1: never offer or open the wrong disk (L)

### WO-06 · Windows: system-disk check fails open
- **Where:** `libflasher_windows/src/sys.rs`, `system_disk()` →
  `is_system: system == Some(number)` (l.237).
- **Fix:**
  - `system_disk()` returns `Result<Vec<u32>>`. `list_devices`, `checked()`
    and `restore` fail with `Error::Tool` ("could not tell which disk Windows
    runs from") when it errors, matching macOS and Linux.
  - Widen "system" from the disk holding `%SystemRoot%` to every disk holding
    the boot volume, the system (EFI) partition or a page file:
    `GetSystemWindowsDirectoryW` plus `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`
    on each of those volumes. A volume that spans disks (dynamic or Storage
    Spaces) then marks all of its disks.
  - `DiskFacts.is_system` stays. Policy test: a `None`/error result never
    yields a candidate.
- **Test:** a policy unit test; the Windows CI test that refuses the system disk stays green.
- **Status: done.** The Windows-directory volume is required (and all its
  disks, via `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`). The EFI partition
  (`SYSTEM\Setup\SystemPartition`) and page files (`ExistingPageFiles`) are
  added when they resolve, so one odd registry value cannot hide every drive.
  Parsers tested in `policy.rs` on every OS; the rest is Windows CI.

### WO-07 · Linux: junk and internal devices listed
- **Where:** `libflasher_linux/src/lib.rs`, `is_candidate` (l.263) and `list_devices`.
- **Fix:**
  - Exclude eMMC hardware partitions: a name matching `mmcblk\d+boot\d+` or
    `mmcblk\d+rpmb`, or with `/sys/block/<n>/partition` present.
  - Accept `mmcblk*` only when `device/type` is `SD`. Internal eMMC reports
    `MMC`, and so do SDIO devices.
  - Drop devices of size 0 (an empty card-reader slot), as Windows already does.
  - Move the decision into a pure function over a fake sysfs, like
    `system.rs` (`candidate_in(sys_root, name)`), so it is tested on every OS.
- **Test:** a fake sysfs with `mmcblk0` (type MMC), `mmcblk0boot0`,
  `mmcblk1` (type SD), a USB `sdb` of size 0 and a USB `sdc`. Only `mmcblk1`
  and `sdc` are candidates.
- **Status: done.** `disk.rs::is_candidate_in`; 5 fake-sysfs tests (run on
  macOS with `rustc --test`, since the crate is Linux-only).

### WO-08 · Linux: the opened fd is not tied to the checked disk
- **Where:** `Linux::open_device`. Size and sector size are read from sysfs
  by name after opening.
- **Fix:**
  - After the fd arrives, `fstat` it. It must be a block device whose
    `st_rdev` major:minor equals `/sys/block/<name>/dev`, or the call fails
    with `Refused` ("the drive changed while it was being opened").
  - Read size and sector size from the fd: `BLKGETSIZE64` and `BLKSSZGET`.
  - In `still_listed`, re-list after opening and compare against the
    `DeviceInfo` (model, size, bus).
  - The helper does the same `st_rdev` check before sending the fd.
- **Test:** pure test of the `dev`-string vs `st_rdev` comparison; conformance
  (root and helper) exercises the real path.
- **Status: done.** Size comes from seeking to the end of the device, not
  `BLKGETSIZE64`, which `libc` lacks and whose number differs by
  architecture. The check runs in the helper and in the caller.

### WO-09 · Linux: volumes mounted through mapper/by-uuid not released
- **Where:** `prepare()` matches mount sources only by `/dev/sdX*` prefix.
- **Fix:** resolve every mount source with `canonicalize`, then walk
  `holders/` from the disk and its partitions (the reverse of `system.rs`'s
  `slaves/` walk). For each dm or md holder: unmount anything mounted from it.
  If a holder is still open (LUKS opened, LVM active), refuse with a clear
  message: "<dev> is in use by <holder> (an encrypted or LVM volume); close it
  first". No bare `EBUSY`.
- **Test:** fake-sysfs test of the holder walk; message test for the refusal.
- **Status: done.** `disk.rs::users_in` (holders walk) + `on_disk`; mounts are
  unmounted newest-first.

### WO-10 · macOS: exclusive claim, and the order of unmount and prompt
- **Where:** `libflasher_macos/src/lib.rs`, `open_device` (l.79).
- **Fix:**
  - Reorder: `authopen` first (the password prompt), then `diskutil
    unmountDisk`. Cancelling the prompt then leaves the user's volumes
    mounted. Raw writes start only after the unmount succeeds.
  - Claim the disk while it is open. Use `DADiskClaim` on the whole disk,
    plus a `DARegisterDiskMountApprovalCallback` that refuses mounts of that
    disk's BSD name on a session owned by the `Disk`. Both run on a
    run-loop thread, as in `watch.rs`.
  - Release on `Drop`, before `diskutil eject`. If the claim fails (another
    claimant), refuse with `Refused` ("another app is using this drive").
  - This makes the `RawDevice` doc ("exclusively-held") true on macOS. Add a
    sentence to the trait doc saying what "exclusive" means per OS.
- **Test:** on CI's `hdiutil` disk, a `diskutil mount` attempt while the
  device is open is refused. Conformance stays green.
- **Status: done, without `DADiskClaim`.** Its completion callback never runs
  on macOS 27 (reproduced in a minimal C program), so a claim cannot be
  confirmed. The mount-approval callback alone is what stops remounts. It
  needs no root, and `claim::tests::a_claimed_disk_cannot_be_mounted`
  proves it on a real attached image (the test fails with the refusal removed).
- **Resolved (four rounds of CI):** the refusal works on macOS 26 too, but
  `diskutil mountDisk` exits 0 there even when refused. Two earlier
  diagnoses (session type, then "26 ignores the refusal") read that exit
  status as truth and were wrong; the tests now read the mount table.
  Second guard: the hold polls `getmntinfo` every 250 ms and force-unmounts
  any volume of the disk it finds, records it, and the open disk then fails
  every read and write with "macOS mounted the drive while it was being
  written … flash it again". (DiskArbitration's "description changed"
  callback did not fire on macOS 26, so it is not used.) Tests:
  `nothing_stays_mounted_while_held` (refusal on) and
  `a_mount_that_gets_through_is_undone` (refusal off), which fails without
  the unmount.

### WO-11 · Windows: verify may read from cache; unaligned buffers
- **Where:** `sys.rs`, `open_device` opens with `FILE_FLAG_WRITE_THROUGH` only.
- **Fix:**
  - Open with `FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH`.
  - `NO_BUFFERING` requires sector-aligned buffers, so `Disk` gets a
    page-aligned bounce buffer (1 MiB, `std::alloc` with 4096 alignment).
    `read` and `write` copy through it, so callers keep using ordinary `Vec`s.
    The copy costs a memcpy per MiB.
  - Query `STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR` and assert the buffer meets it.
- **Test:** none can tell the difference on a healthy VHD (noted in the
  commit). Conformance must still pass with the new flags. That proves the
  alignment is right, because unaligned `NO_BUFFERING` I/O fails with
  `ERROR_INVALID_PARAMETER`.
- **Status: done.** Page alignment (4096) is used instead of querying
  `STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR`: no driver's alignment mask exceeds a
  page. Needs Windows CI conformance to confirm.

---

## Phase 2: privilege and process hygiene (L)

### WO-12 · macOS: authopen fd is inherited by child processes
- **Where:** `authopen.rs`, `recv_fd` (l.67).
- **Fix:** right after receiving the fd, call `fcntl(fd, F_SETFD, FD_CLOEXEC)`.
  macOS has no `MSG_CMSG_CLOEXEC`. Also check `MSG_CTRUNC` and close any
  extra fds received, in both macOS and Linux `recv_fd`.
- **Test:** `passes_a_descriptor` also asserts `fcntl(F_GETFD) & FD_CLOEXEC != 0`.
- **Status: done.** Both `recv_fd`s now take every descriptor received,
  keep the first, close the rest, and fail on `MSG_CTRUNC`. The test fails
  without the `fcntl`.

### WO-13 · Windows: diskpart script in a predictable temp file
- **Where:** `sys.rs` `restore` (l.135).
- **Fix:** no file. Pipe `policy::restore_script(n, label)` to `diskpart`'s
  stdin (`Stdio::piped()`, write, close), then read its output as now.
  Re-run `checked()` right before spawning, so the disk number is the one
  that was checked.
- **Test:** the Windows CI conformance test runs restore; the policy test is unchanged.
- **Status: done, with a file, not stdin.** Fed on stdin, diskpart runs
  interactively: it carries on past an error and may exit 0, so a failed
  restore would look like success. The script is now in `Windows\Temp`,
  created fresh (`create_new`), and held open with write and delete sharing
  denied until diskpart exits. **Confirmed:** Windows CI conformance (which
  ends with a restore) passes, so diskpart reads the held file.

### WO-14 · Linux: `keep_awake` outlives a killed app
- **Where:** `libflasher_linux/src/lib.rs`, `keep_awake`.
- **Fix:**
  - The inhibited command becomes `tail --pid=<our pid> -f /dev/null` in
    place of `sleep infinity`. It exits when we do, and `systemd-inhibit`
    releases the lock, as macOS does with `caffeinate -w`.
  - `pre_exec` sets `PR_SET_PDEATHSIG(SIGTERM)` on `systemd-inhibit` as a
    second guard. Document the Linux gotcha: it fires on death of the
    spawning *thread*, which is harmless here because the guard never
    outlives the thread that took it.
  - If `tail --pid` is missing (busybox), fall back to the current behavior.
- **Test:** spawn a child process that takes `keep_awake`, then `SIGKILL`
  it. The inhibitor is gone within 3 s (`systemd-inhibit --list`). Runs in
  the Linux CI job; skipped where systemd is absent.
- **Status: done.** The `PR_SET_PDEATHSIG` thread caveat is documented in
  the code. Since a container has no systemd, the test checks the mechanism
  `keep_awake` relies on instead: `tail --pid` ends after its target is
  SIGKILLed.

### WO-15 · Volume label may start with `-`
- **Where:** `libflasher_core/src/platform.rs`, `volume_label`.
- **Fix:** reject a leading `-` ("a drive name cannot start with -").
  Separately, on macOS pass the label after the format name, where
  `diskutil` takes it, and on Linux use `mkfs.exfat -n <label> -- <part>`.
- **Test:** `volume_label("-rf")` returns `Err`; the existing label tests are unchanged.
- **Status: done.** `diskutil eraseDisk` takes the label positionally, so
  `--` cannot go there; rejecting `-` covers it. `mkfs.exfat` gets `--`.
  First tests for `volume_label`.

---

## Phase 3: correctness and robustness (L)

### WO-16 · macOS: one vanished disk aborts the whole listing
- **Where:** `lib.rs:46` (`disk_info(&id)?`).
- **Fix:** skip a disk whose `diskutil info` fails, since it was unplugged
  mid-listing. `system_disks()` failing still fails the whole listing
  (fail closed).
- **Test:** factor the loop over a `disk_info` function argument, then unit
  test it with one failing id.
- **Status: done.** `devices_in` takes the lookup as an argument and is
  tested with one disk failing.

### WO-17 · Windows: no retry when locking a volume
- **Where:** `sys.rs:89`.
- **Fix:** retry `FSCTL_LOCK_VOLUME` every 250 ms for up to 5 s, because
  Explorer and antivirus hold handles briefly after a plug-in. Then fail
  with "another program is using <letter>: close it and try again", naming
  the drive letter when it has one.
- **Test:** a policy-level test of the retry schedule; real behavior is covered in CI conformance.
- **Status: done.** `policy::retry` + `busy_message`, tested on every OS.
  The error names the drive letters.

### WO-18 · Extract mode: "drive disconnected" lost in BlockIo sync
- **Where:** `blockio.rs:174, 187`, `d.sync().map_err(io::Error::other)`.
- **Fix:** convert while keeping the OS error. `Error::Io(e) => e`
  passes the inner `io::Error` through unchanged, and any other error gets
  `io::Error::other`.
- **Test:** an `Instrumented`-style device whose `sync` returns ENODEV;
  extract fails with `Error::DeviceGone`.
- **Status: done.** The test fails with the old mapping.

### WO-19 · Extract-mode verify does not check file sizes
- **Where:** `extract.rs` `verify`.
- **Fix:** after streaming a file, the FAT entry's length must equal
  `item.size()` (check `f.seek(End(0))`, or read until `0`). A mismatch
  fails with `VerifyFailed` at that file.
- **Test:** corrupt one directory entry's size on the test disk after
  extraction, then re-run `verify`. It fails.
- **Status: done.** Test: bytes appended to a file through FAT after
  extraction; verify fails (and passes without the check).

### WO-20 · Extract mode: FAT32 partition not capped (> 2 TiB disks)
- **Where:** `extract.rs` / `gpt::layout`.
- **Problem:** FAT32's sector count (`BPB_TotSec32`) is 32 bits, which limits
  a volume to 2 TiB with 512-byte sectors.
- **Fix:** `len = min(len, (u32::MAX as u64 * sector) / ALIGN * ALIGN)`. The
  partition shrinks and the rest of the disk is left unpartitioned. Also
  assert the cluster count stays within FAT32's limit for the cluster size
  that `fatfs` picks.
- **Test:** `layout` on a 3 TiB disk with 512-byte sectors stays ≤ 2 TiB and
  aligned. A sparse-file `FileDevice` of 3 TiB formats successfully; skip
  this on filesystems without sparse files.
- **Status: done.** The cap is in `gpt::layout`, so the table and the
  formatter agree. A 3 TiB sparse-file extraction (`#[cfg(unix)]`, 0.7 s)
  fails without it with fatfs's "Volume has too many sectors".

### WO-21 · `.zip` images
- **Where:** `image.rs`; new `zip.rs` in core, behind a default feature `zip`.
- **Fix:**
  - Detect `PK\x03\x04`. Read the central directory, including ZIP64
    (needed: Raspberry Pi-style zips hold images over 4 GB).
  - Require exactly one file entry that is not a directory, or pick the one
    ending in `.img`/`.iso`. Otherwise refuse, listing the entries.
  - Methods `stored` and `deflate` only, through `flate2`'s raw
    `DeflateDecoder`. `disk_size` is the exact uncompressed size from the
    central directory. Check CRC-32 at the end (`crc32fast`, already a
    dependency) and fail with a clear error on mismatch.
  - Bound all counts, offsets and name lengths, as the ISO and UDF readers do.
  - Add the parser to WO-05's mutation test.
- **Test:** zips made by `zip` (stored and deflate) and a ZIP64 one, written
  and verified. A multi-entry zip is refused with the entry names.
- **Status: done.** `zip.rs`, default feature `zip` (pure Rust).
  `ImageInfo.archive_entry` names the image. Clutter (`__MACOSX/`, `._*`,
  directories) is ignored. Tested: every combination of stored/deflate,
  ZIP64 and data descriptors; archives from Info-ZIP `zip` and macOS
  `ditto`; CRC damage; refusals; an end-to-end flash; and 40,000 damaged
  zips in the mutation test.

---

## Phase 4: what the user is told (L + F)

### WO-22 · Images of unknown kind are written raw without notice
- **L:** no behavior change. `ImageKind::Unknown` is already public. Add a
  `doc` paragraph saying callers should warn.
- **F (GUI):** when `kind == Unknown`, the confirmation adds a warning:
  "This file has no partition table or ISO header. It may not be a disk
  image; the drive might not boot." The "Erase and flash" button needs a
  second click after that warning is shown.
- **F (CLI):** print the same warning. Without `--yes`, the user must type
  `yes`, as for any write.
- **Test:** a headless test with an unknown-kind image shows the warning in
  `Confirm`; a CLI test checks the warning text.
- **Status: done.** No extra click. The warning shows in the confirmation and
  the button reads "Erase and flash anyway", so overruling the warning is
  the confirming click itself. The wording lives in
  `flasher_cli::erase_warnings`, which the window and the terminal share.

### WO-23 · A sidecar SHA256SUMS detects damage, not tampering
- **L:**
  - `Published` gains `kind: Source { PerFile, List }` (non_exhaustive).
  - The `checksum` module docs state plainly that a checksum file from the
    same place as the image proves the download is intact, not that it is
    authentic.
- **F:** the success text depends on where the checksum came from:
  - A typed hash: "matched the SHA-256 you entered".
  - A sidecar file: "matched SHA256SUMS next to it (the download is intact)".
  - The tooltip or help text says that only a hash from the publisher's
    website, or a signature, guards against tampering.
- **Out of scope, recorded as a known gap in the README:** verifying
  signatures (`SHA256SUMS.gpg` / `.sig`). It needs an OpenPGP verifier and a
  trust store, which is its own project.
- **Test:** the headless test asserts the two wordings.
- **Status: done.** The enum is `checksum::SumFile` (`Source` read badly
  next to `Published::source`). Shared wording: `flasher_cli::checksum_matched`.
  The input's hint says how to know an image is genuine.

### WO-24 · Flasher: library work on the UI thread
- **Where:** `Flasher/crates/flasher/src/app.rs:349` (`extract::plan` in
  `load_image`), `:602` and `:709` (`refresh()` after jobs and on demand),
  and `App::with_poll` (the initial `refresh()`).
- **Fix:**
  - `load_image` sets a "Reading image…" state and runs `inspect` + `plan` +
    `find_published` on a worker. The result returns through the existing
    `tick()` channel pattern.
  - A newer `set_image` supersedes an older result: tag each result with a
    generation counter.
  - `refresh()` posts a request to the watcher thread, which already lists
    off-thread, instead of listing inline.
- **Test:** existing headless tests. Add one with a mock platform whose
  `list_devices` sleeps 2 s, and assert that `ui()` returns in < 100 ms.
- **Status: done, without a generation counter.** A newer choice replaces
  the receiver, so a stale result has nowhere to arrive. `App::pending()`
  keeps the host waking and the headless driver waiting. Tests: a 2 s
  listing leaves 10 frames under 500 ms, and the last image chosen wins.

### WO-25 · Flasher: confirmation too thin for large or busy drives
- **Where:** `app.rs`, `Stage::Confirm`; and the CLI `confirm`.
- **Fix:**
  - List `device.mountpoints` ("Volumes on it: /Volumes/Backup").
  - If `device.size > 64 GB`, add a warning in the warning color: "This is a
    large drive (2 TB). Make sure it is not a backup or data disk."
  - The CLI prints both lines too.
- **Test:** a headless confirm with a mock 2 TB drive that has mounts shows both lines.
- **Status: done.** Above 64 GB (`flasher_cli::LARGE_DRIVE`); volumes are
  listed. Tested in `flasher_cli` unit tests, not with a mock 2 TB drive,
  which Windows CI could not create as a sparse file.

---

## Phase 5: documentation and release

### WO-26 · README and docs
- Remove "never … a panic" only if WO-05 is not done; otherwise keep the
  claim and cite the mutation test.
- Update **Safety** (exclusive claim on macOS, Windows fails closed, Linux eMMC rule).
- Update **Known gaps**: add signature verification (WO-23) and the FAT32
  2 TiB cap (WO-20).
- Update **Images**: add `.zip`.
- Update the line and test counts.
- **Status: done.** libflasher README: images (zip), checksums, safety,
  faults, privilege, untrusted input, the mutation test, counts, and gaps
  (signatures, macOS 26 mount refusal, the FAT32 2 TiB cap). Flasher
  README: restored from escaped text (every line had been wrapped in
  backticks); the screenshot and comma edits kept; the image path fixed to
  `miscellaneous/img.png`; Phase 4's changes added.

### WO-27 · Ship to Flasher
1. libflasher: all phases merged, CI green on macOS, Linux and Windows
   (conformance, wim-split, extract-boots).
2. Flasher: `scripts/pin-libflasher.sh <sha>`, then `scripts/check-lock.sh`.
3. Flasher: WO-22 to WO-25 UI work, against the new pin.
4. Flasher: bump to 0.1.3, run the release workflow, and smoke-test the three
   packages. On Linux, also test the AppImage helper prompt path.
5. Real hardware before announcing, at minimum:
   - macOS: a raw write and a Windows-ISO extract, with Finder kept from
     remounting during the write (WO-10).
   - Linux: a laptop with an SD slot, where the eMMC and the empty slot are
     not listed (WO-07).
   - Windows: one USB stick, written and verified (WO-11, WO-17).
- **Status: steps 1–4 done.** libflasher `0a9cea8` passed CI on every job.
  Flasher `50ef964` is pinned to it, carries WO-22 to WO-25 (`b881b13`), is
  versioned 0.1.3, passed CI on macOS, Linux and Windows, and the release
  workflow built the packages. A race in one headless test, found by the
  weekly run against libflasher `main`, was fixed in Flasher `b47ab0f`.
  **Open: step 5, the hardware checklist below** — no CI can stand in for it.
- **Hardware results so far** (macOS 27.0.1, Apple silicon, Flasher `f840cd9`
  and later; October 2026):
  - Raw writes verified: a Raspberry Pi image and an Ubuntu 26.04 ISO.
  - Windows 10 22H2 ISO extracted (906 files, 6.1 GB, `install.wim` in two
    `.swm` parts) and verified. On an MSI desktop it booted as
    "UEFI: … Partition 1" into Windows Setup, which listed all editions
    (Home … Pro N), so Setup read the split set. Not installed further.
  - Found on the way, and fixed: on macOS 26+ an exFAT/FAT volume mounted by
    FSKit holds the disk, so opening it after the password prompt failed
    with EBUSY ("permission denied"). The backend now authorizes, then
    unmounts and claims, then opens through `authopen -extauth`
    (libflasher `105c185`; CI's macOS disk is now a mounted exFAT image).
  - Found on the way, and fixed: the window redrew at the display rate while
    a job ran and never let wgpu reclaim finished frames, so graphics memory
    grew by about 600 MB/s (29 GB in 90 s) and the whole Mac stalled. It now
    polls the device every frame and redraws at 10 fps during jobs; a full
    6.5 GB flash then held 80–260 MB (Flasher, after `f840cd9`).
- **Hardware checklist** (before announcing):
  - **macOS (27 and, if at hand, 26)**:
    - Flash a Raspberry Pi `.img.xz`, then a `.zip` of an image.
    - ~~Extract a Windows ISO.~~ Done: boots into Setup, editions listed.
    - Mid-write, click the stick's volume in Disk Utility → Mount: it must
      be refused.
    - Cancel the password prompt once: the volumes stay mounted.
  - **Linux laptop with an SD slot**:
    - With the slot empty, the internal eMMC (if any) and the slot are not
      listed.
    - Insert a card: it appears as SD.
    - Flash via the AppImage (pkexec prompt).
    - A LUKS-formatted stick is refused naming its `/dev/mapper` volume.
    - Kill Flasher mid-write: `systemd-inhibit --list` no longer shows it.
  - **Windows 11**:
    - Flash and verify one USB stick.
    - Open the stick in Explorer just before flashing: the lock retries,
      then succeeds or names the letter.
    - Restore it to exFAT.

---

## Order and estimates

| Phase | Items | Repo | Size |
|---|---|---|---|
| 0 | WO-01 … 05 | L | S, S, M, S, M |
| 1 | WO-06 … 11 | L | M, M, M, M, L, M |
| 2 | WO-12 … 15 | L | S, S, S, S |
| 3 | WO-16 … 21 | L | S, S, S, S, S, L |
| 4 | WO-22 … 25 | L+F | S, S, M, S |
| 5 | WO-26, 27 | L+F | S, M |

S ≈ under 1 hour, M ≈ half a day, L ≈ a day or more.

**Dependencies:**
- Phase 0 first: WO-05's mutation test is a safety net for everything after it.
- WO-21 (zip) joins WO-05's mutation test.
- WO-10 and WO-12 touch the same file: do WO-12 first.
- WO-22 to WO-25 can run in parallel with Phases 1–3 against a local
  libflasher, but ship only after WO-27 step 1.

---

## Traceability

| Finding | Source | WO |
|---|---|---|
| ISO panic, empty root | reviewer B | 01 |
| UDF short partition map | reviewer B | 02 |
| Shared subdirectory not tested / unbounded reads | reviewer B (note) | 03 |
| WIM lookup table unbounded | both | 04 |
| README "never a panic" claim | reviewer B | 05, 26 |
| Windows system disk fails open | both | 06 |
| Windows verify may hit cache | reviewer B | 11 |
| Windows unaligned buffers | review A | 11 |
| Linux open-by-path race | reviewer B | 08 |
| Linux eMMC / boot0 / rpmb / size-0 listed | both | 07 |
| Linux `keep_awake` leak | reviewer B | 14 |
| Linux LUKS/LVM/by-uuid mounts not released | review A | 09 |
| macOS authopen fd not CLOEXEC | both | 12 |
| macOS listing aborts on one disk | both | 16 |
| macOS no exclusive claim | reviewer B | 10 |
| macOS unmount before password prompt | review A | 10 |
| Label leading `-` | reviewer B | 15 |
| Windows diskpart temp-file script | review A | 13 |
| Windows volume lock, no retry | review A | 17 |
| BlockIo sync loses "drive gone" | review A | 18 |
| Extract verify ignores file sizes | review A | 19 |
| FAT32 > 2 TiB | reviewer B | 20 |
| `.zip` unsupported | reviewer B | 21 |
| Unknown kind written raw | reviewer B | 22 |
| SHA256SUMS ≠ tamper protection | reviewer B | 23 |
| Flasher UI-thread blocking | review A | 24 |
| Flasher confirmation for large/busy drives | review A | 25 |
