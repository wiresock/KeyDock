//! Profile selection and layout change planning.

use crate::config::{Config, LayoutSet};
use crate::device::{KeyboardInfo, KeyboardKind};
use crate::layout::LayoutSpec;
use crate::state::State;

/// Classifies one keyboard. Order: virtual, `ignore`, `external`, then
/// built-in (`builtin` list, state.toml, heuristics), otherwise external.
pub fn classify(kb: &KeyboardInfo, cfg: &Config, state: &State) -> KeyboardKind {
    if kb.is_virtual() {
        KeyboardKind::Virtual
    } else if cfg.ignore.iter().any(|m| m.matches_identity(kb)) {
        KeyboardKind::Ignored
    } else if cfg.external.iter().any(|m| m.matches_identity(kb)) {
        KeyboardKind::External
    } else if cfg.builtin.iter().any(|m| m.matches_identity(kb))
        || state.is_builtin(&kb.instance_id)
        || (cfg.builtin_heuristics && kb.looks_built_in())
    {
        KeyboardKind::BuiltIn
    } else {
        KeyboardKind::External
    }
}

/// The outcome of evaluating the configuration against the present keyboards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Profile name, or `"default"`.
    pub name: String,
    pub set: LayoutSet,
    /// Instance ID of the keyboard that triggered the profile (none for default).
    pub matched_by: Option<String>,
}

/// Picks the first profile (in file order) whose `when` matches any present
/// keyboard, otherwise `[default]`, otherwise `None` (nothing to do).
pub fn select_profile(cfg: &Config, keyboards: &[KeyboardInfo], state: &State) -> Option<Selection> {
    let usable: Vec<(&KeyboardInfo, KeyboardKind)> = keyboards
        .iter()
        .map(|k| (k, classify(k, cfg, state)))
        .filter(|(_, kind)| matches!(kind, KeyboardKind::BuiltIn | KeyboardKind::External))
        .collect();
    for p in &cfg.profiles {
        if let Some((kb, _)) = usable
            .iter()
            .find(|(kb, kind)| p.matches(kb, *kind == KeyboardKind::External))
        {
            return Some(Selection {
                name: p.name.clone(),
                set: p.set.clone(),
                matched_by: Some(kb.instance_id.clone()),
            });
        }
    }
    cfg.default.as_ref().map(|d| Selection {
        name: "default".into(),
        set: d.clone(),
        matched_by: None,
    })
}

/// Whether `m` could match `kb` once the identity data missing from an
/// incomplete keyboard is available: false only when a criterion is
/// definitely not met by data that is present.
fn may_match(m: &crate::config::Matcher, kb: &KeyboardInfo) -> bool {
    if let Some(vid) = m.vid {
        let ids: Vec<(u16, u16)> = kb.ids().filter_map(crate::device::parse_vid_pid).collect();
        if !ids.is_empty() && !ids.iter().any(|(v, p)| *v == vid && m.pid.is_none_or(|w| w == *p)) {
            return false;
        }
    }
    if let (Some(c), Some(k)) = (&m.container_id, &kb.container_id) {
        if !crate::device::guid_eq(c, k) {
            return false;
        }
    }
    if let Some(i) = &m.instance_id {
        if !kb.instance_id.is_empty() && !kb.instance_id.to_ascii_uppercase().contains(&i.to_ascii_uppercase()) {
            return false;
        }
    }
    // A name is optional metadata; it can match whatever the device reports later.
    true
}

/// The incomplete keyboards (see `KeyboardInfo::incomplete`) whose missing
/// identity could change the selected profile. The selection is made
/// without them; it could change only if a profile that comes before the
/// selected one might match one of them once its identity is known (its
/// classification is unknown too, so it might count as external).
pub fn unsettled_by<'a>(cfg: &Config, keyboards: &'a [KeyboardInfo], state: &State) -> Vec<&'a KeyboardInfo> {
    let complete: Vec<KeyboardInfo> = keyboards.iter().filter(|k| k.incomplete.is_none()).cloned().collect();
    let selected = select_profile(cfg, &complete, state);
    // "default" is a reserved name, so the default set has no position.
    let before = selected
        .and_then(|s| cfg.profiles.iter().position(|p| p.name == s.name))
        .unwrap_or(cfg.profiles.len());
    keyboards
        .iter()
        .filter(|k| k.incomplete.is_some())
        .filter(|k| !(k.is_virtual() && !k.instance_id.is_empty()))
        .filter(|k| {
            cfg.profiles[..before]
                .iter()
                .any(|p| p.when.iter().any(|m| may_match(m, k)))
        })
        .collect()
}

