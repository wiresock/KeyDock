//! Hidden top-level window: device, power and session notifications, the
//! evaluation timer and the notification-area (tray) icon.
//!
//! Events are recorded in the scheduler (`keydock::schedule`) with their
//! type and time as they arrive, without borrowing the app, so an event
//! delivered re-entrantly while the app works keeps its hold-off or resume
//! delay. One Win32 timer is armed for the scheduler's due time; a timer
//! that fires early (a stale `WM_TIMER`) re-arms instead of evaluating.

use super::{langorder, settings_ui};
use super::{Engine, Outcome};
use keydock::schedule::{AfterFailure, Scheduler, Timing, Trigger};
use keydock::select::ApplyMode;
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::path::Path;
use std::time::Duration;
use windows::core::{w, PCWSTR};
use windows::Win32::Devices::HumanInterfaceDevice::GUID_DEVINTERFACE_KEYBOARD;
use windows::Win32::Foundation::{GetLastError, HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetMessageW, GetSystemMetrics, IsDialogMessageW, KillTimer, LoadImageW, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterDeviceNotificationW, RegisterWindowMessageW, SetForegroundWindow,
    SetMenuDefaultItem, SetTimer, TrackPopupMenu, TranslateMessage, UnregisterDeviceNotification, DBT_DEVICEARRIVAL,
    DBT_DEVICEREMOVECOMPLETE, DBT_DEVTYP_DEVICEINTERFACE, DEVICE_NOTIFY_WINDOW_HANDLE, DEV_BROADCAST_DEVICEINTERFACE_W,
    HDEVNOTIFY, HICON, IMAGE_ICON, LR_DEFAULTCOLOR, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG,
    PBT_APMRESUMEAUTOMATIC, SM_CXSMICON, SW_SHOWNORMAL, SYSTEM_METRICS_INDEX, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_DEVICECHANGE, WM_ENDSESSION, WM_LBUTTONDBLCLK,
    WM_NULL, WM_POWERBROADCAST, WM_QUERYENDSESSION, WM_RBUTTONUP, WM_TIMER, WM_WTSSESSION_CHANGE, WNDCLASSW,
    WS_OVERLAPPED, WTS_CONSOLE_CONNECT, WTS_REMOTE_CONNECT, WTS_SESSION_UNLOCK,
};

/// Window class of the hidden window; a second instance finds it by this name.
pub const MAIN_CLASS: PCWSTR = w!("KeyDockHiddenWindow");
/// Posted to the running instance to open the settings window.
pub const WM_OPEN_SETTINGS: u32 = WM_APP + 2;
/// Posted by the language-order worker when a restore has ended.
pub const WM_LANGORDER_DONE: u32 = WM_APP + 3;
/// Posted by `keydock.exe --once` while this instance runs: re-apply now.
pub const WM_REAPPLY: u32 = WM_APP + 4;
/// Posted to ourselves when `SetTimer` fails: check the scheduler now.
const WM_KD_DUE: u32 = WM_APP + 5;

/// Replies to `WM_REAPPLY` (sent by `keydock.exe --once`).
pub const HANDOFF_QUEUED: isize = 1;
pub const HANDOFF_PAUSED: isize = 2;

// Icon resource IDs (see build.rs).
pub const IDI_APP: u16 = 1;
pub const IDI_DOCKED: u16 = 2;
pub const IDI_LAPTOP: u16 = 3;
pub const IDI_PAUSED: u16 = 4;

const TIMER_ID: usize = 1;
const WM_TRAY: u32 = WM_APP + 1;
const TRAY_ID: u32 = 1;
const CMD_SETTINGS: usize = 99;
const CMD_PAUSE: usize = 100;
const CMD_REAPPLY: usize = 101;
const CMD_CONFIG: usize = 102;
const CMD_LOG: usize = 103;
const CMD_EXIT: usize = 104;
/// While a restore runs, a deferred evaluation is looked at again this
/// often, in case the completion message is lost.
const BUSY_POLL_MS: u32 = 5000;
/// A previous writer that is overdue is looked at again this often (bounded
/// polling: it is reported, and KeyDock recovers once it has ended).
const BUSY_POLL_OVERDUE_MS: u32 = 30_000;
/// How long Exit waits for a running language-order restore before it
/// stops it (sign-out uses the shorter delay: Windows does not wait long).
const DRAIN_EXIT_MS: u32 = 3000;
const DRAIN_ENDSESSION_MS: u32 = 1500;

