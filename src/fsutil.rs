//! Crash-safe file updates for `config.toml` and `state.toml`.
//!
//! Writers inside KeyDock are serialised by a process-wide lock, each write
//! stages its data in a temporary file of its own (created exclusively, so
//! two writers can never truncate or remove each other's file), and a
//! conditional write re-checks the file on disk immediately before it
//! replaces it. Another program can still change the file between that last
//! check and the rename (a window of microseconds); Windows has no
//! compare-and-swap for files that every editor would honour.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

/// Serialises KeyDock's own writers (settings window, `--remember-builtin`).
static WRITE_LOCK: Mutex<()> = Mutex::new(());
/// Makes temporary file names unique within the process.
static TMP_COUNTER: AtomicU32 = AtomicU32::new(0);

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.file_name().map(OsString::from).unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

/// What a file looked like when it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revision {
    /// The file does not exist.
    Missing,
    /// The file's bytes.
    Content(Vec<u8>),
    /// The file exists (or may exist) but cannot be read; the error text.
    Unreadable(String),
}

impl Revision {
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Revision::Content(b) => Some(b),
            _ => None,
        }
    }
}

/// Reads `path`, telling a missing file apart from one that cannot be read.
pub fn read_revision(path: &Path) -> Revision {
    match fs::read(path) {
        Ok(b) => Revision::Content(b),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Revision::Missing,
        Err(e) => Revision::Unreadable(e.to_string()),
    }
}

/// Why a conditional write did not happen.
#[derive(Debug)]
pub enum WriteError {
    /// The file on disk is no longer the revision the caller expected; it is now this.
    Conflict(Revision),
    Io(io::Error),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Conflict(_) => f.write_str("the file was changed by another program"),
            WriteError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<io::Error> for WriteError {
    fn from(e: io::Error) -> Self {
        WriteError::Io(e)
    }
}

/// Writes `contents` to a new, uniquely named temporary file next to `path`
/// and flushes it; returns its path.
fn stage(path: &Path, contents: &[u8]) -> io::Result<PathBuf> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    for _ in 0..16 {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = sibling(path, &format!(".{}-{n}.tmp", std::process::id()));
        let mut f: File = match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let written = f.write_all(contents).and_then(|_| f.sync_all());
        drop(f);
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        return Ok(tmp);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot create a unique temporary file",
    ))
}

fn write_inner(path: &Path, contents: &[u8], expected: Option<&Revision>) -> Result<(), WriteError> {
    let _lock = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = stage(path, contents)?;
    if let Some(expected) = expected {
        let now = read_revision(path);
        if &now != expected {
            let _ = fs::remove_file(&tmp);
            return Err(WriteError::Conflict(now));
        }
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        WriteError::Io(e)
    })
}

/// Writes `contents` to `path` so that readers see either the old or the new
/// file, never a truncated one: the data goes to a temporary file next to it,
/// is flushed to disk, and then renamed over the original (an atomic replace
/// on NTFS within one volume). Creates the parent directory if needed.
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    write_inner(path, contents.as_bytes(), None).map_err(|e| match e {
        WriteError::Io(e) => e,
        WriteError::Conflict(_) => io::Error::other("unexpected conflict"),
    })
}

/// As [`write_atomic`], but only if the file on disk is still `expected`
/// (checked after the new data is staged, immediately before the rename).
pub fn write_atomic_if(path: &Path, contents: &str, expected: &Revision) -> Result<(), WriteError> {
    write_inner(path, contents.as_bytes(), Some(expected))
}

