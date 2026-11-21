//! When to evaluate. Pure state with explicit time (milliseconds of a
//! monotonic clock); the window turns its answers into `SetTimer` calls.
//!
//! There is one pending evaluation (the strongest [`ApplyMode`] requested)
//! and a due time that is the latest of several independent constraints:
//!
//! * the trailing debounce (every event pushes it out),
//! * the delay after resume or a session change,
//! * the hold-off after a keyboard is removed (docks drop devices briefly),
//! * the back-off after a failed evaluation.
//!
//! Each constraint keeps its own deadline, so a later, shorter request (an
//! arrival after a resume, a restore completion during a back-off) can never
//! shorten another one. A timer that fires early (a stale `WM_TIMER` already
//! queued when the timer was reset) consumes nothing; the caller re-arms.
//!
//! Manual actions (Re-apply, Save/Apply, resume from Paused, `--once` handed
//! to the running instance) skip the debounce and the back-off, but not the
//! removal hold-off or the resume delay: while those run the keyboards are
//! still settling, and applying then could pick an intermediate profile.

use crate::select::{retry_delay, ApplyMode, RETRY_DELAYS_MS};

/// Delays from the configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub debounce_ms: u32,
    pub resume_delay_ms: u32,
    /// After a keyboard is removed, how long the keyboard set must stay
    /// unchanged before a mode with fewer external keyboards is applied (docks
    /// and hubs often drop their devices for a moment).
    pub removal_delay_ms: u32,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            debounce_ms: crate::config::DEFAULT_DEBOUNCE_MS,
            resume_delay_ms: crate::config::DEFAULT_RESUME_DELAY_MS,
            removal_delay_ms: crate::config::DEFAULT_REMOVAL_DELAY_MS,
        }
    }
}

/// What caused an evaluation request from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    KeyboardArrived,
    KeyboardRemoved,
    /// Resume from sleep or hibernation.
    Resumed,
    /// Session unlock, console or remote (re)connect.
    SessionChanged,
}

/// What to do after a failed evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterFailure {
    /// Try again at `due` (attempt `attempt` of `of`).
    Retry { due: u64, attempt: usize, of: usize },
    /// The retries are used up; wait for the next event.
    GiveUp,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scheduler {
    /// The strongest evaluation requested and not yet run.
    queued: Option<ApplyMode>,
    /// Trailing debounce deadline (0 = none).
    debounce_at: u64,
    /// Resume / session-change delay.
    not_before: u64,
    /// Removal hold-off.
    hold_until: u64,
    /// Back-off after a failure.
    retry_at: u64,
    /// Failed evaluations in a row.
    retries: usize,
}

impl Scheduler {
    fn queue(&mut self, mode: ApplyMode) {
        self.queued = Some(self.queued.map_or(mode, |m| m.max(mode)));
    }

    /// When the pending evaluation is due, if there is one.
    pub fn due(&self) -> Option<u64> {
        self.queued?;
        Some(
            self.debounce_at
                .max(self.not_before)
                .max(self.hold_until)
                .max(self.retry_at),
        )
    }

    /// A keyboard, power or session event at `now`. New hardware state is new
    /// information, so it ends a back-off; it never shortens a hold-off or a
    /// resume delay. Returns the due time.
    pub fn on_event(&mut self, trigger: Trigger, now: u64, t: &Timing) -> u64 {
        self.retries = 0;
        self.retry_at = 0;
        self.queue(ApplyMode::Normal);
        self.debounce_at = self.debounce_at.max(now + u64::from(t.debounce_ms));
        match trigger {
            Trigger::KeyboardArrived => {}
            Trigger::KeyboardRemoved => {
                self.hold_until = self.hold_until.max(now + u64::from(t.removal_delay_ms));
            }
            Trigger::Resumed | Trigger::SessionChanged => {
                self.not_before = self.not_before.max(now + u64::from(t.resume_delay_ms));
            }
        }
        self.due().unwrap_or(now)
    }

