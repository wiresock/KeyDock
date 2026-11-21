//! The persistent part of applying a profile: which layouts are enabled for
//! the user, installing the missing ones and removing managed ones, with a
//! fresh, authoritative observation before every removal and after it.
//!
//! The Windows calls sit behind [`LayoutApi`], so the sequence can be tested
//! with a substitute that fails, lies or changes state at any step.
//!
//! Rules:
//! * the enabled-layout list must be known; an unavailable or invalid
//!   observation stops the change (loaded input locales are session state
//!   and never count as "enabled");
//! * every layout of the target is installed and confirmed enabled before
//!   anything is removed;
//! * right before the removal batch the enabled list is read again and must
//!   still contain every target layout (also when nothing was installed);
//! * after the removal the list is read back: target layouts still there,
//!   removed ones gone;
//! * every success, also of an empty plan or of a removal that turned out
//!   unnecessary, rests on a read after the session work: the managed
//!   layouts enabled then are exactly the target (review F5-02).

use crate::layout::LayoutSpec;
use crate::select::{make_plan, Plan};

/// The user's enabled keyboard layouts and the calls that change them.
pub trait LayoutApi {
    /// The enabled keyboard layouts. `Err` when they cannot be determined
    /// reliably (function unavailable, nothing reported, a record that cannot
    /// be parsed).
    fn enabled(&mut self) -> Result<Vec<LayoutSpec>, String>;
    fn install(&mut self, specs: &[LayoutSpec]) -> Result<(), String>;
    fn uninstall(&mut self, specs: &[LayoutSpec]) -> Result<(), String>;
}

/// The changes needed to reach `target`, from a fresh observation.
pub fn plan(
    api: &mut dyn LayoutApi,
    target: &[LayoutSpec],
    managed: &[LayoutSpec],
) -> Result<(Vec<LayoutSpec>, Plan), String> {
    let enabled = api.enabled()?;
    let plan = make_plan(target, managed, |l| Some(enabled.contains(l)));
    Ok((enabled, plan))
}

/// What was changed, as observed in the enabled list (not as requested).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Layouts that were not enabled before and are enabled now.
    pub installed: Vec<LayoutSpec>,
    /// Layouts that were enabled before and are not enabled now.
    pub removed: Vec<LayoutSpec>,
    /// The enabled layouts read back at the end, after the session work.
    pub enabled_after: Vec<LayoutSpec>,
    /// Layouts handed to `install` / `uninstall` (whether or not that worked).
    pub attempted_install: Vec<LayoutSpec>,
    pub attempted_remove: Vec<LayoutSpec>,
}

/// A step that did not complete. `installed` and `removed` are what the
/// last successful observation shows changed; `unknown` is set when a change
/// was attempted and no later observation succeeded, so the user's list may
/// differ from what is reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    pub installed: Vec<LayoutSpec>,
    pub removed: Vec<LayoutSpec>,
    pub attempted_install: Vec<LayoutSpec>,
    pub attempted_remove: Vec<LayoutSpec>,
    pub unknown: bool,
}

