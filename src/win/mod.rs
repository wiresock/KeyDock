//! Windows implementation: CLI entry point, evaluation/apply logic.

macro_rules! info {
    ($($a:tt)*) => { $crate::win::log::write("INFO", &format!($($a)*)) };
}
macro_rules! warn {
    ($($a:tt)*) => { $crate::win::log::write("WARN", &format!($($a)*)) };
}
macro_rules! error {
    ($($a:tt)*) => { $crate::win::log::write("ERROR", &format!($($a)*)) };
}

mod autostart;
mod catalog;
mod child;
mod console;
mod devices;
mod langorder;
mod layouts;
mod log;
mod registry;
mod session;
mod settings_ui;
mod ui;
mod window;

use keydock::config::Config;
use keydock::device::KeyboardInfo;
use keydock::layout::LayoutSpec;
use keydock::reconcile;
use keydock::select::{classify, needs_apply, select_profile, unsettled_by, ApplyMode, Selection, Settle, Settling};
use keydock::state::State;
use keydock::status::{ApplyReport, Check, Stages};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, LPARAM, WPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::KeyboardAndMouse::HKL;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, FindWindowW, GetWindowThreadProcessId, PostMessageW, SendMessageTimeoutW,
    SMTO_ABORTIFHUNG,
};

pub use session::WriterLock;

const USAGE: &str = "KeyDock - switches keyboard layouts when an external keyboard is connected.

