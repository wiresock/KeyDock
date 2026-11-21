//! Keeps the order of the user's language list across layout changes,
//! without overwriting anything else (see `keydock::langorder` for the
//! checks). Restores with the documented `Set-WinUserLanguageList` cmdlet in
//! a hidden Windows PowerShell process that runs in a kill-on-close job (see
//! [`super::child`]), on a worker thread, so the tray never blocks on it.
//!
//! While a restore runs, [`busy`] is true and the tray defers every
//! evaluation; `Set-WinUserLanguageList` writes the whole list, so a layout
//! change made meanwhile could be undone. The gate stays closed until the
//! process has ended (or its end after termination is confirmed). When the
//! restore ends the main window gets [`super::window::WM_LANGORDER_DONE`]; the
//! result is kept for [`take_outcome`] in any case, so a lost message only
//! delays the report until the next evaluation.
//!
//! Lifetime: a new instance stops a restore that a previous instance in this
//! session left behind ([`startup`]); Exit and sign-out wait a bounded time
//! and then cancel ([`shutdown`]); any other end of KeyDock closes the job,
//! which terminates the process. A marker file records a restore in flight,
//! so the next start reports one that was interrupted.

use super::child::{self, Child, End, Event, Job};
use super::registry::Key;
use super::window::WM_LANGORDER_DONE;
use keydock::langorder::{
    explain_change, parse_multi_sz, restore_order, restore_script, verify_restored, OrderPolicy, RegValue, Snapshot,
    CONFLICT_EXIT,
};
use keydock::layout::LayoutSpec;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Registry::HKEY_CURRENT_USER;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

const USER_PROFILE_KEY: &str = "Control Panel\\International\\User Profile";
/// The job of this session's restore (see the module documentation).
/// Tests use their own name, so they never touch a running KeyDock's job.
const JOB_NAME: &str = if cfg!(test) {
    "Local\\KeyDock.LanguageRestore.test"
} else {
    "Local\\KeyDock.LanguageRestore.7f3c2a91"
};
/// A restore that takes longer is stopped, so the tray does not wait forever.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(30);
/// REG_MULTI_SZ.
const REG_MULTI_SZ: u32 = 7;

/// True while a restore runs.
static BUSY: AtomicBool = AtomicBool::new(false);
/// Set once KeyDock is shutting down: no new restore starts.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
/// Main window to notify when a restore ends (0 = none, e.g. `--once`).
static NOTIFY: AtomicIsize = AtomicIsize::new(0);
/// The worker thread of the last restore.
static WORKER: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);
/// Cancels the running restore.
static CANCEL: Mutex<Option<Arc<Event>>> = Mutex::new(None);
/// The result of the last restore, until it is taken.
static OUTCOME: Mutex<Option<Outcome>> = Mutex::new(None);
/// The restore job, created once and kept for the process lifetime.
static JOB: Mutex<Option<Job>> = Mutex::new(None);
/// Marker file for a restore in flight (one per session).
static MARKER: OnceLock<PathBuf> = OnceLock::new();
/// A previous writer that is not known to have stopped (reviews F5-01,
/// S6-01, S6-02): a job of an earlier instance whose end is not confirmed,
/// and processes that are, with credible evidence, an older KeyDock's
/// restore. Every change waits until each is positively known to have
/// stopped or to be absent; a deadline only ends a caller's patience (and
/// makes the tray report it), it never grants a write. A candidate without
/// that evidence (its command line cannot be read) is waited for only up to
/// that deadline, and one whose command line is not KeyDock's is ignored.
static PREVIOUS: Mutex<Previous> = Mutex::new(Previous {
    job: None,
    job_name: None,
    legacy: Vec::new(),
    unidentified: Vec::new(),
    record: None,
    record_unreadable: None,
    saved: None,
    save_error: None,
    attribute: None,
    since: None,
    patience: PREVIOUS_PATIENCE,
    reported: false,
});

#[derive(Default)]
struct Previous {
    /// The earlier instance's restore job. `Some(Some(_))`: open, its process
    /// count is watched; `Some(None)`: it exists but cannot be opened or
    /// queried (indeterminate: looked up again on every check).
    job: Option<Option<child::Owned>>,
    /// The name it is looked up by.
    job_name: Option<String>,
    /// Restore processes of an older KeyDock (0.3.2 and 0.3.3 ran them
    /// outside any job), identified by their command line: waited for until
    /// they have exited, never terminated.
    legacy: Vec<Tracked>,
    /// Candidates whose command line could not be read: waited for at most
    /// `patience` (they are probably not KeyDock's), never recorded.
    unidentified: Vec<(Tracked, Instant)>,
    /// Where `legacy` is recorded, so a restarted KeyDock keeps waiting for
    /// them (it would not detect them again after two minutes).
    record: Option<PathBuf>,
    /// The record exists but cannot be read: evidence of a writer whose
    /// identity is unknown; read again on every check.
    record_unreadable: Option<String>,
    /// What the record holds (written only when this changes).
    saved: Option<Vec<Entry>>,
    /// The last failure to update the record (reported, retried).
    save_error: Option<String>,
    /// How a recorded process that is not confirmed yet is attributed: its
    /// command line, and the recognizer of a KeyDock restore.
    attribute: Option<Attribute>,
    /// When KeyDock started waiting.
    since: Option<Instant>,
    /// After this long the wait is reported as needing attention.
    patience: Duration,
    /// Whether that was logged.
    reported: bool,
}

/// A process KeyDock waits for.
struct Tracked {
    pid: u32,
    /// Creation time (FILETIME), which tells this process from a later one
    /// with the same ID.
    created: u64,
    state: Liveness,
    /// Its command line was recognized as an older KeyDock's restore. A
    /// process recorded by 0.3.7 (which recorded every orphan it found) is
    /// not, until its command line has been checked (review S7-01).
    confirmed: bool,
    /// Why it cannot be attributed yet (its command line cannot be read).
    unattributed: Option<String>,
}

/// How a recorded process is attributed: its command line, and the
/// recognizer of a KeyDock restore.
type Attribute = (fn(u32) -> Result<String, String>, fn(&str) -> bool);

/// One line of the record: process ID, creation time, confirmed.
type Entry = (u32, u64, bool);

/// The first line of a record that says whether each process was confirmed
/// as KeyDock's restore. A record without it was written by KeyDock 0.3.7:
/// its processes are attributed again (review S7-01).
const RECORD_HEADER: &str = "keydock-legacy-writers 2";

enum Liveness {
    /// Running; a synchronize-only handle.
    Running(child::Owned),
    /// Cannot be checked now (the open, the query or the wait failed): still
    /// a writer, checked again later.
    Unknown(String),
}

/// What a check of a recorded process found.
enum Identity {
    /// The same process, running.
    Running(child::Owned),
    /// Exited, no such process, or the ID belongs to a later process.
    Gone,
    /// Cannot be determined now.
    Unknown(String),
}

/// How long a previous writer is waited for before the tray reports it, and
/// the most an unidentified candidate is waited for.
const PREVIOUS_PATIENCE: Duration = Duration::from_secs(60);

/// How an older KeyDock's restore processes are found: by image name, and
/// by their command line, which must be exactly a restore of KeyDock 0.3.2
/// or 0.3.3 (`keydock::langorder::is_historical_restore`, review S7-02).
/// The same rules attribute processes found anew and those in a record.
pub(crate) struct Discovery {
    pub image: String,
    pub recognize: fn(&str) -> bool,
    pub command_line: fn(u32) -> Result<String, String>,
}

impl Discovery {
    fn production() -> Self {
        Self {
            image: "powershell.exe".into(),
            recognize: keydock::langorder::is_historical_restore,
            command_line,
        }
    }
}

/// What a candidate's command line says about it (review S6-01).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Evidence {
    /// It is a KeyDock restore: waited for until it has exited.
    Credible,
    /// It cannot be read: waited for at most [`PREVIOUS_PATIENCE`].
    Unidentified(String),
    /// Readable and not a KeyDock restore: ignored.
    Unrelated,
}

pub(crate) fn classify(command_line: &Result<String, String>, recognize: fn(&str) -> bool) -> Evidence {
    match command_line {
        Ok(c) if recognize(c) => Evidence::Credible,
        Ok(_) => Evidence::Unrelated,
        Err(e) => Evidence::Unidentified(e.clone()),
    }
}

/// A blocked change, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub reason: String,
    /// Waited for longer than KeyDock's patience: the user is told.
    pub overdue: bool,
}

/// How a restore ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The order is back and nothing else changed.
    Restored(Vec<String>),
    /// Not restored because something else changed the list; nothing written.
    Conflict(String),
    /// PowerShell could not be started, failed, or timed out.
    Failed(String),
    /// It ran, but the result is not what was asked for.
    Mismatch(String),
    /// Stopped because KeyDock is exiting.
    Cancelled,
}

impl Outcome {
    /// A description when the user's language order is not as it was.
    pub fn problem(&self) -> Option<String> {
        match self {
            Outcome::Restored(_) => None,
            Outcome::Conflict(e) => Some(format!("language order not restored: {e}")),
            Outcome::Failed(e) => Some(format!("restoring the language order failed: {e}")),
            Outcome::Mismatch(e) => Some(format!("language order restore did not take effect: {e}")),
            Outcome::Cancelled => Some("language order restore was stopped because KeyDock exited".into()),
        }
    }
}

fn set_outcome(o: Outcome) {
    match o.problem() {
        None => info!("language order restored"),
        Some(p) => warn!("{p}"),
    }
    *OUTCOME.lock().unwrap_or_else(|p| p.into_inner()) = Some(o);
}

/// The result of the last restore, once (`None` while running or if taken).
pub fn take_outcome() -> Option<Outcome> {
    if own_busy() {
        return None;
    }
    OUTCOME.lock().unwrap_or_else(|p| p.into_inner()).take()
}

/// Sets the window that receives `WM_LANGORDER_DONE`.
pub fn set_notify_window(hwnd: HWND) {
    NOTIFY.store(hwnd.0 as isize, Ordering::SeqCst);
}

/// Whether a restore is running, or one of an earlier KeyDock may still be
/// writing; layout changes must wait until it ends.
pub fn busy() -> bool {
    own_busy() || previous_writer().is_some()
}

/// This instance's restore: running, or a process still in its job (between
/// restores the job must be empty; a count that cannot be read is a writer).
pub fn own_busy() -> bool {
    if BUSY.load(Ordering::SeqCst) {
        return true;
    }
    if let Ok(g) = JOB.try_lock() {
        if let Some(j) = g.as_ref() {
            return j.active_processes() != Some(0);
        }
    }
    false
}