impl Failure {
    /// Whether anything may have changed (confirmed, attempted or unknown).
    pub fn may_have_changed(&self) -> bool {
        self.unknown
            || !self.installed.is_empty()
            || !self.removed.is_empty()
            || !self.attempted_install.is_empty()
            || !self.attempted_remove.is_empty()
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn list(v: &[LayoutSpec]) -> String {
    v.iter().map(|l| l.to_string()).collect::<Vec<_>>().join(", ")
}

/// Carries out `plan` (from [`plan`], whose observation was `before`).
/// `between` runs after the install is confirmed and before anything is
/// removed; the session work (activating a layout, moving windows off the
/// layouts being removed) goes there. Every operation ends with a fresh read
/// of the enabled list after the session work, also when nothing was
/// installed or removed (review F5-02): it must contain the whole target,
/// none of the removed layouts and no other managed layout.
pub fn apply(
    api: &mut dyn LayoutApi,
    before: &[LayoutSpec],
    target: &[LayoutSpec],
    managed: &[LayoutSpec],
    plan: &Plan,
    between: &mut dyn FnMut(),
) -> Result<Applied, Box<Failure>> {
    let mut done = Applied::default();
    // What the latest observation says changed since `before`.
    let observe = |done: &mut Applied, now: &[LayoutSpec]| {
        done.installed = now.iter().filter(|l| !before.contains(l)).copied().collect();
        done.removed = before.iter().filter(|l| !now.contains(l)).copied().collect();
        done.enabled_after = now.to_vec();
    };
    let fail = |done: &Applied, message: String, unknown: bool| {
        Box::new(Failure {
            message,
            installed: done.installed.clone(),
            removed: done.removed.clone(),
            attempted_install: done.attempted_install.clone(),
            attempted_remove: done.attempted_remove.clone(),
            unknown,
        })
    };
    if !plan.install.is_empty() {
        done.attempted_install = plan.install.clone();
        let call = api.install(&plan.install);
        // Observe in any case: a failing call may still have changed some.
        let enabled = match api.enabled() {
            Ok(e) => e,
            Err(e) => {
                let msg = match call {
                    Ok(()) => format!("installed [{}], but cannot confirm it ({e}); nothing removed", list(&plan.install)),
                    Err(c) => format!("{c}; the enabled layouts cannot be read afterwards ({e}), so what changed is unknown; nothing removed"),
                };
                return Err(fail(&done, msg, true));
            }
        };
        observe(&mut done, &enabled);
        if let Err(c) = call {
            let partial = if done.installed.is_empty() {
                String::new()
            } else {
                format!(" ([{}] did get enabled)", list(&done.installed))
            };
            return Err(fail(&done, format!("{c}{partial}; nothing removed"), false));
        }
        let missing: Vec<LayoutSpec> = target.iter().filter(|l| !enabled.contains(l)).copied().collect();
        if !missing.is_empty() {
            return Err(fail(
                &done,
                format!(
                    "after installing [{}], [{}] is still not enabled; nothing removed",
                    list(&plan.install),
                    list(&missing)
                ),
                false,
            ));
        }
    }
    between();
    let mut remove: Vec<LayoutSpec> = Vec::new();
    if !plan.uninstall.is_empty() {
        // Fresh observation right before the removal batch: the session work
        // above can take more than a second, and another program may have
        // changed the list meanwhile.
        let enabled = api.enabled().map_err(|e| {
            fail(
                &done,
                format!("cannot read the enabled layouts before removing ({e}); nothing removed"),
                false,
            )
        })?;
        observe(&mut done, &enabled);
        let missing: Vec<LayoutSpec> = target.iter().filter(|l| !enabled.contains(l)).copied().collect();
        if !missing.is_empty() {
            return Err(fail(
                &done,
                format!(
                    "[{}] of the profile is not enabled (any more); nothing removed",
                    list(&missing)
                ),
                false,
            ));
        }
        remove = plan.uninstall.iter().filter(|l| enabled.contains(l)).copied().collect();
        if !remove.is_empty() {
            done.attempted_remove = remove.clone();
            if let Err(c) = api.uninstall(&remove) {
                return Err(match api.enabled() {
                    Ok(now) => {
                        observe(&mut done, &now);
                        let gone: Vec<LayoutSpec> = remove.iter().filter(|l| !now.contains(l)).copied().collect();
                        let partial = if gone.is_empty() {
                            String::new()
                        } else {
                            format!(" ([{}] did get removed)", list(&gone))
                        };
                        fail(&done, format!("{c}{partial}"), false)
                    }
                    Err(e) => fail(
                        &done,
                        format!("{c}; the enabled layouts cannot be read afterwards ({e}), so what changed is unknown"),
                        true,
                    ),
                });
            }
        }
    }
    let mutated = !done.attempted_install.is_empty() || !done.attempted_remove.is_empty();
    {
        // Final observation after the session work and any removal: the only
        // basis for reporting success (and for the caller to remember the
        // profile as applied), whether or not anything was changed.
        let now = api.enabled().map_err(|e| {
            let message = if mutated {
                format!("the change cannot be confirmed: the enabled layouts cannot be read ({e})")
            } else {
                format!("the layouts cannot be verified: the enabled layouts cannot be read ({e})")
            };
            fail(&done, message, mutated)
        })?;
        observe(&mut done, &now);
        let lost: Vec<LayoutSpec> = target.iter().filter(|l| !now.contains(l)).copied().collect();
        if !lost.is_empty() {
            return Err(fail(
                &done,
                format!("[{}] of the profile is not enabled at the end", list(&lost)),
                false,
            ));
        }
        let still: Vec<LayoutSpec> = remove.iter().filter(|l| now.contains(l)).copied().collect();
        if !still.is_empty() {
            return Err(fail(
                &done,
                format!("[{}] is still enabled after removing it", list(&still)),
                false,
            ));
        }
        // The whole postcondition (review FP-04): the managed layouts that
        // are enabled now are exactly the profile's. One that something else
        // enabled meanwhile is not silently accepted; the caller tries again
        // from a fresh observation (bounded by its retries). Layouts KeyDock
        // does not manage are never looked at.
        let extra: Vec<LayoutSpec> = now
            .iter()
            .filter(|l| managed.contains(l) && !target.contains(l))
            .copied()
            .collect();
        if !extra.is_empty() {
            return Err(fail(
                &done,
                format!(
                    "[{}] (a managed layout not in this profile) was enabled while the layouts were being changed; checking again",
                    list(&extra)
                ),
                false,
            ));
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(s: &str) -> LayoutSpec {
        LayoutSpec::parse(s).unwrap()
    }

    const US: &str = "0409:00000409";
    const ES: &str = "0409:0000040A";
    const RU: &str = "0419:00000419";
    const DE: &str = "0407:00000407";

    /// A substitute for the Windows calls: an enabled list, scripted faults
    /// and a log of what was called.
    #[derive(Default)]
    struct Fake {
        enabled: Vec<LayoutSpec>,
        /// Results for the next `enabled()` calls (None = the real list).
        reads: Vec<Option<Result<Vec<LayoutSpec>, String>>>,
        install_fails: bool,
        /// Enable only the first requested layout, then fail (a partial batch).
        install_partial: bool,
        install_noop: bool,
        uninstall_noop: bool,
        /// Remove only the first requested layout, then fail.
        uninstall_partial: bool,
        calls: Vec<String>,
    }

    impl LayoutApi for Fake {
        fn enabled(&mut self) -> Result<Vec<LayoutSpec>, String> {
            self.calls.push("enabled".into());
            match (!self.reads.is_empty()).then(|| self.reads.remove(0)).flatten() {
                Some(r) => r,
                None => Ok(self.enabled.clone()),
            }
        }
        fn install(&mut self, specs: &[LayoutSpec]) -> Result<(), String> {
            self.calls.push(format!("install {}", list(specs)));
            if self.install_fails {
                return Err("InstallLayoutOrTip returned FALSE".into());
            }
            if self.install_partial {
                self.enabled.push(specs[0]);
                return Err("InstallLayoutOrTip returned FALSE".into());
            }
            if !self.install_noop {
                for s in specs {
                    if !self.enabled.contains(s) {
                        self.enabled.push(*s);
                    }
                }
            }
            Ok(())
        }
        fn uninstall(&mut self, specs: &[LayoutSpec]) -> Result<(), String> {
            self.calls.push(format!("uninstall {}", list(specs)));
            if self.uninstall_partial {
                self.enabled.retain(|e| *e != specs[0]);
                return Err("InstallLayoutOrTip returned FALSE".into());
            }
            if !self.uninstall_noop {
                self.enabled.retain(|e| !specs.contains(e));
            }
            Ok(())
        }
    }

    fn run_with(
        f: &mut Fake,
        target: &[LayoutSpec],
        managed: &[LayoutSpec],
        between: &mut dyn FnMut(),
    ) -> Result<Applied, Box<Failure>> {
        let (before, p) = plan(f, target, managed).map_err(|message| {
            Box::new(Failure {
                message,
                installed: vec![],
                removed: vec![],
                attempted_install: vec![],
                attempted_remove: vec![],
                unknown: false,
            })
        })?;
        apply(f, &before, target, managed, &p, between)
    }

    fn run(f: &mut Fake, target: &[LayoutSpec], managed: &[LayoutSpec]) -> Result<Applied, Box<Failure>> {
        run_with(f, target, managed, &mut || {})
    }

    #[test]
    fn swap_installs_confirms_then_removes() {
        let mut f = Fake {
            enabled: vec![l(US), l(RU)],
            ..Default::default()
        };
        let r = run(&mut f, &[l(RU), l(ES)], &[l(RU), l(US), l(ES)]).unwrap();
        assert_eq!(r.installed, vec![l(ES)]);
        assert_eq!(r.removed, vec![l(US)]);
        assert_eq!(f.enabled, vec![l(RU), l(ES)]);
        assert_eq!(
            f.calls,
            [
                "enabled",
                &format!("install {ES}"),
                "enabled",
                "enabled",
                &format!("uninstall {US}"),
                "enabled"
            ]
        );
    }

    #[test]
    fn unknown_observation_changes_nothing() {
        // Review KD-03: enumeration unavailable must not fall back to loaded HKLs.
        let mut f = Fake {
            enabled: vec![l(US)],
            reads: vec![Some(Err("EnumEnabledLayoutOrTip reported nothing".into()))],
            ..Default::default()
        };
        assert!(run(&mut f, &[l(ES)], &[l(US), l(ES)]).is_err());
        assert_eq!(f.calls, ["enabled"]);
        assert_eq!(f.enabled, vec![l(US)]);
    }

    #[test]
    fn remove_only_transition_rechecks_the_target() {
        // B is only loaded by an application, not enabled: the plan installs
        // it (loaded state does not count), so nothing is removed unconfirmed.
        let mut f = Fake {
            enabled: vec![l(US)],
            install_noop: true,
            ..Default::default()
        };
        let e = run(&mut f, &[l(ES)], &[l(US), l(ES)]).unwrap_err();
        assert!(e.message.contains("still not enabled"), "{e}");
        assert_eq!(f.enabled, vec![l(US)], "US kept");

        // Remove-only plan whose target disappears before the removal batch.
        let mut f = Fake {
            enabled: vec![l(US), l(ES)],
            reads: vec![None, Some(Ok(vec![l(US)]))],
            ..Default::default()
        };
        let e = run(&mut f, &[l(ES)], &[l(US), l(ES)]).unwrap_err();
        assert!(e.message.contains("nothing removed"), "{e}");
        assert!(!f.calls.iter().any(|c| c.starts_with("uninstall")));
    }

    #[test]
    fn failed_read_before_removal_stops_it() {
        let mut f = Fake {
            enabled: vec![l(US), l(ES)],
            reads: vec![None, Some(Err("busy".into()))],
            ..Default::default()
        };
        let e = run(&mut f, &[l(ES)], &[l(US), l(ES)]).unwrap_err();
        assert!(e.message.contains("before removing"), "{e}");
        assert_eq!(f.enabled, vec![l(US), l(ES)]);
    }

    #[test]
    fn failed_confirmation_after_install_is_unknown() {
        let mut f = Fake {
            enabled: vec![l(US)],
            reads: vec![None, Some(Err("busy".into()))],
            ..Default::default()
        };
        let e = run(&mut f, &[l(ES)], &[l(US), l(ES)]).unwrap_err();
        assert!(e.unknown, "an attempted change that could not be observed");
        assert_eq!(e.attempted_install, vec![l(ES)]);
        assert!(e.installed.is_empty(), "nothing confirmed");
        assert!(e.may_have_changed());
        assert!(e.message.contains("nothing removed"));
        assert!(f.enabled.contains(&l(US)));
    }

    #[test]
    fn failed_install_is_observed_and_partial_writes_are_reported() {
        // The call fails and changed nothing.
        let mut f = Fake {
            enabled: vec![l(US)],
            install_fails: true,
            ..Default::default()
        };
        let e = run(&mut f, &[l(ES), l(DE)], &[l(US), l(ES), l(DE)]).unwrap_err();
        assert!(e.installed.is_empty() && !e.unknown);
        assert_eq!(e.attempted_install, vec![l(ES), l(DE)]);
        assert!(e.may_have_changed(), "attempted, so the language list is compared");
        // Review TP-04: the call fails after enabling one layout of the batch.
        let mut f = Fake {
            enabled: vec![l(US)],
            install_partial: true,
            ..Default::default()
        };
        let e = run(&mut f, &[l(ES), l(DE)], &[l(US), l(ES), l(DE)]).unwrap_err();
        assert_eq!(e.installed, vec![l(ES)], "the observed partial change");
        assert!(e.message.contains("did get enabled"), "{e}");
        assert!(!f.calls.iter().any(|c| c.starts_with("uninstall")));
        // A removal batch that fails half-way.
        let mut f = Fake {
            enabled: vec![l(US), l(RU), l(ES)],
            uninstall_partial: true,
            ..Default::default()
        };
        let e = run(&mut f, &[l(ES)], &[l(US), l(RU), l(ES)]).unwrap_err();
        assert_eq!(e.removed, vec![l(US)]);
        assert_eq!(e.attempted_remove, vec![l(US), l(RU)]);
        assert!(e.message.contains("did get removed"), "{e}");
    }

    #[test]
    fn install_only_change_is_read_back_after_the_session_work() {
        // Review TP-04: B is installed and confirmed, then disappears while
        // the session work runs (another program, a user). The result must
        // not report success with the stale pre-session read. Scripted reads:
        // plan, confirmation after the install, final read (ES gone).
        let mut f = Fake {
            enabled: vec![l(US)],
            reads: vec![None, None, Some(Ok(vec![l(US)]))],
            ..Default::default()
        };
        let mut ran = false;
        let e = run_with(&mut f, &[l(US), l(ES)], &[l(US), l(ES)], &mut || ran = true).unwrap_err();
        assert!(ran);
        assert!(e.message.contains("not enabled at the end"), "{e}");
        assert!(e.installed.is_empty(), "the final observation shows nothing installed");
        assert_eq!(e.attempted_install, vec![l(ES)]);
        assert_eq!(
            f.calls.last().map(String::as_str),
            Some("enabled"),
            "read after the session work"
        );
        // And the final read failing is "unknown", not success.
        let mut f = Fake {
            enabled: vec![l(US)],
            reads: vec![None, None, Some(Err("busy".into()))],
            ..Default::default()
        };
        let e = run(&mut f, &[l(US), l(ES)], &[l(US), l(ES)]).unwrap_err();
        assert!(e.unknown);
        assert_eq!(e.installed, vec![l(ES)], "confirmed before the session work");
    }

    #[test]
    fn out_of_profile_managed_layout_appearing_is_not_accepted() {
        // Review FP-04: install-only change; while the session work runs,
        // another program enables RU, which is managed but not in this
        // profile. The final read shows it: not Ok.
        let mut f = Fake {
            enabled: vec![l(US)],
            reads: vec![None, None, Some(Ok(vec![l(US), l(ES), l(RU)]))],
            ..Default::default()
        };
        let e = run(&mut f, &[l(US), l(ES)], &[l(US), l(ES), l(RU)]).unwrap_err();
        assert!(e.message.contains("not in this profile"), "{e}");
        assert!(!e.unknown);
        assert!(
            !f.calls.iter().any(|c| c.starts_with("uninstall")),
            "nothing removed on the spot"
        );
        // A fresh run removes it (what the retry does).
        let mut f = Fake {
            enabled: vec![l(US), l(ES), l(RU)],
            ..Default::default()
        };
        let r = run(&mut f, &[l(US), l(ES)], &[l(US), l(ES), l(RU)]).unwrap();
        assert_eq!(r.removed, vec![l(RU)]);
        // An unmanaged layout appearing meanwhile is left alone and accepted.
        let mut f = Fake {
            enabled: vec![l(US)],
            reads: vec![None, None, Some(Ok(vec![l(US), l(ES), l(DE)]))],
            ..Default::default()
        };
        assert!(run(&mut f, &[l(US), l(ES)], &[l(US), l(ES)]).is_ok());
    }

    #[test]
    fn removal_is_read_back() {
        // InstallLayoutOrTip reports success but nothing changed.
        let mut f = Fake {
            enabled: vec![l(US), l(ES)],
            uninstall_noop: true,
            ..Default::default()
        };
        let e = run(&mut f, &[l(ES)], &[l(US), l(ES)]).unwrap_err();
        assert!(e.message.contains("still enabled"), "{e}");
        assert!(e.removed.is_empty());

        // The removal also took a target layout away.
        let mut f = Fake {
            enabled: vec![l(US), l(ES)],
            reads: vec![None, None, Some(Ok(vec![]))],
            ..Default::default()
        };
        let e = run(&mut f, &[l(ES)], &[l(US), l(ES)]).unwrap_err();
        assert!(e.message.contains("not enabled at the end"), "{e}");

        // The removal worked but cannot be read back: unknown.
        let mut f = Fake {
            enabled: vec![l(US), l(ES)],
            reads: vec![None, None, Some(Err("busy".into()))],
            ..Default::default()
        };
        let e = run(&mut f, &[l(ES)], &[l(US), l(ES)]).unwrap_err();
        assert!(e.unknown);
        assert_eq!(e.attempted_remove, vec![l(US)]);
    }

    #[test]
    fn already_removed_layouts_are_not_removed_again() {
        let mut f = Fake {
            enabled: vec![l(US), l(ES)],
            // The enabled list changes between plan and removal: US is gone
            // (and stays gone at the final read).
            reads: vec![None, Some(Ok(vec![l(ES)])), Some(Ok(vec![l(ES)]))],
            ..Default::default()
        };
        let r = run(&mut f, &[l(ES)], &[l(US), l(ES)]).unwrap();
        assert!(r.attempted_remove.is_empty());
        assert!(!f.calls.iter().any(|c| c.starts_with("uninstall")));
    }

    #[test]
    fn session_work_runs_between_install_and_removal() {
        let mut f = Fake {
            enabled: vec![l(US)],
            ..Default::default()
        };
        let (before, p) = plan(&mut f, &[l(ES)], &[l(US), l(ES)]).unwrap();
        let mut order = Vec::new();
        let r = apply(&mut f, &before, &[l(ES)], &[l(US), l(ES)], &p, &mut || {
            order.push("between")
        })
        .unwrap();
        assert_eq!(order, ["between"]);
        assert_eq!(r.enabled_after, vec![l(ES)]);
        let i = f.calls.iter().position(|c| c.starts_with("install")).unwrap();
        let u = f.calls.iter().position(|c| c.starts_with("uninstall")).unwrap();
        assert!(i < u);
    }

    #[test]
    fn nothing_to_do_is_verified_after_the_session_work() {
        let mut f = Fake {
            enabled: vec![l(US)],
            ..Default::default()
        };
        let r = run(&mut f, &[l(US)], &[l(US)]).unwrap();
        assert_eq!(
            r,
            Applied {
                enabled_after: vec![l(US)],
                ..Default::default()
            }
        );
        // The plan's read, then the final one; no write.
        assert_eq!(f.calls, ["enabled", "enabled"]);
    }

    #[test]
    fn empty_plan_with_drift_during_the_session_work_is_not_success() {
        // Review F5-02: the plan is empty (the layouts match), but while the
        // session work runs another program changes the list.
        let target = [l(US), l(ES)];
        let managed = [l(US), l(ES), l(RU)];
        // A managed layout outside the profile appears.
        let mut f = Fake {
            enabled: vec![l(US), l(ES)],
            reads: vec![None, Some(Ok(vec![l(US), l(ES), l(RU)]))],
            ..Default::default()
        };
        let mut ran = false;
        let e = run_with(&mut f, &target, &managed, &mut || ran = true).unwrap_err();
        assert!(ran, "the session work ran");
        assert!(e.message.contains("not in this profile"), "{e}");
        assert!(!e.unknown, "nothing was attempted");
        assert!(e.attempted_install.is_empty() && e.attempted_remove.is_empty());
        assert_eq!(e.installed, vec![l(RU)], "what something else did is observed");
        // A target layout disappears.
        let mut f = Fake {
            enabled: vec![l(US), l(ES)],
            reads: vec![None, Some(Ok(vec![l(US)]))],
            ..Default::default()
        };
        let e = run(&mut f, &target, &managed).unwrap_err();
        assert!(e.message.contains("not enabled at the end"), "{e}");
        // The final read fails: unverified, not success.
        let mut f = Fake {
            enabled: vec![l(US), l(ES)],
            reads: vec![None, Some(Err("EnumEnabledLayoutOrTip failed".into()))],
            ..Default::default()
        };
        let e = run(&mut f, &target, &managed).unwrap_err();
        assert!(e.message.contains("cannot be verified"), "{e}");
        assert!(!e.unknown, "nothing was attempted, so nothing unknown changed");
        // An unmanaged layout appearing is left alone and accepted.
        let mut f = Fake {
            enabled: vec![l(US), l(ES)],
            reads: vec![None, Some(Ok(vec![l(US), l(ES), l(DE)]))],
            ..Default::default()
        };
        assert!(run(&mut f, &target, &managed).is_ok());
        assert!(f.calls.iter().all(|c| c == "enabled"), "never a write");
    }

    #[test]
    fn a_removal_that_vanished_before_the_call_is_still_verified() {
        // Review F5-02: ES was planned for removal; before the removal batch
        // something else removed it, so no TIP call is needed. The result
        // still rests on a read after that.
        let target = [l(US)];
        let managed = [l(US), l(ES), l(RU)];
        // Meanwhile RU (managed, not in the profile) was enabled.
        let mut f = Fake {
            enabled: vec![l(US), l(ES)],
            reads: vec![None, Some(Ok(vec![l(US)])), Some(Ok(vec![l(US), l(RU)]))],
            ..Default::default()
        };
        let e = run(&mut f, &target, &managed).unwrap_err();
        assert!(e.message.contains("not in this profile"), "{e}");
        assert!(!f.calls.iter().any(|c| c.starts_with("uninstall")), "no TIP call");
        // The target disappears after the pre-removal read.
        let mut f = Fake {
            enabled: vec![l(US), l(ES)],
            reads: vec![None, Some(Ok(vec![l(US)])), Some(Ok(vec![]))],
            ..Default::default()
        };
        let e = run(&mut f, &target, &managed).unwrap_err();
        assert!(e.message.contains("not enabled at the end"), "{e}");
        // Nothing else changed: success, after the final read.
        let mut f = Fake {
            enabled: vec![l(US), l(ES)],
            reads: vec![None, Some(Ok(vec![l(US)])), Some(Ok(vec![l(US)]))],
            ..Default::default()
        };
        let r = run(&mut f, &target, &managed).unwrap();
        assert_eq!(r.enabled_after, vec![l(US)]);
        assert_eq!(r.removed, vec![l(ES)], "observed, not attempted");
        assert!(r.attempted_remove.is_empty());
        assert_eq!(f.calls, ["enabled", "enabled", "enabled"]);
    }
}