pub struct App {
    pub engine: Engine,
    hwnd: HWND,
    tray: bool,
    /// Why the last evaluation changed nothing because of the session.
    remote: Option<String>,
    /// The configuration error the tray last warned about.
    shown_error: Option<String>,
    /// The problems the tray last warned about.
    shown_problems: Vec<String>,
    /// Settings waits for the result of its Save/Apply.
    save_status: SaveStatus,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    /// When to evaluate (see `keydock::schedule`). Kept outside `App`, so
    /// that a message delivered while the app is busy (re-entrantly, e.g.
    /// while InstallLayoutOrTip waits) is still recorded with its own type
    /// and time; its methods do no I/O, so this borrow is never held across
    /// a message pump.
    static SCHED: RefCell<Scheduler> = RefCell::new(Scheduler::default());
    /// The configured delays, outside `App` for the same reason.
    static TIMING: Cell<Timing> = Cell::new(Timing::default());
    /// Paused from the tray menu or Settings, outside `App` for the same reason.
    static PAUSED: Cell<bool> = const { Cell::new(false) };
    static MAIN: Cell<HWND> = Cell::new(HWND::default());
    /// A WM_KD_DUE fallback is posted and SetTimer has not worked since.
    static DUE_POSTED: Cell<bool> = const { Cell::new(false) };
    /// How long shutdown waits for a language-order restore.
    static DRAIN_MS: Cell<u32> = const { Cell::new(DRAIN_EXIT_MS) };
    /// Icons loaded once (id, size); the shell copies them, so they are reused.
    static ICONS: RefCell<Vec<(u16, i32, HICON)>> = const { RefCell::new(Vec::new()) };
    /// Registered "TaskbarCreated" message (Explorer restarted).
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
    /// Device-notification registration, kept outside `App` so shutdown can
    /// always release it, even while the app is borrowed.
    static DEVNOTIFY: Cell<Option<HDEVNOTIFY>> = const { Cell::new(None) };
    /// Set once shutdown cleanup has run.
    static SHUT_DOWN: Cell<bool> = const { Cell::new(false) };
}

fn delete_tray_icon(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        ..Default::default()
    };
    // SAFETY: identifies the icon added in update_tray; harmless if absent.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// Releases everything the main window registered: closes Settings, stops
/// the timer, removes the tray icon and unregisters notifications, then
/// lets a running language-order restore finish for a bounded time and
/// stops it. Runs once, from WM_DESTROY (Exit, WM_CLOSE from an installer or
/// Restart Manager, or WM_ENDSESSION). Does not need the `App` borrow.
fn shutdown(hwnd: HWND) {
    if SHUT_DOWN.with(|s| s.replace(true)) {
        return;
    }
    if let Some(h) = settings_ui::hwnd() {
        // SAFETY: our own window.
        unsafe {
            let _ = DestroyWindow(h);
        }
    }
    delete_tray_icon(hwnd);
    // SAFETY: handles registered in `run`; failures are irrelevant at exit.
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_ID);
        if let Some(n) = DEVNOTIFY.with(Cell::take) {
            let _ = UnregisterDeviceNotification(n);
        }
        let _ = WTSUnRegisterSessionNotification(hwnd);
    }
    langorder::shutdown(Duration::from_millis(u64::from(DRAIN_MS.with(Cell::get))));
}

fn sched<R>(f: impl FnOnce(&mut Scheduler) -> R) -> R {
    SCHED.with(|s| f(&mut s.borrow_mut()))
}

/// Milliseconds of a monotonic clock.
fn now_ms() -> u64 {
    // SAFETY: no preconditions.
    unsafe { GetTickCount64() }
}

fn timing(cfg: &keydock::config::Config) -> Timing {
    Timing {
        debounce_ms: cfg.debounce_ms,
        resume_delay_ms: cfg.resume_delay_ms,
        removal_delay_ms: cfg.removal_delay_ms,
    }
}

/// Arms the timer for the scheduler's due time (nothing if nothing is pending).
fn arm(why: Option<&str>) {
    let Some(due) = sched(|s| s.due()) else { return };
    let ms = u32::try_from(due.saturating_sub(now_ms())).unwrap_or(u32::MAX).max(1);
    if let Some(why) = why {
        info!("{why}; evaluating in {ms} ms");
    }
    let hwnd = MAIN.with(Cell::get);
    // SAFETY: SetTimer with an existing ID resets it.
    if unsafe { SetTimer(Some(hwnd), TIMER_ID, ms, None) } == 0 {
        // SAFETY: plain query right after the failed call.
        let e = unsafe { GetLastError() };
        error!("SetTimer failed ({e:?}); checking again on the next message");
        // One fallback message per failure streak: an early fallback that
        // cannot arm the timer either does not post itself again.
        if !DUE_POSTED.with(|p| p.replace(true)) {
            // SAFETY: posting to our own window.
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_KD_DUE, WPARAM(0), LPARAM(0));
            }
        }
    } else {
        DUE_POSTED.with(|p| p.set(false));
    }
}

/// The delays new events use now (tests).
#[cfg(test)]
pub fn current_timing() -> Timing {
    TIMING.with(Cell::get)
}

/// The scheduler's delays from a configuration that was just accepted
/// (review TP-06). Deadlines already recorded keep the delay they were
/// recorded with; new events use the new one.
pub fn publish_timing(cfg: &keydock::config::Config) {
    TIMING.with(|t| t.set(timing(cfg)));
}