/// A previous KeyDock's restore that is not known to have stopped, looked at
/// afresh. It ends only on proof (reviews F5-01, S6-02): the job is gone,
/// confirmed empty or stopped now; each tracked process has exited, does not
/// exist, or its ID now belongs to a later process. A failure to open, query
/// or wait for one, or an unreadable record, keeps it a writer (retried on
/// the next check). Waiting longer than KeyDock's patience makes it
/// `overdue`, not absent; only unidentified candidates are let go then.
pub fn previous_writer() -> Option<Blocked> {
    let mut guard = PREVIOUS.lock().unwrap_or_else(|p| p.into_inner());
    let g = &mut *guard;
    let mut reasons = Vec::new();
    if let Some(job) = g.job.as_mut() {
        let ended = match job {
            Some(j) => child::job_processes(j.0) == Some(0),
            // Indeterminate: look again; gone or stopped now is an answer.
            None => match child::stop_stale(g.job_name.as_deref().unwrap_or(JOB_NAME)) {
                Ok(_) => true,
                Err(u) => {
                    *job = u.job;
                    false
                }
            },
        };
        if ended {
            info!("the previous KeyDock's language-order restore has ended");
            g.job = None;
        } else {
            reasons.push("a language-order restore of the previous KeyDock has not ended".to_string());
        }
    }
    // An unreadable record: read it again.
    if g.record_unreadable.is_some() {
        if let Some(f) = g.record.clone() {
            match load_record(&f) {
                Ok(entries) => {
                    info!("the record of an older KeyDock's restore can be used again");
                    g.record_unreadable = None;
                    g.saved = Some(entries.clone());
                    merge_recorded(g, entries);
                }
                Err(e) => g.record_unreadable = Some(e),
            }
        }
    }
    if let Some(e) = &g.record_unreadable {
        reasons.push(format!(
            "the record of an older KeyDock's language-order restore cannot be used ({e}); it is read again until it can (to recover: restart Windows, which ends every older restore, then delete that file)"
        ));
    }
    let before = g.legacy.len();
    let attribute = g.attribute;
    g.legacy.retain_mut(|t| refresh(t) && attribute_recorded(t, attribute));
    if before > 0 && g.legacy.is_empty() {
        info!("the language-order restore process(es) of an older KeyDock have ended");
    }
    let running: Vec<String> = g
        .legacy
        .iter()
        .filter(|t| t.confirmed && matches!(t.state, Liveness::Running(_)))
        .map(|t| t.pid.to_string())
        .collect();
    if !running.is_empty() {
        reasons.push(format!(
            "PowerShell process(es) {} (a language-order restore of an older KeyDock) are still running",
            running.join(", ")
        ));
    }
    for t in &g.legacy {
        if let Liveness::Unknown(e) = &t.state {
            reasons.push(format!(
                "whether PowerShell process {} (a language-order restore of an older KeyDock) is still running cannot be determined ({e})",
                t.pid
            ));
        } else if let Some(e) = &t.unattributed {
            reasons.push(format!(
                "PowerShell process {}, recorded by an older KeyDock, is still running and cannot be identified ({e}); it is checked again",
                t.pid
            ));
        }
    }
    // Unidentified candidates: only up to the patience.
    let patience = g.patience;
    g.unidentified.retain_mut(|(t, since)| {
        if !refresh(t) {
            return false;
        }
        if since.elapsed() >= patience {
            warn!(
                "no longer waiting for PowerShell process {}: it could not be identified as an older KeyDock's restore within {} s",
                t.pid,
                patience.as_secs()
            );
            return false;
        }
        true
    });
    if !g.unidentified.is_empty() {
        reasons.push(format!(
            "PowerShell process(es) {} that could not be identified (perhaps an older KeyDock's restore) are still running; waiting up to {} s",
            g.unidentified
                .iter()
                .map(|(t, _)| t.pid.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            patience.as_secs()
        ));
    }
    save_record(g);
    if let Some(e) = &g.save_error {
        if !reasons.is_empty() {
            reasons.push(format!("the record for a restart cannot be updated ({e})"));
        }
    }
    if reasons.is_empty() {
        g.since = None;
        g.reported = false;
        return None;
    }
    let since = *g.since.get_or_insert_with(Instant::now);
    // Unidentified candidates alone never become "overdue": they are let go.
    let lasting = g.job.is_some() || g.record_unreadable.is_some() || !g.legacy.is_empty();
    let overdue = lasting && since.elapsed() >= g.patience;
    if overdue && !g.reported {
        g.reported = true;
        warn!(
            "still waiting after more than {} s: {}. KeyDock changes no layouts until that has ended (end it, or sign out and in again)",
            g.patience.as_secs(),
            reasons.join("; ")
        );
    }
    Some(Blocked {
        reason: reasons.join("; "),
        overdue,
    })
}

/// Checks a tracked process again; false once it is known to be gone.
fn refresh(t: &mut Tracked) -> bool {
    use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    if let Liveness::Running(h) = &t.state {
        // SAFETY: a synchronize handle owned by the entry.
        let r = unsafe { windows::Win32::System::Threading::WaitForSingleObject(h.0, 0) };
        return if r == WAIT_TIMEOUT {
            true
        } else if r == WAIT_OBJECT_0 {
            false
        } else {
            // WAIT_FAILED: not proof of anything.
            t.state = Liveness::Unknown(format!("cannot wait for it: {}", windows::core::Error::from_thread()));
            true
        };
    }
    match identify(t.pid, t.created) {
        Identity::Running(h) => {
            t.state = Liveness::Running(h);
            true
        }
        Identity::Gone => false,
        Identity::Unknown(e) => {
            t.state = Liveness::Unknown(e);
            true
        }
    }
}

/// For a running recorded process that is not confirmed yet (review
/// S7-01): its command line decides. A KeyDock restore is confirmed; any
/// other command line means it is not one (false: no longer waited for); one
/// that cannot be read keeps it, unattributed, and is read again next time.
fn attribute_recorded(t: &mut Tracked, attribute: Option<Attribute>) -> bool {
    if t.confirmed || !matches!(t.state, Liveness::Running(_)) {
        return true;
    }
    let Some((command_line, recognize)) = attribute else {
        return true;
    };
    match classify(&command_line(t.pid), recognize) {
        Evidence::Credible => {
            info!(
                "PowerShell process {} is a language-order restore of an older KeyDock; waiting until it has ended",
                t.pid
            );
            t.confirmed = true;
            t.unattributed = None;
            true
        }
        Evidence::Unrelated => {
            info!(
                "PowerShell process {}, recorded by an older KeyDock, is not a KeyDock restore; no longer waited for",
                t.pid
            );
            false
        }
        Evidence::Unidentified(e) => {
            t.unattributed = Some(e);
            true
        }
    }
}

/// Whether `pid` is still the process created at `created` and running.
fn identify(pid: u32, created: u64) -> Identity {
    use windows::Win32::Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    // SAFETY: a synchronize-only handle, closed by Owned.
    let h = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(h) => child::Owned(h),
        // No process with that ID: it has exited.
        Err(e) if e.code() == ERROR_INVALID_PARAMETER.to_hresult() => return Identity::Gone,
        Err(e) => return Identity::Unknown(format!("cannot open it: {e}")),
    };
    // SAFETY: a wait on a valid handle.
    let r = unsafe { WaitForSingleObject(h.0, 0) };
    if r == WAIT_OBJECT_0 {
        return Identity::Gone;
    }
    if r != WAIT_TIMEOUT {
        return Identity::Unknown(format!("cannot wait for it: {}", windows::core::Error::from_thread()));
    }
    // The open handle keeps the ID from being reused while it is checked.
    match process_created(pid) {
        Ok(c) if c == created => Identity::Running(h),
        // Another process now has this ID: the recorded one has exited.
        Ok(_) => Identity::Gone,
        Err(e) => Identity::Unknown(format!("cannot read its start time: {e}")),
    }
}

/// The record's entries; an absent record is empty. A record that exists
/// but cannot be read or understood is indeterminate (`Err`), also an empty
/// or truncated one (reviews S7-03, S8-02): KeyDock never writes an empty
/// record (it removes the record instead), but 0.3.7 rewrote it in place,
/// so an interrupted write can leave it damaged while the process it named
/// still runs. KeyDock never discards an indeterminate record by itself
/// (review S8-01: neither its file time nor the clock proves that the
/// process ended); it is read again on every check, and the user deletes it
/// after restarting Windows (see the README).
fn load_record(f: &std::path::Path) -> Result<Vec<Entry>, String> {
    let text = match std::fs::read_to_string(f) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", f.display())),
    };
    parse_record(&text).map_err(|e| format!("{}: {e}", f.display()))
}

/// The entries of a record's text (see [`load_record`]). Only a complete
/// record is used (review S8-02): both 0.3.7 and this version end every row
/// with a line feed, so text that does not end with one was cut off (a
/// number in it may be partial, and taken for another process); and a
/// versioned record names at least one process (an empty set is written as
/// no file), so a header alone was cut off too.
pub(crate) fn parse_record(text: &str) -> Result<Vec<Entry>, String> {
    if text.trim().is_empty() {
        return Err("it is empty (perhaps a write of KeyDock 0.3.7 was interrupted)".into());
    }
    if !text.ends_with('\n') {
        return Err("its last line is incomplete (perhaps a write of KeyDock 0.3.7 was interrupted)".into());
    }
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty()).peekable();
    let versioned = lines.peek() == Some(&RECORD_HEADER);
    if versioned {
        lines.next();
    } else if lines.peek().is_some_and(|l| l.starts_with("keydock-")) {
        return Err("it has a format this KeyDock does not know".into());
    }
    if versioned && lines.peek().is_none() {
        return Err("it names no process (perhaps a write was interrupted)".into());
    }
    lines
        .map(|l| {
            let parts: Vec<&str> = l.split(' ').collect();
            let (pid, created, confirmed) = match (versioned, parts.as_slice()) {
                (true, [p, c, "confirmed"]) => (p, c, true),
                (true, [p, c, "unverified"]) => (p, c, false),
                // 0.3.7: every orphan it found, attributed again.
                (false, [p, c]) => (p, c, false),
                _ => return Err(()),
            };
            Ok((
                pid.parse().map_err(|_| ())?,
                created.parse().map_err(|_| ())?,
                confirmed,
            ))
        })
        .collect::<Result<Vec<_>, ()>>()
        .map_err(|_| "it is not a KeyDock record".to_string())
}

/// Adds recorded processes not tracked yet (checked on the next refresh).
fn merge_recorded(g: &mut Previous, entries: Vec<Entry>) {
    for (pid, created, confirmed) in entries {
        if !g.legacy.iter().any(|t| t.pid == pid && t.created == created) {
            g.legacy.push(Tracked {
                pid,
                created,
                state: Liveness::Unknown("not checked yet".into()),
                confirmed,
                unattributed: None,
            });
        }
    }
}

/// The text of a record with these entries.
pub(crate) fn record_text(entries: &[Entry]) -> String {
    let mut s = format!("{RECORD_HEADER}\n");
    for (p, c, confirmed) in entries {
        s.push_str(&format!(
            "{p} {c} {}\n",
            if *confirmed { "confirmed" } else { "unverified" }
        ));
    }
    s
}

/// Writes the record when the tracked set changed (atomically: readers see
/// the old or the new record), or removes it when nothing is tracked. An
/// unreadable record is left alone. A failure is reported and retried.
fn save_record(g: &mut Previous) {
    let Some(f) = g.record.clone() else { return };
    if g.record_unreadable.is_some() {
        return;
    }
    let mut now: Vec<Entry> = g.legacy.iter().map(|t| (t.pid, t.created, t.confirmed)).collect();
    now.sort_unstable();
    if g.saved.as_ref().is_some_and(|s| {
        let mut s = s.clone();
        s.sort_unstable();
        s == now
    }) {
        return;
    }
    let r = if now.is_empty() {
        match std::fs::remove_file(&f) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    } else {
        keydock::fsutil::write_atomic(&f, &record_text(&now))
    };
    match r {
        Ok(()) => {
            g.saved = Some(now);
            g.save_error = None;
        }
        Err(e) => {
            let e = format!("{}: {e}", f.display());
            if g.save_error.as_deref() != Some(e.as_str()) {
                warn!("cannot update the record of an older KeyDock's restore ({e}); trying again");
            }
            g.save_error = Some(e);
        }
    }
}

