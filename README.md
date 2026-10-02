# libflasher

Write disk images to USB drives and SD cards, from Rust, on macOS, Linux and
Windows. The engine behind [Flasher](https://github.com/developer180527/Flasher).

- Detects what an image is (disk image, hybrid ISO, non-hybrid ISO) and how it
  is compressed (gzip, xz, zstd, bzip2), and reads an `.xz`'s exact size
  without decompressing it.
- Checks the image against its published SHA-256, found automatically in a
  `SHA256SUMS` or `<image>.sha256` next to it.
- Writes in 1 MiB requests with periodic flushes, gives up on a stalled drive
  instead of feeding it more, names an unplugged drive as such, and re-checks
  that a drive picked from a list is still the same disk before touching it.
- Reads everything back to verify, ejects, keeps the computer awake, and can
  restore a flashed drive to an ordinary exFAT drive.

```toml
[dependencies]
libflasher = "0.1"
```

See the crate docs for a complete example; it runs as a test against
file-backed mock drives, so it needs no hardware.

| Crate | What it is |
|---|---|
| `libflasher` | The one to depend on: everything below, re-exported |
| `libflasher_core` | OS-independent: images, checksums, write/verify, the `Platform` trait, mock drives |
| `libflasher_platform` | Picks the backend for the OS you build for |
| `libflasher_macos` / `_linux` / `_windows` | The backends |

Writing a raw disk needs administrator rights: an `authopen` password prompt
on macOS, root on Linux, an elevated process on Windows.

## Licence

MIT. libflasher contains no GPL code. Bootloaders for extract mode, which
are GPL, will come as a separate, optional package.