/// `WM_REAPPLY`: recorded in the scheduler without the app (so it is not
/// lost when it arrives while the app is busy), and answered at once.
///
/// The answer is only "paused" or "queued" (review FP-07): whether the
/// configuration can be used and whether this session may change layouts is
/// decided by the queued evaluation, which reads both fresh from disk, not
/// from the previous evaluation (a file repaired since then is applied).
fn handoff() -> isize {
    if PAUSED.with(Cell::get) {
        info!("re-apply requested by keydock.exe --once, but paused");
        return HANDOFF_PAUSED;
    }
    info!("re-apply requested by keydock.exe --once");
    sched(|s| s.manual(now_ms()));
    arm(Some("re-apply queued"));
    HANDOFF_QUEUED
}

/// A keyboard, power or session event, recorded with its type and time.
fn on_event(trigger: Trigger, why: &str) {
    if PAUSED.with(Cell::get) {
        return;
    }
    let t = TIMING.with(Cell::get);
    sched(|s| s.on_event(trigger, now_ms(), &t));
    arm(Some(why));
}

/// The timer fired (or the fallback message arrived): evaluate if due.
fn on_due() {
    let Some(mode) = sched(|s| s.on_timer(now_ms())) else {
        // Early or stale: nothing consumed.
        arm(None);
        return;
    };
    if with_app(|a| a.evaluate(mode)).is_none() {
        // The app is busy (a re-entrant timer): keep the request and look
        // again shortly.
        sched(|s| {
            s.requeue(mode);
            s.request(mode, 200, now_ms())
        });
        arm(None);
    }
    // Outside the app borrow: refresh the settings window's device list.
    settings_ui::refresh_devices();
}

/// Runs `f` with the app; `None` if it is already borrowed (a message
/// delivered re-entrantly while it works). Events do not need the app; see
/// the module documentation.
pub fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => guard.as_mut().map(f),
        Err(_) => None,
    })
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn copy_str(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

/// An embedded icon (see build.rs) at the size of a system metric such as
/// `SM_CXSMICON`. Each (id, size) is loaded once and kept for the process
/// lifetime: Shell_NotifyIconW and WM_SETICON copy or share the handle, and
/// loading it again on every tray update would leak one icon each time.
pub fn cached_icon(id: u16, metric: SYSTEM_METRICS_INDEX) -> HICON {
    // SAFETY: plain metric query.
    let size = unsafe { GetSystemMetrics(metric) };
    if let Some(h) = ICONS.with(|c| c.borrow().iter().find(|(i, s, _)| *i == id && *s == size).map(|e| e.2)) {
        return h;
    }
    // SAFETY: icon resource embedded by build.rs.
    let h = unsafe {
        let inst = GetModuleHandleW(None).ok().map(Into::into);
        LoadImageW(
            inst,
            PCWSTR(id as usize as *const u16),
            IMAGE_ICON,
            size,
            size,
            LR_DEFAULTCOLOR,
        )
        .map(|h| HICON(h.0))
        .unwrap_or_default()
    };
    if !h.is_invalid() {
        ICONS.with(|c| c.borrow_mut().push((id, size, h)));
    }
    h
}

/// First line of a (possibly multi-line) error message.
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

/// What comes after a failed evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    /// A keyboard is still being set up: it is sampled again soon.
    Resample,
    /// An ordinary retry is scheduled.
    Retry { attempt: usize, of: usize },
    /// The retries are used up.
    GiveUp,
}

/// The result of a Save/Apply that Settings shows (review KD-02). It stays
/// pending while the apply is still being worked on (a keyboard that is
/// being set up is sampled again, or an ordinary retry is scheduled): then
/// Settings says so, and the real result follows. Only the end (success, or
/// the retries used up) is final. A newer save, paused or not, supersedes a
/// pending one. The result belongs to the Settings window (session) that
/// submitted it and is shown only there: a window opened later, after that
/// one was closed, never gets it (final review).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SaveStatus {
    /// The submitting window's session, while its result is pending.
    pending: Option<u64>,
}

impl SaveStatus {
    /// A save from Settings window `session` waits for its result.
    fn saved(&mut self, session: Option<u64>) {
        self.pending = session;
    }

    /// A newer save that applies nothing (paused) replaces a pending one.
    fn superseded(&mut self) {
        self.pending = None;
    }

    /// An evaluation succeeded; the text for Settings and its recipient, if
    /// a save waits for it.
    fn succeeded(&mut self, restoring: bool, result: String) -> Option<(u64, String)> {
        let to = self.pending?;
        if restoring {
            // The final result comes with the check after the restore.
            return Some((to, "Saved and applied; restoring the language order\u{2026}".into()));
        }
        self.pending = None;
        Some((to, result))
    }

    /// An evaluation failed; the text for Settings and its recipient, if a
    /// save waits for it.
    fn failed(&mut self, e: &str, next: Next) -> Option<(u64, String)> {
        let to = self.pending?;
        Some((
            to,
            match next {
                Next::Resample => "Saved; waiting for a keyboard that is still being set up\u{2026}".to_string(),
                Next::Retry { attempt, of } => format!(
                    "Saved; applying did not work yet ({}); trying again ({attempt} of {of})\u{2026}",
                    first_line(e)
                ),
                Next::GiveUp => {
                    self.pending = None;
                    format!("Saved, but applying failed: {}", first_line(e))
                }
            },
        ))
    }
}

