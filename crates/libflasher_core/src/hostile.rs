//! The promise that a malformed image is an error, never a panic or a hang,
//! checked by brute force: well-formed ISO 9660, Rock Ridge, UDF and WIM
//! images, damaged tens of thousands of seeded ways, each read as extract mode
//! would read it.
//!
//! Deterministic (a fixed-seed generator), so a failure names a seed that
//! reproduces it. Directed tests for each bound found this way live next to
//! the code they test.

use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

use crate::iso9660::build::{iso, File};
use crate::iso9660::Iso;
use crate::udf::{build, Udf};

const SEEDS: u64 = 20_000;
/// One damaged image must be dealt with in this long, or it counts as a hang.
const LIMIT: Duration = Duration::from_secs(2);
/// Files larger than this are not read back: a damaged size field can claim
/// gigabytes of holes, which is slow, not wrong.
const READ_LIMIT: u64 = 1 << 20;

/// xorshift64*: small, deterministic, good enough to pick bytes.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Damage `img` one to four ways. Half the edits land in `hot`, the byte
/// ranges where the structures are, so few are wasted on file contents.
fn mutate(img: &[u8], hot: &[std::ops::Range<usize>], rng: &mut Rng) -> Vec<u8> {
    let mut m = img.to_vec();
    for _ in 0..1 + rng.below(4) {
        if m.is_empty() {
            break;
        }
        let pos = if rng.below(2) == 0 && !hot.is_empty() {
            let r = &hot[rng.below(hot.len())];
            (r.start + rng.below(r.len())).min(m.len() - 1)
        } else {
            rng.below(m.len())
        };
        match rng.below(6) {
            0 => m[pos] ^= 1 << rng.below(8),
            1 => m[pos] = [0, 1, 0x7F, 0x80, 0xFF][rng.below(5)],
            2 => {
                let v: u32 = match rng.below(4) {
                    0 => rng.below(8) as u32,
                    1 => [0x7FFF_FFFF, u32::MAX][rng.below(2)],
                    _ => rng.next() as u32,
                };
                let at = pos & !3;
                let end = (at + 4).min(m.len());
                m[at..end].copy_from_slice(&v.to_le_bytes()[..end - at]);
            }
            3 => m.truncate(rng.below(m.len())),
            4 => {
                // Copy a span over another: a record or descriptor pointing
                // where another one does.
                let from = rng.below(m.len());
                let len = (1 + rng.below(32)).min(m.len() - from).min(m.len() - pos);
                let span = m[from..from + len].to_vec();
                m[pos..pos + len].copy_from_slice(&span);
            }
            _ => m[pos] = rng.next() as u8,
        }
    }
    m
}

fn read_iso(img: Vec<u8>) {
    let Ok(mut iso) = Iso::open(Cursor::new(img)) else {
        return;
    };
    let Ok(entries) = iso.walk() else { return };
    let mut buf = vec![0u8; 1 << 16];
    for e in entries.iter().filter(|e| !e.is_dir && e.size <= READ_LIMIT) {
        let _ = iso.read_file(e, &mut buf, |_| Ok(()));
    }
}

fn read_udf(img: Vec<u8>) {
    let Ok(mut udf) = Udf::open(Cursor::new(img)) else {
        return;
    };
    let Ok(entries) = udf.walk() else { return };
    let mut buf = vec![0u8; 1 << 16];
    for e in entries.iter().filter(|e| !e.is_dir && e.size <= READ_LIMIT) {
        let _ = udf.read_file(e, &mut buf, |_| Ok(()));
    }
}

fn read_wim(img: Vec<u8>) {
    let len = img.len() as u64;
    if let Ok(parts) = crate::wim::split(&mut &img[..], len, 4000, "install") {
        let mut buf = vec![0u8; 4096];
        for p in parts {
            let _ = p.stream(&mut &img[..], &mut buf, &mut |_| Ok(()));
        }
    }
}

