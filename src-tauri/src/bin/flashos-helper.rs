//! Privileged helper: writes an image to a raw external disk, verifies, ejects.
//! Runs as root (launched via osascript "with administrator privileges").
//!
//! Usage: flashos-helper --image <path> --disk <diskN> --work-dir <dir>
//!
//! Appends one JSON line per event to <work-dir>/progress.jsonl. A file named
//! <work-dir>/cancel requests cancellation. Exit codes: 0 done, 1 error, 2 cancelled.

use flashos_lib::diskutil::{self, is_allowed_target, is_whole_disk_id};
use flashos_lib::image::ImageInfo;
use flashos_lib::writer::{self, FlashError};
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

const THROTTLE: Duration = Duration::from_millis(250);

struct Args {
    image: PathBuf,
    disk: String,
    work_dir: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let mut image = None;
    let mut disk = None;
    let mut work_dir = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let slot = match a.as_str() {
            "--image" => &mut image,
            "--disk" => &mut disk,
            "--work-dir" => &mut work_dir,
            _ => return Err(format!("unknown argument: {a}")),
        };
        *slot = Some(it.next().ok_or(format!("{a} needs a value"))?);
    }
    Ok(Args {
        image: image.ok_or("missing --image")?.into(),
        disk: disk.ok_or("missing --disk")?,
        work_dir: work_dir.ok_or("missing --work-dir")?.into(),
    })
}

struct Progress(File);

impl Progress {
    fn open(work_dir: &Path) -> io::Result<Self> {
        // O_NOFOLLOW: we're root writing into a user-owned dir.
        OpenOptions::new()
            .append(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(work_dir.join("progress.jsonl"))
            .map(Self)
    }

    fn emit(&mut self, v: Value) {
        // One write per line so the app never sees a torn line mid-UTF-8.
        let _ = self.0.write_all(format!("{v}\n").as_bytes());
    }
}

enum Failure {
    Cancelled,
    Error(String),
}

impl From<FlashError> for Failure {
    fn from(e: FlashError) -> Self {
        match e {
            FlashError::Cancelled => Failure::Cancelled,
            e => Failure::Error(e.to_string()),
        }
    }
}

fn err(msg: impl Into<String>) -> Failure {
    Failure::Error(msg.into())
}

fn run_diskutil(args: &[&str]) -> Result<(), Failure> {
    let out = Command::new("/usr/sbin/diskutil")
        .args(args)
        .output()
        .map_err(|e| err(format!("diskutil {}: {e}", args[0])))?;
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr);
        let msg = if msg.trim().is_empty() {
            String::from_utf8_lossy(&out.stdout)
        } else {
            msg
        };
        Err(err(format!("diskutil {} failed: {}", args[0], msg.trim())))
    }
}

/// Fails with ENOSPC instead of writing past the end of the disk.
struct Limited<W> {
    inner: W,
    left: u64,
}

impl<W: Write> Write for Limited<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() as u64 > self.left {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        let n = self.inner.write(buf)?;
        self.left -= n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn full_fsync(f: &File) {
    // SAFETY: valid fd for the lifetime of `f`.
    let r = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_FULLFSYNC) };
    if r != 0 {
        // Raw devices may not support F_FULLFSYNC; fall back. Verify is the real check.
        let _ = f.sync_all();
    }
}

fn flash(args: &Args, progress: &mut Progress) -> Result<u64, Failure> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(err("The helper must run as root"));
    }
    if !is_whole_disk_id(&args.disk) {
        return Err(err(format!("Invalid disk id: {}", args.disk)));
    }
    match is_allowed_target(&args.disk) {
        Ok(true) => {}
        Ok(false) => {
            return Err(err(format!(
                "{} is not an allowed target (only external, non-boot disks)",
                args.disk
            )));
        }
        Err(e) => return Err(err(format!("Checking the disk failed: {e:#}"))),
    }
    let info = ImageInfo::from_path(&args.image).map_err(|e| err(format!("{e:#}")))?;
    let disk = diskutil::disk_info(&args.disk).map_err(|e| err(format!("{e:#}")))?;
    if info.size_known && info.file_size > disk.size_bytes {
        return Err(err("Image is larger than the disk"));
    }

    let cancel_path = args.work_dir.join("cancel");
    let should_cancel = || cancel_path.exists();
    if should_cancel() {
        return Err(Failure::Cancelled);
    }

    let dev_node = format!("/dev/{}", args.disk);
    let raw_node = format!("/dev/r{}", args.disk);
    run_diskutil(&["unmountDisk", "force", &dev_node])?;

    // Write.
    let (src, src_count) = writer::open_source(&info)?;
    let total = info.size_known.then_some(info.file_size);
    let dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&raw_node)
        .map_err(|e| err(format!("Opening {raw_node}: {e}")))?;
    progress.emit(json!({"phase": "writing", "done": 0, "total": total,
        "srcDone": 0, "srcTotal": info.file_size}));
    let mut last = Instant::now();
    let result = writer::copy_image(
        src,
        Limited {
            inner: &dev,
            left: disk.size_bytes,
        },
        |done| {
            if last.elapsed() >= THROTTLE {
                last = Instant::now();
                let src_done = src_count.load(std::sync::atomic::Ordering::Relaxed);
                progress.emit(json!({"phase": "writing", "done": done, "total": total,
                    "srcDone": src_done, "srcTotal": info.file_size}));
            }
        },
        should_cancel,
    )?;
    full_fsync(&dev);
    drop(dev);
    progress.emit(json!({"phase": "writing", "done": result.bytes_written,
        "total": result.bytes_written, "srcDone": info.file_size, "srcTotal": info.file_size}));

    // Verify on a fresh open.
    let dev = File::open(&raw_node).map_err(|e| err(format!("Opening {raw_node}: {e}")))?;
    let len = result.bytes_written;
    progress.emit(json!({"phase": "verifying", "done": 0, "total": len}));
    let mut last = Instant::now();
    writer::verify(
        &dev,
        len,
        result.sha256,
        |done| {
            if last.elapsed() >= THROTTLE {
                last = Instant::now();
                progress.emit(json!({"phase": "verifying", "done": done, "total": len}));
            }
        },
        should_cancel,
    )?;
    drop(dev);

    run_diskutil(&["eject", &dev_node])?;
    Ok(len)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("flashos-helper: {e}");
            eprintln!("usage: flashos-helper --image <path> --disk <diskN> --work-dir <dir>");
            return ExitCode::from(1);
        }
    };
    let mut progress = match Progress::open(&args.work_dir) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("flashos-helper: can't open progress file: {e}");
            return ExitCode::from(1);
        }
    };
    match flash(&args, &mut progress) {
        Ok(bytes) => {
            progress.emit(json!({"phase": "done", "bytes": bytes}));
            ExitCode::SUCCESS
        }
        Err(Failure::Cancelled) => {
            let _ = run_diskutil(&["unmountDisk", &format!("/dev/{}", args.disk)]);
            progress.emit(json!({"phase": "cancelled"}));
            ExitCode::from(2)
        }
        Err(Failure::Error(message)) => {
            eprintln!("flashos-helper: {message}");
            progress.emit(json!({"phase": "error", "message": message}));
            ExitCode::from(1)
        }
    }
}