    /// An internal request (a restore finished, a deferred evaluation) of at
    /// least `mode`, not before `now + delay`. Respects every other deadline,
    /// including a running back-off. Returns the due time.
    pub fn request(&mut self, mode: ApplyMode, delay: u32, now: u64) -> u64 {
        self.queue(mode);
        self.debounce_at = self.debounce_at.max(now + u64::from(delay));
        self.due().unwrap_or(now)
    }

    /// A manual action (see the module documentation). Returns the due time;
    /// `now` when it may run at once.
    pub fn manual(&mut self, now: u64) -> u64 {
        self.queue(ApplyMode::Force);
        self.retries = 0;
        self.retry_at = 0;
        self.debounce_at = now;
        self.due().unwrap_or(now)
    }

    /// An evaluation found a language-order restore running: keep `mode` and
    /// look again after `poll_ms` (in case the completion message is lost).
    /// [`Self::restore_finished`] replaces that poll.
    pub fn defer_busy(&mut self, mode: ApplyMode, poll_ms: u32, now: u64) -> u64 {
        self.queue(mode);
        self.debounce_at = now + u64::from(poll_ms);
        self.due().unwrap_or(now)
    }

    /// The restore ended: verify after the debounce, replacing the busy poll
    /// but keeping every other deadline (hold-off, resume delay, back-off).
    pub fn restore_finished(&mut self, debounce_ms: u32, now: u64) -> u64 {
        self.queue(ApplyMode::Verify);
        self.debounce_at = now + u64::from(debounce_ms);
        self.due().unwrap_or(now)
    }

    /// Keeps a request without changing any deadline (the app was busy).
    pub fn requeue(&mut self, mode: ApplyMode) {
        self.queue(mode);
    }

    /// The timer fired at `now`: the mode to evaluate with, or `None` when
    /// nothing is due yet (an early or stale timer; re-arm for [`Self::due`]).
    pub fn on_timer(&mut self, now: u64) -> Option<ApplyMode> {
        if self.due()? > now {
            return None;
        }
        self.debounce_at = 0;
        self.not_before = 0;
        self.hold_until = 0;
        self.retry_at = 0;
        self.queued.take()
    }

    pub fn on_success(&mut self) {
        self.retries = 0;
    }

    /// An evaluation in `mode` failed at `now`: retry it with back-off, or give up.
    pub fn on_failure(&mut self, mode: ApplyMode, now: u64) -> AfterFailure {
        match retry_delay(self.retries) {
            Some(d) => {
                self.retries += 1;
                self.queue(mode);
                self.retry_at = now + u64::from(d);
                AfterFailure::Retry {
                    due: self.due().unwrap_or(self.retry_at),
                    attempt: self.retries,
                    of: RETRY_DELAYS_MS.len(),
                }
            }
            None => {
                self.retries = 0;
                AfterFailure::GiveUp
            }
        }
    }