/// A synchronize-only handle to `pid` if it can be opened and is running.
fn open_running(pid: u32) -> Option<child::Owned> {
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    // SAFETY: a synchronize-only handle, closed by Owned.
    let h = child::Owned(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }.ok()?);
    // SAFETY: a wait on a valid handle.
    (unsafe { WaitForSingleObject(h.0, 0) } == windows::Win32::Foundation::WAIT_TIMEOUT).then_some(h)
}

/// The creation time of `pid` (FILETIME units).
fn process_created(pid: u32) -> Result<u64, String> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: a limited query handle, closed by Owned.
    let h =
        child::Owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.map_err(|e| e.to_string())?);
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: valid handle and out pointers.
    unsafe { GetProcessTimes(h.0, &mut c, &mut e, &mut k, &mut u) }.map_err(|e| e.to_string())?;
    Ok((u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime))
}

/// The command line of `pid` (`ProcessCommandLineInformation`, Windows 8.1
/// and later; a process of the same user can be queried).
fn command_line(pid: u32) -> Result<String, String> {
    use windows::core::{s, w};
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    type Query =
        unsafe extern "system" fn(windows::Win32::Foundation::HANDLE, u32, *mut std::ffi::c_void, u32, *mut u32) -> i32;
    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum: u16,
        buffer: *const u16,
    }
    const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;
    // SAFETY: a limited query handle, closed by Owned.
    let h = child::Owned(
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .map_err(|e| format!("cannot open it: {e}"))?,
    );
    // SAFETY: ntdll is always loaded; the export has this signature.
    let query: Query = unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).map_err(|e| e.to_string())?;
        let f = GetProcAddress(ntdll, s!("NtQueryInformationProcess")).ok_or("NtQueryInformationProcess is missing")?;
        std::mem::transmute::<unsafe extern "system" fn() -> isize, Query>(f)
    };
    let mut len = 0u32;
    // SAFETY: a size query (no buffer).
    let _ = unsafe { query(h.0, PROCESS_COMMAND_LINE_INFORMATION, std::ptr::null_mut(), 0, &mut len) };
    if (len as usize) < std::mem::size_of::<UnicodeString>() {
        return Err("its command line cannot be read".into());
    }
    // u64 elements: aligned for the structure at its start.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: a buffer of `len` bytes.
    let st = unsafe {
        query(
            h.0,
            PROCESS_COMMAND_LINE_INFORMATION,
            buf.as_mut_ptr().cast(),
            len,
            &mut len,
        )
    };
    if st < 0 {
        return Err(format!(
            "its command line cannot be read (NTSTATUS 0x{:08X})",
            st as u32
        ));
    }
    // SAFETY: the call filled a UNICODE_STRING whose buffer lies in `buf`.
    let s = unsafe {
        let us = &*(buf.as_ptr() as *const UnicodeString);
        if us.buffer.is_null() {
            return Ok(String::new());
        }
        std::slice::from_raw_parts(us.buffer, usize::from(us.length) / 2)
    };
    Ok(String::from_utf16_lossy(s))
}

fn marker_path() -> Option<&'static PathBuf> {
    MARKER.get()
}

/// Call once the single-instance lock is held: stops a restore left running
/// by a previous instance in this session, starts watching previous writers
/// (see [`previous_writer`]) and reports a restore that was interrupted or
/// stopped. Returns that note for the user, if any: something that happened,
/// not the live waiting state, which [`previous_writer`] reports (review
/// S6-05).
pub fn startup(marker: PathBuf) -> Option<String> {
    startup_with(marker, &Discovery::production(), JOB_NAME, PREVIOUS_PATIENCE)
}

/// Why no layout change may start now, if one may not (review FP-01): a
/// restore of this instance runs, or one of an earlier instance (its job,
/// or an orphaned process of an older KeyDock) may still be writing. Checked
/// at the shared mutation boundary, so the tray and `--once` both honour it.
pub fn writer_gate() -> Option<String> {
    if own_busy() {
        return Some("a language-order restore is running".into());
    }
    previous_writer().map(|b| b.reason)
}

/// Waits up to `limit` until no writer is left (see [`writer_gate`]); true
/// when none is. Running out of time ends only the wait: the writer still
/// blocks every change.
pub fn wait_for_writers(limit: Duration) -> bool {
    let start = Instant::now();
    while busy() {
        if start.elapsed() >= limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// [`startup`] with how older restore processes are found, the job name and
/// the patience (tests use their own harmless fixture and job, and a short
/// patience).
pub(crate) fn startup_with(
    marker: PathBuf,
    discovery: &Discovery,
    job_name: &str,
    patience: Duration,
) -> Option<String> {
    let record = marker.with_extension("legacy-writers");
    let _ = MARKER.set(marker);
    let mut note = None;
    let mut previous = Previous {
        job_name: Some(job_name.to_string()),
        record: Some(record.clone()),
        patience,
        attribute: Some((discovery.command_line, discovery.recognize)),
        ..Default::default()
    };
    match child::stop_stale(job_name) {
        Ok(Some(n)) if n > 0 => {
            warn!("stopped a language-order restore that a previous KeyDock left running ({n} process(es))");
            note = Some("a language-order restore of the previous KeyDock was stopped".to_string());
        }
        Ok(_) => {}
        Err(u) => {
            // Not confirmed: no change until it is gone (review TP-03). This
            // is live state, reported by the evaluation (review S6-05).
            warn!("a language-order restore of the previous KeyDock did not stop ({u:?}); KeyDock changes nothing until it has ended");
            previous.job = Some(u.job);
        }
    }
    // Processes a previous instance was already waiting for (review S6-02):
    // kept until each is proved gone.
    match load_record(&record) {
        Ok(entries) => {
            previous.saved = Some(entries.clone());
            merge_recorded(&mut previous, entries);
        }
        Err(e) => {
            warn!(
                "the record of an older KeyDock's restore cannot be used ({e}); KeyDock changes nothing until it can"
            );
            previous.record_unreadable = Some(e);
        }
    }
    // After an upgrade from 0.3.2 or 0.3.3: their restore ran outside any
    // job. Wait for it to finish instead of killing processes KeyDock cannot
    // prove are its own; only one whose command line is KeyDock's restore
    // script counts as one (review S6-01).
    for (pid, created, handle) in orphaned_processes(&discovery.image) {
        if previous.legacy.iter().any(|t| t.pid == pid && t.created == created) {
            continue;
        }
        let mut tracked = Tracked {
            pid,
            created,
            state: Liveness::Running(handle),
            confirmed: false,
            unattributed: None,
        };
        match classify(&(discovery.command_line)(pid), discovery.recognize) {
            Evidence::Credible => {
                tracked.confirmed = true;
                info!("waiting for PowerShell process {pid}, a language-order restore of an older KeyDock; no layout changes until it has ended");
                previous.legacy.push(tracked);
            }
            Evidence::Unidentified(e) => {
                info!(
                    "PowerShell process {pid} has no parent and may be an older KeyDock's restore, but it cannot be identified ({e}); waiting for it up to {} s",
                    patience.as_secs()
                );
                previous.unidentified.push((tracked, Instant::now()));
            }
            Evidence::Unrelated => {
                info!("PowerShell process {pid} has no parent but is not a KeyDock restore; ignored")
            }
        }
    }
    save_record(&mut previous);
    *PREVIOUS.lock().unwrap_or_else(|p| p.into_inner()) = previous;
    if let Some(p) = marker_path() {
        if let Ok(text) = std::fs::read_to_string(p) {
            let now = snapshot().map(|s| s.languages.join(", ")).unwrap_or_default();
            warn!(
                "a language-order restore did not finish ({}); the order is now {now}",
                text.trim()
            );
            note = Some(format!(
                "a language-order restore did not finish; the language order is now {now}"
            ));
            let _ = std::fs::remove_file(p);
        }
    }
    note
}

/// What is known about a candidate's parent process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Parent {
    /// No process has that ID.
    NoSuchProcess,
    /// It has exited (a handle to it is still open somewhere).
    Exited,
    /// Running, with this creation time (`None`: unreadable).
    Running(Option<u64>),
    /// It cannot be opened (for example another user's process).
    Inaccessible,
}

/// Whether the parent of a process created at `child_created` is known to
/// be gone (review S6-01): it does not exist, has exited, or its ID now
/// belongs to a later process. A parent that cannot be opened or queried is
/// not known to be gone.
pub(crate) fn parent_gone(parent: &Parent, child_created: u64) -> bool {
    match parent {
        Parent::NoSuchProcess | Parent::Exited => true,
        Parent::Running(Some(pc)) => *pc > child_created,
        Parent::Running(None) | Parent::Inaccessible => false,
    }
}

fn probe_parent(pid: u32) -> Parent {
    use windows::Win32::Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    // SAFETY: a synchronize-only handle, closed by Owned.
    let h = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(h) => child::Owned(h),
        Err(e) if e.code() == ERROR_INVALID_PARAMETER.to_hresult() => return Parent::NoSuchProcess,
        Err(_) => return Parent::Inaccessible,
    };
    // SAFETY: a wait on a valid handle.
    match unsafe { WaitForSingleObject(h.0, 0) } {
        r if r == WAIT_OBJECT_0 => Parent::Exited,
        r if r == WAIT_TIMEOUT => Parent::Running(process_created(pid).ok()),
        _ => Parent::Inaccessible,
    }
}

/// Processes of image `image` (`powershell.exe` in production) in this
/// session, started within the last two minutes, whose parent process is
/// known to be gone: what an older KeyDock's restore looks like after that
/// KeyDock exited (whether it is one is decided by its command line).
/// Returned with synchronize-only handles and creation times.
fn orphaned_processes(image: &str) -> Vec<(u32, u64, child::Owned)> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    let session = |pid: u32| {
        let mut s = 0u32;
        // SAFETY: valid out pointer.
        unsafe { ProcessIdToSessionId(pid, &mut s) }.ok().map(|_| s)
    };
    // SAFETY: no preconditions.
    let me = unsafe { GetCurrentProcessId() };
    let Some(my_session) = session(me) else {
        return Vec::new();
    };
    let Ok(now) = process_created(me) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // SAFETY: a process snapshot, closed by Owned; entries sized as required.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let snap = child::Owned(snap);
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap.0, &mut e).is_ok();
        while ok {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..len]);
            let pid = e.th32ProcessID;
            if name.eq_ignore_ascii_case(image) && session(pid) == Some(my_session) {
                if let Ok(c) = process_created(pid) {
                    // 100-ns units: started within two minutes before us.
                    let recent = now.saturating_sub(c) < 120 * 10_000_000;
                    if recent && parent_gone(&probe_parent(e.th32ParentProcessID), c) {
                        // Synchronize only: KeyDock waits for these, it never
                        // terminates a process it cannot prove is its own.
                        if let Some(h) = open_running(pid) {
                            // The handle pins the ID: still the same process?
                            if process_created(pid) == Ok(c) {
                                out.push((pid, c, h));
                            }
                        }
                    }
                }
            }
            ok = Process32NextW(snap.0, &mut e).is_ok();
        }
    }
    out
}