/// What a manual action led to.
pub enum Manual {
    /// Evaluated now (the result is already shown).
    Ran,
    /// Runs later; why.
    Deferred(String),
}

impl App {
    /// What needs attention now, stage by stage (see `keydock::status`).
    fn problems(&self) -> Vec<String> {
        self.engine.stages.problems()
    }

    pub fn paused(&self) -> bool {
        PAUSED.with(Cell::get)
    }

    fn set_paused(&mut self, paused: bool) {
        if paused != self.paused() {
            info!("{}", if paused { "paused" } else { "resumed" });
        }
        PAUSED.with(|p| p.set(paused));
    }

    pub fn evaluate(&mut self, mode: ApplyMode) {
        if self.paused() {
            self.update_tray(false, false);
            return;
        }
        let previous = langorder::previous_writer();
        if langorder::own_busy() || previous.is_some() {
            // A layout change now could be undone by the restore; the worker
            // posts WM_LANGORDER_DONE when it ends (and this is looked at
            // again after BUSY_POLL_MS in case that message is lost). A
            // previous writer that is overdue is reported and looked at less
            // often, until it is known to have ended (review F5-01).
            let overdue = previous.as_ref().is_some_and(|b| b.overdue);
            let poll = if overdue { BUSY_POLL_OVERDUE_MS } else { BUSY_POLL_MS };
            sched(|s| s.defer_busy(mode, poll, now_ms()));
            if let Some(b) = &previous {
                // Live status (review S6-05): the next evaluation that runs
                // replaces it; the balloon only once it is overdue.
                self.engine.note_previous_writer(b);
                arm(Some("previous language-order restore still running"));
                self.update_tray_warn(false, overdue);
            } else {
                arm(Some("language-order restore still running"));
            }
            return;
        }
        let before = self.engine.last_applied.clone();
        let mut warn_now = false;
        let result = self.engine.evaluate(mode);
        match result {
            Ok(out) => {
                sched(Scheduler::on_success);
                self.note_session(&out);
                if out.restoring {
                    // The completion message triggers the check; this poll
                    // covers its loss (it finds the result in any case).
                    sched(|s| s.defer_busy(ApplyMode::Verify, BUSY_POLL_MS, now_ms()));
                    arm(None);
                }
                warn_now = true;
                let result = self.result_text(&out);
                if let Some((to, t)) = self.save_status.succeeded(out.restoring, result) {
                    settings_ui::show_status_for(to, &t);
                }
            }
            Err(e) => {
                error!("{e}");
                let next = if let Some(ms) = self.engine.take_settle_resample() {
                    // A keyboard is still being set up: sample it again on
                    // its own schedule, not on the failure back-off (whose
                    // retries earlier enumeration errors may have used up;
                    // review FP-06). Bounded by the settling budgets.
                    sched(|s| s.request(mode, ms, now_ms()));
                    arm(Some("keyboard still being set up"));
                    Next::Resample
                } else {
                    match sched(|s| s.on_failure(mode, now_ms())) {
                        AfterFailure::Retry { attempt, of, .. } => {
                            arm(Some(&format!("retry {attempt} of {of}")));
                            Next::Retry { attempt, of }
                        }
                        AfterFailure::GiveUp => {
                            warn!("giving up until the next keyboard, power or session event");
                            warn_now = true;
                            Next::GiveUp
                        }
                    }
                };
                if let Some((to, t)) = self.save_status.failed(&e, next) {
                    settings_ui::show_status_for(to, &t);
                }
            }
        }
        let changed = self.engine.last_applied.is_some() && self.engine.last_applied != before;
        self.update_tray_warn(changed && self.engine.cfg.notify, warn_now);
    }

    fn note_session(&mut self, out: &Outcome) {
        if out.blocked != self.remote {
            match &out.blocked {
                Some(b) => info!("{b}: not changing layouts here"),
                None => info!("console session again: changing layouts"),
            }
            self.remote = out.blocked.clone();
        }
    }

    /// The text Settings shows after its Apply ran.
    fn result_text(&self, out: &Outcome) -> String {
        if let Some(b) = &out.blocked {
            return format!("Saved; not applied: {b}.");
        }
        let problems = self.problems();
        if problems.is_empty() {
            format!("Saved and applied. {}", self.status_text())
        } else {
            format!("Saved and applied, but: {}", problems.join("; "))
        }
    }

    /// A manual action (Re-apply, Save/Apply, unpause): runs now unless the
    /// keyboards are settling or a restore runs (see `keydock::schedule`).
    pub fn manual(&mut self, why: &str) -> Manual {
        info!("{why}");
        let now = now_ms();
        let due = sched(|s| s.manual(now));
        if langorder::busy() {
            sched(|s| s.defer_busy(ApplyMode::Force, BUSY_POLL_MS, now));
            arm(None);
            return Manual::Deferred(match langorder::previous_writer() {
                Some(b) => format!("not applied yet: {}", b.reason),
                None => "applying when the language-order restore has finished".into(),
            });
        }
        if due > now {
            arm(Some("keyboards are settling"));
            return Manual::Deferred(format!(
                "applying in {} s, when the keyboards have settled",
                (due - now).div_ceil(1000)
            ));
        }
        match sched(|s| s.on_timer(now)) {
            Some(mode) => self.evaluate(mode),
            None => arm(None),
        }
        Manual::Ran
    }