/// What to do about the incomplete keyboards that could change the profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settle {
    /// None (left) to wait for.
    Ready,
    /// Keep the current layouts and sample again: these are still within
    /// their own settling budget.
    Wait(Vec<String>),
    /// These used up their budget just now; they are left out from now on
    /// (reported once).
    GaveUp(Vec<String>),
}

/// A settling budget per incomplete keyboard interface (review TP-07): a
/// keyboard that starts settling gets its own budget, so one that never
/// becomes readable cannot make a later arrival skip waiting. An entry is
/// forgotten once the interface is no longer unsettled, so it gets a fresh
/// budget if it becomes incomplete again. Retries stay bounded: each
/// interface waits at most `budget_ms` from when it was first seen, and one
/// waiting episode at most `episode_ms`.
///
/// Falling back (giving up on every interface still waited for) ends the
/// episode (review F5-03): those interfaces are exhausted and never restart
/// a wait while they stay unsettled, and an interface that is new after
/// that gets a fresh episode with its own budget.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settling {
    since: std::collections::BTreeMap<String, u64>,
    /// Interfaces whose budget ran out (or whose episode ended), while they
    /// stay unsettled.
    gave_up: std::collections::BTreeSet<String>,
    /// When the current waiting episode began (`None`: not waiting).
    episode: Option<u64>,
}

impl Settling {
    /// `unsettled`: interface paths of the keyboards that could change the
    /// profile now (see [`unsettled_by`]); `now` in milliseconds. Each
    /// interface waits at most `budget_ms`; all waiting together ends
    /// `episode_ms` after the first unsettled sample of a run, so keyboards
    /// that keep appearing incomplete one after the other cannot hold back
    /// the profile forever.
    pub fn assess(&mut self, unsettled: &[String], now: u64, budget_ms: u64, episode_ms: u64) -> Settle {
        self.assess_present(unsettled, unsettled, now, budget_ms, episode_ms)
    }

    /// [`Self::assess`] knowing every interface that is present and
    /// incomplete (`incomplete`), whether or not it could change the profile
    /// now (`unsettled`, a subset). Review S6-04: an interface keeps its
    /// budget and its exhaustion while it stays present and incomplete, also
    /// while a complete keyboard makes it irrelevant for a while; only its
    /// absence or a readable sample clears them. Only `unsettled` interfaces
    /// can make an evaluation wait.
    pub fn assess_present(
        &mut self,
        incomplete: &[String],
        unsettled: &[String],
        now: u64,
        budget_ms: u64,
        episode_ms: u64,
    ) -> Settle {
        self.since
            .retain(|k, _| incomplete.contains(k) || unsettled.contains(k));
        self.gave_up.retain(|k| incomplete.contains(k) || unsettled.contains(k));
        // Exhausted interfaces neither wait nor start an episode.
        let candidates: Vec<&String> = unsettled.iter().filter(|k| !self.gave_up.contains(*k)).collect();
        if candidates.is_empty() {
            self.episode = None;
            return Settle::Ready;
        }
        let episode = *self.episode.get_or_insert(now);
        let episode_over = now.saturating_sub(episode) >= episode_ms;
        for k in &candidates {
            self.since.entry((*k).clone()).or_insert(now);
        }
        let waiting: Vec<String> = candidates
            .iter()
            .filter(|k| !episode_over && now.saturating_sub(self.since[**k]) < budget_ms)
            .map(|k| (*k).clone())
            .collect();
        if !waiting.is_empty() {
            return Settle::Wait(waiting);
        }
        // Fall back: every candidate is exhausted, and the episode ends.
        let newly: Vec<String> = candidates.into_iter().cloned().collect();
        self.gave_up.extend(newly.iter().cloned());
        self.episode = None;
        Settle::GaveUp(newly)
    }
}

/// How an evaluation decides whether to apply the selected profile. Ordered
/// by strength: when several evaluations are requested before one runs, the
/// strongest wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ApplyMode {
    /// Apply only when the selected profile changed (device, power and session events).
    Normal,
    /// As `Normal`, and also when the profile is unchanged but the layouts no
    /// longer match it (after another program may have changed them).
    Verify,
    /// Always apply, including the switch to the `activate` layout (start,
    /// Save/Apply, Re-apply, resume from Paused).
    Force,
}

