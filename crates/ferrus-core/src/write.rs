//! Raw image → device write engine (DD mode).

use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

pub const CHUNK_SIZE: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteProgress {
    pub bytes_done: u64,
    pub total: u64,
    pub verifying: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Completed,
    Cancelled,
}

/// Fill `buf` completely unless EOF arrives first; returns bytes read.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Write `src_path` byte-for-byte to the already-open `dst`, optionally
/// verifying by reading back and comparing. Reports progress after every
/// chunk and honours `cancel` between chunks.
pub fn write_image_to_device(
    src_path: &Path,
    dst: &mut File,
    verify: bool,
    progress: &dyn Fn(WriteProgress),
    cancel: &AtomicBool,
) -> anyhow::Result<WriteOutcome> {
    let total = std::fs::metadata(src_path)?.len();
    let mut src = BufReader::with_capacity(CHUNK_SIZE, File::open(src_path)?);

    dst.seek(SeekFrom::Start(0))?;
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut done: u64 = 0;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(WriteOutcome::Cancelled);
        }
        let n = read_full(&mut src, &mut buf)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])?;
        done += n as u64;
        progress(WriteProgress {
            bytes_done: done,
            total,
            verifying: false,
        });
    }
    dst.flush()?;
    dst.sync_all()?;

    if !verify {
        return Ok(WriteOutcome::Completed);
    }

    match verify_image_on_device(src_path, dst, progress, cancel)? {
        WriteOutcome::Completed => Ok(WriteOutcome::Completed),
        WriteOutcome::Cancelled => Ok(WriteOutcome::Cancelled),
    }
}

/// Verify that an already-open `dst` contains exactly the bytes of
/// `src_path`, comparing in lockstep chunks.
pub fn verify_image_on_device(
    src_path: &Path,
    dst: &mut File,
    progress: &dyn Fn(WriteProgress),
    cancel: &AtomicBool,
) -> anyhow::Result<WriteOutcome> {
    let total = std::fs::metadata(src_path)?.len();
    let mut vsrc = BufReader::with_capacity(CHUNK_SIZE, File::open(src_path)?);
    dst.seek(SeekFrom::Start(0))?;
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut vbuf = vec![0u8; CHUNK_SIZE];
    let mut vdone: u64 = 0;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(WriteOutcome::Cancelled);
        }
        let ns = read_full(&mut vsrc, &mut buf)?;
        // Source exhausted: done. Trailing bytes on the (necessarily larger)
        // block device are ignored — they are not part of the image.
        if ns == 0 {
            break;
        }
        let nd = read_full(dst, &mut vbuf)?;
        if nd < ns {
            anyhow::bail!(
                "destination shorter than image: missing {} bytes at offset {}",
                ns - nd,
                vdone
            );
        }
        if buf[..ns] != vbuf[..ns] {
            let mut pos = 0usize;
            while pos < ns && buf[pos] == vbuf[pos] {
                pos += 1;
            }
            anyhow::bail!("verification mismatch at offset {}", vdone + pos as u64);
        }
        vdone += ns as u64;
        progress(WriteProgress {
            bytes_done: vdone,
            total,
            verifying: true,
        });
    }

    Ok(WriteOutcome::Completed)
}

/// Convenience for tests/tools targeting regular files instead of block devices.
pub fn open_target(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lcg(u64);
    impl Lcg {
        fn next_u8(&mut self) -> u8 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as u8
        }
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "ferrus-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ))
    }

    fn make_source(len: usize) -> std::path::PathBuf {
        let path = temp_path("src");
        let mut lcg = Lcg(42);
        let data: Vec<u8> = (0..len).map(|_| lcg.next_u8()).collect();
        std::fs::write(&path, &data).unwrap();
        path
    }

    #[test]
    fn write_and_verify_roundtrip() {
        const LEN: usize = CHUNK_SIZE * 2 + 12345;
        let src = make_source(LEN);
        let dstp = temp_path("dst");
        std::fs::write(&dstp, vec![0u8; LEN]).unwrap();
        let mut dst = open_target(&dstp).unwrap();

        let outcome = write_image_to_device(&src, &mut dst, true, &|_| {}, &AtomicBool::new(false))
            .expect("engine failed");

        assert_eq!(outcome, WriteOutcome::Completed);
        assert_eq!(
            std::fs::read(&src).unwrap(),
            std::fs::read(&dstp).unwrap()
        );
        std::fs::remove_file(&src).ok();
        std::fs::remove_file(&dstp).ok();
    }

    #[test]
    fn cancel_before_start_yields_cancelled() {
        const LEN: usize = 1 << 20;
        let src = make_source(LEN);
        let dstp = temp_path("dst2");
        std::fs::write(&dstp, vec![0u8; LEN]).unwrap();
        let mut dst = open_target(&dstp).unwrap();

        let flag = AtomicBool::new(true);
        let outcome = write_image_to_device(&src, &mut dst, false, &|_| {}, &flag).unwrap();
        assert_eq!(outcome, WriteOutcome::Cancelled);

        std::fs::remove_file(&src).ok();
        std::fs::remove_file(&dstp).ok();
    }

    #[test]
    fn destination_larger_than_image_verifies_ok() {
        const LEN: usize = CHUNK_SIZE + (1 << 20);
        let src = make_source(LEN);
        let dstp = temp_path("dst4");
        std::fs::write(&dstp, vec![0u8; LEN + 32 * 1024 * 1024]).unwrap();
        let mut dst = open_target(&dstp).unwrap();

        write_image_to_device(&src, &mut dst, true, &|_| {}, &AtomicBool::new(false))
            .expect("write+verify on larger target failed");

        std::fs::remove_file(&src).ok();
        std::fs::remove_file(&dstp).ok();
    }

    #[test]
    fn corrupted_destination_fails_verify() {
        const LEN: usize = 3 * 1024 * 1024;
        let src = make_source(LEN);
        let dstp = temp_path("dst3");
        std::fs::write(&dstp, vec![0u8; LEN]).unwrap();

        {
            let mut dst = open_target(&dstp).unwrap();
            write_image_to_device(&src, &mut dst, false, &|_| {}, &AtomicBool::new(false)).unwrap();
        }
        let mut data = std::fs::read(&dstp).unwrap();
        data[CHUNK_SIZE / 2] ^= 0xff;
        std::fs::write(&dstp, &data).unwrap();

        let mut dst = open_target(&dstp).unwrap();
        let err = verify_image_on_device(&src, &mut dst, &|_| {}, &AtomicBool::new(false));
        assert!(err.is_err(), "verify must fail on corruption");

        std::fs::remove_file(&src).ok();
        std::fs::remove_file(&dstp).ok();
    }
}