Usage: keydock.exe [options]

  (no options)        Run in the notification area; opens Settings on first run
                      (or in the already running instance).
  --background        Run in the notification area without opening Settings
                      (used by \"Start with Windows\").
  --settings          Open Settings (in the running instance if there is one).
  --list-keyboards    Write the present keyboards, their IDs and the profile
                      that would be selected to the log (and the console).
  --once              Evaluate and apply once, then exit (or ask the running
                      instance to re-apply).
  --dry-run           Log what would change, but change nothing.
  --remember-builtin  Record the keyboards present now as built-in, then exit.
  --remove-autostart  Remove the \"Start with Windows\" entry if it starts this
                      copy (or a missing file), then exit. Used by the
                      uninstaller.
  --config <path>     Use another configuration file.
  --help              Show this help.

Exit codes of --once: 0 done (or nothing to do), 1 error (also an unusable
config.toml), 2 applied but something needs attention (see the log),
3 nothing changed: Remote Desktop or other non-console session (set
remote_sessions = true to allow it), 4 handed to the running KeyDock, which
applies in the background, 5 the running KeyDock is paused: nothing applied.

Config: %APPDATA%\\KeyDock\\config.toml   Log: %LOCALAPPDATA%\\KeyDock\\keydock.log";

#[derive(Debug, Default)]
struct Args {
    list: bool,
    once: bool,
    dry_run: bool,
    remember_builtin: bool,
    background: bool,
    settings: bool,
    remove_autostart: bool,
    help: bool,
    config: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--list-keyboards" => a.list = true,
            "--once" => a.once = true,
            "--dry-run" => a.dry_run = true,
            "--remember-builtin" => a.remember_builtin = true,
            "--background" => a.background = true,
            "--settings" => a.settings = true,
            "--remove-autostart" => a.remove_autostart = true,
            "--help" | "-h" | "/?" => a.help = true,
            "--config" => a.config = Some(PathBuf::from(it.next().ok_or("--config needs a path")?)),
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(a)
}

pub struct Paths {
    pub config: PathBuf,
    pub state: PathBuf,
    pub log: PathBuf,
}

fn env_dir(var: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("KeyDock")
}

impl Paths {
    fn new(config_override: Option<PathBuf>) -> Self {
        let cfg_dir = env_dir("APPDATA");
        Self {
            config: config_override.unwrap_or_else(|| cfg_dir.join("config.toml")),
            state: cfg_dir.join("state.toml"),
            log: env_dir("LOCALAPPDATA").join("keydock.log"),
        }
    }

    /// Marker of a language-order restore in flight (see `langorder`).
    /// One per session, so another session's KeyDock never reads (or
    /// deletes) the marker of a restore that is still running here.
    fn restore_marker(&self) -> PathBuf {
        let mut sid = 0u32;
        // SAFETY: valid out pointer.
        let _ = unsafe {
            windows::Win32::System::RemoteDesktop::ProcessIdToSessionId(
                windows::Win32::System::Threading::GetCurrentProcessId(),
                &mut sid,
            )
        };
        self.log.with_file_name(format!("langorder-restore.{sid}.pending"))
    }
}

/// Loads the configuration; a missing file means "not configured yet". Any
/// other read problem is an error (not an empty configuration).
pub fn load_config(paths: &Paths) -> Result<Config, String> {
    use keydock::fsutil::{read_revision, Revision};
    match read_revision(&paths.config) {
        Revision::Missing => Ok(Config::default()),
        Revision::Unreadable(e) => Err(format!("cannot read {}: {e}", paths.config.display())),
        Revision::Content(bytes) => {
            let text = String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8 text", paths.config.display()))?;
            Config::parse(&text).map_err(|e| e.to_string())
        }
    }
}

fn save_state(paths: &Paths, state: &State) -> Result<(), String> {
    keydock::fsutil::write_atomic(&paths.state, &state.to_toml())
        .map_err(|e| format!("cannot write {}: {e}", paths.state.display()))?;
    info!("wrote {}", paths.state.display());
    Ok(())
}

/// `--remember-builtin`: records the keyboards present now. The running tray
/// instance reads state.toml at its start, so it uses the new list after a
/// restart.
fn remember_builtin(paths: &Paths, keyboards: &[KeyboardInfo]) -> Result<State, String> {
    let state = State {
        builtin_instance_ids: keyboards
            .iter()
            .filter(|k| !k.is_virtual() && !k.instance_id.is_empty() && k.incomplete.is_none())
            .map(|k| k.instance_id.clone())
            .collect(),
    };
    for id in &state.builtin_instance_ids {
        info!("remembering built-in keyboard: {id}");
    }
    save_state(paths, &state)?;
    Ok(state)
}

/// Loads state.toml (keyboards recorded with `--remember-builtin`, or on
/// first run by KeyDock 0.3.2 and older). A missing file is normal and is not
/// created: KeyDock never records the present keyboards on its own, because
/// on a first run at a desk that would include the external keyboard.
pub fn load_state(paths: &Paths) -> State {
    match std::fs::read_to_string(&paths.state) {
        Ok(text) => {
            let state = State::parse(&text).unwrap_or_else(|e| {
                error!("{e}; treating as empty");
                State::default()
            });
            if !state.builtin_instance_ids.is_empty() {
                info!(
                    "{} keyboard(s) count as built-in because {} lists them",
                    state.builtin_instance_ids.len(),
                    paths.state.display()
                );
            }
            state
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
        Err(e) => {
            error!("cannot read {}: {e}; treating as empty", paths.state.display());
            State::default()
        }
    }
}

fn log_keyboards(keyboards: &[KeyboardInfo], cfg: &Config, state: &State) {
    info!("{} keyboard interface(s) present:", keyboards.len());
    for (i, k) in keyboards.iter().enumerate() {
        let kind = classify(k, cfg, state);
        info!(
            "[{}] {} - {}{}{}",
            i + 1,
            k.name.as_deref().unwrap_or("(no name)"),
            kind.as_str(),
            if k.has_mouse {
                " (also exposes a mouse interface)"
            } else {
                ""
            },
            match &k.incomplete {
                Some(r) => format!(" (incomplete: {r})"),
                None => String::new(),
            }
        );
        if let Some(p) = &k.product {
            info!("    product:      {p}");
        }
        info!("    instance_id:  {}", k.instance_id);
        if let Some((v, p)) = k.vid_pid() {
            info!("    vid/pid:      vid = \"{v:04X}\", pid = \"{p:04X}\"");
        }
        info!(
            "    container_id: {}{}",
            k.container_id.as_deref().unwrap_or("(none)"),
            if k.container_per_port {
                " (new on every USB port: no serial number; remembered by VID:PID unless marked for this port only)"
            } else {
                ""
            }
        );
        for h in &k.hardware_ids {
            info!("    hardware_id:  {h}");
        }
        info!("    interface:    {}", k.interface_path);
    }
}

/// What an evaluation did, for the tray, Settings and `--once`. What needs
/// the user's attention is in [`Engine::stages`], which outlives a single
/// evaluation (see `keydock::status`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The selected profile (`"default"` for `[default]`), if any.
    pub profile: Option<String>,
    /// Layouts were changed (or may have been).
    pub changed: bool,
    /// A language-order restore was started; its result arrives later.
    pub restoring: bool,
    /// Nothing was changed because of the session (see `session::blocked`).
    pub blocked: Option<String>,
}

/// Evaluation context shared by `--once` and the background loop.
pub struct Engine {
    pub paths: Paths,
    pub cfg: Config,
    pub state: State,
    pub dry_run: bool,
    /// Name of the last successfully applied selection.
    pub last_applied: Option<String>,
    /// The selection itself, for the read-only check after a language-order
    /// restore of `--once` (review KD-01).
    applied_selection: Option<Selection>,
    /// Whether an external keyboard was present at the last evaluation.
    pub external_present: bool,
    /// Why config.toml could not be used, while that is the case. KeyDock then
    /// keeps the last configuration that worked (none at startup).
    pub config_error: Option<String>,
    /// What the last operation left that needs attention, per stage.
    pub stages: Stages,
    /// Called with every new valid configuration read from disk (the tray
    /// publishes its timing to the scheduler this way, review TP-06).
    pub on_config: Option<fn(&Config)>,
    last_keyboard_ids: Vec<String>,
    /// Re-applications in a row of an unchanged profile in `ApplyMode::Verify`.
    verify_streak: u32,
    /// Verification gave up with the layouts still differing: the next event
    /// checks them again even if the profile is unchanged.
    mismatch: bool,
    /// Settling budgets of incomplete keyboards (review TP-07).
    settling: Settling,
    /// The last evaluation is waiting for a keyboard to finish setting up:
    /// sample again after this many ms (review FP-06).
    settle_resample: Option<u32>,
    clock: Instant,
    /// The last evaluation was blocked by the session policy.
    was_blocked: bool,
    /// `--once`: the configuration read by the evaluation itself must be
    /// usable; the tray instead keeps the last usable one (review F5-04).
    strict_config: bool,
}

/// How many times in a row a `Verify` evaluation may re-apply an unchanged
/// profile before KeyDock gives up (guards against a loop with another
/// program that keeps changing the layouts back).
const MAX_VERIFY_STREAK: u32 = 2;
/// How long each incomplete keyboard may hold back a change of profile (see
/// `keydock::select::Settling`); afterwards it is left out.
const SETTLE_WINDOW: Duration = Duration::from_secs(15);
/// However many different keyboards keep appearing incomplete, KeyDock stops
/// waiting for them this long after the first one did (churning identities).
const SETTLE_EPISODE: Duration = Duration::from_secs(60);
/// How soon an incomplete keyboard is sampled again.
const SETTLE_RESAMPLE_MS: u32 = 1000;

impl Engine {
    fn new(paths: Paths, cfg: Config, state: State, dry_run: bool, config_error: Option<String>) -> Self {
        Self {
            paths,
            cfg,
            state,
            dry_run,
            last_applied: None,
            external_present: false,
            config_error,
            stages: Stages::default(),
            on_config: None,
            last_keyboard_ids: Vec::new(),
            verify_streak: 0,
            mismatch: false,
            settling: Settling::default(),
            settle_resample: None,
            clock: Instant::now(),
            was_blocked: false,
            strict_config: false,
            applied_selection: None,
        }
    }

    fn reload_config(&mut self) {
        match load_config(&self.paths) {
            Ok(c) => {
                if self.config_error.take().is_some() {
                    info!("configuration error resolved");
                }
                if c != self.cfg {
                    info!(
                        "configuration loaded: {} profile(s), default {}",
                        c.profiles.len(),
                        if c.default.is_some() { "set" } else { "not set" }
                    );
                    // A changed configuration may change the target even with the same profile name.
                    self.last_applied = None;
                }
                self.cfg = c;
                if let Some(f) = self.on_config {
                    f(&self.cfg);
                }
            }
            Err(e) => {
                if self.config_error.as_deref() != Some(e.as_str()) {
                    error!("{e}; keeping the previous configuration");
                }
                self.config_error = Some(e);
            }
        }
    }

    /// Re-evaluates from scratch; `mode` decides whether an unchanged profile
    /// is applied again (see [`ApplyMode`]). `Err` for a failure worth
    /// retrying (nothing or only part was changed); the reason is also in
    /// [`Self::stages`].
    pub fn evaluate(&mut self, mode: ApplyMode) -> Result<Outcome, String> {
        let r = self.evaluate_inner(mode);
        if let Err(e) = &r {
            // A failed apply has already recorded the error in its stage.
            if !self.stages.problems().contains(e) {
                self.stages.evaluation_failed(e.clone());
            }
        }
        r
    }

    fn evaluate_inner(&mut self, mode: ApplyMode) -> Result<Outcome, String> {
        let mut mode = mode;
        // Fresh configuration first: the session policy below depends on it.
        self.reload_config();
        if self.strict_config {
            // The read that decides the profile and the session policy: a
            // file that became unusable (for example during the wait for a
            // previous writer) is an error, never the older configuration.
            if let Some(e) = &self.config_error {
                return Err(format!("{e}; nothing changed"));
            }
        }
        // A restore that ended (even if its completion message was lost):
        // record its result, and check the layouts, which it rewrote, even
        // if the profile is unchanged.
        if let Some(o) = langorder::take_outcome() {
            self.stages.restore_finished(o.problem());
            mode = mode.max(ApplyMode::Verify);
        }
        if let Some(reason) = session::blocked(&self.cfg) {
            self.was_blocked = true;
            // The evaluation itself ran (a waiting notice is over).
            self.stages.evaluated();
            return Ok(Outcome {
                blocked: Some(reason),
                ..Default::default()
            });
        }
        if std::mem::take(&mut self.was_blocked) {
            // Another session may have changed the layouts meanwhile.
            mode = mode.max(ApplyMode::Verify);
        }
        let all = devices::enumerate()?;
        let unsettled: Vec<String> = unsettled_by(&self.cfg, &all, &self.state)
            .iter()
            .map(|k| k.interface_path.clone())
            .collect();
        // Every interface that is still incomplete, relevant now or not: its
        // budget is kept while it stays so (review S6-04).
        let incomplete: Vec<String> = all
            .iter()
            .filter(|k| k.incomplete.is_some())
            .map(|k| k.interface_path.clone())
            .collect();
        let now = self.clock.elapsed().as_millis() as u64;
        match self.settling.assess_present(
            &incomplete,
            &unsettled,
            now,
            SETTLE_WINDOW.as_millis() as u64,
            SETTLE_EPISODE.as_millis() as u64,
        ) {
            Settle::Ready => {}
            Settle::Wait(w) => {
                // Keep the current layouts (the last trustworthy topology)
                // and sample again soon, on the settling schedule.
                self.settle_resample = Some(SETTLE_RESAMPLE_MS);
                return Err(format!("waiting for keyboard {} to finish setting up", w.join(", ")));
            }
            Settle::GaveUp(g) => warn!(
                "keyboard {} could still not be read after {} s; leaving it out",
                g.join(", "),
                SETTLE_WINDOW.as_secs()
            ),
        }
        let keyboards = devices::complete(all);
        let mut ids: Vec<String> = keyboards.iter().map(|k| k.instance_id.clone()).collect();
        ids.sort();
        self.external_present = keyboards
            .iter()
            .any(|k| classify(k, &self.cfg, &self.state) == keydock::device::KeyboardKind::External);
        if ids != self.last_keyboard_ids {
            log_keyboards(&keyboards, &self.cfg, &self.state);
            self.last_keyboard_ids = ids;
        }
        let Some(sel) = select_profile(&self.cfg, &keyboards, &self.state) else {
            info!("no profile matches and no [default] is configured; nothing to do");
            self.last_applied = None;
            self.stages.evaluated();
            return Ok(Outcome::default());
        };
        if self.mismatch && mode == ApplyMode::Normal {
            // A new event after verification gave up: check again.
            mode = ApplyMode::Verify;
            self.verify_streak = 0;
        }
        let same = self.last_applied.as_deref() == Some(sel.name.as_str());
        if !needs_apply(mode, same, || self.plan_is_empty(&sel)) {
            if mode == ApplyMode::Verify {
                // The layouts match: a later mismatch starts a new streak.
                // Only the membership is re-checked here; other stages keep
                // their result (review TP-05).
                self.verify_streak = 0;
                self.mismatch = false;
                self.stages.membership_verified();
            } else {
                self.stages.evaluated();
            }
            return Ok(Outcome {
                profile: Some(sel.name),
                ..Default::default()
            });
        }
        if same && mode == ApplyMode::Verify {
            if self.verify_streak >= MAX_VERIFY_STREAK {
                let p = format!(
                    "the layouts still differ from profile \"{}\" after {} re-applications (another program may be changing them)",
                    sel.name, MAX_VERIFY_STREAK
                );
                warn!("{p}; leaving them until the next change");
                self.mismatch = true;
                self.stages.membership_problem(p);
                return Ok(Outcome {
                    profile: Some(sel.name),
                    ..Default::default()
                });
            }
            self.verify_streak += 1;
            warn!("layouts no longer match profile \"{}\"; applying it again", sel.name);
        } else {
            self.verify_streak = 0;
        }
        match &sel.matched_by {
            Some(id) => info!("selected profile \"{}\" (matched {id})", sel.name),
            None => info!("selected profile \"{}\"", sel.name),
        }
        // Forget the previous profile first: if applying fails part-way, a
        // later return to that profile must not be skipped as "unchanged".
        self.last_applied = None;
        self.applied_selection = None;
        let (report, result) = self.apply(&sel);
        if self.dry_run {
            self.stages.evaluated();
        } else {
            self.stages.applied(report);
        }
        let mut out = result?;
        self.mismatch = false;
        self.last_applied = Some(sel.name.clone());
        self.applied_selection = Some(sel.clone());
        out.profile = Some(sel.name);
        Ok(out)
    }

    /// Review KD-01: after a language-order restore that the apply started has
    /// ended, the enabled layouts are read once more (read only: nothing is
    /// applied or restored) and compared with the selection that apply used.
    /// The restore only checks its own snapshot, which was taken after the
    /// reconciliation's last read; something may have changed the layouts in
    /// between. A missing profile layout, an unexpected managed layout or an
    /// unreadable list is recorded as a membership problem.
    fn check_membership_after_restore(&mut self, api: &mut dyn reconcile::LayoutApi) {
        let Some(sel) = self.applied_selection.clone() else {
            return;
        };
        let fmt = |v: &[LayoutSpec]| v.iter().map(|l| l.to_string()).collect::<Vec<_>>().join(", ");
        match reconcile::plan(api, &sel.set.layouts, &self.cfg.managed_layouts()) {
            Ok((_, plan)) if plan.is_empty() => {
                info!(
                    "after the language-order restore the layouts still match profile \"{}\"",
                    sel.name
                );
            }
            Ok((_, plan)) => {
                let mut what = Vec::new();
                if !plan.install.is_empty() {
                    what.push(format!("[{}] missing", fmt(&plan.install)));
                }
                if !plan.uninstall.is_empty() {
                    what.push(format!("[{}] enabled but not in the profile", fmt(&plan.uninstall)));
                }
                let p = format!(
                    "after the language-order restore the layouts no longer match profile \"{}\": {} (something else changed them)",
                    sel.name,
                    what.join(", ")
                );
                warn!("{p}");
                self.stages.membership_problem(p);
            }
            Err(e) => {
                let p = format!("after the language-order restore the enabled layouts cannot be read ({e}); profile \"{}\" is not confirmed", sel.name);
                warn!("{p}");
                self.stages.membership_problem(p);
            }
        }
    }

    /// A previous writer holds back every change (review S6-05): live
    /// evaluation status, replaced by the next evaluation that runs.
    pub fn note_previous_writer(&mut self, b: &langorder::Blocked) {
        self.stages
            .evaluation_failed(format!("{}; no layouts are changed until it has ended", b.reason));
    }

    /// Takes the request to sample again because a keyboard is settling.
    pub fn take_settle_resample(&mut self) -> Option<u32> {
        self.settle_resample.take()
    }

    /// Whether the enabled layouts already match `sel`; unknown counts as
    /// "no" (applying then reports why it cannot proceed).
    fn plan_is_empty(&self, sel: &Selection) -> bool {
        let mut api = layouts::WinLayouts::new();
        reconcile::plan(&mut api, &sel.set.layouts, &self.cfg.managed_layouts())
            .map(|(_, p)| p.is_empty())
            .unwrap_or(false)
    }

    /// Applies `sel`. The report has every stage's result, also when the
    /// apply fails part-way (then the result is the error to retry on).
    fn apply(&self, sel: &Selection) -> (ApplyReport, Result<Outcome, String>) {
        let mut report = ApplyReport::default();
        let r = self.apply_stages(sel, &mut report);
        report.membership = match &r {
            Ok(_) => Check::Clear,
            Err(e) => Check::Problem(e.clone()),
        };
        (report, r)
    }

    fn apply_stages(&self, sel: &Selection, report: &mut ApplyReport) -> Result<Outcome, String> {
        // The shared mutation boundary (review FP-01): no change starts while
        // a restore of this or an earlier KeyDock may still be writing, for
        // the tray and `--once` alike.
        if let Some(r) = langorder::writer_gate() {
            return Err(format!("{r}; nothing changed"));
        }
        #[cfg(test)]
        tests::BOUNDARY.lock().unwrap().push(Instant::now());
        let fmt = |v: &[LayoutSpec]| v.iter().map(|l| l.to_string()).collect::<Vec<_>>().join(", ");
        let present = layouts::current_hkls();
        info!(
            "loaded input locales: {}",
            present
                .iter()
                .map(|h| format!("0x{:08X}", layouts::hkl_low32(*h)))
                .collect::<Vec<_>>()
                .join(", ")
        );
        // One writer of the user's layouts, also across sessions; no write
        // without proof of ownership (review TP-02).
        let _writer = if self.dry_run {
            None
        } else {
            Some(match WriterLock::acquire(Duration::ZERO, None) {
                session::Acquire::Owned(l) => l,
                session::Acquire::Abandoned(l) => {
                    warn!("the previous holder of the writer lock ended without releasing it (a change may have been cut short); working from a fresh reading");
                    l
                }
                session::Acquire::Busy | session::Acquire::Cancelled => {
                    return Err("another KeyDock of this user is changing the layouts".into())
                }
                session::Acquire::Failed(e) => {
                    return Err(format!(
                        "cannot make sure no other KeyDock is changing the layouts ({e}); nothing changed"
                    ))
                }
            })
        };
        // Again under the writer lock (review F5-01): a cooperating writer of
        // any session is excluded by the lock from here on, and a previous
        // writer of this session (which never took the lock) must still be
        // known to have stopped right before the first read and write.
        #[cfg(test)]
        if let Some(f) = *tests::BEFORE_LOCKED_GATE.lock().unwrap() {
            f();
        }
        if let Some(r) = langorder::writer_gate() {
            return Err(format!("{r}; nothing changed"));
        }
        #[cfg(test)]
        tests::LOCKED_BOUNDARY.lock().unwrap().push(Instant::now());
        let mut api = layouts::WinLayouts::new();
        let managed = self.cfg.managed_layouts();
        let (enabled, plan) = reconcile::plan(&mut api, &sel.set.layouts, &managed)
            .map_err(|e| format!("cannot read the enabled layouts ({e}); nothing changed"))?;
        info!("enabled layouts: {}", fmt(&enabled));
        // Managed layouts that are no longer enabled but still loaded in this
        // session (a removal earlier while they were in use): not part of the
        // plan, but worth unloading along with the next removal.
        let leftovers: Vec<LayoutSpec> = managed
            .iter()
            .filter(|l| !sel.set.layouts.contains(l) && !enabled.contains(l))
            .filter(|l| layouts::find_loaded(l, &present).is_some())
            .copied()
            .collect();
        if !leftovers.is_empty() {
            info!("no longer enabled but still loaded: [{}]", fmt(&leftovers));
        }
        if plan.is_empty() {
            info!("layouts already match profile \"{}\"", sel.name);
        } else {
            info!(
                "plan: install [{}], uninstall [{}]",
                fmt(&plan.install),
                fmt(&plan.uninstall)
            );
        }
        if self.dry_run {
            if let Some(a) = sel.set.activate {
                info!("dry run: would activate {a}");
            }
            info!("dry run: no changes made");
            return Ok(Outcome::default());
        }
        // InstallLayoutOrTip may reorder the user's languages (and with that
        // the display language): take a complete snapshot to compare with.
        let order_before = if plan.is_empty() {
            None
        } else {
            match langorder::snapshot() {
                Ok(s) => {
                    info!("language order: {}", s.languages.join(", "));
                    Some(s)
                }
                Err(e) => {
                    return Err(format!(
                        "cannot read the language list before changing layouts ({e}); nothing changed"
                    ))
                }
            }
        };
        let mut out = Outcome::default();
        let to_unload: Vec<LayoutSpec> = plan.uninstall.iter().chain(&leftovers).copied().collect();
        let mut removed_hkls: Vec<(LayoutSpec, HKL)> = Vec::new();
        // The activation stage: foreground and resolution problems, and the
        // default input's early result (replaced by the final check below).
        let mut activation: Vec<String> = Vec::new();
        let mut early_default: Option<String> = None;
        // Session work, between the confirmed install and the removal.
        let session_work_ran = std::cell::Cell::new(false);
        let activated_hkl: std::cell::Cell<Option<HKL>> = std::cell::Cell::new(None);
        let mut between = || {
            session_work_ran.set(true);
            let mut activated = None;
            if let Some(a) = sel.set.activate {
                match layouts::resolve_hkl(&a) {
                    Ok(h) => {
                        let act = layouts::activate(h);
                        match &act.foreground {
                            layouts::Foreground::Requested(p) => info!(
                                "activated {a} (HKL 0x{:08X}): default input set, switch requested from {p}",
                                layouts::hkl_low32(h)
                            ),
                            layouts::Foreground::NoWindow => info!(
                                "activated {a} (HKL 0x{:08X}): default input set; no active window to switch",
                                layouts::hkl_low32(h)
                            ),
                            layouts::Foreground::Refused(_) => {}
                        }
                        if let Some(p) = act.problem() {
                            warn!("activating {a}: {p}");
                        }
                        if let Err(e) = &act.default_input {
                            early_default = Some(format!("switching to {a}: {e}"));
                        }
                        if let layouts::Foreground::Refused(e) = &act.foreground {
                            activation.push(format!(
                                "switching to {a}: the active window did not accept the switch ({e})"
                            ));
                        }
                        activated = Some(h);
                        activated_hkl.set(Some(h));
                    }
                    Err(e) => {
                        warn!("could not activate {a}: {e}");
                        activation.push(format!("could not switch to {a}: {e}"));
                    }
                }
            }
            if !to_unload.is_empty() {
                let loaded = layouts::current_hkls();
                removed_hkls = to_unload
                    .iter()
                    .filter_map(|l| layouts::find_loaded(l, &loaded).map(|h| (*l, h)))
                    .collect();
                let low: Vec<u32> = removed_hkls.iter().map(|(_, h)| layouts::hkl_low32(*h)).collect();
                self.move_off_removed(sel, &to_unload, &low, activated, &loaded);
            }
        };
        let result = reconcile::apply(&mut api, &enabled, &sel.set.layouts, &managed, &plan, &mut between);
        // Checked only if the session work ran (it does not after a failed install).
        report.activation = if session_work_ran.get() {
            activation_status(&activation, early_default.clone(), None)
        } else {
            Check::Unchecked
        };
        // What changed as observed, and what was attempted (the language list
        // is compared whenever anything may have changed, review TP-04).
        let (installed, removed, attempted_i, attempted_r, may_have_changed) = match &result {
            Ok(a) => (
                a.installed.clone(),
                a.removed.clone(),
                a.attempted_install.clone(),
                a.attempted_remove.clone(),
                !a.attempted_install.is_empty() || !a.attempted_remove.is_empty(),
            ),
            Err(f) => (
                f.installed.clone(),
                f.removed.clone(),
                f.attempted_install.clone(),
                f.attempted_remove.clone(),
                f.may_have_changed(),
            ),
        };
        if !installed.is_empty() {
            info!("installed [{}] (confirmed enabled)", fmt(&installed));
        }
        if !removed.is_empty() {
            info!("uninstalled [{}] (confirmed)", fmt(&removed));
        }
        out.changed = may_have_changed;
        if let (Some(before), true) = (&order_before, may_have_changed) {
            // Only what KeyDock attempted can explain a change of the
            // language list (review FP-03); what else was observed is logged,
            // and makes a restore a conflict.
            let unattempted: Vec<LayoutSpec> = installed
                .iter()
                .filter(|l| !attempted_i.contains(l))
                .chain(removed.iter().filter(|l| !attempted_r.contains(l)))
                .copied()
                .collect();
            if !unattempted.is_empty() {
                info!(
                    "enabled layouts also changed by something else meanwhile: [{}]",
                    fmt(&unattempted)
                );
            }
            let lo = langorder::after_change(before, &attempted_i, &attempted_r, self.cfg.language_order);
            out.restoring = lo.restoring;
            report.restoring = lo.restoring;
            report.language = Check::from_problem((!lo.problems.is_empty()).then(|| lo.problems.join("; ")));
        }
        if result.is_ok() {
            // Only layouts that are really gone from the user's list.
            removed_hkls.retain(|(l, _)| removed.contains(l) || leftovers.contains(l));
            // Checked only when there was something to unload.
            if !removed_hkls.is_empty() {
                report.unload = Check::from_problem(self.unload_removed(&removed_hkls));
            }
            // Removing a layout can make Windows reset the default input to
            // another layout (seen on Windows 10 22H2 after the switch above):
            // the profile's layout for new windows is checked once all the
            // changes are done, and set again if it changed.
            // Its verified result is the default input's status for this
            // apply (review: a recovered early failure is not reported).
            if let (Some(h), Some(a)) = (activated_hkl.get(), sel.set.activate) {
                let last = keep_default_input(&mut WinDefaultInput(h), &a, layouts::hkl_low32(h));
                if let Err(p) = &last {
                    warn!("{p}");
                } else if early_default.is_some() {
                    info!("the default input is {a} now (the check while switching had failed)");
                }
                report.activation = activation_status(&activation, early_default, Some(last));
            }
        }
        match result {
            Ok(_) => Ok(out),
            Err(f) => {
                for c in [&report.activation, &report.language] {
                    if let Check::Problem(p) = c {
                        warn!("{p}");
                    }
                }
                Err(f.message)
            }
        }
    }

    /// Before uninstalling: a layout that is still active somewhere stays
    /// loaded in the session (and in the language switcher) after
    /// InstallLayoutOrTip removes it from the user's list, because Windows
    /// cannot unload an input locale that is in use. So switch every thread
    /// that uses a removed layout to a remaining one first, even when the
    /// profile says "don't switch".
    fn move_off_removed(
        &self,
        sel: &Selection,
        uninstall: &[LayoutSpec],
        removed_low: &[u32],
        activated: Option<HKL>,
        loaded: &[HKL],
    ) {
        if removed_low.is_empty() {
            return;
        }
        let u = layouts::usage(removed_low);
        if !u.any() {
            return;
        }
        let to = activated.or_else(|| {
            keydock::select::switch_candidates(&sel.set, uninstall)
                .iter()
                .find_map(|l| layouts::find_loaded(l, loaded))
        });
        let Some(to) = to else {
            warn!(
                "layouts being removed are still in use by {}, and no remaining layout is loaded to switch to",
                u.describe()
            );
            return;
        };
        let posted = layouts::move_off(&u, to);
        info!(
            "switching away from layouts being removed to HKL 0x{:08X}: {} ({} window thread(s) asked)",
            layouts::hkl_low32(to),
            u.describe(),
            posted
        );
        let left = layouts::wait_unused(removed_low, MOVE_OFF_TIMEOUT);
        if left.any() {
            warn!(
                "still in use after {} ms: {}",
                MOVE_OFF_TIMEOUT.as_millis(),
                left.describe()
            );
        }
    }

    /// After uninstalling: unloads removed layouts that are still loaded in
    /// the session. Returns a note about whatever is left (it stays in the
    /// language switcher of this session; Windows does not load it at the
    /// next sign-in because it is no longer enabled).
    fn unload_removed(&self, removed: &[(LayoutSpec, HKL)]) -> Option<String> {
        let still = |r: &[(LayoutSpec, HKL)]| {
            std::thread::sleep(SETTLE_DELAY);
            let now = layouts::current_hkls();
            r.iter()
                .filter(|(_, h)| now.iter().any(|n| layouts::hkl_low32(*n) == layouts::hkl_low32(*h)))
                .copied()
                .collect::<Vec<_>>()
        };
        if removed.is_empty() {
            return None;
        }
        let mut left = still(removed);
        if left.is_empty() {
            return None;
        }
        for (l, h) in &left {
            match layouts::unload(*h) {
                Ok(()) => info!("unloaded {l} (HKL 0x{:08X}) from the session", layouts::hkl_low32(*h)),
                Err(e) => info!("{l}: {e}"),
            }
        }
        left = still(&left);
        if left.is_empty() {
            return None;
        }
        let low: Vec<u32> = left.iter().map(|(_, h)| layouts::hkl_low32(*h)).collect();
        let u = layouts::usage(&low);
        let names = left
            .iter()
            .map(|(l, h)| format!("{l} (HKL 0x{:08X})", layouts::hkl_low32(*h)))
            .collect::<Vec<_>>()
            .join(", ");
        warn!(
            "removed layout(s) still loaded in this session: {names}{}",
            if u.any() {
                format!("; in use by {}", u.describe())
            } else {
                String::new()
            }
        );
        Some(format!(
            "removed layout(s) {names} stay in the language switcher of this session (still in use); they are not loaded after the next sign-in"
        ))
    }
}

/// The default input language (read, and set with a read-back).
trait DefaultInput {
    /// The low 32 bits of the default input's HKL; `None`: unreadable.
    fn read(&mut self) -> Option<u32>;
    /// Sets the wanted layout and reads it back.
    fn set(&mut self) -> Result<(), String>;
}

/// The real one, for the HKL being made the default.
struct WinDefaultInput(HKL);

impl DefaultInput for WinDefaultInput {
    fn read(&mut self) -> Option<u32> {
        layouts::default_input_lang().map(layouts::hkl_low32)
    }
    fn set(&mut self) -> Result<(), String> {
        layouts::set_default_input(self.0)
    }
}

/// After the layout changes: the default input is still `wanted` (the
/// profile's `activate` layout `a`), or it is set again and read back. `Ok`:
/// verified now; `Err`: it is not the wanted layout.
fn keep_default_input(io: &mut dyn DefaultInput, a: &LayoutSpec, wanted: u32) -> Result<(), String> {
    let now = io.read();
    if !default_input_changed(now, wanted) {
        return Ok(());
    }
    info!(
        "the default input changed to {} while the layouts were changed; setting {a} (HKL 0x{wanted:08X}) again",
        now.map_or_else(|| "an unknown layout".to_string(), |d| format!("0x{d:08X}"))
    );
    io.set().map_err(|e| format!("switching new windows to {a}: {e}"))
}

/// The activation stage of one apply: the foreground and resolution
/// problems, and the default input's status. Its last verified observation
/// (`last`, after all changes) replaces the early one; without it (the apply
/// stopped first) the early one stands. The early failure stays in the log.
fn activation_status(others: &[String], early_default: Option<String>, last: Option<Result<(), String>>) -> Check {
    let default = match last {
        Some(Ok(())) => None,
        Some(Err(e)) => Some(e),
        None => early_default,
    };
    let all: Vec<String> = others.iter().cloned().chain(default).collect();
    Check::from_problem((!all.is_empty()).then(|| all.join("; ")))
}

/// Whether the default input (`None`: unreadable) is not the wanted one.
fn default_input_changed(now: Option<u32>, wanted: u32) -> bool {
    now != Some(wanted)
}

/// How long `--once` waits for a previous KeyDock's restore to end; then it
/// fails with exit code 1 without changing anything (review F5-01).
const ONCE_WRITER_WAIT: Duration = Duration::from_secs(75);

/// How long to wait for other applications to leave a layout being removed.
const MOVE_OFF_TIMEOUT: Duration = Duration::from_millis(1500);
/// Pause before checking what is still loaded after a change.
const SETTLE_DELAY: Duration = Duration::from_millis(250);

/// RAII single-instance guard (named mutex in the session namespace).
struct SingleInstance(windows::Win32::Foundation::HANDLE);

impl SingleInstance {
    fn acquire() -> Option<Self> {
        // SAFETY: named mutex creation; GetLastError right after the call.
        unsafe {
            let h = CreateMutexW(None, true, w!("Local\\KeyDock.SingleInstance.7f3c2a91")).ok()?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(h);
                return None;
            }
            Some(Self(h))
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: handle owned by this guard.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Posts `msg` to the running instance's main window and lets that process
/// bring a window to the front (this process has the user's foreground
/// rights, the tray process does not). False if no window was found.
fn post_to_running(msg: u32) -> bool {
    // SAFETY: plain window lookup, a foreground grant and a posted message.
    unsafe {
        let Ok(h) = FindWindowW(window::MAIN_CLASS, PCWSTR::null()) else {
            return false;
        };
        let mut pid = 0u32;
        if GetWindowThreadProcessId(h, Some(&mut pid)) != 0 && pid != 0 {
            let _ = AllowSetForegroundWindow(pid);
        }
        PostMessageW(Some(h), msg, WPARAM(0), LPARAM(0)).is_ok()
    }
}

/// Asks the running instance to re-apply and waits (bounded) for its answer:
/// `Some(code)` with the instance's `HANDOFF_*` reply, `None` if there is no
/// window or it did not answer.
fn handoff_to_running() -> Option<isize> {
    // SAFETY: plain window lookup, a foreground grant and a sent message with
    // a timeout (a hung instance cannot hang this process).
    unsafe {
        let h = FindWindowW(window::MAIN_CLASS, PCWSTR::null()).ok()?;
        let mut pid = 0u32;
        if GetWindowThreadProcessId(h, Some(&mut pid)) != 0 && pid != 0 {
            let _ = AllowSetForegroundWindow(pid);
        }
        let mut result = 0usize;
        let r = SendMessageTimeoutW(
            h,
            window::WM_REAPPLY,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            5000,
            Some(&mut result),
        );
        (r.0 != 0).then_some(result as isize)
    }
}

/// `--once`: evaluate, wait for a language-order restore, report. An
/// unusable configuration is an error, not "nothing to do" (review TP-10).
fn run_once(engine: &mut Engine) -> i32 {
    run_once_with(engine, ONCE_WRITER_WAIT)
}

/// [`run_once`] with the time to wait for a previous writer.
fn run_once_with(engine: &mut Engine, writer_wait: Duration) -> i32 {
    engine.strict_config = true;
    engine.reload_config();
    if let Some(e) = &engine.config_error {
        error!("{e}; nothing changed");
        return 1;
    }
    // A restore of an earlier KeyDock (its job, or an older version's
    // PowerShell) must end first; bounded, then a documented failure. The
    // wait ending does not mean it ended: only proof opens the gate.
    if langorder::busy() {
        info!(
            "waiting up to {} s for a language-order restore of a previous KeyDock to end",
            writer_wait.as_secs()
        );
        if !langorder::wait_for_writers(writer_wait) {
            let why = langorder::writer_gate().unwrap_or_else(|| "a previous writer".into());
            error!("{why}; nothing changed");
            return 1;
        }
    }
    let out = match engine.evaluate(ApplyMode::Force) {
        Ok(o) => o,
        Err(e) => {
            error!("{e}");
            // A restore started before the failure still runs; report it too.
            if let Some(o) = langorder::wait() {
                engine.stages.restore_finished(o.problem());
            }
            for p in engine.stages.problems() {
                warn!("needs attention: {p}");
            }
            return 1;
        }
    };
    if let Some(b) = &out.blocked {
        info!("{b}: nothing changed");
        return 3;
    }
    if out.restoring {
        info!("waiting for the language-order restore");
    }
    let restore = langorder::wait();
    finish_once(engine, restore, &mut layouts::WinLayouts::new())
}

/// The end of `--once` once its apply has run: the restore's result and,
/// when a restore ran, the membership check after it (review KD-01).
fn finish_once(engine: &mut Engine, restore: Option<langorder::Outcome>, api: &mut dyn reconcile::LayoutApi) -> i32 {
    if let Some(o) = restore {
        engine.stages.restore_finished(o.problem());
        if !engine.dry_run {
            engine.check_membership_after_restore(api);
        }
    }
    let problems = engine.stages.problems();
    if problems.is_empty() {
        0
    } else {
        for p in &problems {
            warn!("needs attention: {p}");
        }
        2
    }
}

pub fn run() -> i32 {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if console::attach() {
                eprintln!("{e}\n\n{USAGE}");
            }
            return 1;
        }
    };
    if args.help {
        if console::attach() {
            println!("{USAGE}");
        }
        return 0;
    }
    let paths = Paths::new(args.config.clone());
    log::init(&paths.log);
    // A panic in a window procedure aborts the process; leave a trace first.
    std::panic::set_hook(Box::new(|info| log::write_panic(&format!("panic: {info}"))));
    let interactive = args.list || args.once || args.remember_builtin;
    if interactive && console::attach() {
        log::set_console(true);
    }
    info!(
        "KeyDock {} starting (pid {})",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    );

    if args.remove_autostart {
        return match autostart::remove_if_ours() {
            Ok(()) => 0,
            Err(e) => {
                error!("{e}");
                1
            }
        };
    }

    if args.remember_builtin {
        return match devices::enumerate().and_then(|k| remember_builtin(&paths, &k)) {
            Ok(_) => 0,
            Err(e) => {
                error!("{e}");
                1
            }
        };
    }

    let (cfg, config_error) = match load_config(&paths) {
        Ok(c) => (c, None),
        Err(e) => {
            error!("{e}; KeyDock changes nothing until the file is fixed or saved from Settings");
            (Config::default(), Some(e))
        }
    };
    let state = load_state(&paths);
    // A broken file is not a first run: Settings must not greet the user as if
    // nothing was configured.
    let first_run = !keydock::settings::is_configured(&cfg) && config_error.is_none();
    let has_config_error = config_error.is_some();
    let mut engine = Engine::new(paths, cfg, state, args.dry_run, config_error);

    if args.list {
        let keyboards = match devices::enumerate() {
            Ok(k) => k,
            Err(e) => {
                error!("{e}");
                return 1;
            }
        };
        log_keyboards(&keyboards, &engine.cfg, &engine.state);
        info!(
            "loaded input locales: {}",
            layouts::current_hkls()
                .iter()
                .map(|h| format!("0x{:08X}", layouts::hkl_low32(*h)))
                .collect::<Vec<_>>()
                .join(", ")
        );
        match layouts::enabled_layouts() {
            Ok(e) => info!(
                "enabled layouts: {}",
                e.iter().map(|l| l.to_string()).collect::<Vec<_>>().join(", ")
            ),
            Err(e) => warn!("cannot read the enabled layouts: {e}"),
        }
        match langorder::snapshot() {
            Ok(s) => info!("language list: {}", s.languages.join(", ")),
            Err(e) => warn!("cannot read the language list: {e}"),
        }
        if let Some(b) = session::blocked(&engine.cfg) {
            info!("{b}: KeyDock would not change layouts here");
        }
        let complete = devices::complete(keyboards);
        match select_profile(&engine.cfg, &complete, &engine.state) {
            Some(s) => info!("profile that would be selected: \"{}\"", s.name),
            None => info!("no profile would be selected (no match and no [default])"),
        }
        info!("log file: {}", engine.paths.log.display());
        return 0;
    }

    if args.once {
        // Only one process may change layouts: with the tray instance running,
        // ask it to re-apply instead of racing it. A dry run changes nothing.
        let guard = SingleInstance::acquire();
        if guard.is_none() && !args.dry_run {
            // The running instance queues the request durably and answers at
            // once; it applies in the background (exit code 4, not 0: the
            // apply itself has not happened yet).
            return match handoff_to_running() {
                Some(window::HANDOFF_QUEUED) => {
                    info!("KeyDock is already running; it queued a re-apply (see its tray icon or the log for the result)");
                    4
                }
                Some(window::HANDOFF_PAUSED) => {
                    warn!("KeyDock is already running but paused; nothing is applied");
                    5
                }
                _ => {
                    error!("another KeyDock instance is running, but it did not answer");
                    1
                }
            };
        }
        if guard.is_some() {
            if let Some(n) = langorder::startup(engine.paths.restore_marker()) {
                warn!("{n}");
            }
        }
        let rc = run_once(&mut engine);
        langorder::shutdown(Duration::from_secs(35));
        drop(guard);
        return rc;
    }

    let Some(_guard) = SingleInstance::acquire() else {
        if !args.background {
            // Bring up Settings in the running instance.
            post_to_running(window::WM_OPEN_SETTINGS);
        }
        info!("another KeyDock instance is already running; exiting");
        return 0;
    };
    let note = langorder::startup(engine.paths.restore_marker());
    autostart::repair_stale();
    let show_settings = args.settings || ((first_run || has_config_error) && !args.background);
    window::run(engine, show_settings, note)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// When the mutation boundary was passed (instrumentation, tests only).
    pub(crate) static BOUNDARY: std::sync::Mutex<Vec<Instant>> = std::sync::Mutex::new(Vec::new());
    /// When the check under the writer lock was passed.
    pub(crate) static LOCKED_BOUNDARY: std::sync::Mutex<Vec<Instant>> = std::sync::Mutex::new(Vec::new());
    /// Runs right before the check under the writer lock.
    pub(crate) static BEFORE_LOCKED_GATE: std::sync::Mutex<Option<fn()>> = std::sync::Mutex::new(None);

    #[test]
    fn once_with_a_truncated_record_fails_without_any_change() {
        // Reviews S7-03 and S8-02 through the one-shot entry point: a damaged
        // record (empty, a versioned header alone, a 0.3.7 row cut off in its
        // start time) whose process still runs past the discovery window.
        // --once exits 1 at the end of its wait without reaching the
        // mutation path; the tray's check stays closed too. A complete
        // record keeps it closed while the writer runs; its end opens it.
        let _g = langorder::tests::previous_guard();
        let fx = langorder::tests::LegacyFixture::start(60, "once-empty");
        let created = langorder::tests::fixture_created(fx.pid);
        let full = format!("{} {created}\n", fx.pid);
        let damaged = [
            "".to_string(),
            "keydock-legacy-writers 2\n".to_string(),
            full[..full.len() - 2].to_string(),
        ];
        let paths = scratch("emptyrec");
        std::fs::write(&paths.config, ONE_LAYOUT).unwrap();
        let marker = paths.restore_marker();
        let record = marker.with_extension("legacy-writers");
        let own_job = format!(r"Local\KeyDock.TestStartup.{}", std::process::id());
        let mut e = Engine::new(paths, Config::default(), State::default(), true, None);
        for content in &damaged {
            std::fs::write(&record, content).unwrap();
            let _ = langorder::startup_with(
                marker.clone(),
                &langorder::tests::nothing_new(),
                &own_job,
                Duration::from_millis(200),
            );
            BOUNDARY.lock().unwrap().clear();
            assert_eq!(run_once_with(&mut e, Duration::from_secs(2)), 1, "{content:?}");
            assert!(
                BOUNDARY.lock().unwrap().is_empty(),
                "{content:?}: the mutation path was not entered"
            );
            let b = langorder::previous_writer().expect("the tray's check is closed as well");
            assert!(b.overdue && b.reason.contains("cannot be used"), "{content:?}: {b:?}");
            assert!(fx.alive() && record.exists());
        }
        // The complete record becomes readable: the writer still runs.
        std::fs::write(&record, langorder::tests::complete_record(fx.pid, created)).unwrap();
        assert_eq!(run_once_with(&mut e, Duration::from_secs(2)), 1);
        assert!(BOUNDARY.lock().unwrap().is_empty());
        // Proved gone: the change runs.
        fx.stop();
        assert_eq!(run_once_with(&mut e, Duration::from_secs(2)), 0);
        assert_eq!(BOUNDARY.lock().unwrap().len(), 1);
        assert!(!record.exists());
    }

    #[test]
    fn the_check_under_the_writer_lock_stops_a_change() {
        // Review S7-03: the record becomes unusable between the first check
        // and the one under the writer lock: nothing beyond it runs.
        let _g = langorder::tests::previous_guard();
        let paths = scratch("lockedgate");
        std::fs::write(&paths.config, ONE_LAYOUT).unwrap();
        let mut e = Engine::new(paths, Config::default(), State::default(), true, None);
        BOUNDARY.lock().unwrap().clear();
        LOCKED_BOUNDARY.lock().unwrap().clear();
        *BEFORE_LOCKED_GATE.lock().unwrap() = Some(langorder::tests::make_record_unusable_now);
        let r = e.evaluate(ApplyMode::Force);
        *BEFORE_LOCKED_GATE.lock().unwrap() = None;
        let err = r.unwrap_err();
        assert!(
            err.contains("cannot be used") && err.contains("nothing changed"),
            "{err}"
        );
        assert_eq!(BOUNDARY.lock().unwrap().len(), 1, "the first check passed");
        assert!(LOCKED_BOUNDARY.lock().unwrap().is_empty(), "the second one stopped it");
    }

    const ONE_LAYOUT: &str = "remote_sessions = true\n[default]\nlayouts = [\"0409:00000409\"]\n";

    /// A one-shot engine (dry run) whose startup detected `fx` as a previous
    /// writer, with `patience` before it counts as overdue.
    fn once_engine(tag: &str, fx: &langorder::tests::LegacyFixture, patience: Duration) -> Engine {
        let paths = scratch(tag);
        std::fs::write(&paths.config, ONE_LAYOUT).unwrap();
        let own_job = format!(r"Local\KeyDock.TestStartup.{}", std::process::id());
        let discovery = langorder::tests::credible();
        let _ = langorder::startup_with(paths.restore_marker(), &discovery, &own_job, patience);
        assert!(langorder::busy(), "the fixture {} is detected", fx.pid);
        BOUNDARY.lock().unwrap().clear();
        Engine::new(paths, Config::default(), State::default(), true, None)
    }

    #[test]
    fn once_waits_for_a_previous_writer_before_any_change() {
        // Review FP-01 / F5-01 (confirmed exit), through the real one-shot
        // entry point: an orphaned "previous restore" (a harmless sleeper of
        // this test binary, in a job its fixture owns) is detected at
        // startup; run_once must not pass the mutation boundary before it
        // has ended.
        let _g = langorder::tests::previous_guard();
        let fx = langorder::tests::LegacyFixture::start(3, "once-wait");
        let h = fx.handle();
        let mut e = once_engine("stale", &fx, Duration::from_secs(60));
        let watcher = std::thread::spawn(move || {
            let h = h;
            // SAFETY: a wait on the retained handle.
            unsafe { windows::Win32::System::Threading::WaitForSingleObject(h.0, 60_000) };
            Instant::now()
        });
        let rc = run_once(&mut e);
        let ended = watcher.join().unwrap();
        let entered = BOUNDARY.lock().unwrap().first().copied();
        let entered = entered.expect("the evaluation reached the mutation boundary afterwards");
        assert!(entered >= ended, "no change started before the previous writer ended");
        assert_eq!(rc, 0, "dry run after the wait");
    }

    #[test]
    fn once_fails_without_any_change_when_its_wait_runs_out() {
        // Review F5-01 (timeout): the previous writer is still running when
        // the wait ends, also past KeyDock's patience. Exit code 1, the
        // mutation boundary is never passed, the fixture is not touched.
        let _g = langorder::tests::previous_guard();
        let fx = langorder::tests::LegacyFixture::start(60, "once-timeout");
        let mut e = once_engine("timeout", &fx, Duration::from_millis(200));
        let start = Instant::now();
        assert_eq!(run_once_with(&mut e, Duration::from_secs(2)), 1);
        assert!(start.elapsed() < Duration::from_secs(10), "bounded");
        assert!(BOUNDARY.lock().unwrap().is_empty(), "no change was started");
        assert!(fx.alive(), "the previous writer is waited for, never ended");
        // After it has ended, a new attempt proceeds.
        fx.stop();
        assert_eq!(run_once_with(&mut e, Duration::from_secs(2)), 0);
        assert_eq!(BOUNDARY.lock().unwrap().len(), 1);
    }

    #[test]
    fn once_rejects_a_config_that_becomes_unusable_while_it_waits() {
        // Review F5-04: the file is valid when --once starts and broken by
        // the time the previous writer has ended; the evaluation's own read
        // decides: exit code 1 before anything is applied.
        let _g = langorder::tests::previous_guard();
        let fx = langorder::tests::LegacyFixture::start(3, "once-badcfg");
        let mut e = once_engine("waitcfg", &fx, Duration::from_secs(60));
        let cfg = e.paths.config.clone();
        let editor = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1000));
            std::fs::write(&cfg, "remote_sessions = true\n[default]\nlayouts = [\"nonsense\"]\n").unwrap();
        });
        let rc = run_once_with(&mut e, Duration::from_secs(30));
        editor.join().unwrap();
        assert_eq!(rc, 1, "an unusable configuration is an error");
        assert!(BOUNDARY.lock().unwrap().is_empty(), "nothing was applied");
        drop(fx);
        // A file removed meanwhile is "not configured": nothing to do (0).
        let fx = langorder::tests::LegacyFixture::start(3, "once-nocfg");
        let mut e = once_engine("waitnocfg", &fx, Duration::from_secs(60));
        let cfg = e.paths.config.clone();
        let editor = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1000));
            std::fs::remove_file(&cfg).unwrap();
        });
        let rc = run_once_with(&mut e, Duration::from_secs(30));
        editor.join().unwrap();
        // 0, or 3 where this test runs outside the console session (without
        // a file there is no remote_sessions opt-in): nothing to do either way.
        assert!(
            rc == 0 || rc == 3,
            "a missing configuration stays inert: {rc} {:?}",
            e.stages.problems()
        );
        assert!(BOUNDARY.lock().unwrap().is_empty(), "no profile, nothing applied");
    }

    #[test]
    fn a_waiting_notice_ends_with_the_next_evaluation() {
        // Review S6-05, through the engine: the tray noted a previous writer
        // while it waited; once that has ended the next evaluation runs and
        // its result replaces the notice (no matching profile, or a profile
        // whose apply succeeds). A startup note about an interrupted restore
        // stays.
        let _g = langorder::tests::previous_guard();
        let waiting = langorder::Blocked {
            reason: "PowerShell process(es) 42 (a language-order restore of an older KeyDock) are still running".into(),
            overdue: false,
        };
        let interrupted = "a language-order restore did not finish; the language order is now en-US, ru";
        for (tag, config) in [("waitnone", "remote_sessions = true\n"), ("waitprofile", ONE_LAYOUT)] {
            let paths = scratch(tag);
            std::fs::write(&paths.config, config).unwrap();
            let mut e = Engine::new(paths, Config::default(), State::default(), true, None);
            e.stages.restore_finished(Some(interrupted.into()));
            e.note_previous_writer(&waiting);
            assert_eq!(e.stages.problems().len(), 2, "{tag}");
            e.evaluate(ApplyMode::Force).unwrap();
            assert_eq!(e.stages.problems(), vec![interrupted.to_string()], "{tag}");
        }
    }

    /// Enabled layouts that tests change between reads (as another program
    /// would), with a log of what was called.
    struct FakeLayouts {
        enabled: Vec<LayoutSpec>,
        unreadable: bool,
        calls: Vec<String>,
    }

    impl reconcile::LayoutApi for FakeLayouts {
        fn enabled(&mut self) -> Result<Vec<LayoutSpec>, String> {
            self.calls.push("enabled".into());
            if self.unreadable {
                return Err("EnumEnabledLayoutOrTip failed".into());
            }
            Ok(self.enabled.clone())
        }
        fn install(&mut self, specs: &[LayoutSpec]) -> Result<(), String> {
            self.calls.push("install".into());
            for s in specs {
                if !self.enabled.contains(s) {
                    self.enabled.push(*s);
                }
            }
            Ok(())
        }
        fn uninstall(&mut self, specs: &[LayoutSpec]) -> Result<(), String> {
            self.calls.push("uninstall".into());
            self.enabled.retain(|l| !specs.contains(l));
            Ok(())
        }
    }

    fn spec(s: &str) -> LayoutSpec {
        LayoutSpec::parse(s).unwrap()
    }

    /// Review KD-01, the audited sequence: the profile adds two German
    /// layouts, the reconciliation confirms both, another program removes
    /// one before the restore's snapshot, the attribution accepts what is
    /// left (a subset of what KeyDock installed), and the restore of that
    /// snapshot succeeds. --once must not exit 0. Returns the exit code and
    /// the layout calls made after the restore.
    fn once_after_restore(external_edit: bool, unreadable: bool) -> (i32, Vec<String>, Vec<String>) {
        use keydock::langorder::{explain_change, RegValue, Snapshot};
        let cfg = Config::parse(
            "language_order = \"restore\"\n[default]\nlayouts = [\"0409:00000409\", \"0407:00000407\", \"0407:00010407\"]\n",
        )
        .unwrap();
        let target = cfg.default.clone().unwrap();
        let (us, de, de_ibm) = (spec("0409:00000409"), spec("0407:00000407"), spec("0407:00010407"));
        // The apply: install both German layouts, confirmed by the final read.
        let mut api = FakeLayouts {
            enabled: vec![us],
            unreadable: false,
            calls: Vec::new(),
        };
        let (before, plan) = reconcile::plan(&mut api, &target.layouts, &cfg.managed_layouts()).unwrap();
        let applied = reconcile::apply(
            &mut api,
            &before,
            &target.layouts,
            &cfg.managed_layouts(),
            &plan,
            &mut || {},
        )
        .unwrap();
        assert_eq!(applied.installed, vec![de, de_ibm]);
        // Another program removes one of them before the restore's snapshot.
        if external_edit {
            api.enabled.retain(|l| *l != de_ibm);
        }
        // The language snapshots: German was added (at the end); what is left
        // of it is a subset of what KeyDock installed, so it is accepted.
        let snap = |langs: &[(&str, &[&str])]| {
            let mut s = Snapshot {
                languages: langs.iter().map(|(t, _)| t.to_string()).collect(),
                ..Default::default()
            };
            for (tag, tips) in langs {
                let mut m = std::collections::BTreeMap::new();
                for t in tips.iter() {
                    m.insert(
                        t.to_ascii_lowercase(),
                        RegValue {
                            ty: 4,
                            data: vec![1, 0, 0, 0],
                        },
                    );
                }
                s.subkeys.insert(tag.to_ascii_lowercase(), m);
            }
            s
        };
        let lang_before = snap(&[("en-US", &["0409:00000409"])]);
        let german: &[&str] = if external_edit {
            &["0407:00000407"]
        } else {
            &["0407:00000407", "0407:00010407"]
        };
        let lang_after = snap(&[("en-US", &["0409:00000409"]), ("de-DE", german)]);
        let attempted: Vec<String> = applied.attempted_install.iter().map(|l| l.to_string()).collect();
        assert!(
            explain_change(&lang_before, &lang_after, &attempted, &[]).is_ok(),
            "the attribution accepts it"
        );
        // The engine as the one-shot apply left it, then the restore ends.
        let paths = scratch(if external_edit { "kd01-edit" } else { "kd01-ok" });
        let mut e = Engine::new(paths, cfg, State::default(), false, None);
        e.applied_selection = Some(Selection {
            name: "default".into(),
            set: target,
            matched_by: None,
        });
        e.stages.applied(ApplyReport {
            membership: Check::Clear,
            activation: Check::Clear,
            language: Check::Clear,
            restoring: true,
            ..Default::default()
        });
        api.calls.clear();
        api.unreadable = unreadable;
        let rc = finish_once(
            &mut e,
            Some(langorder::Outcome::Restored(vec!["en-US".into(), "de-DE".into()])),
            &mut api,
        );
        (rc, api.calls.clone(), e.stages.problems())
    }

    #[test]
    fn once_checks_the_profile_after_a_restore() {
        // Review KD-01: one required layout removed after the reconciliation.
        let (rc, calls, problems) = once_after_restore(true, false);
        assert_eq!(rc, 2, "not 0: the profile is incomplete");
        assert!(
            problems.iter().any(|p| p.contains("[0407:00010407] missing")),
            "{problems:?}"
        );
        assert_eq!(calls, ["enabled"], "a single read; nothing applied again");
        // The enabled list cannot be read after the restore: not confirmed.
        let (rc, calls, problems) = once_after_restore(false, true);
        assert_eq!(rc, 2);
        assert!(problems.iter().any(|p| p.contains("cannot be read")), "{problems:?}");
        assert_eq!(calls, ["enabled"]);
        // The membership is right: 0.
        let (rc, calls, problems) = once_after_restore(false, false);
        assert_eq!(rc, 0, "{problems:?}");
        assert_eq!(calls, ["enabled"]);
    }

    #[test]
    fn once_without_a_restore_reads_nothing_more() {
        // Review KD-01: the check runs only after a restore; other paths keep
        // their exit codes and make no extra read.
        let paths = scratch("kd01-none");
        let mut e = Engine::new(paths, Config::default(), State::default(), false, None);
        let mut api = FakeLayouts {
            enabled: vec![],
            unreadable: true,
            calls: Vec::new(),
        };
        assert_eq!(finish_once(&mut e, None, &mut api), 0);
        assert!(api.calls.is_empty());
        // A restore that failed is still reported (2), with the check.
        e.applied_selection = Some(Selection {
            name: "default".into(),
            set: keydock::config::LayoutSet {
                layouts: vec![spec("0409:00000409")],
                activate: None,
            },
            matched_by: None,
        });
        api.unreadable = false;
        api.enabled = vec![spec("0409:00000409")];
        let rc = finish_once(
            &mut e,
            Some(langorder::Outcome::Failed("PowerShell exited with code 1".into())),
            &mut api,
        );
        assert_eq!(rc, 2);
        assert_eq!(
            e.stages.problems().len(),
            1,
            "only the restore's result: {:?}",
            e.stages.problems()
        );
    }

    /// A default input whose reads and the set's result the test decides.
    struct FakeDefault {
        reads: Vec<Option<u32>>,
        set_result: Result<(), String>,
        sets: usize,
    }

    impl DefaultInput for FakeDefault {
        fn read(&mut self) -> Option<u32> {
            if self.reads.is_empty() {
                None
            } else {
                self.reads.remove(0)
            }
        }
        fn set(&mut self) -> Result<(), String> {
            self.sets += 1;
            self.set_result.clone()
        }
    }

    /// The early check while switching failed (Russian requested, the
    /// read-back failed); then the final check runs with `io`. Returns the
    /// activation status, the number of sets, and --once's exit code.
    fn after_early_failure(io: &mut FakeDefault, foreground: &[&str]) -> (Check, usize, i32) {
        let a = spec("0419:00000419");
        let early = Some("switching to 0419:00000419: cannot read the default input language back".to_string());
        let others: Vec<String> = foreground.iter().map(|s| s.to_string()).collect();
        let last = keep_default_input(io, &a, 0x0419_0419);
        let status = activation_status(&others, early, Some(last));
        let paths = scratch("default-status");
        let mut e = Engine::new(paths, Config::default(), State::default(), false, None);
        e.stages.applied(ApplyReport {
            membership: Check::Clear,
            activation: status.clone(),
            ..Default::default()
        });
        let rc = finish_once(
            &mut e,
            None,
            &mut FakeLayouts {
                enabled: vec![],
                unreadable: false,
                calls: vec![],
            },
        );
        (status, io.sets, rc)
    }

    #[test]
    fn a_recovered_default_input_leaves_no_warning() {
        // Review (final): the early read-back failed, the final read shows the
        // wanted layout: no set, no warning, --once exits 0.
        let (status, sets, rc) = after_early_failure(
            &mut FakeDefault {
                reads: vec![Some(0x0419_0419)],
                set_result: Ok(()),
                sets: 0,
            },
            &[],
        );
        assert_eq!((status, sets, rc), (Check::Clear, 0, 0));
        // The final read shows another layout: one corrective set, read back
        // by the setter: no warning either.
        let (status, sets, rc) = after_early_failure(
            &mut FakeDefault {
                reads: vec![Some(0x0409_0409)],
                set_result: Ok(()),
                sets: 0,
            },
            &[],
        );
        assert_eq!((status, sets, rc), (Check::Clear, 1, 0));
    }

    #[test]
    fn an_unresolved_default_input_or_a_refused_switch_is_still_reported() {
        // The correction fails: its own error (not the early one), exit 2.
        let (status, sets, rc) = after_early_failure(
            &mut FakeDefault {
                reads: vec![None],
                set_result: Err("the default input language is 0x04090409, not 0x04190419".into()),
                sets: 0,
            },
            &[],
        );
        assert_eq!(sets, 1);
        assert_eq!(rc, 2);
        let Check::Problem(p) = status else {
            panic!("a problem expected")
        };
        assert!(
            p.contains("switching new windows to 0419:00000419") && !p.contains("cannot read"),
            "{p}"
        );
        // A refused foreground switch stays, independently of a recovered
        // default input.
        let refused = "switching to 0419:00000419: the active window did not accept the switch (Access is denied.)";
        let (status, _, rc) = after_early_failure(
            &mut FakeDefault {
                reads: vec![Some(0x0419_0419)],
                set_result: Ok(()),
                sets: 0,
            },
            &[refused],
        );
        assert_eq!(status, Check::Problem(refused.into()));
        assert_eq!(rc, 2);
        // No final check (the apply stopped before it): the early result stands.
        assert_eq!(
            activation_status(&[], Some("early".into()), None),
            Check::Problem("early".into())
        );
    }

    #[test]
    fn the_default_input_is_set_again_only_when_it_changed() {
        // The removal reset it (seen on Windows 10 22H2): set again.
        assert!(default_input_changed(Some(0x0409_0409), 0x0419_0419));
        // Unreadable: set again (and read back by the setter).
        assert!(default_input_changed(None, 0x0419_0419));
        // Unchanged: nothing to do.
        assert!(!default_input_changed(Some(0x0419_0419), 0x0419_0419));
    }

    #[test]
    fn the_tray_keeps_its_last_usable_configuration() {
        // Review F5-04: only --once is strict; the tray evaluation keeps the
        // configuration it had (and reports the error).
        let paths = scratch("traycfg");
        std::fs::write(&paths.config, ONE_LAYOUT).unwrap();
        let mut e = Engine::new(paths, Config::default(), State::default(), true, None);
        e.reload_config();
        assert!(e.config_error.is_none() && e.cfg.default.is_some());
        std::fs::write(&e.paths.config, "[default]\nlayouts = [\"nonsense\"]\n").unwrap();
        e.reload_config();
        assert!(e.config_error.is_some());
        assert!(e.cfg.default.is_some(), "the last usable configuration is kept");
        assert!(!e.strict_config);
    }

    fn scratch(name: &str) -> Paths {
        let d = std::env::temp_dir().join(format!("keydock-engine-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Paths {
            config: d.join("config.toml"),
            state: d.join("state.toml"),
            log: d.join("keydock.log"),
        }
    }

    #[test]
    fn hand_edited_timing_reaches_the_scheduler() {
        // Review TP-06: removal_delay_ms edited in the file is published on
        // the next reload, and the next removal uses it.
        let paths = scratch("timing");
        std::fs::write(&paths.config, "removal_delay_ms = 10000\n").unwrap();
        let mut e = Engine::new(paths, Config::default(), State::default(), true, None);
        e.on_config = Some(window::publish_timing);
        window::publish_timing(&Config::default());
        assert_eq!(window::current_timing().removal_delay_ms, 2500);
        e.reload_config();
        let t = window::current_timing();
        assert_eq!(t.removal_delay_ms, 10_000);
        let mut s = keydock::schedule::Scheduler::default();
        assert_eq!(s.on_event(keydock::schedule::Trigger::KeyboardRemoved, 0, &t), 10_000);
        // An unusable file does not publish anything.
        std::fs::write(&e.paths.config, "removal_delay_ms = ").unwrap();
        e.reload_config();
        assert!(e.config_error.is_some());
        assert_eq!(window::current_timing().removal_delay_ms, 10_000);
    }

    #[test]
    fn once_with_an_unusable_config_is_an_error() {
        // Review TP-10: not "nothing to do" (exit 0).
        let paths = scratch("badcfg");
        std::fs::write(&paths.config, "[default]\nlayouts = [\"nonsense\"]\n").unwrap();
        let mut e = Engine::new(paths, Config::default(), State::default(), true, None);
        assert_eq!(run_once(&mut e), 1);
        // A missing file is "not configured", not an error.
        let paths = scratch("nocfg");
        let mut e = Engine::new(paths, Config::default(), State::default(), true, None);
        e.reload_config();
        assert!(e.config_error.is_none());
    }
}
