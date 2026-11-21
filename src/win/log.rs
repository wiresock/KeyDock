//! Minimal file logger (`%LOCALAPPDATA%\KeyDock\keydock.log`), optionally
//! mirrored to an attached console. The file is rotated to `keydock.log.old`
//! when it exceeds 1 MiB, at startup and while the tray app runs.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, TryLockError};
use windows::Win32::System::SystemInformation::GetLocalTime;

const MAX_LOG_BYTES: u64 = 1024 * 1024;

struct Logger {
    file: Option<File>,
    path: Option<PathBuf>,
    /// Bytes in the current file (approximate: other processes may append too).
    size: u64,
    console: bool,
}

static LOGGER: Mutex<Logger> = Mutex::new(Logger {
    file: None,
    path: None,
    size: 0,
    console: false,
});

/// The logger, also after a panic poisoned the lock.
fn logger() -> MutexGuard<'static, Logger> {
    LOGGER.lock().unwrap_or_else(|e| e.into_inner())
}

fn old_path(path: &Path) -> PathBuf {
    path.with_extension("log.old")
}

fn open(path: &Path) -> (Option<File>, u64) {
    let file = OpenOptions::new().create(true).append(true).open(path).ok();
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    (file, size)
}

/// Opens the log file, rotating it first when it exceeds 1 MiB.
pub fn init(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if fs::metadata(path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false) {
        let _ = fs::rename(path, old_path(path));
    }
    let (file, size) = open(path);
    let mut l = logger();
    l.file = file;
    l.size = size;
    l.path = Some(path.to_path_buf());
}

pub fn set_console(on: bool) {
    logger().console = on;
}

fn line(level: &str, msg: &str) -> String {
    // SAFETY: GetLocalTime has no preconditions.
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} [{level}] {msg}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

fn emit(l: &mut Logger, line: &str, msg: &str) {
    if let Some(f) = l.file.as_mut() {
        if writeln!(f, "{line}").is_ok() {
            let _ = f.flush();
            l.size += line.len() as u64 + 1;
        }
    }
    if l.console {
        let _ = writeln!(std::io::stdout(), "{msg}");
    }
    if l.size > MAX_LOG_BYTES {
        if let Some(path) = l.path.clone() {
            // Close, rotate, reopen. The file is opened with delete sharing,
            // so another KeyDock process that has it open does not block this.
            l.file = None;
            let _ = fs::rename(&path, old_path(&path));
            let (file, size) = open(&path);
            l.file = file;
            l.size = size;
        }
    }
}

pub fn write(level: &str, msg: &str) {
    let line = line(level, msg);
    emit(&mut logger(), &line, msg);
}

/// For the panic hook: never blocks (the panic may have happened inside
/// `write`, with the lock held by this thread).
pub fn write_panic(msg: &str) {
    let line = line("ERROR", msg);
    match LOGGER.try_lock() {
        Ok(mut l) => emit(&mut l, &line, msg),
        Err(TryLockError::Poisoned(e)) => emit(&mut e.into_inner(), &line, msg),
        Err(TryLockError::WouldBlock) => {}
    }
}