    /// Applies a configuration the settings window just saved.
    pub fn apply_saved_settings(&mut self, cfg: keydock::config::Config, paused: bool, session: Option<u64>) -> Manual {
        self.set_paused(paused);
        TIMING.with(|t| t.set(timing(&cfg)));
        self.engine.cfg = cfg;
        self.engine.last_applied = None;
        if paused {
            // Supersedes a save still pending: its result is not shown.
            self.save_status.superseded();
            self.update_tray(false, false);
            settings_ui::show_status("Saved. Paused: nothing is applied until you resume.");
            return Manual::Ran;
        }
        // The result is shown in the submitting Settings window when the
        // evaluation has run (a newer save supersedes a pending one).
        self.save_status.saved(session);
        self.manual("settings saved; applying")
    }

    fn mode_text(&self) -> &'static str {
        if self.engine.external_present {
            "External keyboard connected"
        } else {
            "Laptop only"
        }
    }

    fn status_text(&self) -> String {
        let profile = match self.engine.last_applied.as_deref() {
            Some("default") => "Laptop only".to_string(),
            Some(p) => p.to_string(),
            None if keydock::settings::is_configured(&self.engine.cfg) => "none".to_string(),
            None => "not configured".to_string(),
        };
        let mut t = format!("{} \u{00B7} profile: {profile}", self.mode_text());
        if self.engine.config_error.is_some() {
            t = format!("config.toml has an error \u{00B7} {t}");
        }
        if self.paused() {
            t.push_str(" (paused)");
        } else if self.remote.is_some() {
            t.push_str(" (remote session: inactive)");
        } else if !self.problems().is_empty() {
            t.push_str(" (needs attention)");
        }
        if self.engine.dry_run {
            t.push_str(" (dry run)");
        }
        t
    }

    fn update_tray(&mut self, add: bool, balloon: bool) {
        self.update_tray_inner(add, balloon, false);
    }

    fn update_tray_warn(&mut self, balloon: bool, warn: bool) {
        self.update_tray_inner(false, balloon, warn);
    }

    fn update_tray_inner(&mut self, add: bool, balloon: bool, warn_problems: bool) {
        // Warn once per distinct configuration error, even with notifications off.
        let new_error = match &self.engine.config_error {
            Some(e) if self.shown_error.as_deref() != Some(e.as_str()) => Some(e.clone()),
            _ => None,
        };
        self.shown_error = self.engine.config_error.clone();
        // Likewise once per distinct set of problems.
        let problems = self.problems();
        let new_problem =
            (warn_problems && !problems.is_empty() && problems != self.shown_problems).then(|| problems.join("; "));
        if warn_problems {
            self.shown_problems = problems;
        }
        if !self.tray {
            return;
        }
        let icon = if self.paused() {
            IDI_PAUSED
        } else if self.engine.external_present {
            IDI_DOCKED
        } else {
            IDI_LAPTOP
        };
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: cached_icon(icon, SM_CXSMICON),
            ..Default::default()
        };
        copy_str(&mut nid.szTip, &format!("KeyDock \u{2014} {}", self.status_text()));
        if let Some(e) = &new_error {
            nid.uFlags |= NIF_INFO;
            nid.dwInfoFlags = NIIF_WARNING;
            copy_str(&mut nid.szInfoTitle, "KeyDock: config.toml has an error");
            copy_str(
                &mut nid.szInfo,
                &format!(
                    "{} Until it is fixed, KeyDock uses the last settings that loaded, if any. Open Settings or the log.",
                    first_line(e)
                ),
            );
        } else if let Some(p) = &new_problem {
            nid.uFlags |= NIF_INFO;
            nid.dwInfoFlags = NIIF_WARNING;
            copy_str(&mut nid.szInfoTitle, "KeyDock needs attention");
            copy_str(&mut nid.szInfo, p);
        } else if balloon {
            nid.uFlags |= NIF_INFO;
            nid.dwInfoFlags = NIIF_INFO;
            copy_str(&mut nid.szInfoTitle, "KeyDock");
            copy_str(&mut nid.szInfo, &format!("Switched layouts: {}", self.status_text()));
        }
        // SAFETY: nid is fully initialised.
        unsafe {
            if add || !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
                let _ = Shell_NotifyIconW(NIM_ADD, &nid);
            }
        }
    }

    fn open_args(&self) -> settings_ui::OpenArgs {
        settings_ui::OpenArgs {
            cfg: self.engine.cfg.clone(),
            state: self.engine.state.clone(),
            paused: self.paused(),
            config_path: self.engine.paths.config.clone(),
            config_error: self.engine.config_error.clone(),
        }
    }
}

/// Re-apply now (tray menu, `--once` while running).
fn reapply(why: &str) {
    with_app(|a| {
        a.manual(why);
    });
}