/// Whether to apply. `plan_is_empty` is only evaluated when the answer
/// depends on it (it reads the system state).
pub fn needs_apply(mode: ApplyMode, same_profile: bool, plan_is_empty: impl FnOnce() -> bool) -> bool {
    match mode {
        ApplyMode::Force => true,
        _ if !same_profile => true,
        ApplyMode::Normal => false,
        ApplyMode::Verify => !plan_is_empty(),
    }
}

/// Delays before retrying a failed evaluation (enumeration or layout change).
pub const RETRY_DELAYS_MS: [u32; 5] = [1_000, 2_000, 5_000, 10_000, 30_000];

/// The delay before retry number `attempt` (0-based), `None` once the retries are used up.
pub fn retry_delay(attempt: usize) -> Option<u32> {
    RETRY_DELAYS_MS.get(attempt).copied()
}

/// What has to change to reach a target layout set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Layouts to install (always done first, so the user never ends up with none).
    pub install: Vec<LayoutSpec>,
    /// Managed layouts that are not in the target set and have to be removed.
    pub uninstall: Vec<LayoutSpec>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.install.is_empty() && self.uninstall.is_empty()
    }
}

/// Builds the plan. `is_present` reports whether a layout is currently
/// installed: `Some(true)`/`Some(false)` when known, `None` when it cannot be
/// determined (then the action is performed anyway, both operations are
/// harmless when repeated). Only layouts in `managed` are ever removed.
pub fn make_plan(
    target: &[LayoutSpec],
    managed: &[LayoutSpec],
    is_present: impl Fn(&LayoutSpec) -> Option<bool>,
) -> Plan {
    let mut plan = Plan::default();
    for t in target {
        if is_present(t) != Some(true) && !plan.install.contains(t) {
            plan.install.push(*t);
        }
    }
    for m in managed {
        if !target.contains(m) && is_present(m) != Some(false) && !plan.uninstall.contains(m) {
            plan.uninstall.push(*m);
        }
    }
    plan
}

