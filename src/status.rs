//! What the last operation left that needs the user's attention, per stage.
//!
//! Applying a profile has stages with separate results: the persistent
//! membership (enabled layouts), the switch to the `activate` layout (default
//! input and the request to the focused window), unloading removed layouts
//! from the session, and the language order (which may finish later, in the
//! background). A later step clears only the problem it re-checked itself:
//! a successful membership check after a restore clears a membership problem,
//! never a failed activation or a language-order warning.
//!
//! Each stage of an apply is either checked (clear, or a problem) or not
//! checked at all (review FP-05): a retry whose plan is empty does not look
//! at the language list, so it must not clear a warning that an earlier,
//! partly failed change left. A checked stage replaces the earlier result; an
//! unchecked one keeps it.
//!
//! The language list has two separate facts (review F5-06): what the layout
//! change did to it (for example the first language removed with its last
//! layout), found when the apply compares the list, and how the background
//! restore of the order ended. One never replaces the other; each is
//! cleared by its own check (a new comparison, or a restore that succeeds)
//! or superseded by a newer apply that compares the list again.

/// The stages of one apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// Evaluating at all (devices, configuration, writer lock).
    Evaluation,
    Membership,
    Activation,
    Unload,
    /// What the layout change did to the language list.
    LanguageOrder,
    /// How the background restore of the language order ended.
    Restore,
}

/// One stage's result in one apply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Check {
    /// The stage did not run (nothing to check): the earlier result stays.
    #[default]
    Unchecked,
    /// Checked, no problem.
    Clear,
    /// Checked, this needs attention.
    Problem(String),
}

impl Check {
    pub fn from_problem(p: Option<String>) -> Self {
        match p {
            Some(p) => Check::Problem(p),
            None => Check::Clear,
        }
    }

    fn merge(self, old: &mut Option<String>) {
        match self {
            Check::Unchecked => {}
            Check::Clear => *old = None,
            Check::Problem(p) => *old = Some(p),
        }
    }
}

/// The result of one apply, stage by stage.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub membership: Check,
    pub activation: Check,
    pub unload: Check,
    pub language: Check,
    /// A language-order restore was started; its result comes later.
    pub restoring: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stages {
    evaluation: Option<String>,
    membership: Option<String>,
    activation: Option<String>,
    unload: Option<String>,
    language: Option<String>,
    restore: Option<String>,
    restoring: bool,
}

impl Stages {
    /// An apply ran (completely or not): each stage it checked replaces the
    /// earlier result; the stages it did not check keep theirs.
    pub fn applied(&mut self, r: ApplyReport) {
        self.evaluation = None;
        if r.language != Check::Unchecked {
            // A newer comparison of the language list: the outcome of an
            // earlier restore is superseded (a new one may be starting).
            self.restoring = r.restoring;
            self.restore = None;
        }
        r.membership.merge(&mut self.membership);
        r.activation.merge(&mut self.activation);
        r.unload.merge(&mut self.unload);
        r.language.merge(&mut self.language);
    }

    /// The evaluation could not run (or is waiting); the stages of the last
    /// apply stay as they are.
    pub fn evaluation_failed(&mut self, e: String) {
        self.evaluation = Some(e);
    }

    /// An evaluation ran to the end without needing to apply.
    pub fn evaluated(&mut self) {
        self.evaluation = None;
    }

    /// The enabled layouts were checked and match the profile.
    pub fn membership_verified(&mut self) {
        self.evaluation = None;
        self.membership = None;
    }

    /// The enabled layouts were checked and still differ.
    pub fn membership_problem(&mut self, p: String) {
        self.evaluation = None;
        self.membership = Some(p);
    }

    /// The background language-order restore ended (`None` = restored). Its
    /// outcome is kept beside, never instead of, what the layout change did
    /// to the list.
    pub fn restore_finished(&mut self, problem: Option<String>) {
        self.restoring = false;
        self.restore = problem;
    }

    pub fn restoring(&self) -> bool {
        self.restoring
    }