/// As [`write_atomic_if`], and when `backup` is given (the revision the user
/// approved replacing) it becomes `<name>.bak` in the same commit. The
/// earlier backup is only replaced once the file is known to be `expected`,
/// and it is put back if the final replace fails, so an aborted or failed
/// save never costs the previous recovery copy (review TP-09). Another
/// program can still change the file in the instant between the last check
/// and the rename.
pub fn write_atomic_if_with_backup(
    path: &Path,
    contents: &str,
    expected: &Revision,
    backup: Option<&[u8]>,
) -> Result<(), WriteError> {
    let _lock = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let bak = sibling(path, ".bak");
    let tmp = stage(path, contents.as_bytes())?;
    let bak_tmp = match backup.map(|b| stage(&bak, b)).transpose() {
        Ok(t) => t,
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(WriteError::Io(e));
        }
    };
    let cleanup = |also: Option<&PathBuf>| {
        let _ = fs::remove_file(&tmp);
        if let Some(b) = also {
            let _ = fs::remove_file(b);
        }
    };
    let now = read_revision(path);
    if &now != expected {
        cleanup(bak_tmp.as_ref());
        return Err(WriteError::Conflict(now));
    }
    // Move the old backup aside (not away) and put the new one in place.
    let mut prev: Option<PathBuf> = None;
    if let Some(bt) = &bak_tmp {
        if bak.exists() {
            let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let p = sibling(path, &format!(".bak.{}-{n}.prev", std::process::id()));
            if let Err(e) = fs::rename(&bak, &p) {
                cleanup(bak_tmp.as_ref());
                return Err(WriteError::Io(e));
            }
            prev = Some(p);
        }
        if let Err(e) = fs::rename(bt, &bak) {
            if let Some(p) = &prev {
                let _ = fs::rename(p, &bak);
            }
            cleanup(bak_tmp.as_ref());
            return Err(WriteError::Io(e));
        }
    }
    if let Err(e) = fs::rename(&tmp, path) {
        // Undo: the old backup comes back.
        if bak_tmp.is_some() {
            let _ = fs::remove_file(&bak);
            if let Some(p) = &prev {
                let _ = fs::rename(p, &bak);
            }
        }
        cleanup(None);
        return Err(WriteError::Io(e));
    }
    if let Some(p) = prev {
        let _ = fs::remove_file(p);
    }
    Ok(())
}

/// Keeps `bytes` (a revision of `path` the user approved replacing) as
/// `<name>.bak`, replacing an older backup; returns the backup path.
pub fn backup_bytes(path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let bak = sibling(path, ".bak");
    write_inner(&bak, bytes, None).map_err(|e| match e {
        WriteError::Io(e) => e,
        WriteError::Conflict(_) => io::Error::other("unexpected conflict"),
    })?;
    Ok(bak)
}