    /// The pending request, if any (for diagnostics and tests).
    pub fn queued(&self) -> Option<ApplyMode> {
        self.queued
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ApplyMode::*;

    const T: Timing = Timing {
        debounce_ms: 500,
        resume_delay_ms: 2000,
        removal_delay_ms: 2500,
    };

    #[test]
    fn events_debounce_and_resume_delay() {
        let mut s = Scheduler::default();
        assert_eq!(s.on_event(Trigger::KeyboardArrived, 0, &T), 500);
        assert_eq!(s.on_event(Trigger::Resumed, 100, &T), 2100);
        assert_eq!(s.on_event(Trigger::SessionChanged, 100, &T), 2100);
        assert_eq!(s.on_timer(2100), Some(Normal));
        assert_eq!(s.queued(), None, "the request is consumed");
        assert_eq!(s.on_timer(3000), None, "a stray timer does nothing");
    }

    #[test]
    fn arrival_after_resume_keeps_the_resume_delay() {
        // Review KD-05: resume at 0, arrival at 100 used to evaluate at 600.
        let mut s = Scheduler::default();
        assert_eq!(s.on_event(Trigger::Resumed, 0, &T), 2000);
        assert_eq!(s.on_event(Trigger::KeyboardArrived, 100, &T), 2000);
        assert_eq!(s.on_timer(600), None, "the old 600 ms timer is early");
        assert_eq!(s.due(), Some(2000));
        assert_eq!(s.on_timer(2000), Some(Normal));
    }

    #[test]
    fn removal_holds_off_until_the_set_is_stable() {
        let mut s = Scheduler::default();
        assert_eq!(s.on_event(Trigger::KeyboardRemoved, 0, &T), 2500);
        // The keyboard comes back quickly: no earlier evaluation than the hold-off.
        assert_eq!(s.on_event(Trigger::KeyboardArrived, 1000, &T), 2500);
        // A second removal extends the hold-off.
        assert_eq!(s.on_event(Trigger::KeyboardRemoved, 2000, &T), 4500);
        assert_eq!(s.on_timer(4499), None);
        assert_eq!(s.on_timer(4500), Some(Normal));
        // Hold-off over: arrivals are fast again.
        assert_eq!(s.on_event(Trigger::KeyboardArrived, 5000, &T), 5500);
        assert_eq!(s.on_timer(5500), Some(Normal));
        // Internal requests respect a running hold-off too.
        s.on_event(Trigger::KeyboardRemoved, 10_000, &T);
        assert_eq!(s.request(Verify, 500, 10_100), 12_500);
    }

    #[test]
    fn stale_timer_consumes_nothing() {
        let mut s = Scheduler::default();
        s.on_event(Trigger::KeyboardRemoved, 0, &T);
        // A WM_TIMER from an earlier, shorter arming arrives at 500.
        assert_eq!(s.on_timer(500), None);
        assert_eq!(s.queued(), Some(Normal), "still pending");
        assert_eq!(s.due(), Some(2500));
    }

    #[test]
    fn strongest_request_wins() {
        let mut s = Scheduler::default();
        s.request(Force, 500, 0);
        s.on_event(Trigger::KeyboardArrived, 10, &T);
        assert_eq!(s.on_timer(600), Some(Force));
        s.requeue(Verify);
        s.on_event(Trigger::KeyboardRemoved, 700, &T);
        assert_eq!(s.on_timer(3200), Some(Verify));
    }

    #[test]
    fn busy_app_keeps_the_request() {
        let mut s = Scheduler::default();
        s.request(Force, 500, 0);
        let mode = s.on_timer(500).unwrap();
        // The app was busy: the window puts the request back.
        s.requeue(mode);
        assert_eq!(s.request(Normal, 500, 600), 1100);
        assert_eq!(s.on_timer(1100), Some(Force), "not downgraded by the reschedule");
    }

    #[test]
    fn restore_completion_does_not_shorten_a_back_off() {
        // Review KD-05 / section D: a failed apply arms a 5 s retry, and the
        // restore it started finishes 300 ms later.
        let mut s = Scheduler::default();
        s.on_failure(Force, 0);
        s.on_timer(1000);
        s.on_failure(Force, 1000);
        let AfterFailure::Retry { due, attempt, .. } = s.on_failure(Force, 3000) else {
            panic!("expected a retry")
        };
        assert_eq!((due, attempt), (8000, 3));
        assert_eq!(s.request(Verify, 500, 3300), 8000, "completion keeps the back-off");
        assert_eq!(s.on_timer(3800), None);
        assert_eq!(s.on_timer(8000), Some(Force), "the failed mode, not downgraded");
    }

    #[test]
    fn failures_back_off_then_give_up_until_an_event() {
        let mut s = Scheduler::default();
        let mut delays = Vec::new();
        let mut now = 0;
        while let AfterFailure::Retry { due, attempt, of } = s.on_failure(Force, now) {
            assert_eq!(of, RETRY_DELAYS_MS.len());
            assert_eq!(attempt, delays.len() + 1);
            delays.push((due - now) as u32);
            assert_eq!(s.on_timer(due - 1), None, "not before the back-off");
            assert_eq!(s.on_timer(due), Some(Force), "the failed mode is retried");
            now = due;
        }
        assert_eq!(delays, RETRY_DELAYS_MS);
        // A new event starts the retries over.
        s.on_event(Trigger::KeyboardArrived, now, &T);
        assert!(matches!(
            s.on_failure(Normal, now),
            AfterFailure::Retry { attempt: 1, .. }
        ));
        s.on_success();
        assert!(matches!(
            s.on_failure(Normal, now),
            AfterFailure::Retry { attempt: 1, .. }
        ));
    }

    #[test]
    fn manual_actions_skip_back_off_but_not_hold_off() {
        let mut s = Scheduler::default();
        s.on_failure(Normal, 0);
        assert_eq!(s.manual(100), 100, "Re-apply does not wait for the back-off");
        assert_eq!(s.on_timer(100), Some(Force));
        // A removal hold-off still applies: the keyboards are settling.
        s.on_event(Trigger::KeyboardRemoved, 1000, &T);
        assert_eq!(s.manual(1200), 3500);
        assert_eq!(s.on_timer(1200), None);
        assert_eq!(s.on_timer(3500), Some(Force));
        // So does the resume delay.
        s.on_event(Trigger::Resumed, 5000, &T);
        assert_eq!(s.manual(5100), 7000);
        // Manual resets the retries.
        assert!(matches!(
            s.on_failure(Force, 7000),
            AfterFailure::Retry { attempt: 1, .. }
        ));
    }

    #[test]
    fn reentrant_removal_keeps_its_hold_off() {
        // An event that arrives while the app is busy is recorded with its
        // own trigger and time (the window calls on_event directly).
        let mut s = Scheduler::default();
        s.request(Force, 0, 0);
        let mode = s.on_timer(0).unwrap();
        // While evaluating, a removal arrives at 50.
        s.on_event(Trigger::KeyboardRemoved, 50, &T);
        // The evaluation was deferred (restore running) and is put back.
        s.requeue(mode);
        assert_eq!(s.due(), Some(2550), "the hold-off survives");
        assert_eq!(s.on_timer(2550), Some(Force));
    }

    #[test]
    fn busy_poll_is_replaced_by_the_completion() {
        let mut s = Scheduler::default();
        s.manual(0);
        let mode = s.on_timer(0).unwrap();
        // The restore is still running: poll again in 5 s.
        assert_eq!(s.defer_busy(mode, 5000, 0), 5000);
        // It finishes at 300: verify at 800, not at 5000.
        assert_eq!(s.restore_finished(500, 300), 800);
        assert_eq!(s.on_timer(800), Some(Force), "the manual request is kept");
        // A removal during the restore keeps its hold-off.
        s.defer_busy(Normal, 5000, 1000);
        s.on_event(Trigger::KeyboardRemoved, 1100, &T);
        assert_eq!(s.restore_finished(500, 1200), 3600);
        // Save/Apply during a restore: deferred, then applied after it.
        let mut s = Scheduler::default();
        s.defer_busy(Normal, 5000, 0);
        s.manual(100);
        s.defer_busy(Force, 5000, 100);
        assert_eq!(s.restore_finished(500, 2000), 2500);
        assert_eq!(s.on_timer(2500), Some(Force));
    }

    #[test]
    fn lost_completion_is_caught_by_the_poll() {
        // Section D of the third review: an idle evaluation starts a restore
        // and nothing else is queued; the completion message is lost.
        let mut s = Scheduler::default();
        s.manual(0);
        assert_eq!(s.on_timer(0), Some(Force));
        // The window arms a poll right after starting the restore.
        assert_eq!(s.defer_busy(Verify, 5000, 10), 5010);
        // No completion message ever arrives: the poll still evaluates.
        assert_eq!(s.on_timer(5010), Some(Verify));
    }

    #[test]
    fn rapid_undock_redock_evaluates_once_after_settling() {
        let mut s = Scheduler::default();
        // Undock, redock, undock, redock within a second.
        s.on_event(Trigger::KeyboardRemoved, 0, &T);
        s.on_event(Trigger::KeyboardArrived, 200, &T);
        s.on_event(Trigger::KeyboardRemoved, 400, &T);
        s.on_event(Trigger::KeyboardArrived, 600, &T);
        for t in [500, 1100, 2500] {
            assert_eq!(s.on_timer(t), None, "nothing before the last hold-off ends");
        }
        assert_eq!(s.on_timer(2900), Some(Normal), "one evaluation, from the final sample");
        assert_eq!(s.on_timer(3000), None);
    }

    #[test]
    fn settling_resample_does_not_depend_on_the_failure_budget() {
        // Review FP-06: enumeration failed five times (the back-off is used
        // up), then it works but a keyboard is only half described. The
        // window asks for a resample instead of another failure retry.
        let mut s = Scheduler::default();
        let mut now = 0;
        while let AfterFailure::Retry { due, .. } = s.on_failure(Normal, now) {
            assert_eq!(s.on_timer(due), Some(Normal));
            now = due;
        }
        assert_eq!(s.queued(), None, "given up: nothing pending");
        // The settling resample is an ordinary request: it is due.
        assert_eq!(s.request(Normal, 1000, now), now + 1000);
        assert_eq!(s.on_timer(now + 1000), Some(Normal));
    }

    #[test]
    fn a_late_arrival_is_resampled_after_retries_and_a_fallback_are_used_up() {
        // Review F5-03 with the ordinary retries exhausted: keyboard A was
        // given up on, the failure back-off is used up, and much later B
        // arrives incomplete. B is waited for (fresh budget) and each sample
        // is scheduled as an ordinary request, one second apart, until B's
        // budget ends.
        use crate::select::{Settle, Settling};
        let a = vec!["A".to_string()];
        let ab = vec!["A".to_string(), "B".to_string()];
        let mut st = Settling::default();
        assert!(matches!(st.assess(&a, 0, 15_000, 60_000), Settle::Wait(_)));
        assert!(matches!(st.assess(&a, 15_000, 15_000, 60_000), Settle::GaveUp(_)));
        let mut s = Scheduler::default();
        let mut now = 20_000;
        while let AfterFailure::Retry { due, .. } = s.on_failure(Normal, now) {
            assert_eq!(s.on_timer(due), Some(Normal));
            now = due;
        }
        now += 600_000;
        let mut samples = 0;
        loop {
            match st.assess(&ab, now, 15_000, 60_000) {
                Settle::Wait(w) => {
                    assert_eq!(w, vec!["B".to_string()]);
                    let due = s.request(Normal, 1000, now);
                    assert_eq!(due, now + 1000, "resampled on its own schedule");
                    assert_eq!(s.on_timer(due), Some(Normal));
                    now = due;
                    samples += 1;
                }
                Settle::GaveUp(g) => {
                    assert_eq!(g, vec!["B".to_string()]);
                    break;
                }
                Settle::Ready => panic!("B must be waited for"),
            }
        }
        assert_eq!(samples, 15, "one sample per second for B's 15 s budget");
    }

    #[test]
    fn restore_then_verify() {
        let mut s = Scheduler::default();
        // Evaluation deferred during a restore, then the restore finishes.
        s.requeue(Normal);
        s.requeue(Verify);
        assert_eq!(s.request(Verify, 500, 0), 500);
        assert_eq!(s.on_timer(500), Some(Verify));
    }
}
