//! Core copy/verify logic, generic over Read/Write so it's testable without root.

use crate::image::{ImageInfo, ImageKind};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub const CHUNK: usize = 4 * 1024 * 1024;
pub const SECTOR: usize = 512;

#[derive(Debug, thiserror::Error)]
pub enum FlashError {
    #[error("Cancelled")]
    Cancelled,
    #[error("Image is larger than the disk")]
    DiskFull,
    #[error("Verification failed: the data read back doesn't match the image")]
    Mismatch,
    #[error("{0}: {1}")]
    Io(&'static str, io::Error),
}

pub type Result<T> = std::result::Result<T, FlashError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyResult {
    /// Image bytes written, without the zero padding.
    pub bytes_written: u64,
    pub sha256: [u8; 32],
}

fn round_up(n: usize) -> usize {
    n.div_ceil(SECTOR) * SECTOR
}

/// Fill `buf` from `src` until full or EOF. Returns bytes read.
fn fill<R: Read>(src: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match src.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

fn is_disk_full(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::ENOSPC)
        || e.kind() == io::ErrorKind::StorageFull
        || e.kind() == io::ErrorKind::WriteZero
}

/// Copy `src` to `dst` in 4 MiB chunks. The final chunk is zero-padded to a 512-byte
/// multiple. The hash covers only the real (unpadded) bytes.
pub fn copy_image<R: Read, W: Write>(
    mut src: R,
    mut dst: W,
    mut on_progress: impl FnMut(u64),
    should_cancel: impl Fn() -> bool,
) -> Result<CopyResult> {
    let mut buf = vec![0u8; CHUNK];
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    loop {
        if should_cancel() {
            return Err(FlashError::Cancelled);
        }
        let n = fill(&mut src, &mut buf).map_err(|e| FlashError::Io("Reading the image", e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        let padded = round_up(n);
        buf[n..padded].fill(0);
        dst.write_all(&buf[..padded]).map_err(|e| {
            if is_disk_full(&e) {
                FlashError::DiskFull
            } else {
                FlashError::Io("Writing to the disk", e)
            }
        })?;
        total += n as u64;
        on_progress(total);
        if n < CHUNK {
            break;
        }
    }
    dst.flush()
        .map_err(|e| FlashError::Io("Writing to the disk", e))?;
    Ok(CopyResult {
        bytes_written: total,
        sha256: hasher.finalize().into(),
    })
}

/// Read exactly `len` bytes back (in 512-byte-aligned reads) and compare the SHA-256.
pub fn verify<R: Read>(
    mut dev: R,
    len: u64,
    expected: [u8; 32],
    mut on_progress: impl FnMut(u64),
    should_cancel: impl Fn() -> bool,
) -> Result<()> {
    let mut buf = vec![0u8; CHUNK];
    let mut hasher = Sha256::new();
    let mut done: u64 = 0;
    while done < len {
        if should_cancel() {
            return Err(FlashError::Cancelled);
        }
        let want = (len - done).min(CHUNK as u64) as usize;
        let padded = round_up(want);
        dev.read_exact(&mut buf[..padded]).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                FlashError::Mismatch
            } else {
                FlashError::Io("Reading back the disk", e)
            }
        })?;
        hasher.update(&buf[..want]);
        done += want as u64;
        on_progress(done);
    }
    let got: [u8; 32] = hasher.finalize().into();
    if got == expected {
        Ok(())
    } else {
        Err(FlashError::Mismatch)
    }
}