/// Copies `path` to `<name>.bak` (replacing an older backup) and returns the
/// backup path.
pub fn backup(path: &Path) -> io::Result<PathBuf> {
    let bak = sibling(path, ".bak");
    fs::copy(path, &bak)?;
    Ok(bak)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("keydock-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .map(|r| {
                r.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.ends_with(".tmp"))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn writes_replaces_and_backs_up() {
        let dir = temp_dir("atomic");
        let file = dir.join("sub").join("config.toml");
        write_atomic(&file, "a = 1\n").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "a = 1\n");
        write_atomic(&file, "a = 2\n").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "a = 2\n");
        assert!(
            leftovers(file.parent().unwrap()).is_empty(),
            "temporary file is renamed away"
        );

        let bak = backup(&file).unwrap();
        assert_eq!(bak.file_name().unwrap(), "config.toml.bak");
        assert_eq!(fs::read_to_string(&bak).unwrap(), "a = 2\n");
        let bak = backup_bytes(&file, b"approved\n").unwrap();
        assert_eq!(fs::read_to_string(&bak).unwrap(), "approved\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_write_leaves_original() {
        let dir = temp_dir("fail");
        let file = dir.join("config.toml");
        write_atomic(&file, "keep\n").unwrap();
        // A directory in place of the target makes the rename fail.
        let blocked = dir.join("blocked.toml");
        fs::create_dir_all(&blocked).unwrap();
        assert!(write_atomic(&blocked, "lost\n").is_err());
        assert!(leftovers(&dir).is_empty(), "the staged file is removed after a failure");
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn revisions_tell_missing_from_unreadable() {
        let dir = temp_dir("rev");
        let file = dir.join("config.toml");
        assert_eq!(read_revision(&file), Revision::Missing);
        write_atomic(&file, "x").unwrap();
        assert_eq!(read_revision(&file), Revision::Content(b"x".to_vec()));
        // A directory with the file's name cannot be read as a file.
        let d = dir.join("dir.toml");
        fs::create_dir_all(&d).unwrap();
        assert!(matches!(read_revision(&d), Revision::Unreadable(_)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn conditional_write_detects_deletion_and_late_edits() {
        let dir = temp_dir("cond");
        let file = dir.join("config.toml");
        write_atomic(&file, "v1").unwrap();
        let seen = read_revision(&file);
        // Deleted after it was read: no silent re-creation.
        fs::remove_file(&file).unwrap();
        match write_atomic_if(&file, "mine", &seen) {
            Err(WriteError::Conflict(Revision::Missing)) => {}
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(read_revision(&file), Revision::Missing);
        // Edited after it was read.
        write_atomic(&file, "v2").unwrap();
        match write_atomic_if(&file, "mine", &seen) {
            Err(WriteError::Conflict(Revision::Content(b))) => assert_eq!(b, b"v2"),
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&file).unwrap(), "v2");
        // The revision the user approved replacing: the write goes through.
        write_atomic_if(&file, "mine", &read_revision(&file)).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "mine");
        // A missing file that is expected to be missing is created.
        let new = dir.join("new.toml");
        write_atomic_if(&new, "n", &Revision::Missing).unwrap();
        assert!(leftovers(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn aborted_or_failed_save_keeps_the_previous_backup() {
        let dir = temp_dir("bak");
        let file = dir.join("config.toml");
        let bak = sibling(&file, ".bak");
        write_atomic(&file, "B").unwrap();
        fs::write(&bak, "A (known good)").unwrap();
        // Review TP-09: the user approves replacing B, but another editor
        // writes C while the question is open.
        let approved = read_revision(&file);
        write_atomic(&file, "C").unwrap();
        match write_atomic_if_with_backup(&file, "mine", &approved, approved.bytes()) {
            Err(WriteError::Conflict(Revision::Content(b))) => assert_eq!(b, b"C"),
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&bak).unwrap(), "A (known good)", "old backup kept");
        assert_eq!(fs::read_to_string(&file).unwrap(), "C");
        assert!(leftovers(&dir).is_empty());
        // The user approves C now: it is committed together with its backup.
        let approved = read_revision(&file);
        write_atomic_if_with_backup(&file, "mine", &approved, approved.bytes()).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "mine");
        assert_eq!(fs::read_to_string(&bak).unwrap(), "C");
        assert!(fs::read_dir(&dir)
            .unwrap()
            .all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".prev")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failure_at_the_final_replace_keeps_the_previous_backup() {
        let dir = temp_dir("bakfail");
        // A directory where the file should be: readable as a revision
        // (Unreadable), but the final rename onto it fails.
        let file = dir.join("config.toml");
        fs::create_dir_all(&file).unwrap();
        let bak = sibling(&file, ".bak");
        fs::write(&bak, "A (known good)").unwrap();
        let expected = read_revision(&file);
        assert!(matches!(expected, Revision::Unreadable(_)));
        let r = write_atomic_if_with_backup(&file, "mine", &expected, Some(b"approved"));
        assert!(matches!(r, Err(WriteError::Io(_))), "{r:?}");
        assert_eq!(
            fs::read_to_string(&bak).unwrap(),
            "A (known good)",
            "old backup restored"
        );
        assert!(leftovers(&dir).is_empty());
        assert!(fs::read_dir(&dir)
            .unwrap()
            .all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".prev")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_writers_do_not_share_a_temporary_file() {
        let dir = temp_dir("concurrent");
        let file = dir.join("state.toml");
        // A stale temporary file with the old fixed name is not touched.
        fs::create_dir_all(&dir).unwrap();
        fs::write(sibling(&file, ".tmp"), "debris").unwrap();
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let f = file.clone();
                std::thread::spawn(move || write_atomic(&f, &format!("writer {i}\n").repeat(1000)))
            })
            .collect();
        for t in threads {
            t.join().unwrap().unwrap();
        }
        let text = fs::read_to_string(&file).unwrap();
        let first = text.lines().next().unwrap().to_string();
        assert!(text.lines().all(|l| l == first), "one writer's complete content");
        assert_eq!(fs::read_to_string(sibling(&file, ".tmp")).unwrap(), "debris");
        let _ = fs::remove_dir_all(&dir);
    }
}