/// The job for the next restore (review S6-03): always a fresh one. A job
/// is terminated when its restore ends (to end any descendant), and Windows
/// may refuse a new process in a job that was just terminated, even when it
/// is empty. The previous one is released only once it is confirmed empty
/// (until then it keeps the gate closed, see [`own_busy`]).
fn next_job<'a>(slot: &'a mut Option<Job>, name: &str) -> Result<&'a Job, String> {
    if let Some(old) = slot.as_ref() {
        if old.active_processes() != Some(0) {
            return Err("the previous restore has not ended".into());
        }
    }
    *slot = None;
    let job = match Job::create(Some(name))? {
        (job, false) => job,
        (held, true) => {
            // The name is held open by another process (an earlier instance's
            // job): end what is in it and leave it; this restore runs in a
            // job of its own (unnamed: kill-on-close still contains it).
            held.terminate();
            let start = Instant::now();
            while held.active_processes() != Some(0) {
                if start.elapsed() > Duration::from_secs(5) {
                    return Err("a previous restore did not stop".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Job::create(None)?.0
        }
    };
    Ok(slot.insert(job))
}

/// Stops new restores, waits up to `drain` for a running one, then cancels
/// it and waits (bounded) until its process is gone.
///
/// Only this instance's restore is waited for: a previous writer is not
/// KeyDock's to end, and exiting does not need it to end.
pub fn shutdown(drain: Duration) {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    if !own_busy() {
        return;
    }
    info!("waiting up to {} ms for the language-order restore", drain.as_millis());
    let start = Instant::now();
    while own_busy() && start.elapsed() < drain {
        std::thread::sleep(Duration::from_millis(20));
    }
    if own_busy() {
        if let Some(c) = CANCEL.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            c.set();
        }
        let start = Instant::now();
        while own_busy() && start.elapsed() < Duration::from_secs(6) {
            std::thread::sleep(Duration::from_millis(20));
        }
        if own_busy() {
            // The job is closed when the process ends, which terminates it.
            warn!("the language-order restore did not stop in time; it ends with KeyDock");
        }
    }
}

/// Waits for a running restore and returns its result (used by `--once`).
pub fn wait() -> Option<Outcome> {
    let h = WORKER.lock().ok().and_then(|mut g| g.take());
    if let Some(h) = h {
        let _ = h.join();
    }
    take_outcome()
}

fn u16s(data: &[u8]) -> Vec<u16> {
    data.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect()
}

/// The whole `User Profile` key (see `keydock::langorder::Snapshot`).
pub fn snapshot() -> Result<Snapshot, String> {
    let key = Key::open(HKEY_CURRENT_USER, USER_PROFILE_KEY)
        .ok_or("cannot open HKCU\\Control Panel\\International\\User Profile")?;
    let mut s = Snapshot::default();
    let mut has_languages = false;
    for (name, ty, data) in key.values()? {
        if name.eq_ignore_ascii_case("Languages") {
            if ty != REG_MULTI_SZ {
                return Err("the Languages value is not a list".into());
            }
            s.languages = parse_multi_sz(&u16s(&data));
            has_languages = true;
        } else {
            s.root.insert(name.to_ascii_lowercase(), RegValue { ty, data });
        }
    }
    if !has_languages || s.languages.is_empty() {
        return Err("the language list is empty or missing".into());
    }
    for sub in key.subkeys_checked()? {
        let k = Key::open(HKEY_CURRENT_USER, &format!("{USER_PROFILE_KEY}\\{sub}"))
            .ok_or_else(|| format!("cannot open the settings of {sub}"))?;
        let values = k
            .values()?
            .into_iter()
            .map(|(n, ty, data)| (n.to_ascii_lowercase(), RegValue { ty, data }))
            .collect();
        s.subkeys.insert(sub.to_ascii_lowercase(), values);
    }
    Ok(s)
}

/// The display-language override (`Set-WinUILanguageOverride`), if any.
pub fn ui_override() -> Option<String> {
    Key::open(HKEY_CURRENT_USER, USER_PROFILE_KEY)?
        .string("WindowsOverride")
        .filter(|s| !s.is_empty())
}

/// Ends a restore: clears [`BUSY`] and notifies the main window. Runs on
/// drop, so it also happens if the worker panics.
struct Finished;

impl Drop for Finished {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::SeqCst);
        let h = NOTIFY.load(Ordering::SeqCst);
        if h != 0 {
            // SAFETY: posting to our own main window; a stale handle only makes this fail.
            let posted = unsafe {
                PostMessageW(
                    Some(HWND(h as *mut std::ffi::c_void)),
                    WM_LANGORDER_DONE,
                    WPARAM(0),
                    LPARAM(0),
                )
            };
            if let Err(e) = posted {
                // The tray also polls while a restore runs, and takes the
                // result at its next evaluation.
                warn!("cannot notify the main window that the restore ended: {e}");
            }
        }
    }
}

/// What happened to the language order after a layout change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Something the user should know (the order could not be kept, the
    /// first language was removed). The tray and `--once` show it.
    pub problems: Vec<String>,
    /// A restore was started; its result arrives later.
    pub restoring: bool,
}

/// After a layout change: compares with the snapshot taken before it and,
/// if KeyDock's change reordered the list and nothing else changed it,
/// starts a restore in the background.
pub fn after_change(
    before: &Snapshot,
    installed: &[LayoutSpec],
    removed: &[LayoutSpec],
    policy: OrderPolicy,
) -> Report {
    let mut report = Report::default();
    let after = match snapshot() {
        Ok(s) => s,
        Err(e) => {
            report
                .problems
                .push(format!("cannot read the language list after the change: {e}"));
            return report;
        }
    };
    let tips = |v: &[LayoutSpec]| v.iter().map(|l| l.to_string()).collect::<Vec<_>>();
    let change = match explain_change(before, &after, &tips(installed), &tips(removed)) {
        Ok(c) => c,
        Err(e) => {
            if before.languages != after.languages {
                report.problems.push(format!(
                    "language order changed ({} -> {}) but is not restored: something else changed the language settings at the same time ({e})",
                    before.languages.join(", "),
                    after.languages.join(", ")
                ));
            } else {
                info!("language settings changed by something else during the layout change ({e})");
            }
            return report;
        }
    };
    if let Some(first) = &change.first_removed {
        let note = match ui_override() {
            Some(o) => format!("{first}, the first language, was removed with its last layout (display language stays {o})"),
            None => format!(
                "{first}, the first language, was removed with its last layout; Windows may use another display language at the next sign-in"
            ),
        };
        report.problems.push(note);
    }
    let Some(want) = restore_order(&before.languages, &after.languages) else {
        info!("language order kept: {}", after.languages.join(", "));
        return report;
    };
    let first_changed = after.languages.first().map(|a| !a.eq_ignore_ascii_case(&want[0])) == Some(true);
    warn!(
        "the layout change reordered the languages: before {}, after {}{}",
        before.languages.join(", "),
        after.languages.join(", "),
        if first_changed {
            match ui_override() {
                Some(o) => format!(" (first language changed; display language is pinned to {o})"),
                None => " (first language changed: this changes the Windows display language)".to_string(),
            }
        } else {
            String::new()
        }
    );
    if policy == OrderPolicy::Report {
        report.problems.push(format!(
            "language order changed to {} (language_order = \"report\": not restored)",
            after.languages.join(", ")
        ));
        return report;
    }
    if SHUTTING_DOWN.load(Ordering::SeqCst) {
        report
            .problems
            .push("language order changed; not restored because KeyDock is exiting".into());
        return report;
    }
    match start(after, want) {
        Ok(()) => report.restoring = true,
        Err(e) => report.problems.push(format!("cannot restore the language order: {e}")),
    }
    report
}

fn powershell() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| "C:\\Windows".into())
        .join("System32\\WindowsPowerShell\\v1.0\\powershell.exe")
}

/// Starts the restore worker for `expected` (the list as KeyDock's change
/// left it) and the target order `want`.
fn start(expected: Snapshot, want: Vec<String>) -> Result<(), String> {
    let script = restore_script(&want, &expected.expected_list()).ok_or("unexpected language tag or input method")?;
    let cancel = Arc::new(Event::new()?);
    *CANCEL.lock().unwrap_or_else(|p| p.into_inner()) = Some(cancel.clone());
    *OUTCOME.lock().unwrap_or_else(|p| p.into_inner()) = None;
    BUSY.store(true, Ordering::SeqCst);
    let spawned = std::thread::Builder::new().name("langorder".into()).spawn(move || {
        let _finished = Finished;
        let outcome = run(&expected, &want, &script, &cancel);
        // A cancelled restore keeps its marker: the next start reports it.
        if outcome != Outcome::Cancelled {
            if let Some(p) = marker_path() {
                let _ = std::fs::remove_file(p);
            }
        }
        set_outcome(outcome);
    });
    match spawned {
        Ok(handle) => {
            if let Ok(mut g) = WORKER.lock() {
                *g = Some(handle);
            }
            Ok(())
        }
        Err(e) => {
            BUSY.store(false, Ordering::SeqCst);
            Err(format!("cannot start a thread: {e}"))
        }
    }
}