fn open_settings() {
    if let Some(args) = with_app(|a| a.open_args()) {
        settings_ui::open(args);
    }
}

fn open_path(path: &Path) {
    let p = to_wide(&path.to_string_lossy());
    // SAFETY: NUL-terminated strings that outlive the calls.
    unsafe {
        let r = ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(p.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        if r.0 as usize <= 32 {
            // No file association (e.g. for .toml): fall back to Notepad.
            let quoted = to_wide(&format!("\"{}\"", path.display()));
            ShellExecuteW(
                None,
                w!("open"),
                w!("notepad.exe"),
                PCWSTR(quoted.as_ptr()),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    }
}

fn show_menu(hwnd: HWND) {
    let Some((paused, tip, problem)) = with_app(|a| (a.paused(), a.status_text(), a.problems().first().cloned()))
    else {
        return;
    };
    // SAFETY: standard popup-menu sequence (SetForegroundWindow before, WM_NULL
    // after, as documented for notification-area menus).
    let cmd = unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let tip_w = to_wide(&tip);
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(tip_w.as_ptr()));
        let problem_w = problem.map(|p| {
            let short: String = p.chars().take(90).collect();
            to_wide(&format!(
                "Needs attention: {short}{}",
                if p.chars().count() > 90 { "\u{2026}" } else { "" }
            ))
        });
        if let Some(p) = &problem_w {
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(p.as_ptr()));
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, CMD_SETTINGS, w!("Settings\u{2026}"));
        let _ = SetMenuDefaultItem(menu, CMD_SETTINGS as u32, 0);
        let pause_flags = if paused { MF_STRING | MF_CHECKED } else { MF_STRING };
        let _ = AppendMenuW(menu, pause_flags, CMD_PAUSE, w!("Paused"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_REAPPLY, w!("Re-apply now"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, CMD_CONFIG, w!("Open config file"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_LOG, w!("Open log"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, CMD_EXIT, w!("Exit"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd.0 as usize
    };
    match cmd {
        CMD_SETTINGS => open_settings(),
        CMD_PAUSE => {
            with_app(|a| {
                let p = !a.paused();
                a.set_paused(p);
                if p {
                    // A restore already running finishes (it only puts the
                    // order back); no new change starts.
                    a.update_tray(false, false);
                } else {
                    a.engine.last_applied = None;
                    a.manual("resumed from Paused; applying");
                }
            });
        }
        CMD_REAPPLY => reapply("re-apply requested from the menu"),
        CMD_CONFIG => {
            if let Some(p) = with_app(|a| a.engine.paths.config.clone()) {
                // Create a starter file only if there is none (not if it
                // merely cannot be read).
                let text = keydock::config::Config::default().to_toml();
                match keydock::fsutil::write_atomic_if(&p, &text, &keydock::fsutil::Revision::Missing) {
                    Ok(()) => info!("created {}", p.display()),
                    Err(keydock::fsutil::WriteError::Conflict(_)) => {}
                    Err(e) => error!("cannot create {}: {e}", p.display()),
                }
                open_path(&p);
            }
        }
        CMD_LOG => {
            if let Some(p) = with_app(|a| a.engine.paths.log.clone()) {
                open_path(&p);
            }
        }
        CMD_EXIT => {
            // SAFETY: our own window.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        _ => {}
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_DEVICECHANGE => {
            let ev = wparam.0 as u32;
            if ev == DBT_DEVICEARRIVAL {
                on_event(Trigger::KeyboardArrived, "keyboard arrived");
            } else if ev == DBT_DEVICEREMOVECOMPLETE {
                on_event(Trigger::KeyboardRemoved, "keyboard removed");
            }
            return LRESULT(1);
        }
        WM_POWERBROADCAST => {
            if wparam.0 as u32 == PBT_APMRESUMEAUTOMATIC {
                on_event(Trigger::Resumed, "resumed from sleep");
            }
            return LRESULT(1);
        }
        WM_WTSSESSION_CHANGE => {
            let what = match wparam.0 as u32 {
                WTS_SESSION_UNLOCK => Some("session unlocked"),
                // Back on the local console, or connected over Remote Desktop.
                WTS_CONSOLE_CONNECT => Some("session connected to the console"),
                WTS_REMOTE_CONNECT => Some("session connected over Remote Desktop"),
                _ => None,
            };
            if let Some(what) = what {
                on_event(Trigger::SessionChanged, what);
            }
            return LRESULT(0);
        }
        WM_TIMER if wparam.0 == TIMER_ID => {
            // SAFETY: our own timer.
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_ID);
            }
            on_due();
            return LRESULT(0);
        }
        WM_KD_DUE => {
            on_due();
            return LRESULT(0);
        }
        WM_TRAY => {
            let ev = (lparam.0 as u32) & 0xFFFF;
            if ev == WM_RBUTTONUP || ev == WM_CONTEXTMENU {
                show_menu(hwnd);
            } else if ev == WM_LBUTTONDBLCLK {
                open_settings();
            }
            return LRESULT(0);
        }
        WM_OPEN_SETTINGS => {
            open_settings();
            return LRESULT(0);
        }
        WM_LANGORDER_DONE => {
            // Recorded outside the app, so it survives a busy app. Keeps any
            // longer deadline (hold-off, back-off).
            let debounce = TIMING.with(Cell::get).debounce_ms;
            sched(|s| s.restore_finished(debounce, now_ms()));
            arm(Some("language-order restore finished; checking the layouts"));
            return LRESULT(0);
        }
        WM_REAPPLY => {
            return LRESULT(handoff());
        }
        WM_QUERYENDSESSION => {
            // Never block sign-out, shutdown or an installer's Restart Manager.
            return LRESULT(1);
        }
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                info!("session ending or application close requested; exiting");
                DRAIN_MS.with(|d| d.set(DRAIN_ENDSESSION_MS));
                // SAFETY: our own window; cleanup runs in WM_DESTROY before
                // Windows may terminate the process after this message.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            }
            return LRESULT(0);
        }
        WM_CLOSE => {
            // Sent by Exit-like requests from outside, e.g. the installer's
            // CloseApplication during an upgrade or uninstall.
            info!("close requested; exiting");
            // SAFETY: our own window.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return LRESULT(0);
        }
        WM_DESTROY => {
            shutdown(hwnd);
            // SAFETY: ends the message loop (exit code 0).
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        _ => {}
    }
    if msg != 0 && msg == TASKBAR_CREATED.with(Cell::get) {
        // Explorer restarted: add the icon again.
        with_app(|a| a.update_tray(true, false));
        return LRESULT(0);
    }
    // SAFETY: default processing.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

pub fn run(mut engine: Engine, show_settings: bool, startup_note: Option<String>) -> i32 {
    // A restore of an earlier instance that did not finish stays visible
    // until the next apply.
    if let Some(n) = startup_note {
        engine.stages.restore_finished(Some(n));
    }
    engine.on_config = Some(publish_timing);
    // SAFETY: standard window-class registration and creation; all pointers
    // passed are valid for the duration of the calls.
    unsafe {
        let hinstance = match GetModuleHandleW(None) {
            Ok(h) => h.into(),
            Err(e) => {
                error!("GetModuleHandleW failed: {e}");
                return 1;
            }
        };
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: MAIN_CLASS,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            error!("RegisterClassW failed");
            return 1;
        }
        // A real (never shown) top-level window rather than a message-only
        // window: message-only windows do not receive broadcasts such as
        // WM_POWERBROADCAST.
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            MAIN_CLASS,
            w!("KeyDock"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                error!("CreateWindowExW failed: {e}");
                return 1;
            }
        };
        MAIN.with(|m| m.set(hwnd));

        let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
            dbcc_size: std::mem::size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
            dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE.0,
            dbcc_classguid: GUID_DEVINTERFACE_KEYBOARD,
            ..Default::default()
        };
        let devnotify = match RegisterDeviceNotificationW(
            HANDLE(hwnd.0),
            &filter as *const DEV_BROADCAST_DEVICEINTERFACE_W as *const c_void,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        ) {
            Ok(n) => Some(n),
            Err(e) => {
                error!("RegisterDeviceNotificationW failed: {e}; keyboard changes will not be detected");
                None
            }
        };
        if let Err(e) = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) {
            warn!("WTSRegisterSessionNotification failed: {e}; unlock will not trigger a re-check");
        }
        TASKBAR_CREATED.with(|c| c.set(RegisterWindowMessageW(w!("TaskbarCreated"))));
        langorder::set_notify_window(hwnd);
        TIMING.with(|t| t.set(timing(&engine.cfg)));
        let tray = engine.cfg.tray;
        let app = App {
            engine,
            hwnd,
            tray,
            remote: None,
            shown_error: None,
            shown_problems: Vec::new(),
            save_status: SaveStatus::default(),
        };
        DEVNOTIFY.with(|c| c.set(devnotify));
        APP.with(|c| *c.borrow_mut() = Some(app));
        with_app(|a| {
            info!(
                "watching for keyboard changes (debounce {} ms)",
                a.engine.cfg.debounce_ms
            );
            a.update_tray(true, false);
            a.manual("starting; applying");
        });
        if show_settings {
            open_settings();
        }

        let mut msg = MSG::default();
        loop {
            // GetMessageW returns -1 on error, 0 for WM_QUIT.
            let r = GetMessageW(&mut msg, None, 0, 0).0;
            if r == 0 {
                break;
            }
            if r == -1 {
                error!("GetMessageW failed ({:?}); exiting", GetLastError());
                shutdown(hwnd);
                break;
            }
            if let Some(s) = settings_ui::hwnd() {
                if IsDialogMessageW(s, &msg).as_bool() {
                    continue;
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        APP.with(|c| c.borrow_mut().take());
        info!("exiting");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the open Settings window (`open`) shows of a result.
    fn shown(open: Option<u64>, r: Option<(u64, String)>) -> Option<String> {
        r.filter(|(to, _)| settings_ui::is_recipient(open, *to)).map(|(_, t)| t)
    }

    #[test]
    fn a_save_stays_pending_while_its_apply_is_retried() {
        // Review KD-02, in the window that saved (session 1).
        let w = Some(1);
        let mut s = SaveStatus::default();
        s.saved(w);
        let t = shown(
            w,
            s.failed("waiting for keyboard X to finish setting up", Next::Resample),
        )
        .unwrap();
        assert!(t.contains("waiting for a keyboard") && !t.contains("failed"), "{t}");
        assert_eq!(
            shown(w, s.succeeded(false, "Saved and applied. ok".into())).as_deref(),
            Some("Saved and applied. ok")
        );
        assert_eq!(s.pending, None);
        // An ordinary retry, then success.
        s.saved(w);
        let t = shown(
            w,
            s.failed("cannot read the enabled layouts", Next::Retry { attempt: 1, of: 5 }),
        )
        .unwrap();
        assert!(
            t.contains("trying again (1 of 5)") && !t.starts_with("Saved, but"),
            "{t}"
        );
        let t = shown(
            w,
            s.failed("cannot read the enabled layouts", Next::Retry { attempt: 2, of: 5 }),
        )
        .unwrap();
        assert!(t.contains("2 of 5"), "{t}");
        assert_eq!(
            shown(w, s.succeeded(false, "Saved and applied. ok".into())).as_deref(),
            Some("Saved and applied. ok")
        );
        // Retries used up: the failure is final.
        s.saved(w);
        s.failed("x", Next::Retry { attempt: 5, of: 5 });
        let t = shown(w, s.failed("cannot read the enabled layouts", Next::GiveUp)).unwrap();
        assert_eq!(t, "Saved, but applying failed: cannot read the enabled layouts");
        assert_eq!(s.pending, None);
        // A later, unrelated evaluation does not touch Settings.
        assert_eq!(s.succeeded(false, "x".into()), None);
        assert_eq!(s.failed("x", Next::GiveUp), None);
        // A restore keeps it pending until the check after it.
        s.saved(w);
        assert!(shown(w, s.succeeded(true, "x".into())).unwrap().contains("restoring"));
        assert_eq!(s.pending, w);
        assert_eq!(shown(w, s.succeeded(false, "done".into())).as_deref(), Some("done"));
    }

    #[test]
    fn a_save_result_reaches_only_the_window_that_saved() {
        // Final review: Apply in window 1 while a keyboard is settling; the
        // window is closed and Settings opened again (window 2) before the
        // apply completes. Window 2 never shows window 1's result.
        let mut s = SaveStatus::default();
        s.saved(Some(1));
        assert_eq!(
            shown(Some(1), s.failed("settling", Next::Resample)).map(|t| t.contains("waiting")),
            Some(true)
        );
        assert_eq!(
            shown(Some(2), s.succeeded(false, "Saved and applied.".into())),
            None,
            "late completion"
        );
        assert_eq!(s.pending, None, "and it is over");
        // A restore pending, the window closed (nothing open), then reopened.
        s.saved(Some(2));
        assert_eq!(shown(None, s.succeeded(true, "x".into())), None);
        assert_eq!(shown(Some(3), s.succeeded(false, "done".into())), None);
        // A newer save in the new window supersedes the old one: only the new
        // window gets a result, once.
        s.saved(Some(3));
        s.saved(Some(4));
        assert_eq!(shown(Some(3), s.failed("e", Next::Retry { attempt: 1, of: 5 })), None);
        assert_eq!(
            shown(Some(4), s.succeeded(false, "Saved and applied.".into())).as_deref(),
            Some("Saved and applied.")
        );
        assert_eq!(shown(Some(4), s.succeeded(false, "again".into())), None, "only once");
        // A newer paused save supersedes it as well: nothing comes later.
        s.saved(Some(5));
        s.superseded();
        assert_eq!(s.succeeded(false, "late".into()), None);
        assert_eq!(s.failed("late", Next::GiveUp), None);
        // A retry that succeeds in the original, still open window.
        s.saved(Some(6));
        let t = shown(Some(6), s.failed("e", Next::Retry { attempt: 1, of: 5 })).unwrap();
        assert!(t.contains("trying again"), "{t}");
        assert_eq!(
            shown(Some(6), s.succeeded(false, "Saved and applied.".into())).as_deref(),
            Some("Saved and applied.")
        );
    }

    #[test]
    fn handoff_queues_unless_paused() {
        // Review FP-07: the reply does not depend on what the previous
        // evaluation found (a broken config, a remote session); the queued
        // evaluation reads both fresh. Only Paused is answered at once.
        SCHED.with(|s| *s.borrow_mut() = Scheduler::default());
        PAUSED.with(|p| p.set(true));
        assert_eq!(handoff(), HANDOFF_PAUSED);
        assert_eq!(sched(|s| s.queued()), None, "nothing queued while paused");
        PAUSED.with(|p| p.set(false));
        assert_eq!(handoff(), HANDOFF_QUEUED);
        assert_eq!(sched(|s| s.queued()), Some(ApplyMode::Force), "a durable Force request");
        // SAFETY: the thread timer `arm` created (no window in a test).
        unsafe {
            let _ = KillTimer(None, TIMER_ID);
        }
    }
}