/// Counts raw bytes read from the underlying file (compressed bytes for `.img.xz`).
pub struct CountingReader<R> {
    inner: R,
    count: Arc<AtomicU64>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// Open the image for reading, decompressing `.img.xz` on the fly. The counter tracks
/// raw file bytes read, for percent progress (÷ `info.file_size`).
pub fn open_source(info: &ImageInfo) -> Result<(Box<dyn Read + Send>, Arc<AtomicU64>)> {
    let file = File::open(&info.path).map_err(|e| FlashError::Io("Opening the image", e))?;
    let count = Arc::new(AtomicU64::new(0));
    let counting = CountingReader {
        inner: file,
        count: count.clone(),
    };
    let reader: Box<dyn Read + Send> = match info.kind {
        ImageKind::ImgXz => Box::new(liblzma::read::XzDecoder::new_multi_decoder(counting)),
        ImageKind::Iso | ImageKind::Img => Box::new(counting),
    };
    Ok((reader, count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};

    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("flashos-writer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn info(path: &Path) -> ImageInfo {
        ImageInfo::from_path(path).unwrap()
    }

    #[test]
    fn plain_round_trip() {
        let src = data(CHUNK * 2 + 1000);
        let p = tmp("plain.img");
        std::fs::write(&p, &src).unwrap();
        let (r, count) = open_source(&info(&p)).unwrap();
        let mut out = Vec::new();
        let mut last = 0;
        let res = copy_image(r, &mut out, |n| last = n, || false).unwrap();
        assert_eq!(res.bytes_written, src.len() as u64);
        assert_eq!(last, src.len() as u64);
        assert_eq!(count.load(Ordering::Relaxed), src.len() as u64);
        assert_eq!(&out[..src.len()], &src[..]);
        verify(
            Cursor::new(&out),
            res.bytes_written,
            res.sha256,
            |_| {},
            || false,
        )
        .unwrap();
    }

    #[test]
    fn xz_round_trip() {
        let src = data(CHUNK + 12345);
        let mut xz = Vec::new();
        liblzma::read::XzEncoder::new(&src[..], 6)
            .read_to_end(&mut xz)
            .unwrap();
        let p = tmp("pack.img.xz");
        std::fs::write(&p, &xz).unwrap();
        let (r, count) = open_source(&info(&p)).unwrap();
        let mut out = Vec::new();
        let res = copy_image(r, &mut out, |_| {}, || false).unwrap();
        assert_eq!(res.bytes_written, src.len() as u64);
        assert_eq!(count.load(Ordering::Relaxed), xz.len() as u64);
        assert_eq!(&out[..src.len()], &src[..]);
        let expected: [u8; 32] = Sha256::digest(&src).into();
        assert_eq!(res.sha256, expected);
    }

    #[test]
    fn final_chunk_is_padded() {
        let src = data(1000);
        let mut out = Vec::new();
        let res = copy_image(&src[..], &mut out, |_| {}, || false).unwrap();
        assert_eq!(res.bytes_written, 1000);
        assert_eq!(out.len(), 1024);
        assert!(out[1000..].iter().all(|&b| b == 0));

        let mut out = Vec::new();
        copy_image(&data(CHUNK)[..], &mut out, |_| {}, || false).unwrap();
        assert_eq!(out.len(), CHUNK);
    }

    #[test]
    fn mismatch_detected() {
        let src = data(5000);
        let mut out = Vec::new();
        let res = copy_image(&src[..], &mut out, |_| {}, || false).unwrap();
        out[4000] ^= 1;
        let err = verify(
            Cursor::new(&out),
            res.bytes_written,
            res.sha256,
            |_| {},
            || false,
        );
        assert!(matches!(err, Err(FlashError::Mismatch)));
        // Short device.
        let err = verify(
            Cursor::new(&out[..2048]),
            5000,
            res.sha256,
            |_| {},
            || false,
        );
        assert!(matches!(err, Err(FlashError::Mismatch)));
    }

    #[test]
    fn cancel_stops() {
        let src = data(CHUNK * 3);
        let mut out = Vec::new();
        let calls = std::cell::Cell::new(0);
        let err = copy_image(
            &src[..],
            &mut out,
            |_| {},
            || {
                calls.set(calls.get() + 1);
                calls.get() > 1
            },
        );
        assert!(matches!(err, Err(FlashError::Cancelled)));
        assert_eq!(out.len(), CHUNK);
        let err = verify(Cursor::new(&out), CHUNK as u64, [0; 32], |_| {}, || true);
        assert!(matches!(err, Err(FlashError::Cancelled)));
    }

    #[test]
    fn disk_full_maps_to_error() {
        let mut dev = [0u8; 1024];
        let err = copy_image(&data(4096)[..], &mut dev[..], |_| {}, || false);
        assert!(matches!(err, Err(FlashError::DiskFull)));
    }
}