/// The restore itself, on the worker thread.
fn run(expected: &Snapshot, want: &[String], script: &str, cancel: &Event) -> Outcome {
    if cancel.is_set() {
        return Outcome::Cancelled;
    }
    // One writer per user, also across sessions (see `super::WriterLock`).
    // The wait also ends when KeyDock is exiting (review TP-03), and any
    // doubt about the ownership means no write (review TP-02).
    let _writer = match super::WriterLock::acquire(Duration::from_secs(10), Some(cancel.handle())) {
        super::session::Acquire::Owned(l) => l,
        super::session::Acquire::Abandoned(l) => {
            warn!(
                "the previous holder of the writer lock ended without releasing it; checking the language list afresh"
            );
            l
        }
        super::session::Acquire::Busy => {
            return Outcome::Conflict("another KeyDock of this user is changing the layouts".into())
        }
        super::session::Acquire::Cancelled => return Outcome::Cancelled,
        super::session::Acquire::Failed(e) => return Outcome::Failed(e),
    };
    // The lock was released between KeyDock's own change and this point (it
    // is thread-affine), so this is the authoritative check under the newly
    // acquired lock: nothing may have changed since KeyDock's own change.
    match snapshot() {
        Ok(now) if &now == expected => {}
        Ok(_) => return Outcome::Conflict("the language settings changed before the restore could start".into()),
        Err(e) => return Outcome::Failed(format!("cannot read the language list: {e}")),
    }
    if let Some(p) = marker_path() {
        let _ = std::fs::write(p, format!("wanted {}", want.join(", ")));
    }
    let mut job_slot = JOB.lock().unwrap_or_else(|p| p.into_inner());
    let job = match next_job(&mut job_slot, JOB_NAME) {
        Ok(j) => j,
        Err(e) => return Outcome::Failed(e),
    };
    // Last point before the launch: exiting KeyDock starts nothing new.
    if cancel.is_set() || SHUTTING_DOWN.load(Ordering::SeqCst) {
        return Outcome::Cancelled;
    }
    info!("restoring the language order: {}", want.join(", "));
    let args = ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", script];
    // No -ExecutionPolicy: it governs script files, not -Command, and the
    // cmdlets used work under the default (Restricted) policy.
    let finished = Child::spawn(job, &powershell(), &args).and_then(|c| c.wait(RESTORE_TIMEOUT, Some(cancel)));
    let finished = match finished {
        Ok(f) => f,
        Err(e) => {
            // Keep the gate closed until the job is really empty.
            let start = Instant::now();
            while job.active_processes() != Some(0) && start.elapsed() < RESTORE_TIMEOUT {
                job.terminate();
                std::thread::sleep(Duration::from_millis(100));
            }
            return Outcome::Failed(e);
        }
    };
    let first_line = |s: &str| s.lines().next().unwrap_or("").to_string();
    match finished.end {
        End::Exited(0) => match snapshot() {
            Ok(now) => match verify_restored(expected, &now, want) {
                Ok(()) => Outcome::Restored(now.languages),
                Err(e) => Outcome::Mismatch(e),
            },
            Err(e) => Outcome::Mismatch(format!("cannot read the result: {e}")),
        },
        End::Exited(CONFLICT_EXIT) => {
            Outcome::Conflict("the language list changed just before the restore; nothing was written".into())
        }
        End::Exited(code) => Outcome::Failed(format!(
            "PowerShell exited with code {code}{}",
            match first_line(&finished.stderr) {
                l if l.is_empty() => String::new(),
                l => format!(": {l}"),
            }
        )),
        End::TimedOut => Outcome::Failed(format!(
            "stopped after {} s{}",
            RESTORE_TIMEOUT.as_secs(),
            match snapshot() {
                Ok(now) if verify_restored(expected, &now, want).is_ok() =>
                    " (the order had been restored)".to_string(),
                Ok(now) => format!("; the order is {}", now.languages.join(", ")),
                Err(_) => String::new(),
            }
        )),
        End::Cancelled => Outcome::Cancelled,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! The restore script and worker against substitute cmdlets: real Windows
    //! PowerShell runs the generated script, but `Get-WinUserLanguageList`
    //! and `Set-WinUserLanguageList` are functions defined first, so the
    //! user's language list is never written.
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    /// Substitute cmdlets: Get returns `list`, Set writes the order it got to `out`.
    fn prelude(list: &[(&str, &[&str])], out: &std::path::Path, set_body: Option<&str>) -> String {
        let items = list
            .iter()
            .map(|(t, tips)| {
                let tips = tips.iter().map(|x| format!("'{x}'")).collect::<Vec<_>>().join(",");
                format!("[pscustomobject]@{{ LanguageTag = '{t}'; InputMethodTips = @({tips}) }}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let set = set_body.map(str::to_string).unwrap_or_else(|| {
            format!(
                "Set-Content -LiteralPath '{}' -Value ((@($LanguageList) | ForEach-Object {{ $_.LanguageTag }}) -join ',')",
                out.display()
            )
        });
        format!(
            "function Get-WinUserLanguageList {{ @({items}) }}; \
             function Set-WinUserLanguageList {{ param($LanguageList, [switch]$Force) {set} }}; "
        )
    }

    fn run_ps(command: &str) -> child::Finished {
        let (job, _) = Job::create(None).unwrap();
        Child::spawn(
            &job,
            &powershell(),
            &["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", command],
        )
        .unwrap()
        .wait(Duration::from_secs(60), None)
        .unwrap()
    }

    fn temp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("keydock-ps-{name}-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    const LIST: &[(&str, &[&str])] = &[("ru", &["0419:00000419"]), ("en-US", &["0409:0000040A"])];

    #[test]
    fn script_handles_one_language_and_both_output_shapes() {
        // Found in the Windows 10 VM run: with one language, a Get that
        // writes items gave a single object, and the check misfired. The real
        // cmdlet writes the whole list as one object (`,@(...)` here).
        let one: &[(&str, &[&str])] = &[("en-US", &["0409:00000409"])];
        for (list, want) in [(one, v(&["en-US"])), (LIST, v(&["en-US", "ru"]))] {
            let exp: Vec<(String, Vec<String>)> = list
                .iter()
                .map(|(t, tips)| (t.to_string(), tips.iter().map(|x| x.to_string()).collect()))
                .collect();
            let script = restore_script(&want, &exp).unwrap();
            for as_one_object in [false, true] {
                let out = temp(&format!("shape-{}-{as_one_object}", list.len()));
                let mut pre = prelude(list, &out, None);
                if as_one_object {
                    pre = pre.replacen("{ @(", "{ ,@(", 1);
                }
                let f = run_ps(&format!("{pre}{script}"));
                assert_eq!(
                    f.end,
                    End::Exited(0),
                    "{} language(s), one object: {as_one_object}: {}",
                    list.len(),
                    f.stderr
                );
                assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), want.join(","));
                let _ = std::fs::remove_file(&out);
            }
        }
    }

    fn expect() -> Vec<(String, Vec<String>)> {
        vec![
            ("ru".to_string(), v(&["0419:00000419"])),
            ("en-US".to_string(), v(&["0409:0000040A"])),
        ]
    }

    #[test]
    fn script_reorders_when_the_list_is_unchanged() {
        for clm in [false, true] {
            let out = temp(if clm { "clm" } else { "full" });
            let script = restore_script(&v(&["en-US", "ru"]), &expect()).unwrap();
            let mode = if clm {
                "$ExecutionContext.SessionState.LanguageMode = 'ConstrainedLanguage'; "
            } else {
                ""
            };
            let f = run_ps(&format!("{}{mode}{script}", prelude(LIST, &out, None)));
            assert_eq!(f.end, End::Exited(0), "clm={clm}: {}", f.stderr);
            assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "en-US,ru", "clm={clm}");
            let _ = std::fs::remove_file(&out);
        }
    }

    #[test]
    fn script_writes_nothing_after_a_concurrent_edit() {
        let script = restore_script(&v(&["en-US", "ru"]), &expect()).unwrap();
        // Another input method was added meanwhile.
        let out = temp("tips");
        let edited: &[(&str, &[&str])] = &[
            ("ru", &["0419:00000419", "0419:00010419"]),
            ("en-US", &["0409:0000040A"]),
        ];
        let f = run_ps(&format!("{}{script}", prelude(edited, &out, None)));
        assert_eq!(f.end, End::Exited(CONFLICT_EXIT), "{}", f.stderr);
        assert!(!out.exists(), "nothing written");
        // The user reordered meanwhile.
        let out = temp("order");
        let edited: &[(&str, &[&str])] = &[("en-US", &["0409:0000040A"]), ("ru", &["0419:00000419"])];
        let f = run_ps(&format!("{}{script}", prelude(edited, &out, None)));
        assert_eq!(f.end, End::Exited(CONFLICT_EXIT));
        assert!(!out.exists());
        // A language was added meanwhile.
        let out = temp("added");
        let edited: &[(&str, &[&str])] = &[
            ("ru", &["0419:00000419"]),
            ("en-US", &["0409:0000040A"]),
            ("de-DE", &["0407:00000407"]),
        ];
        let f = run_ps(&format!("{}{script}", prelude(edited, &out, None)));
        assert_eq!(f.end, End::Exited(CONFLICT_EXIT));
        assert!(!out.exists());
    }

    #[test]
    fn worker_reports_each_outcome() {
        // The worker against this machine's real (unchanged) list: it wants
        // the current order, so verification passes without a real write.
        // Every test that runs the worker (its job slot and the test lock)
        // takes the shared guard (review S7-04).
        let _g = previous_guard();
        let Ok(expected) = snapshot() else {
            eprintln!("no language list readable here; skipped");
            return;
        };
        let want = expected.languages.clone();
        let list = expected.expected_list();
        let owned: Vec<(&str, Vec<&str>)> = list
            .iter()
            .map(|(t, tips)| (t.as_str(), tips.iter().map(String::as_str).collect()))
            .collect();
        let refs: Vec<(&str, &[&str])> = owned.iter().map(|(t, tips)| (*t, tips.as_slice())).collect();
        let script = restore_script(&want, &list).unwrap();
        let cancel = Event::new().unwrap();
        let out = temp("worker");

        // The setter succeeds: restored and verified.
        let ok = format!("{}{script}", prelude(&refs, &out, None));
        assert_eq!(run(&expected, &want, &ok, &cancel), Outcome::Restored(want.clone()));

        // The setter fails (policy, missing module): a failure with its message.
        let failing = format!("{}{script}", prelude(&refs, &out, Some("throw 'blocked by policy'")));
        match run(&expected, &want, &failing, &cancel) {
            Outcome::Failed(e) => assert!(e.contains("blocked by policy"), "{e}"),
            other => panic!("expected a failure, got {other:?}"),
        }

        // The list changed just before the write: a conflict.
        if refs.len() > 1 {
            let mut other = refs.clone();
            other.reverse();
            let conflict = format!("{}{script}", prelude(&other, &out, None));
            assert!(matches!(
                run(&expected, &want, &conflict, &cancel),
                Outcome::Conflict(_)
            ));
        }

        // A stale expectation: the worker does not even start PowerShell.
        let mut stale = expected.clone();
        stale.languages.push("xx-stale".into());
        assert!(matches!(run(&stale, &want, &ok, &cancel), Outcome::Conflict(_)));

        // A cancel (KeyDock exiting) stops a setter that hangs.
        let hang = format!("{}{script}", prelude(&refs, &out, Some("Start-Sleep -Seconds 60")));
        cancel.set();
        assert_eq!(run(&expected, &want, &hang, &cancel), Outcome::Cancelled);
        let _ = std::fs::remove_file(&out);
    }

    /// Live test against the real language list. It changes the user's
    /// language settings, so it runs only in a disposable VM or test account
    /// (`cargo test -- --ignored live_` with KEYDOCK_LIVE_TESTS=1) and needs
    /// at least two languages. The order is restored at the end.
    #[test]
    #[ignore]
    fn live_restore_round_trip_and_conflicts() {
        let _g = previous_guard();
        if std::env::var("KEYDOCK_LIVE_TESTS").as_deref() != Ok("1") {
            eprintln!("set KEYDOCK_LIVE_TESTS=1 to run (changes the language list)");
            return;
        }
        let real = |cmd: &str| {
            let f = run_ps(cmd);
            assert_eq!(f.end, End::Exited(0), "{cmd}: {}", f.stderr);
        };
        // What Windows did in 0.3.2: the first language moved to the end.
        let move_first_last = "$l = Get-WinUserLanguageList; \
            $n = @($l | Select-Object -Skip 1) + @($l | Select-Object -First 1); \
            Set-WinUserLanguageList -LanguageList $n -Force -WarningAction SilentlyContinue";
        let original = snapshot().unwrap();
        assert!(
            original.languages.len() >= 2,
            "needs two languages: {:?}",
            original.languages
        );
        eprintln!("original: {:?}", original.expected_list());
        let cancel = Event::new().unwrap();

        // 1. Restore: the real worker, the real cmdlet, read back.
        real(move_first_last);
        let moved = snapshot().unwrap();
        assert_ne!(moved.languages, original.languages);
        let want = restore_order(&original.languages, &moved.languages).unwrap();
        let script = restore_script(&want, &moved.expected_list()).unwrap();
        let out = run(&moved, &want, &script, &cancel);
        eprintln!("1. restore: {out:?}");
        assert_eq!(out, Outcome::Restored(original.languages.clone()));
        let now = snapshot().unwrap();
        for t in &original.languages {
            assert_eq!(now.tips(t), original.tips(t), "input methods of {t} kept");
        }
        if std::env::var("KEYDOCK_LIVE_RESTORE_ONLY").as_deref() == Ok("1") {
            // Under an enforced language mode the test's own setup steps
            // below (which call .NET methods) cannot run; the restore can.
            return;
        }

        // 2. The user adds an input method after KeyDock's snapshot: the
        //    worker sees it before PowerShell starts and writes nothing.
        real(move_first_last);
        let moved = snapshot().unwrap();
        let want = restore_order(&original.languages, &moved.languages).unwrap();
        let script = restore_script(&want, &moved.expected_list()).unwrap();
        let first = original.languages[0].clone();
        real(&format!(
            "$l = Get-WinUserLanguageList; ($l | Where-Object {{ $_.LanguageTag -eq '{first}' }}).InputMethodTips.Add('0409:00010409'); \
             Set-WinUserLanguageList -LanguageList $l -Force -WarningAction SilentlyContinue"
        ));
        let edited = snapshot().unwrap();
        let out = run(&moved, &want, &script, &cancel);
        eprintln!("2. edit before the restore: {out:?}");
        assert!(matches!(out, Outcome::Conflict(_)));
        assert_eq!(snapshot().unwrap(), edited, "nothing written");

        // 3. The edit lands between the worker's check and the script's read
        //    (simulated: the worker's view is current, the script's is not):
        //    the script's own check refuses to write.
        let out = run(&edited, &want, &script, &cancel);
        eprintln!("3. edit just before the write: {out:?}");
        assert!(matches!(out, Outcome::Conflict(_)));
        assert_eq!(snapshot().unwrap(), edited, "nothing written");

        // 4. With the edit included, the restore goes through and keeps it.
        let want = restore_order(&original.languages, &edited.languages).unwrap();
        let script = restore_script(&want, &edited.expected_list()).unwrap();
        let out = run(&edited, &want, &script, &cancel);
        eprintln!("4. restore keeping the edit: {out:?}");
        assert_eq!(out, Outcome::Restored(original.languages.clone()));
        assert!(snapshot().unwrap().tips(&first).contains("0409:00010409"));

        // Clean up: remove the added input method again.
        real(&format!(
            "$l = Get-WinUserLanguageList; [void]($l | Where-Object {{ $_.LanguageTag -eq '{first}' }}).InputMethodTips.Remove('0409:00010409'); \
             Set-WinUserLanguageList -LanguageList $l -Force -WarningAction SilentlyContinue"
        ));
        let end = snapshot().unwrap();
        eprintln!("end: {:?}", end.expected_list());
        assert_eq!(end.languages, original.languages);
    }

    /// The tests that change the previous-writer state run one at a time,
    /// each starting and ending with no previous writer (also on failure).
    static PREVIOUS_TESTS: Mutex<()> = Mutex::new(());

    pub(crate) struct PreviousGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    pub(crate) fn previous_guard() -> PreviousGuard {
        let g = PREVIOUS_TESTS.lock().unwrap_or_else(|p| p.into_inner());
        reset_previous();
        PreviousGuard(g)
    }

    impl Drop for PreviousGuard {
        fn drop(&mut self) {
            reset_previous();
        }
    }

    fn reset_previous() {
        let old = std::mem::take(&mut *PREVIOUS.lock().unwrap_or_else(|p| p.into_inner()));
        if let Some(f) = old.record {
            let _ = std::fs::remove_file(f);
        }
    }

    /// An "older KeyDock's restore" (review F5-05): a sleeper of this test
    /// binary whose parent has exited, in a job this fixture owns. Only that
    /// job is ever ended (on drop too, so a failing test cleans up).
    pub(crate) struct LegacyFixture {
        job: Job,
        _parent: Child,
        pub pid: u32,
        ready: PathBuf,
    }

    impl LegacyFixture {
        pub(crate) fn start(secs: u64, tag: &str) -> Self {
            use super::child::tests as c;
            let (job, _) = Job::create(None).unwrap();
            let ready = std::env::temp_dir().join(format!("keydock-legacy-{tag}-{}.txt", std::process::id()));
            let _ = std::fs::remove_file(&ready);
            let parent = c::spawn_helper(&job, &format!("orphan-parent:{secs}"), &ready);
            // The parent exits on its own (Child::wait would end the whole
            // job, the sleeper included); its handle stays open, as an
            // installer's handle to an old KeyDock would.
            // SAFETY: a wait on the retained process handle.
            let r = unsafe { windows::Win32::System::Threading::WaitForSingleObject(parent.process(), 30_000) };
            assert_eq!(
                r,
                windows::Win32::Foundation::WAIT_OBJECT_0,
                "the fixture's parent exits"
            );
            let pid = std::fs::read_to_string(&ready)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .expect("the fixture's sleeper reported its process ID");
            Self {
                job,
                _parent: parent,
                pid,
                ready,
            }
        }

        /// The image name the detector looks for (this test binary).
        pub(crate) fn image() -> String {
            std::env::current_exe()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        }

        pub(crate) fn alive(&self) -> bool {
            open_running(self.pid).is_some()
        }

        /// A synchronize handle to the sleeper, to wait for its end.
        pub(crate) fn handle(&self) -> child::Owned {
            open_running(self.pid).expect("the fixture runs")
        }

        /// Ends the fixture (its own job only) and waits until it is gone.
        pub(crate) fn stop(&self) {
            self.job.terminate();
            let start = Instant::now();
            while self.alive() {
                assert!(start.elapsed() < Duration::from_secs(10), "the fixture ends");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl Drop for LegacyFixture {
        fn drop(&mut self) {
            self.job.terminate();
            let _ = std::fs::remove_file(&self.ready);
            let _ = std::fs::remove_file(format!("{}.sleeper", self.ready.display()));
        }
    }

    fn marker(tag: &str) -> PathBuf {
        let m = std::env::temp_dir().join(format!("keydock-previous-{tag}-{}.pending", std::process::id()));
        let _ = std::fs::remove_file(&m);
        let _ = std::fs::remove_file(m.with_extension("legacy-writers"));
        m
    }

    /// The fixture's sleeper counts as KeyDock's restore: its command line
    /// carries this signature (its helper mode).
    pub(crate) fn credible() -> Discovery {
        Discovery {
            image: LegacyFixture::image(),
            recognize: fixture_is_a_restore,
            command_line,
        }
    }

    /// The production signature: the fixture is an unrelated process.
    fn unrelated() -> Discovery {
        Discovery {
            image: LegacyFixture::image(),
            recognize: keydock::langorder::is_historical_restore,
            command_line,
        }
    }

    /// Command lines cannot be read (as for a process KeyDock may not query).
    fn unreadable() -> Discovery {
        Discovery {
            image: LegacyFixture::image(),
            recognize: keydock::langorder::is_historical_restore,
            command_line: |_| Err("cannot open it: access is denied (test)".into()),
        }
    }

    /// Finds nothing new (a restart more than two minutes later).
    pub(crate) fn nothing_new() -> Discovery {
        Discovery {
            image: "no-such-image.exe".into(),
            recognize: keydock::langorder::is_historical_restore,
            command_line,
        }
    }

    fn job_name() -> String {
        format!("Local\\KeyDock.TestPrevious.{}", std::process::id())
    }

    #[test]
    fn orphan_detection_finds_exactly_its_own_fixture() {
        // Reviews FP-08 and F5-05: the fixture is this test binary's own
        // helper in a job the fixture owns. The detector must find that exact
        // process (ID and creation time); the only termination attempt is
        // against that fixture, through the detector's handle, and it must
        // fail (synchronize only); cleanup is the fixture's job only.
        let _g = previous_guard();
        let fx = LegacyFixture::start(120, "detect");
        let found = orphaned_processes(&LegacyFixture::image());
        let pids: Vec<u32> = found.iter().map(|(p, _, _)| *p).collect();
        let Some((_, created, h)) = found.iter().find(|(p, _, _)| *p == fx.pid) else {
            panic!("the fixture's sleeper {} is found in {pids:?}", fx.pid);
        };
        assert_eq!(Ok(*created), process_created(fx.pid), "with its creation time");
        // SAFETY: a terminate attempt on the detector's handle to the
        // fixture's own process; it must fail, so nothing is touched.
        let r = unsafe { windows::Win32::System::Threading::TerminateProcess(h.0, 1) };
        assert!(r.is_err(), "a detector handle must not allow termination");
        assert!(fx.alive(), "the fixture is still running");
        // Its command line can be read (the evidence of review S6-01).
        let c = command_line(fx.pid).unwrap();
        assert!(c.contains("kdhelper=sleep:"), "{c}");
        // This test process (its parent alive) is never reported, and other
        // images are not considered.
        assert!(!pids.contains(&std::process::id()));
        assert!(orphaned_processes("no-such-image.exe").is_empty());
        drop(found);
        fx.stop();
    }

    #[test]
    fn evidence_and_parent_rules() {
        // Reviews S6-01 and S7-02: only a KeyDock 0.3.2/0.3.3 restore is
        // credible (see keydock::langorder::is_historical_restore); one that
        // cannot be read is unidentified (bounded); others are ignored, also
        // when they carry the setter in a comment.
        let recognize: fn(&str) -> bool = keydock::langorder::is_historical_restore;
        let comment = Ok("powershell.exe -NoProfile -NonInteractive -Command Start-Sleep -Seconds 420; # Set-WinUserLanguageList -LanguageList $new -Force".to_string());
        assert_eq!(classify(&comment, recognize), Evidence::Unrelated);
        assert_eq!(
            classify(&Ok("powershell.exe -File backup.ps1".into()), recognize),
            Evidence::Unrelated
        );
        assert_eq!(
            classify(&Err("denied".into()), recognize),
            Evidence::Unidentified("denied".into())
        );
        assert_eq!(
            classify(&Ok("x kdhelper=sleep:5|y".into()), fixture_is_a_restore),
            Evidence::Credible
        );
        // A parent that cannot be opened or queried is not known to be gone.
        assert!(parent_gone(&Parent::NoSuchProcess, 100));
        assert!(parent_gone(&Parent::Exited, 100));
        assert!(parent_gone(&Parent::Running(Some(200)), 100), "its ID was reused");
        assert!(!parent_gone(&Parent::Running(Some(50)), 100));
        assert!(!parent_gone(&Parent::Running(None), 100));
        assert!(!parent_gone(&Parent::Inaccessible, 100));
    }

    #[test]
    fn an_unrelated_orphan_does_not_block_a_clean_start() {
        // Review S6-01: a harmless PowerShell-like process without a parent
        // (here the fixture, whose command line is not a KeyDock restore)
        // must not hold back anything, nor be recorded.
        let _g = previous_guard();
        let fx = LegacyFixture::start(60, "unrelated");
        let m = marker("unrelated");
        let _ = startup_with(m.clone(), &unrelated(), &job_name(), Duration::from_millis(300));
        assert_eq!(previous_writer(), None);
        assert_eq!(writer_gate(), None);
        assert!(!m.with_extension("legacy-writers").exists(), "nothing recorded");
        assert!(fx.alive(), "and nothing touched");
        fx.stop();
    }

    #[test]
    fn an_unidentified_orphan_is_waited_for_only_up_to_the_deadline() {
        // Review S6-01: a candidate whose command line cannot be read may be
        // an older KeyDock's restore: waited for, but only up to the
        // patience, never recorded, never reported as overdue.
        let _g = previous_guard();
        let fx = LegacyFixture::start(60, "unidentified");
        let m = marker("unidentified");
        let _ = startup_with(m.clone(), &unreadable(), &job_name(), Duration::from_millis(400));
        let b = previous_writer().expect("waited for at first");
        assert!(b.reason.contains("could not be identified") && !b.overdue, "{b:?}");
        assert!(!m.with_extension("legacy-writers").exists(), "not recorded");
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(previous_writer(), None, "let go after the deadline");
        assert!(fx.alive(), "never terminated");
        fx.stop();
    }

    #[test]
    fn a_credible_previous_writer_blocks_past_the_deadline_until_it_has_exited() {
        // Reviews F5-01 and S6-01: the deadline ends the wait, not the
        // protection, for a process identified as a KeyDock restore.
        let _g = previous_guard();
        let fx = LegacyFixture::start(60, "deadline");
        let m = marker("deadline");
        let _ = startup_with(m.clone(), &credible(), &job_name(), Duration::from_millis(300));
        let b = previous_writer().expect("the legacy writer blocks");
        assert!(!b.overdue && b.reason.contains(&fx.pid.to_string()), "{b:?}");
        assert!(m.with_extension("legacy-writers").exists(), "recorded for a restart");
        std::thread::sleep(Duration::from_millis(500));
        let b = previous_writer().expect("still blocking after the deadline");
        assert!(b.overdue, "{b:?}");
        assert!(writer_gate().is_some(), "no change may start");
        assert!(
            !wait_for_writers(Duration::from_millis(300)),
            "the wait ends at its limit"
        );
        assert!(writer_gate().is_some(), "and the gate is still closed");
        assert!(fx.alive(), "never terminated by KeyDock");
        // Proof: the process has exited.
        fx.stop();
        assert_eq!(previous_writer(), None);
        assert_eq!(writer_gate(), None);
        assert!(!m.with_extension("legacy-writers").exists(), "the record is removed");
    }

    #[test]
    fn a_restarted_instance_keeps_waiting_for_a_recorded_legacy_writer() {
        // Reviews F5-01 and S6-02, restart while waiting: a new instance
        // would not detect the process again (it is no longer "recently
        // started"), so it relies on the record, checked by process ID and
        // creation time.
        let _g = previous_guard();
        let fx = LegacyFixture::start(60, "restart");
        let m = marker("restart");
        let record = m.with_extension("legacy-writers");
        // A record whose creation time does not match (the ID was reused).
        std::fs::write(&record, format!("{} 123\n", fx.pid)).unwrap();
        let _ = startup_with(m.clone(), &nothing_new(), &job_name(), Duration::from_secs(60));
        assert_eq!(
            previous_writer(),
            None,
            "another process with a reused ID is not waited for"
        );
        assert!(!record.exists(), "and the record is updated");
        // The first instance detects it and records it ...
        let _ = startup_with(m.clone(), &credible(), &job_name(), Duration::from_secs(60));
        assert!(previous_writer().is_some());
        assert!(std::fs::read_to_string(&record)
            .unwrap()
            .contains(&format!("\n{} ", fx.pid)));
        // ... and the restarted one (which detects nothing new) still waits.
        reset_keep_record();
        let _ = startup_with(m.clone(), &nothing_new(), &job_name(), Duration::from_secs(60));
        let b = previous_writer().expect("still blocking after the restart");
        assert!(b.reason.contains(&fx.pid.to_string()), "{b:?}");
        fx.stop();
        assert_eq!(previous_writer(), None);
        assert!(!record.exists());
    }

    #[test]
    fn an_unreadable_record_keeps_blocking_after_a_restart() {
        // Review S6-02: the restart happens after the discovery window and
        // the record cannot be read (opened exclusively by someone): that is
        // not "no previous writer". Once it is readable, the recorded writer
        // is checked; it blocks until it has exited.
        use std::os::windows::fs::OpenOptionsExt;
        let _g = previous_guard();
        let fx = LegacyFixture::start(60, "unreadable");
        let m = marker("unreadable");
        let record = m.with_extension("legacy-writers");
        let _ = startup_with(m.clone(), &credible(), &job_name(), Duration::from_millis(300));
        assert!(record.exists());
        reset_keep_record();
        let exclusive = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&record)
            .unwrap();
        let _ = startup_with(m.clone(), &nothing_new(), &job_name(), Duration::from_millis(300));
        let b = previous_writer().expect("an unreadable record blocks");
        assert!(b.reason.contains("cannot be used"), "{b:?}");
        std::thread::sleep(Duration::from_millis(500));
        let b = previous_writer().expect("also after the deadline");
        assert!(b.overdue, "{b:?}");
        assert!(!wait_for_writers(Duration::from_millis(200)));
        drop(exclusive);
        let b = previous_writer().expect("readable again: the recorded writer still runs");
        assert!(
            b.reason.contains(&fx.pid.to_string()) && !b.reason.contains("cannot be used"),
            "{b:?}"
        );
        fx.stop();
        assert_eq!(previous_writer(), None, "confirmed gone");
        assert!(!record.exists());
    }

    #[test]
    fn the_record_is_replaced_only_when_the_tracked_set_changes() {
        // Review S6-02: no rewrite on every check; a failed update is
        // reported and retried, and tracking goes on meanwhile.
        use std::os::windows::fs::OpenOptionsExt;
        let _g = previous_guard();
        let a = LegacyFixture::start(60, "record-a");
        let b = LegacyFixture::start(60, "record-b");
        let m = marker("record");
        let record = m.with_extension("legacy-writers");
        let _ = startup_with(m.clone(), &credible(), &job_name(), Duration::from_secs(60));
        let written = std::fs::metadata(&record).unwrap().modified().unwrap();
        for _ in 0..3 {
            std::thread::sleep(Duration::from_millis(30));
            assert!(previous_writer().is_some());
        }
        assert_eq!(
            std::fs::metadata(&record).unwrap().modified().unwrap(),
            written,
            "not rewritten"
        );
        // A changes; the record cannot be replaced right now.
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&record)
            .unwrap();
        a.stop();
        let r = previous_writer().expect("B still blocks");
        assert!(
            r.reason.contains(&b.pid.to_string()) && !r.reason.contains(&format!(" {},", a.pid)),
            "{r:?}"
        );
        assert!(r.reason.contains("cannot be updated"), "{r:?}");
        drop(lock);
        let r = previous_writer().expect("B still blocks");
        assert!(!r.reason.contains("cannot be updated"), "{r:?}");
        let text = std::fs::read_to_string(&record).unwrap();
        assert!(
            text.contains(&format!("\n{} ", b.pid)) && !text.contains(&format!("\n{} ", a.pid)),
            "{text}"
        );
        b.stop();
        assert_eq!(previous_writer(), None);
        assert!(!record.exists());
    }

    /// Forgets the in-memory state, as an exiting instance does, but keeps
    /// the record on disk.
    fn reset_keep_record() {
        let _ = std::mem::take(&mut *PREVIOUS.lock().unwrap_or_else(|p| p.into_inner()));
    }

    #[test]
    fn an_inaccessible_previous_job_blocks_past_the_deadline_until_it_is_gone() {
        // Review F5-01: a job that exists but cannot be opened is
        // indeterminate, not absent, also after the deadline.
        let _g = previous_guard();
        let name = format!("Local\\KeyDock.TestDeniedPrevious.{}", std::process::id());
        let job = super::child::tests::denied_job(&name);
        let _ = startup_with(marker("denied"), &nothing_new(), &name, Duration::from_millis(300));
        assert!(previous_writer().is_some(), "blocks");
        std::thread::sleep(Duration::from_millis(500));
        let b = previous_writer().expect("still blocking after the deadline");
        assert!(b.overdue, "{b:?}");
        assert!(!own_busy(), "this instance's own restore is not involved");
        // Proof: the job is gone.
        drop(job);
        assert_eq!(previous_writer(), None);
    }

    #[test]
    fn successive_launches_each_get_a_fresh_job() {
        // Review S6-03 at the launcher: back-to-back inert children (much
        // faster than a restore), each in the job the worker would use. A
        // reused, just-terminated job refuses some of them on Windows (see
        // child::tests::diagnose_job_reuse_after_termination).
        let name = format!("Local\\KeyDock.TestNextJob.{}", std::process::id());
        let cmd = std::path::PathBuf::from(
            std::env::var("ComSpec").unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".into()),
        );
        let mut slot = None;
        for i in 0..8 {
            let job = next_job(&mut slot, &name).unwrap();
            let r = Child::spawn(job, &cmd, &["/d", "/c", "exit 0"]).and_then(|c| c.wait(Duration::from_secs(5), None));
            assert_eq!(r.map(|f| f.end), Ok(End::Exited(0)), "launch {i}");
        }
        // A job that still has a process is never replaced.
        let job = next_job(&mut slot, &name).unwrap();
        let ready = std::env::temp_dir().join(format!("keydock-nextjob-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&ready);
        let c = super::child::tests::spawn_helper(job, "sleep:30", &ready);
        let (_, c) = super::child::tests::wait_ready_child(c, &ready);
        assert!(
            next_job(&mut slot, &name).is_err(),
            "the previous restore has not ended"
        );
        slot.as_ref().unwrap().terminate();
        let _ = c.wait(Duration::from_secs(5), None);
        let _ = std::fs::remove_file(&ready);
    }

    #[test]
    fn two_restores_in_a_row_each_get_a_job() {
        // Review S6-03: two inert restores back to back in one worker (no
        // pause between them); each runs in a fresh job, which is released
        // once its restore has ended.
        let _g = previous_guard();
        let Ok(expected) = snapshot() else {
            eprintln!("no language list readable here; skipped");
            return;
        };
        let want = expected.languages.clone();
        let list = expected.expected_list();
        let owned: Vec<(&str, Vec<&str>)> = list
            .iter()
            .map(|(t, tips)| (t.as_str(), tips.iter().map(String::as_str).collect()))
            .collect();
        let refs: Vec<(&str, &[&str])> = owned.iter().map(|(t, tips)| (*t, tips.as_slice())).collect();
        let script = restore_script(&want, &list).unwrap();
        let cancel = Event::new().unwrap();
        let out = temp("twice");
        let ok = format!("{}{script}", prelude(&refs, &out, None));
        for i in 0..3 {
            assert_eq!(
                run(&expected, &want, &ok, &cancel),
                Outcome::Restored(want.clone()),
                "restore {i}"
            );
            assert!(JOB.lock().unwrap().is_some(), "kept until the next restore checks it");
            assert!(!own_busy(), "and empty");
        }
        let _ = std::fs::remove_file(&out);
    }

    /// The fixture's creation time (for records in other modules' tests).
    pub(crate) fn fixture_created(pid: u32) -> u64 {
        process_created(pid).unwrap()
    }

    /// A complete, confirmed record naming that process.
    pub(crate) fn complete_record(pid: u32, created: u64) -> String {
        record_text(&[(pid, created, true)])
    }

    /// The fixture's sleeper stands for a KeyDock restore in these tests.
    fn fixture_is_a_restore(c: &str) -> bool {
        c.contains("kdhelper=sleep:")
    }

    /// Writes a 0.3.7-format record (`pid created`, no header).
    fn write_0_3_7_record(record: &std::path::Path, entries: &[(u32, u64)]) {
        let text: String = entries.iter().map(|(p, c)| format!("{p} {c}\n")).collect();
        std::fs::write(record, text).unwrap();
    }

    /// Replaces the in-memory state with a record KeyDock cannot use, as if
    /// it had become unreadable right now (see the under-the-lock test).
    pub(crate) fn make_record_unusable_now() {
        let mut g = PREVIOUS.lock().unwrap_or_else(|p| p.into_inner());
        g.record = None;
        g.record_unreadable = Some("unusable (test)".into());
    }

    #[test]
    fn a_0_3_7_record_of_an_unrelated_process_is_dropped() {
        // Review S7-01: 0.3.7 recorded every orphan it found. A recorded,
        // still running process whose readable command line is not a
        // KeyDock restore is not waited for, and the record goes.
        let _g = previous_guard();
        let fx = LegacyFixture::start(60, "m-unrelated");
        let m = marker("m-unrelated");
        let record = m.with_extension("legacy-writers");
        write_0_3_7_record(&record, &[(fx.pid, process_created(fx.pid).unwrap())]);
        let _ = startup_with(m.clone(), &nothing_new(), &job_name(), Duration::from_millis(300));
        assert_eq!(previous_writer(), None);
        assert!(!record.exists(), "the record is updated");
        assert!(fx.alive(), "nothing touched");
        fx.stop();
    }

    #[test]
    fn a_0_3_7_record_of_a_restore_is_confirmed_and_keeps_blocking() {
        // Review S7-01: the same record for a process that is a restore
        // (here: recognized as one) is confirmed, rewritten in the versioned
        // format, and blocks past the deadline until the process has ended.
        let _g = previous_guard();
        let fx = LegacyFixture::start(60, "m-restore");
        let m = marker("m-restore");
        let record = m.with_extension("legacy-writers");
        let created = process_created(fx.pid).unwrap();
        write_0_3_7_record(&record, &[(fx.pid, created)]);
        let restart = Discovery {
            image: "no-such-image.exe".into(),
            recognize: fixture_is_a_restore,
            command_line,
        };
        let _ = startup_with(m.clone(), &restart, &job_name(), Duration::from_millis(300));
        let b = previous_writer().expect("blocks");
        assert!(b.reason.contains(&fx.pid.to_string()), "{b:?}");
        assert_eq!(
            std::fs::read_to_string(&record).unwrap(),
            record_text(&[(fx.pid, created, true)]),
            "rewritten as confirmed"
        );
        std::thread::sleep(Duration::from_millis(500));
        assert!(previous_writer().expect("past the deadline").overdue);
        fx.stop();
        assert_eq!(previous_writer(), None);
        assert!(!record.exists());
    }

    #[test]
    fn a_0_3_7_record_with_a_reused_or_missing_process_is_dropped() {
        // Review S7-01: the recorded identity no longer exists (its ID now
        // belongs to another process, or to none).
        let _g = previous_guard();
        let fx = LegacyFixture::start(60, "m-reused");
        let m = marker("m-reused");
        let record = m.with_extension("legacy-writers");
        write_0_3_7_record(&record, &[(fx.pid, 123), (0x7FFF_FFF0, 456)]);
        let restart = Discovery {
            image: "no-such-image.exe".into(),
            recognize: fixture_is_a_restore,
            command_line,
        };
        let _ = startup_with(m.clone(), &restart, &job_name(), Duration::from_secs(60));
        assert_eq!(previous_writer(), None, "neither is the recorded process");
        assert!(!record.exists());
        fx.stop();
    }

    static COMMAND_LINE_DENIED: AtomicBool = AtomicBool::new(false);

    fn command_line_or_denied(pid: u32) -> Result<String, String> {
        if COMMAND_LINE_DENIED.load(Ordering::SeqCst) {
            Err("cannot open it: access is denied (test)".into())
        } else {
            command_line(pid)
        }
    }

    #[test]
    fn a_recorded_process_that_cannot_be_identified_blocks_until_it_can() {
        // Review S7-01: a recorded process whose command line cannot be read
        // is indeterminate: it keeps blocking (also past the deadline, and
        // across a restart, as unverified) until the command line can be
        // read; then it is attributed (here: not a restore, so dropped).
        let _g = previous_guard();
        let fx = LegacyFixture::start(60, "m-denied");
        let m = marker("m-denied");
        let record = m.with_extension("legacy-writers");
        let created = process_created(fx.pid).unwrap();
        write_0_3_7_record(&record, &[(fx.pid, created)]);
        let restart = Discovery {
            image: "no-such-image.exe".into(),
            recognize: keydock::langorder::is_historical_restore,
            command_line: command_line_or_denied,
        };
        COMMAND_LINE_DENIED.store(true, Ordering::SeqCst);
        let _ = startup_with(m.clone(), &restart, &job_name(), Duration::from_millis(300));
        let b = previous_writer().expect("indeterminate: blocks");
        assert!(b.reason.contains("cannot be identified"), "{b:?}");
        assert_eq!(
            std::fs::read_to_string(&record).unwrap(),
            format!("{} {created}\n", fx.pid),
            "kept (unchanged, still unverified) for a restart"
        );
        std::thread::sleep(Duration::from_millis(500));
        assert!(previous_writer().expect("still").overdue);
        COMMAND_LINE_DENIED.store(false, Ordering::SeqCst);
        assert_eq!(previous_writer(), None, "readable now: not a restore");
        assert!(!record.exists());
        fx.stop();
    }

    #[test]
    fn an_empty_record_blocks_until_it_is_readable_or_gone() {
        // Review S7-03: an existing empty (or blank) record, its process
        // still running and past the discovery window (nothing finds it
        // anew): indeterminate, blocking, also past the deadline.
        let _g = previous_guard();
        let fx = LegacyFixture::start(60, "empty");
        let m = marker("empty");
        let record = m.with_extension("legacy-writers");
        for blank in ["", "  \r\n\n"] {
            reset_previous();
            std::fs::write(&record, blank).unwrap();
            let _ = startup_with(m.clone(), &nothing_new(), &job_name(), Duration::from_millis(300));
            let b = previous_writer().expect("an empty record blocks");
            assert!(b.reason.contains("is empty"), "{b:?}");
        }
        std::thread::sleep(Duration::from_millis(500));
        assert!(previous_writer().expect("past the deadline").overdue);
        assert!(!wait_for_writers(Duration::from_millis(200)));
        assert!(record.exists(), "never removed by KeyDock while indeterminate");
        // The evidence becomes readable: the recorded writer still runs.
        let created = process_created(fx.pid).unwrap();
        std::fs::write(&record, record_text(&[(fx.pid, created, true)])).unwrap();
        let b = previous_writer().expect("the recorded writer blocks");
        assert!(
            b.reason.contains(&fx.pid.to_string()) && !b.reason.contains("is empty"),
            "{b:?}"
        );
        fx.stop();
        assert_eq!(previous_writer(), None);
        assert!(!record.exists());
        // The documented recovery: the file is deleted.
        std::fs::write(&record, "").unwrap();
        let _ = startup_with(m.clone(), &nothing_new(), &job_name(), Duration::from_secs(60));
        assert!(previous_writer().is_some());
        std::fs::remove_file(&record).unwrap();
        assert_eq!(previous_writer(), None, "deleted: no record, no writer");
    }

    #[test]
    fn an_old_looking_damaged_record_is_never_discarded_by_itself() {
        // Review S8-01: a damaged record whose file time lies before Windows
        // last started (as a forward clock correction can make it look; only
        // the file's time is changed here, never the clock) while the
        // process it names still runs: it keeps blocking. Only a complete
        // record and the process's end, or the user deleting it, end that.
        let _g = previous_guard();
        let fx = LegacyFixture::start(60, "preboot");
        let m = marker("preboot");
        let record = m.with_extension("legacy-writers");
        let created = process_created(fx.pid).unwrap();
        let truncated = format!("{} {}", fx.pid, created / 10);
        for content in ["", "garbage\n", RECORD_HEADER, truncated.as_str()] {
            reset_previous();
            std::fs::write(&record, content).unwrap();
            let f = std::fs::OpenOptions::new().write(true).open(&record).unwrap();
            f.set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000))
                .unwrap();
            drop(f);
            let _ = startup_with(m.clone(), &nothing_new(), &job_name(), Duration::from_millis(200));
            let b = previous_writer().unwrap_or_else(|| panic!("{content:?} must block"));
            assert!(b.reason.contains("cannot be used"), "{content:?}: {b:?}");
            std::thread::sleep(Duration::from_millis(300));
            assert!(previous_writer().is_some_and(|b| b.overdue), "{content:?}");
            assert!(record.exists(), "{content:?}: not deleted by KeyDock");
        }
        // A complete record: the writer it names blocks until it has ended.
        std::fs::write(&record, record_text(&[(fx.pid, created, true)])).unwrap();
        let b = previous_writer().expect("the recorded writer still runs");
        assert!(
            b.reason.contains(&fx.pid.to_string()) && !b.reason.contains("cannot be used"),
            "{b:?}"
        );
        fx.stop();
        assert_eq!(previous_writer(), None);
        assert!(!record.exists());
    }

    #[test]
    fn truncated_records_keep_blocking_after_a_restart() {
        // Review S8-02 at startup: a versioned header alone, and a 0.3.7 row
        // cut off in its start time (digits only, no line end), for a live
        // process that nothing finds anew: the persisted evidence alone must
        // keep the gate closed (a partial start time must not pass for
        // "its ID was reused"). Then the complete record, and the writer's
        // end, open it.
        let _g = previous_guard();
        let fx = LegacyFixture::start(60, "truncated");
        let m = marker("truncated");
        let record = m.with_extension("legacy-writers");
        let created = process_created(fx.pid).unwrap();
        let full = format!("{} {created}\n", fx.pid);
        let cut = &full[..full.len() - 2];
        for content in [format!("{RECORD_HEADER}\n"), RECORD_HEADER.to_string(), cut.to_string()] {
            reset_previous();
            std::fs::write(&record, &content).unwrap();
            let _ = startup_with(m.clone(), &nothing_new(), &job_name(), Duration::from_secs(60));
            let b = previous_writer().unwrap_or_else(|| panic!("{content:?} must block"));
            assert!(b.reason.contains("cannot be used"), "{content:?}: {b:?}");
            assert!(writer_gate().is_some());
            assert!(record.exists());
        }
        // The complete row (0.3.7 format): unverified, attributed by its
        // command line; the fixture stands for a restore here.
        reset_previous();
        std::fs::write(&record, &full).unwrap();
        let restart = Discovery {
            image: "no-such-image.exe".into(),
            recognize: fixture_is_a_restore,
            command_line,
        };
        let _ = startup_with(m.clone(), &restart, &job_name(), Duration::from_secs(60));
        let b = previous_writer().expect("the recorded restore still runs");
        assert!(b.reason.contains(&fx.pid.to_string()), "{b:?}");
        fx.stop();
        assert_eq!(previous_writer(), None, "proved gone");
        assert!(!record.exists());
    }

    #[test]
    fn record_formats() {
        // Reviews S7-01 and S7-03.
        assert_eq!(
            parse_record("12 34\n56 78\n"),
            Ok(vec![(12, 34, false), (56, 78, false)])
        );
        assert_eq!(
            parse_record(&record_text(&[(1, 2, true), (3, 4, false)])),
            Ok(vec![(1, 2, true), (3, 4, false)])
        );
        assert!(parse_record("").is_err());
        assert!(parse_record(" \n\t\n").is_err());
        assert!(parse_record("keydock-legacy-writers 3\n1 2 confirmed\n").is_err());
        assert!(parse_record("keydock-legacy-writers 2\n1 2\n").is_err());
        assert!(parse_record("12 x\n").is_err());
        // Review S8-02: cut-off records.
        assert!(parse_record("keydock-legacy-writers 2\n").is_err(), "a header alone");
        assert!(parse_record("keydock-legacy-writers 2").is_err());
        assert!(
            parse_record("12 3439734225").is_err(),
            "a start time without its line end"
        );
        assert!(parse_record("12 34\n56 7").is_err(), "the last of two rows");
        assert!(parse_record("keydock-legacy-writers 2\n1 2 confirmed").is_err());
        assert!(parse_record("keydock-legacy-writers 2\n1 2 confir").is_err());
        // Complete records still parse (also with a CRLF line end).
        assert_eq!(parse_record("12 34\r\n"), Ok(vec![(12, 34, false)]));
        assert_eq!(
            parse_record("keydock-legacy-writers 2\n1 2 confirmed\n"),
            Ok(vec![(1, 2, true)])
        );
    }

    #[test]
    fn outcomes_describe_problems() {
        assert_eq!(Outcome::Restored(v(&["en-US"])).problem(), None);
        assert!(Outcome::Conflict("x".into())
            .problem()
            .unwrap()
            .contains("not restored"));
        assert!(Outcome::Failed("x".into()).problem().unwrap().contains("failed"));
        assert!(Outcome::Mismatch("x".into())
            .problem()
            .unwrap()
            .contains("did not take effect"));
        assert!(Outcome::Cancelled.problem().is_some());
    }
}