/// Layouts to switch to when the active one is about to be removed, in order
/// of preference: the set's `activate` layout, then the target layouts in
/// configured order. Never contains a layout from `removed`.
pub fn switch_candidates(set: &LayoutSet, removed: &[LayoutSpec]) -> Vec<LayoutSpec> {
    let mut out: Vec<LayoutSpec> = Vec::new();
    for l in set.activate.iter().chain(set.layouts.iter()) {
        if !removed.contains(l) && !out.contains(l) {
            out.push(*l);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_modes() {
        use ApplyMode::*;
        let never = || -> bool { panic!("plan must not be computed") };
        assert!(needs_apply(Force, true, never));
        assert!(needs_apply(Normal, false, never));
        assert!(needs_apply(Verify, false, never));
        assert!(!needs_apply(Normal, true, never));
        assert!(!needs_apply(Verify, true, || true), "layouts still match");
        assert!(
            needs_apply(Verify, true, || false),
            "layouts were changed behind KeyDock's back"
        );
        assert!(Normal < Verify && Verify < Force);
        assert_eq!(Normal.max(Verify), Verify);
    }

    #[test]
    fn retries_are_bounded_and_growing() {
        let delays: Vec<u32> = (0..).map_while(retry_delay).collect();
        assert_eq!(delays, RETRY_DELAYS_MS);
        assert!(delays.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(retry_delay(RETRY_DELAYS_MS.len()), None);
    }

    #[test]
    fn switch_candidates_prefer_activate_and_skip_removed() {
        let us = LayoutSpec::parse("0409:00000409").unwrap();
        let ru = LayoutSpec::parse("0419:00000419").unwrap();
        let es = LayoutSpec::parse("0409:0000040A").unwrap();
        let set = LayoutSet {
            layouts: vec![ru, us, es],
            activate: Some(us),
        };
        assert_eq!(switch_candidates(&set, &[es]), vec![us, ru]);
        let set = LayoutSet {
            layouts: vec![es, ru, us],
            activate: None,
        };
        assert_eq!(switch_candidates(&set, &[es]), vec![ru, us]);
        assert!(switch_candidates(&LayoutSet::default(), &[es]).is_empty());
    }

    const CFG: &str = r#"
ignore = [{ vid = "1050" }]

[[profile]]
name = "us-keyboard"
layouts = ["0419:00000419", "0409:00000409"]
activate = "0409:00000409"
when = { vid = "046D", pid = "B35F" }

[[profile]]
name = "any-external"
layouts = ["0419:00000419", "0407:00000407"]
when = { any_external = true }

[default]
layouts = ["0419:00000419", "0C0A:0000040A"]
activate = "0419:00000419"
"#;

    fn kb(id: &str) -> KeyboardInfo {
        KeyboardInfo {
            instance_id: id.into(),
            ..Default::default()
        }
    }

    fn builtin() -> KeyboardInfo {
        kb("ACPI\\MSF0001\\4&1&0")
    }

    #[test]
    fn incomplete_keyboard_waits_only_when_it_could_change_the_profile() {
        let cfg = Config::parse(CFG).unwrap();
        // Review KD-07: an internal HID keyboard whose container cannot be
        // read yet might be external (any-external comes before default).
        let mut half = kb("HID\\VID_06CB&PID_CE2D&MI_00\\8&1");
        half.incomplete = Some("container ID unreadable".into());
        let all = [builtin(), half.clone()];
        assert_eq!(unsettled_by(&cfg, &all, &State::default()).len(), 1);
        // An external keyboard already selects the first profile: nothing an
        // incomplete keyboard reports can come before it.
        let all = [builtin(), kb("HID\\VID_046D&PID_B35F\\1"), half.clone()];
        assert!(unsettled_by(&cfg, &all, &State::default()).is_empty());
        // Data that is present rules matchers out: a different VID/PID cannot
        // match the VID/PID profile, but any-external still could.
        let cfg2 = Config::parse(
            "[[profile]]\nname = \"p\"\nlayouts = [\"0409:00000409\"]\nwhen = { vid = \"046D\", pid = \"B35F\" }\n\
             [default]\nlayouts = [\"0419:00000419\"]\n",
        )
        .unwrap();
        assert!(unsettled_by(&cfg2, &[builtin(), half.clone()], &State::default()).is_empty());
        // Unknown instance ID and hardware IDs: it could be that keyboard.
        let mut unknown = KeyboardInfo {
            incomplete: Some("instance ID unreadable".into()),
            ..Default::default()
        };
        assert_eq!(
            unsettled_by(&cfg2, &[builtin(), unknown.clone()], &State::default()).len(),
            1
        );
        // A complete keyboard is never "unsettled".
        unknown.incomplete = None;
        assert!(unsettled_by(&cfg2, &[builtin(), unknown], &State::default()).is_empty());
    }

    #[test]
    fn each_incomplete_keyboard_gets_its_own_budget() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let mut st = Settling::default();
        // Keyboard A never becomes readable.
        assert_eq!(st.assess(&s(&["A"]), 0, 15_000, 1_000_000), Settle::Wait(s(&["A"])));
        assert_eq!(
            st.assess(&s(&["A"]), 14_999, 15_000, 1_000_000),
            Settle::Wait(s(&["A"]))
        );
        assert_eq!(
            st.assess(&s(&["A"]), 15_000, 15_000, 1_000_000),
            Settle::GaveUp(s(&["A"]))
        );
        assert_eq!(
            st.assess(&s(&["A"]), 20_000, 15_000, 1_000_000),
            Settle::Ready,
            "reported once"
        );
        // Review TP-07: B arrives incomplete while A is still incomplete; B
        // must get its own budget instead of inheriting A's expired one.
        assert_eq!(
            st.assess(&s(&["A", "B"]), 30_000, 15_000, 1_000_000),
            Settle::Wait(s(&["B"]))
        );
        assert_eq!(
            st.assess(&s(&["A", "B"]), 44_000, 15_000, 1_000_000),
            Settle::Wait(s(&["B"]))
        );
        assert_eq!(
            st.assess(&s(&["A", "B"]), 45_000, 15_000, 1_000_000),
            Settle::GaveUp(s(&["B"]))
        );
        // A resolves; if it becomes incomplete again it starts over.
        assert_eq!(st.assess(&s(&["B"]), 50_000, 15_000, 1_000_000), Settle::Ready);
        assert_eq!(
            st.assess(&s(&["A", "B"]), 51_000, 15_000, 1_000_000),
            Settle::Wait(s(&["A"]))
        );
        // Nothing unsettled: ready, and everything is forgotten.
        assert_eq!(st.assess(&[], 52_000, 15_000, 1_000_000), Settle::Ready);
        assert_eq!(st, Settling::default());
    }

    #[test]
    fn churning_incomplete_keyboards_are_bounded() {
        // Reviews FP-06 and F5-03: a new incomplete identity every 10 s (a
        // flapping hub), sampled every second. Each gets its own budget, but
        // one waiting episode ends 60 s after it began; then the profile is
        // applied without the incomplete ones, and only a later identity
        // starts a new (equally bounded) episode.
        let mut st = Settling::default();
        let mut ids: Vec<String> = Vec::new();
        let mut waiting_since: Option<u64> = None;
        let mut fallbacks = Vec::new();
        for now in (0..200_000u64).step_by(1000) {
            if now % 10_000 == 0 {
                ids.push(format!("K{}", now / 10_000));
            }
            match st.assess(&ids, now, 15_000, 60_000) {
                Settle::Wait(_) => {
                    let since = *waiting_since.get_or_insert(now);
                    assert!(now - since < 60_000, "waited without a fallback for {} ms", now - since);
                }
                Settle::GaveUp(g) => {
                    fallbacks.push(now);
                    waiting_since = None;
                    // The exhausted ones never wait again while present.
                    assert!(!g.is_empty());
                }
                Settle::Ready => waiting_since = None,
            }
        }
        assert_eq!(fallbacks.first(), Some(&60_000), "the first episode ends at 60 s");
        assert!(fallbacks.windows(2).all(|w| w[1] - w[0] >= 10_000), "{fallbacks:?}");
        // Once nothing is unsettled the next episode starts afresh.
        assert_eq!(st.assess(&[], 200_000, 15_000, 60_000), Settle::Ready);
        assert!(matches!(
            st.assess(&["N".to_string()], 200_001, 15_000, 60_000),
            Settle::Wait(_)
        ));
    }

    #[test]
    fn a_late_arrival_after_a_fallback_gets_a_fresh_budget() {
        // Review F5-03: A never becomes readable and is given up on; ten
        // minutes later, with A still incomplete, B arrives incomplete. B
        // must be waited for (it used to be given up on at once because A
        // kept the old episode alive).
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let mut st = Settling::default();
        assert_eq!(st.assess(&s(&["A"]), 0, 15_000, 60_000), Settle::Wait(s(&["A"])));
        assert_eq!(st.assess(&s(&["A"]), 15_000, 15_000, 60_000), Settle::GaveUp(s(&["A"])));
        // Quiet: A stays incomplete and never restarts a wait.
        for now in [16_000, 120_000, 599_000] {
            assert_eq!(st.assess(&s(&["A"]), now, 15_000, 60_000), Settle::Ready);
        }
        assert_eq!(
            st.assess(&s(&["A", "B"]), 600_000, 15_000, 60_000),
            Settle::Wait(s(&["B"]))
        );
        assert_eq!(
            st.assess(&s(&["A", "B"]), 614_000, 15_000, 60_000),
            Settle::Wait(s(&["B"]))
        );
        assert_eq!(
            st.assess(&s(&["A", "B"]), 615_000, 15_000, 60_000),
            Settle::GaveUp(s(&["B"]))
        );
        assert_eq!(st.assess(&s(&["A", "B"]), 616_000, 15_000, 60_000), Settle::Ready);
        // B becomes readable; C arrives later and is waited for as well.
        assert_eq!(
            st.assess(&s(&["A", "C"]), 700_000, 15_000, 60_000),
            Settle::Wait(s(&["C"]))
        );
    }

    #[test]
    fn docking_does_not_give_an_exhausted_interface_a_new_budget() {
        // Review S6-04, with the real selection: internal keyboard A stays
        // present and incomplete; a complete external keyboard B (selects
        // the first profile, so A cannot change anything) comes and goes.
        let cfg = Config::parse(CFG).unwrap();
        let mut a = kb("HID\\VID_06CB&PID_CE2D&MI_00\\8&1");
        a.incomplete = Some("container ID unreadable".into());
        a.interface_path = "A".into();
        let dock = kb("HID\\VID_046D&PID_B35F\\1");
        let undocked = vec![builtin(), a.clone()];
        let docked = vec![builtin(), dock, a.clone()];
        let sample = |st: &mut Settling, all: &[KeyboardInfo], now: u64| {
            let incomplete: Vec<String> = all
                .iter()
                .filter(|k| k.incomplete.is_some())
                .map(|k| k.interface_path.clone())
                .collect();
            let unsettled: Vec<String> = unsettled_by(&cfg, all, &State::default())
                .iter()
                .map(|k| k.interface_path.clone())
                .collect();
            st.assess_present(&incomplete, &unsettled, now, 15_000, 60_000)
        };
        let mut st = Settling::default();
        assert_eq!(sample(&mut st, &undocked, 0), Settle::Wait(vec!["A".to_string()]));
        assert_eq!(
            sample(&mut st, &undocked, 15_000),
            Settle::GaveUp(vec!["A".to_string()])
        );
        let mut now = 20_000;
        for _ in 0..3 {
            assert_eq!(sample(&mut st, &docked, now), Settle::Ready, "docked: A is irrelevant");
            now += 30_000;
            assert_eq!(
                sample(&mut st, &undocked, now),
                Settle::Ready,
                "undocked again: A was given up on and is still incomplete"
            );
            now += 30_000;
        }
        // A readable sample of A clears it; incomplete again later, it waits.
        let mut readable = a.clone();
        readable.incomplete = None;
        assert_eq!(sample(&mut st, &[builtin(), readable], now), Settle::Ready);
        assert_eq!(
            sample(&mut st, &undocked, now + 1000),
            Settle::Wait(vec!["A".to_string()])
        );
        // A new interface still gets its own budget while A is exhausted.
        assert_eq!(
            sample(&mut st, &undocked, now + 16_000),
            Settle::GaveUp(vec!["A".to_string()])
        );
        let mut c = a.clone();
        c.interface_path = "C".into();
        let both = vec![builtin(), a.clone(), c];
        assert_eq!(
            sample(&mut st, &both, now + 20_000),
            Settle::Wait(vec!["C".to_string()])
        );
    }

    #[test]
    fn an_exhausted_interface_waits_again_only_after_it_disappeared() {
        // Review F5-03: while it stays unsettled, an exhausted interface is
        // remembered; once it is gone (unplugged, or readable) and comes
        // back incomplete, it is a new arrival with a fresh budget.
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let mut st = Settling::default();
        assert_eq!(st.assess(&s(&["A"]), 0, 15_000, 60_000), Settle::Wait(s(&["A"])));
        assert_eq!(st.assess(&s(&["A"]), 15_000, 15_000, 60_000), Settle::GaveUp(s(&["A"])));
        assert_eq!(st.assess(&s(&["A"]), 30_000, 15_000, 60_000), Settle::Ready);
        assert_eq!(st.assess(&[], 31_000, 15_000, 60_000), Settle::Ready);
        assert_eq!(st, Settling::default(), "forgotten once gone");
        assert_eq!(st.assess(&s(&["A"]), 32_000, 15_000, 60_000), Settle::Wait(s(&["A"])));
        assert_eq!(st.assess(&s(&["A"]), 47_000, 15_000, 60_000), Settle::GaveUp(s(&["A"])));
    }

    #[test]
    fn laptop_alone_uses_default() {
        let cfg = Config::parse(CFG).unwrap();
        let s = select_profile(&cfg, &[builtin()], &State::default()).unwrap();
        assert_eq!(s.name, "default");
        assert_eq!(s.set.layouts[1], LayoutSpec::new(0x0C0A, 0x040A));
    }

    #[test]
    fn specific_keyboard_wins_in_file_order() {
        let cfg = Config::parse(CFG).unwrap();
        let bt = kb("HID\\{00001124-0000-1000-8000-00805f9b34fb}_VID&0002046d_PID&b35f\\9&1&0&0000");
        let s = select_profile(
            &cfg,
            &[builtin(), kb("HID\\VID_3434&PID_0281\\1"), bt],
            &State::default(),
        )
        .unwrap();
        assert_eq!(s.name, "us-keyboard");
        assert_eq!(s.set.activate, Some(LayoutSpec::new(0x0409, 0x0409)));
    }

    #[test]
    fn any_external_and_remembered_builtin() {
        let cfg = Config::parse(CFG).unwrap();
        let internal_usb = kb("HID\\VID_06CB&PID_CE2D&MI_00\\8&1");
        let state = State {
            builtin_instance_ids: vec![internal_usb.instance_id.clone()],
        };
        // Remembered built-in keyboard alone -> default.
        assert_eq!(
            select_profile(&cfg, std::slice::from_ref(&internal_usb), &state)
                .unwrap()
                .name,
            "default"
        );
        // Without state it would count as external.
        assert_eq!(
            select_profile(&cfg, std::slice::from_ref(&internal_usb), &State::default())
                .unwrap()
                .name,
            "any-external"
        );
        // Another keyboard -> any-external.
        let s = select_profile(&cfg, &[internal_usb, kb("HID\\VID_3434&PID_0281\\1")], &state).unwrap();
        assert_eq!(s.name, "any-external");
        assert_eq!(s.matched_by.as_deref(), Some("HID\\VID_3434&PID_0281\\1"));
    }

    #[test]
    fn virtual_and_ignored_keyboards_do_not_count() {
        let cfg = Config::parse(CFG).unwrap();
        let rdp = kb("TERMINPUT_BUS\\UMB\\2&1&0&RDP_KBD");
        let yubikey = kb("HID\\VID_1050&PID_0407&MI_00\\8&1");
        let s = select_profile(&cfg, &[builtin(), rdp, yubikey], &State::default()).unwrap();
        assert_eq!(s.name, "default");
    }

    #[test]
    fn manual_marks_override() {
        let base = Config::parse(CFG).unwrap();
        // Mark the ACPI keyboard as external: now any-external matches.
        let mut cfg = base.clone();
        cfg.external.push(crate::config::Matcher::instance("ACPI\\MSF0001"));
        assert_eq!(
            select_profile(&cfg, &[builtin()], &State::default()).unwrap().name,
            "any-external"
        );
        // Mark a USB keyboard as built-in: default stays.
        let mut cfg = base;
        let usb = KeyboardInfo {
            instance_id: "HID\\VID_3434&PID_0281\\1".into(),
            container_id: Some("{8C1C3E4A-1111-2222-3333-444455556666}".into()),
            ..Default::default()
        };
        cfg.builtin.push(crate::config::Matcher::container(
            "{8C1C3E4A-1111-2222-3333-444455556666}",
        ));
        assert_eq!(
            select_profile(&cfg, &[builtin(), usb], &State::default()).unwrap().name,
            "default"
        );
    }

    #[test]
    fn heuristics_can_be_disabled() {
        let cfg = Config::parse(&format!("builtin_heuristics = false\n{CFG}")).unwrap();
        assert_eq!(
            select_profile(&cfg, &[builtin()], &State::default()).unwrap().name,
            "any-external"
        );
    }

    #[test]
    fn nothing_to_do_without_default() {
        let cfg = Config::parse("[[profile]]\nname='x'\nlayouts=['0409:00000409']\nwhen={vid='046D'}").unwrap();
        assert!(select_profile(&cfg, &[builtin()], &State::default()).is_none());
    }

    #[test]
    fn plan_install_then_uninstall_managed_only() {
        let cfg = Config::parse(CFG).unwrap();
        let ru = LayoutSpec::new(0x0419, 0x0419);
        let us = LayoutSpec::new(0x0409, 0x0409);
        let es = LayoutSpec::new(0x0C0A, 0x040A);
        let de = LayoutSpec::new(0x0407, 0x0407);
        let fr = LayoutSpec::new(0x040C, 0x040C); // unmanaged, user-installed
        let present = [ru, es, fr];
        let p = make_plan(&[ru, us], &cfg.managed_layouts(), |l| Some(present.contains(l)));
        assert_eq!(p.install, vec![us]);
        assert_eq!(p.uninstall, vec![es]);
        assert!(!p.uninstall.contains(&fr));
        assert!(!p.uninstall.contains(&de), "not present, nothing to remove");
    }

    #[test]
    fn plan_is_idempotent() {
        let cfg = Config::parse(CFG).unwrap();
        let ru = LayoutSpec::new(0x0419, 0x0419);
        let us = LayoutSpec::new(0x0409, 0x0409);
        let present = [ru, us];
        let p = make_plan(&[ru, us], &cfg.managed_layouts(), |l| Some(present.contains(l)));
        assert!(p.is_empty());
    }

    #[test]
    fn plan_unknown_presence_acts() {
        let ru = LayoutSpec::new(0x0419, 0x0419);
        let dv = LayoutSpec::new(0x0409, 0x0001_0409);
        let p = make_plan(&[ru], &[ru, dv], |l| if l.is_variant() { None } else { Some(true) });
        assert!(p.install.is_empty());
        assert_eq!(p.uninstall, vec![dv]);
    }
}
