//! Byte-addressed I/O over a window of a raw device, which only takes whole
//! sectors.
//!
//! Filesystem code (`fatfs`) reads and writes a few bytes at a time; raw
//! disks on macOS and Windows refuse anything not sector-aligned. This sits
//! between them: a cache of blocks, each remembering which of its sectors
//! hold real data and which have been changed. Writing a whole sector never
//! reads it first, so copying a file sequentially costs one write per byte,
//! not a read and a write.
//!
//! It also keeps the safety rules the plain image writer follows: every
//! device request is timed against a stall limit, and the drive is flushed
//! every so often so it never holds a large backlog.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::time::{Duration, Instant};

use crate::RawDevice;

/// Sectors per cached block.
const BLOCK_SECTORS: u64 = 128;
/// Blocks kept before the oldest is written out.
const MAX_BLOCKS: usize = 256;

struct Block {
    data: Vec<u8>,
    valid: Vec<bool>,
    dirty: Vec<bool>,
}

/// A byte-addressed window `[start, start + len)` of a device.
pub(crate) struct BlockIo<'a> {
    dev: &'a mut dyn RawDevice,
    start: u64,
    len: u64,
    sector: u64,
    pos: u64,
    blocks: HashMap<u64, Block>,
    order: VecDeque<u64>,
    stall: Duration,
    sync_every: u64,
    since_sync: u64,
}

impl<'a> BlockIo<'a> {
    /// `start` and `len` must be multiples of the device's sector size.
    pub fn new(
        dev: &'a mut dyn RawDevice,
        start: u64,
        len: u64,
        stall: Duration,
        sync_every: u64,
    ) -> Self {
        let sector = dev.sector_size().max(512) as u64;
        debug_assert!(start.is_multiple_of(sector) && len.is_multiple_of(sector));
        Self {
            dev,
            start,
            len,
            sector,
            pos: 0,
            blocks: HashMap::new(),
            order: VecDeque::new(),
            stall,
            sync_every,
            since_sync: 0,
        }
    }

    fn block_bytes(&self) -> u64 {
        self.sector * BLOCK_SECTORS
    }

    /// Run one device request, failing with `TimedOut` if it took too long.
    fn timed<T>(&mut self, op: impl FnOnce(&mut dyn RawDevice) -> io::Result<T>) -> io::Result<T> {
        let t = Instant::now();
        let r = op(&mut *self.dev)?;
        if t.elapsed() > self.stall {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{:.0} s for one request", t.elapsed().as_secs_f32()),
            ));
        }
        Ok(r)
    }

    fn block(&mut self, idx: u64) -> io::Result<&mut Block> {
        if !self.blocks.contains_key(&idx) {
            if self.blocks.len() >= MAX_BLOCKS {
                if let Some(old) = self.order.pop_front() {
                    self.write_back(old)?;
                    self.blocks.remove(&old);
                }
            }
            let n = BLOCK_SECTORS as usize;
            self.blocks.insert(
                idx,
                Block {
                    data: vec![0; self.block_bytes() as usize],
                    valid: vec![false; n],
                    dirty: vec![false; n],
                },
            );
            self.order.push_back(idx);
        }
        Ok(self.blocks.get_mut(&idx).expect("just inserted"))
    }

    /// Read the sectors `[from, to)` of block `idx` that are not yet valid.
    fn fill(&mut self, idx: u64, from: usize, to: usize) -> io::Result<()> {
        let sector = self.sector as usize;
        let base = self.start + idx * self.block_bytes();
        let mut s = from;
        while s < to {
            if self.block(idx)?.valid[s] {
                s += 1;
                continue;
            }
            let mut e = s;
            while e < to && !self.block(idx)?.valid[e] {
                e += 1;
            }
            let mut tmp = vec![0u8; (e - s) * sector];
            let at = base + (s * sector) as u64;
            self.timed(|d| {
                d.seek(SeekFrom::Start(at))?;
                d.read_exact(&mut tmp)
            })?;
            let b = self.block(idx)?;
            b.data[s * sector..e * sector].copy_from_slice(&tmp);
            b.valid[s..e].iter_mut().for_each(|v| *v = true);
            s = e;
        }
        Ok(())
    }

    /// Write a block's changed sectors, in runs.
    fn write_back(&mut self, idx: u64) -> io::Result<()> {
        let Some(b) = self.blocks.get(&idx) else {
            return Ok(());
        };
        let sector = self.sector as usize;
        let base = self.start + idx * self.block_bytes();
        let mut runs = Vec::new();
        let mut s = 0;
        while s < b.dirty.len() {
            if !b.dirty[s] {
                s += 1;
                continue;
            }
            let mut e = s;
            while e < b.dirty.len() && b.dirty[e] {
                e += 1;
            }
            runs.push((s, e, b.data[s * sector..e * sector].to_vec()));
            s = e;
        }
        let mut written = 0u64;
        for (s, _, data) in &runs {
            let at = base + (*s * sector) as u64;
            self.timed(|d| {
                d.seek(SeekFrom::Start(at))?;
                d.write_all(data)
            })?;
            written += data.len() as u64;
        }
        if let Some(b) = self.blocks.get_mut(&idx) {
            b.dirty.iter_mut().for_each(|d| *d = false);
        }
        self.since_sync += written;
        if self.since_sync >= self.sync_every {
            self.since_sync = 0;
            self.timed(|d| d.sync().map_err(io::Error::other))?;
        }
        Ok(())
    }

    /// Write everything changed and flush the drive.
    pub fn sync(&mut self) -> io::Result<()> {
        let mut idx: Vec<u64> = self.blocks.keys().copied().collect();
        idx.sort_unstable();
        for i in idx {
            self.write_back(i)?;
        }
        self.since_sync = 0;
        self.timed(|d| d.sync().map_err(io::Error::other))
    }

    /// Forget everything cached (after `sync`), so later reads come from
    /// the device: what verification needs.
    pub fn drop_cache(&mut self) {
        self.blocks.clear();
        self.order.clear();
    }
}