#[cfg(feature = "zip")]
fn read_zip(img: Vec<u8>) {
    let mut c = Cursor::new(img);
    let Ok(entry) = crate::zip::find(&mut c) else {
        return;
    };
    if let Ok(r) = crate::zip::reader(c, &entry) {
        // A damaged size can claim gigabytes; the bytes that exist are few.
        let _ = std::io::copy(
            &mut std::io::Read::take(r, READ_LIMIT),
            &mut std::io::sink(),
        );
    }
}

/// Run `read` on `SEEDS` damaged copies of `img`; describe every panic or hang.
fn torture(
    name: &str,
    img: &[u8],
    hot: &[std::ops::Range<usize>],
    read: fn(Vec<u8>),
) -> Vec<String> {
    let mut failures = Vec::new();
    for seed in 0..SEEDS {
        let m = mutate(img, hot, &mut Rng::new(seed));
        let start = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(|| read(m)));
        let took = start.elapsed();
        if let Err(e) = outcome {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            failures.push(format!("{name} seed {seed}: panic: {msg}"));
        } else if took > LIMIT {
            failures.push(format!("{name} seed {seed}: took {took:?}"));
        }
    }
    failures
}

fn sectors(from: usize, to: usize) -> std::ops::Range<usize> {
    from * 2048..to * 2048
}

#[test]
fn damaged_images_are_errors_never_panics_or_hangs() {
    let files = vec![
        File {
            path: "EFI/BOOT/BOOTX64.EFI",
            data: vec![0xEF; 3000],
        },
        File {
            path: "boot/grub/grub.cfg",
            data: b"menuentry 'x' {}\n".to_vec(),
        },
        File {
            path: "a/b/c/deep.txt",
            data: b"deep".to_vec(),
        },
    ];
    let udf_nodes = {
        use build::Node::*;
        vec![
            Dir(vec![("EFI", 1), ("setup.exe", 3), ("sources", 4)]),
            Dir(vec![("BOOT", 2)]),
            Dir(vec![("BOOTX64.EFI", 5)]),
            File(b"MZ".to_vec()),
            Dir(vec![]),
            File(vec![0xEF; 1500]),
        ]
    };
    // Volume descriptors, then the directories the builders put after them.
    // The descriptors' numeric fields get ranges of their own: a length or
    // count of exactly the wrong small value is what breaks a parser.
    let pvd_root = 16 * 2048 + 150..16 * 2048 + 190;
    let lvd = 33 * 2048;
    let iso_hot = [sectors(16, 18), sectors(18, 26), pvd_root];
    let udf_hot = [
        sectors(16, 19),
        sectors(32, 35),
        sectors(256, 257),
        sectors(300, 313),
        lvd + 208..lvd + 272, // block size, file set, map length
        lvd + 440..lvd + 448, // partition map 0
    ];

    let wim = crate::wim::tests::wim(2, &[1000, 1500, 800]);
    let wim_hot = [0..208, wim.len() - 400..wim.len()];

    let mut failures = Vec::new();
    failures.extend(torture("iso", &iso("T", &files, false), &iso_hot, read_iso));
    failures.extend(torture(
        "rock ridge",
        &iso("T", &files, true),
        &iso_hot,
        read_iso,
    ));
    failures.extend(torture(
        "udf",
        &build::udf(&udf_nodes, 6),
        &udf_hot,
        read_udf,
    ));
    failures.extend(torture("wim", &wim, &wim_hot, read_wim));
    #[cfg(feature = "zip")]
    {
        use crate::zip::build::{zip, Options};
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 97) as u8).collect();
        for (deflate, zip64) in [(false, false), (true, true)] {
            let o = Options {
                deflate,
                zip64,
                descriptor: false,
            };
            let z = zip(&[("a/", b""), ("readme", b"hi"), ("disk.img", &data)], &o);
            // The headers: the local ones up front, the central directory
            // and end records at the back.
            let hot = [0..64, z.len().saturating_sub(400)..z.len()];
            failures.extend(torture("zip", &z, &hot, read_zip));
        }
    }
    assert!(
        failures.is_empty(),
        "{} damaged images failed:\n{}",
        failures.len(),
        failures
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
