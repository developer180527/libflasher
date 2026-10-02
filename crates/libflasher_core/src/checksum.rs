//! Checking an image against the SHA-256 its publisher lists, before writing.
//!
//! The hash is of the file as downloaded (the `.img.xz`, the `.iso`), which is
//! what distributions publish. [`find_published`] picks it up from the files
//! publishers put next to their images — `SHA256SUMS`, `<name>.sha256` — so
//! the check can happen without anyone pasting anything.

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::{Digest, Sha256};

use crate::{Error, ImageInfo, Progress, Result};

/// `expected` in the form checks compare: 64 lowercase hex digits, with any
/// surrounding space or a `sha256:` prefix removed. `None` if it is not one.
pub fn normalize(expected: &str) -> Option<String> {
    let s = expected.trim();
    let s = s
        .strip_prefix("sha256:")
        .or_else(|| s.strip_prefix("SHA256:"))
        .unwrap_or(s)
        .trim();
    (s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| s.to_ascii_lowercase())
}

/// Hash the image file, reporting [`Progress::Checking`]; fail with
/// [`Error::ChecksumMismatch`] if it is not `expected`.
pub fn verify_image(
    image: &ImageInfo,
    expected: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    let Some(expected) = normalize(expected) else {
        return Err(Error::ChecksumMismatch {
            expected: expected.trim().into(),
            actual: "(not a SHA-256: need 64 hex digits)".into(),
        });
    };
    let actual = sha256_file(&image.path, image.file_size, cancel, progress)?;
    if actual != expected {
        return Err(Error::ChecksumMismatch { expected, actual });
    }
    Ok(())
}

/// Hash a file, reporting [`Progress::Checking`] against `total` bytes.
pub fn sha256_file(
    path: &Path,
    total: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<String> {
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        progress(Progress::Checking { done, total });
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Files publishers put next to images, in the order worth trusting:
/// a per-file one before a list of many.
const CANDIDATES: &[&str] = &[
    "{name}.sha256",
    "{name}.sha256sum",
    "{name}.sha256.txt",
    "SHA256SUMS",
    "SHA256SUMS.txt",
    "sha256sum.txt",
    "sha256sums.txt",
];

/// Where a published checksum was found, and what it says.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Published {
    /// The SHA-256, as 64 lowercase hex digits.
    pub sha256: String,
    /// The file it came from, for telling the user.
    pub source: String,
}

/// Look next to `image` for a published SHA-256 of it.
///
/// Understands the formats in use: `sha256sum` output (`<hash>  <name>`,
/// `<hash> *<name>`), BSD style (`SHA256 (<name>) = <hash>`), and a file
/// holding only the hash. A list must name this exact file; a lone hash is
/// accepted only from a file named after the image.
pub fn find_published(image: &Path) -> Option<Published> {
    let dir = image.parent()?;
    let name = image.file_name()?.to_str()?;
    for pattern in CANDIDATES {
        let candidate = pattern.replace("{name}", name);
        let per_file = pattern.contains("{name}");
        let path = dir.join(&candidate);
        // Checksum files are small; anything large is not one.
        if fs::metadata(&path)
            .map(|m| m.len() > 1 << 20)
            .unwrap_or(true)
        {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(sha256) = parse(&text, name, per_file) {
            return Some(Published {
                sha256,
                source: candidate,
            });
        }
    }
    None
}

fn parse(text: &str, name: &str, lone_hash_ok: bool) -> Option<String> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    for line in &lines {
        // BSD: SHA256 (name) = hash
        if let Some(rest) = line.strip_prefix("SHA256 (") {
            if let Some((file, hash)) = rest.split_once(") = ") {
                if file == name {
                    return normalize(hash);
                }
            }
            continue;
        }
        // GNU: hash  name   or   hash *name
        let mut parts = line.splitn(2, char::is_whitespace);
        let (Some(hash), Some(file)) = (parts.next(), parts.next()) else {
            continue;
        };
        let file = file.trim_start().trim_start_matches('*');
        if file == name || Path::new(file).file_name().and_then(|f| f.to_str()) == Some(name) {
            if let Some(h) = normalize(hash) {
                return Some(h);
            }
        }
    }
    if lone_hash_ok && lines.len() == 1 {
        return normalize(lines[0]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "61d95799550aac32788bb3cacc3d471dcc860f8053ce989dec4aecc388b799dd";
    const OTHER: &str = "601e30fbf5d97759367c632e2c33630665039b7e2158fd068403da3ccf1bda1f";

    #[test]
    fn normalizes() {
        assert_eq!(
            normalize(&format!("  sha256:{}\n", H.to_uppercase())).as_deref(),
            Some(H)
        );
        assert_eq!(normalize("abc"), None);
        assert_eq!(normalize(&H.replace('d', "z")), None);
    }

    #[test]
    fn parses_the_formats_in_use() {
        let gnu = format!("{OTHER}  other.iso\n{H} *pi.img.xz\n");
        assert_eq!(parse(&gnu, "pi.img.xz", false).as_deref(), Some(H));
        let bsd = format!("SHA256 (other.iso) = {OTHER}\nSHA256 (pi.img.xz) = {H}\n");
        assert_eq!(parse(&bsd, "pi.img.xz", false).as_deref(), Some(H));
        let pathy = format!("{H}  ./images/pi.img.xz\n");
        assert_eq!(parse(&pathy, "pi.img.xz", false).as_deref(), Some(H));
        // A list that does not name the file says nothing about it.
        assert_eq!(
            parse(&format!("{OTHER}  other.iso\n"), "pi.img.xz", false),
            None
        );
        // A lone hash counts only from a file named after the image.
        assert_eq!(parse(H, "pi.img.xz", true).as_deref(), Some(H));
        assert_eq!(parse(H, "pi.img.xz", false), None);
    }

    #[test]
    fn finds_and_checks_a_published_hash() {
        let dir = std::env::temp_dir().join(format!("flasher_sum_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let img = dir.join("pi.img");
        fs::write(&img, b"hello").unwrap();
        // sha256("hello")
        let hello = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        fs::write(
            dir.join("SHA256SUMS"),
            format!("{OTHER}  x.iso\n{hello}  pi.img\n"),
        )
        .unwrap();

        let found = find_published(&img).unwrap();
        assert_eq!(
            found,
            Published {
                sha256: hello.into(),
                source: "SHA256SUMS".into()
            }
        );

        let info = crate::image::inspect(&img).unwrap();
        let no = AtomicBool::new(false);
        verify_image(&info, hello, &no, &mut |_| {}).unwrap();
        let bad = verify_image(&info, OTHER, &no, &mut |_| {});
        assert!(
            matches!(bad, Err(Error::ChecksumMismatch { .. })),
            "{bad:?}"
        );
        fs::remove_dir_all(dir).ok();
    }
}