impl Read for BlockIo<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let bb = self.block_bytes();
        let (idx, off) = (self.pos / bb, (self.pos % bb) as usize);
        let n = buf
            .len()
            .min(bb as usize - off)
            .min((self.len - self.pos) as usize);
        let sector = self.sector as usize;
        self.fill(idx, off / sector, (off + n).div_ceil(sector))?;
        buf[..n].copy_from_slice(&self.block(idx)?.data[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for BlockIo<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.pos >= self.len {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "past the end of the partition",
            ));
        }
        let bb = self.block_bytes();
        let (idx, off) = (self.pos / bb, (self.pos % bb) as usize);
        let n = buf
            .len()
            .min(bb as usize - off)
            .min((self.len - self.pos) as usize);
        let sector = self.sector as usize;
        let (first, last) = (off / sector, (off + n).div_ceil(sector));
        // Only partly-covered end sectors need their old contents.
        if off % sector != 0 {
            self.fill(idx, first, first + 1)?;
        }
        if !(off + n).is_multiple_of(sector) {
            self.fill(idx, last - 1, last)?;
        }
        let b = self.block(idx)?;
        b.data[off..off + n].copy_from_slice(&buf[..n]);
        b.valid[first..last].iter_mut().for_each(|v| *v = true);
        b.dirty[first..last].iter_mut().for_each(|v| *v = true);
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(()) // `sync` is the real flush; fatfs calls this often.
    }
}

impl Seek for BlockIo<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let p = match to {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::End(d) => self.len as i128 + d as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        if p < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start",
            ));
        }
        self.pos = p as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::FileDevice;

    fn device(len: usize) -> (std::path::PathBuf, FileDevice) {
        let p =
            std::env::temp_dir().join(format!("libflasher_blockio_{}_{len}", std::process::id()));
        std::fs::write(&p, vec![0x5Au8; len]).unwrap();
        let d = FileDevice::open(&p).unwrap(); // rejects unaligned writes, like a raw disk
        (p, d)
    }

    #[test]
    fn unaligned_io_round_trips_through_an_aligned_device() {
        let (path, mut dev) = device(4 << 20);
        let mut io = BlockIo::new(&mut dev, 1 << 20, 2 << 20, Duration::from_secs(20), 1 << 20);
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7) as u8).collect();
        io.seek(SeekFrom::Start(1234)).unwrap();
        io.write_all(&data).unwrap();
        io.sync().unwrap();
        io.drop_cache();
        let mut back = vec![0u8; data.len() + 20];
        io.seek(SeekFrom::Start(1224)).unwrap();
        io.read_exact(&mut back).unwrap();
        assert_eq!(&back[..10], &[0x5A; 10], "bytes before the write kept");
        assert_eq!(&back[10..10 + data.len()], &data[..]);
        assert_eq!(
            &back[10 + data.len()..],
            &[0x5A; 10],
            "bytes after the write kept"
        );
        drop(io);
        // Nothing outside the window was touched.
        let raw = std::fs::read(&path).unwrap();
        assert!(raw[..1 << 20].iter().all(|&b| b == 0x5A));
        assert!(raw[3 << 20..].iter().all(|&b| b == 0x5A));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn stays_inside_its_window() {
        let (path, mut dev) = device(2 << 20);
        let mut io = BlockIo::new(&mut dev, 0, 1 << 20, Duration::from_secs(20), 1 << 20);
        io.seek(SeekFrom::Start((1 << 20) - 2)).unwrap();
        assert_eq!(io.write(&[1, 2, 3, 4]).unwrap(), 2);
        assert!(io.write(&[5]).is_err());
        std::fs::remove_file(path).ok();
    }
}