    pub fn get(&self, s: Stage) -> Option<&str> {
        match s {
            Stage::Evaluation => self.evaluation.as_deref(),
            Stage::Membership => self.membership.as_deref(),
            Stage::Activation => self.activation.as_deref(),
            Stage::Unload => self.unload.as_deref(),
            Stage::LanguageOrder => self.language.as_deref(),
            Stage::Restore => self.restore.as_deref(),
        }
    }

    /// Everything that needs attention, in stage order.
    pub fn problems(&self) -> Vec<String> {
        [
            &self.evaluation,
            &self.membership,
            &self.activation,
            &self.unload,
            &self.language,
            &self.restore,
        ]
        .into_iter()
        .flatten()
        .cloned()
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Check {
        Check::Problem(s.into())
    }

    #[test]
    fn failed_activation_survives_a_successful_restore_and_verify() {
        // Review TP-05: the apply starts a restore and the switch to the
        // activate layout is refused (an elevated window).
        let mut s = Stages::default();
        s.applied(ApplyReport {
            membership: Check::Clear,
            activation: p("the active window did not accept the switch"),
            language: Check::Clear,
            restoring: true,
            ..Default::default()
        });
        assert!(s.restoring());
        // The restore succeeds, then Verify finds the layouts matching.
        s.restore_finished(None);
        s.membership_verified();
        assert_eq!(
            s.problems(),
            vec!["the active window did not accept the switch".to_string()]
        );
        assert!(!s.restoring());
    }

    #[test]
    fn empty_plan_retry_keeps_the_language_warning() {
        // Review FP-05: a removal took away the first language and then
        // failed its read-back; the automatic retry finds the layouts right
        // (empty plan), so it does not look at the language list.
        let mut s = Stages::default();
        s.applied(ApplyReport {
            membership: p("the change cannot be confirmed: the enabled layouts cannot be read"),
            activation: Check::Clear,
            unload: Check::Unchecked,
            language: p("en-US, the first language, was removed with its last layout"),
            restoring: false,
        });
        assert_eq!(s.problems().len(), 2);
        // The retry: membership checked and clear, nothing else checked.
        s.applied(ApplyReport {
            membership: Check::Clear,
            activation: Check::Clear,
            ..Default::default()
        });
        assert_eq!(
            s.problems(),
            vec!["en-US, the first language, was removed with its last layout".to_string()],
            "the user still sees the warning"
        );
        // The next change that does compare the language list decides.
        s.applied(ApplyReport {
            membership: Check::Clear,
            language: Check::Clear,
            ..Default::default()
        });
        assert!(s.problems().is_empty());
    }

    #[test]
    fn each_stage_clears_only_itself() {
        let mut s = Stages::default();
        s.applied(ApplyReport {
            membership: p("ES is not enabled at the end"),
            unload: p("ES stays in the switcher"),
            language: p("en-US, the first language, was removed"),
            ..Default::default()
        });
        s.membership_verified();
        assert_eq!(s.get(Stage::Membership), None);
        assert!(s.get(Stage::Unload).is_some() && s.get(Stage::LanguageOrder).is_some());
        // A failed restore adds to the language stage; a later check of the
        // membership does not clear it.
        s.restore_finished(Some("language order not restored: conflict".into()));
        s.membership_verified();
        assert_eq!(s.get(Stage::Restore), Some("language order not restored: conflict"));
        assert_eq!(
            s.get(Stage::LanguageOrder),
            Some("en-US, the first language, was removed"),
            "kept beside the restore's outcome"
        );
        // A transient evaluation error is shown and cleared on the next run
        // without touching the apply's stages.
        s.evaluation_failed("waiting for a keyboard".into());
        assert_eq!(s.problems().len(), 4);
        s.evaluated();
        assert_eq!(s.problems().len(), 3);
        // An apply that checks every stage and finds them clear clears them.
        s.applied(ApplyReport {
            membership: Check::Clear,
            activation: Check::Clear,
            unload: Check::Clear,
            language: Check::Clear,
            restoring: false,
        });
        assert!(s.problems().is_empty());
    }

    const FIRST_GONE: &str = "en-US, the first language, was removed with its last layout";

    /// One apply removed the first language and started a restore of the
    /// order of the remaining ones.
    fn removed_first_and_restoring() -> Stages {
        let mut s = Stages::default();
        s.applied(ApplyReport {
            membership: Check::Clear,
            activation: Check::Clear,
            language: p(FIRST_GONE),
            restoring: true,
            ..Default::default()
        });
        assert!(s.restoring());
        s
    }

    #[test]
    fn a_failed_restore_keeps_the_language_list_warning() {
        // Review F5-06: both facts of the same operation stay visible.
        let mut s = removed_first_and_restoring();
        s.restore_finished(Some(
            "restoring the language order failed: PowerShell exited with code 1".into(),
        ));
        assert!(!s.restoring());
        assert_eq!(s.get(Stage::LanguageOrder), Some(FIRST_GONE));
        assert_eq!(
            s.get(Stage::Restore),
            Some("restoring the language order failed: PowerShell exited with code 1")
        );
        assert_eq!(s.problems().len(), 2);
        // The check after the restore (Verify: membership only) and an
        // empty-plan retry (nothing compared) clear neither.
        s.membership_verified();
        s.applied(ApplyReport {
            membership: Check::Clear,
            activation: Check::Clear,
            ..Default::default()
        });
        assert_eq!(s.problems().len(), 2, "{:?}", s.problems());
        // A newer apply that compares the list decides both: the side
        // effect is re-checked, the old restore outcome is superseded.
        s.applied(ApplyReport {
            membership: Check::Clear,
            language: Check::Clear,
            ..Default::default()
        });
        assert!(s.problems().is_empty(), "{:?}", s.problems());
    }

    #[test]
    fn a_successful_restore_resolves_only_its_own_fact() {
        // Review F5-06: the order is back, but the first language is still
        // gone (Windows may change the display language at sign-in).
        let mut s = removed_first_and_restoring();
        s.restore_finished(None);
        assert!(!s.restoring());
        assert_eq!(s.problems(), vec![FIRST_GONE.to_string()]);
        // A later failed restore, then a successful one: the latter clears
        // the restore's fact only.
        s.restore_finished(Some("language order not restored: conflict".into()));
        assert_eq!(s.problems().len(), 2);
        s.restore_finished(None);
        assert_eq!(s.problems(), vec![FIRST_GONE.to_string()]);
    }

    #[test]
    fn a_waiting_notice_is_live_status() {
        // Review S6-05: "waiting for a previous writer" is the evaluation's
        // status. A successful empty-plan apply (nothing compared) or an
        // evaluation without a matching profile replaces it; a historical
        // interrupted-restore warning and the language-list fact stay.
        let waiting = "PowerShell process(es) 42 (a language-order restore of an older KeyDock) are still running";
        let interrupted = "a language-order restore did not finish; the language order is now en-US, ru";
        let mut s = Stages::default();
        s.restore_finished(Some(interrupted.into()));
        s.evaluation_failed(waiting.into());
        assert_eq!(s.problems().len(), 2);
        s.applied(ApplyReport {
            membership: Check::Clear,
            activation: Check::Clear,
            ..Default::default()
        });
        assert_eq!(s.problems(), vec![interrupted.to_string()]);
        // Again, then an evaluation with no matching profile.
        s.evaluation_failed(waiting.into());
        s.evaluated();
        assert_eq!(s.problems(), vec![interrupted.to_string()]);
        // With a language-list fact from an earlier apply: kept as well.
        let mut s = removed_first_and_restoring();
        s.restore_finished(None);
        s.evaluation_failed(waiting.into());
        s.membership_verified();
        assert_eq!(s.problems(), vec![FIRST_GONE.to_string()]);
    }

    #[test]
    fn a_failed_apply_keeps_its_stage_problems() {
        // A reconcile error that had already started a restore and failed
        // to activate: all of it stays visible.
        let mut s = Stages::default();
        s.applied(ApplyReport {
            membership: p("[DE] is still enabled after removing it"),
            activation: p("could not switch to 0409:00000409"),
            language: Check::Clear,
            restoring: true,
            ..Default::default()
        });
        assert_eq!(s.problems().len(), 2);
        s.membership_problem("layouts still differ after 2 re-applications".into());
        assert_eq!(
            s.get(Stage::Membership),
            Some("layouts still differ after 2 re-applications")
        );
        assert_eq!(s.problems().len(), 2);
    }
}
