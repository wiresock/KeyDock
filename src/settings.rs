//! The model behind the settings window and its mapping to [`Config`].
//!
//! The window manages two things: the `[default]` layout set ("Laptop only")
//! and one profile named [`GUI_EXTERNAL_PROFILE`] ("External keyboard
//! connected"). Everything else in the configuration (other hand-written
//! profiles, ignore/builtin/external lists, unknown-to-the-GUI options) is
//! carried over unchanged when saving.

use crate::config::{Config, LayoutSet, Matcher, Profile, GUI_EXTERNAL_PROFILE};
use crate::fsutil::Revision;
use crate::layout::LayoutSpec;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModeSettings {
    pub layouts: Vec<LayoutSpec>,
    pub activate: Option<LayoutSpec>,
}

impl ModeSettings {
    fn from_set(s: &LayoutSet) -> Self {
        Self {
            layouts: s.layouts.clone(),
            activate: s.activate,
        }
    }

    fn to_set(&self) -> LayoutSet {
        LayoutSet {
            layouts: self.layouts.clone(),
            activate: self.activate,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsModel {
    pub laptop: ModeSettings,
    pub external: ModeSettings,
    /// True: any external keyboard triggers the external mode.
    pub external_any: bool,
    /// When `external_any` is false: the keyboards that trigger it (any of them).
    pub external_devices: Vec<Matcher>,
    pub debounce_ms: u32,
    pub resume_delay_ms: u32,
    pub notify: bool,
}

impl SettingsModel {
    pub fn from_config(cfg: &Config) -> Self {
        let ext = gui_profile(cfg);
        let external_any = ext.is_none_or(|p| p.when == [Matcher::any_external()]);
        Self {
            laptop: cfg.default.as_ref().map(ModeSettings::from_set).unwrap_or_default(),
            external: ext.map(|p| ModeSettings::from_set(&p.set)).unwrap_or_default(),
            external_any,
            external_devices: if external_any {
                Vec::new()
            } else {
                ext.map(|p| p.when.clone()).unwrap_or_default()
            },
            debounce_ms: cfg.debounce_ms,
            resume_delay_ms: cfg.resume_delay_ms,
            notify: cfg.notify,
        }
    }

    /// The model the settings window shows for `cfg`. On first run (nothing
    /// configured yet) both modes start from the layouts `loaded` now, so a
    /// first save keeps the current layouts. Once there is a configuration, a
    /// mode it lacks starts with nothing ticked: saving other changes then
    /// leaves it absent instead of saving the loaded layouts as a new profile,
    /// which would make each of them managed and so removable by the other
    /// mode.
    pub fn for_window(cfg: &Config, loaded: &[LayoutSpec]) -> Self {
        let mut m = Self::from_config(cfg);
        if !is_configured(cfg) {
            m.laptop.layouts = loaded.to_vec();
            m.external.layouts = loaded.to_vec();
        }
        m
    }

    /// Which modes stay out of the saved configuration (laptop, external):
    /// absent from `base` and still without layouts (the external mode also
    /// without a choice of keyboards).
    fn left_absent(&self, base: &Config) -> (bool, bool) {
        (
            self.laptop.layouts.is_empty() && base.default.is_none(),
            self.external.layouts.is_empty() && self.external_any && gui_profile(base).is_none(),
        )
    }

    /// Checks that the model can be saved over `base`. A mode that is saved
    /// needs at least one layout; a mode absent from `base` may stay absent
    /// as long as the saved configuration still sets up something.
    pub fn validate(&self, base: &Config) -> Result<(), String> {
        let (lap_absent, ext_absent) = self.left_absent(base);
        let others = !other_profiles(base).is_empty();
        for (name, m, absent, other_mode_saved) in [
            ("Laptop only", &self.laptop, lap_absent, !ext_absent),
            ("External keyboard connected", &self.external, ext_absent, !lap_absent),
        ] {
            if absent && (other_mode_saved || others) {
                continue;
            }
            if m.layouts.is_empty() {
                return Err(format!("\"{name}\" needs at least one layout."));
            }
            if let Some(a) = m.activate {
                if !m.layouts.contains(&a) {
                    return Err(format!(
                        "\"{name}\": the layout to switch to must be one of the kept layouts."
                    ));
                }
            }
        }
        if !self.external_any && self.external_devices.is_empty() {
            return Err("Select at least one keyboard for \"External keyboard connected\", or choose \"Any external keyboard\".".into());
        }
        Ok(())
    }

    /// Builds the configuration to save, starting from `base` (which carries
    /// device marks, other profiles and options the window does not show).
    /// A mode absent from `base` that was not set up stays absent.
    pub fn to_config(&self, base: &Config) -> Config {
        let mut c = base.clone();
        c.debounce_ms = self.debounce_ms.clamp(50, 60_000);
        c.resume_delay_ms = self.resume_delay_ms.clamp(50, 120_000);
        c.notify = self.notify;
        let (lap_absent, ext_absent) = self.left_absent(base);
        if !lap_absent {
            c.default = Some(self.laptop.to_set());
        }
        if ext_absent {
            return c;
        }
        let when = if self.external_any {
            vec![Matcher::any_external()]
        } else {
            self.external_devices.clone()
        };
        let profile = Profile {
            name: GUI_EXTERNAL_PROFILE.to_string(),
            when,
            set: self.external.to_set(),
        };
        match c
            .profiles
            .iter_mut()
            .find(|p| p.name.eq_ignore_ascii_case(GUI_EXTERNAL_PROFILE))
        {
            Some(p) => *p = profile,
            // Appended last: hand-written, more specific profiles keep priority.
            None => c.profiles.push(profile),
        }
        c
    }
}

/// The profile managed by the settings window, if present.
pub fn gui_profile(cfg: &Config) -> Option<&Profile> {
    cfg.profiles
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(GUI_EXTERNAL_PROFILE))
}

/// Names of the profiles the settings window does not manage (kept as they are).
pub fn other_profiles(cfg: &Config) -> Vec<String> {
    cfg.profiles
        .iter()
        .filter(|p| !p.name.eq_ignore_ascii_case(GUI_EXTERNAL_PROFILE))
        .map(|p| p.name.clone())
        .collect()
}

/// True once the user has configured anything (otherwise the settings window opens on start).
pub fn is_configured(cfg: &Config) -> bool {
    cfg.default.is_some() || !cfg.profiles.is_empty()
}

/// Why saving must ask before it replaces `config.toml`, or `None` when it
/// may write. `opened` is the file as the settings window read it (or last
/// wrote it), `now` as it is now. Asks when the file is unreadable or unusable
/// (it may hold profiles the window does not show), when it was changed, and
/// when it was deleted since: recreating it silently could resume layout
/// changes the user meant to stop.
pub fn save_conflict(opened: &Revision, now: &Revision) -> Option<String> {
    match now {
        Revision::Unreadable(e) => Some(format!("cannot be read: {e}")),
        Revision::Missing => match opened {
            Revision::Missing => None,
            _ => Some("was deleted since Settings was opened.".into()),
        },
        Revision::Content(bytes) => match std::str::from_utf8(bytes) {
            Err(_) => Some("is not UTF-8 text".into()),
            Ok(text) => match Config::parse(text) {
                Err(e) => Some(format!("cannot be used:\n\n{e}")),
                Ok(_) if opened.bytes() != Some(bytes.as_slice()) => {
                    Some("was changed since Settings was opened (for example in a text editor).".into())
                }
                Ok(_) => None,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_asks_on_deletion_edits_and_unusable_files() {
        let a = Revision::Content(b"debounce_ms = 500\n".to_vec());
        let b = Revision::Content(b"debounce_ms = 700\n".to_vec());
        assert_eq!(save_conflict(&a, &a), None);
        assert_eq!(save_conflict(&Revision::Missing, &Revision::Missing), None);
        // Review KD-06: deleted after Settings opened it.
        assert!(save_conflict(&a, &Revision::Missing).unwrap().contains("deleted"));
        assert!(save_conflict(&a, &b).unwrap().contains("changed"));
        // Created meanwhile.
        assert!(save_conflict(&Revision::Missing, &a).unwrap().contains("changed"));
        assert!(save_conflict(&a, &Revision::Unreadable("locked".into()))
            .unwrap()
            .contains("cannot be read"));
        assert!(save_conflict(&a, &Revision::Content(vec![0xFF, 0xFE]))
            .unwrap()
            .contains("UTF-8"));
        assert!(save_conflict(&a, &Revision::Content(b"nonsense =".to_vec()))
            .unwrap()
            .contains("cannot be used"));
    }

    const RU: LayoutSpec = LayoutSpec::new(0x0419, 0x0000_0419);
    const US: LayoutSpec = LayoutSpec::new(0x0409, 0x0000_0409);
    const EN_ES: LayoutSpec = LayoutSpec::new(0x0409, 0x0000_040A);

    fn owner_model() -> SettingsModel {
        SettingsModel {
            laptop: ModeSettings {
                layouts: vec![RU, EN_ES],
                activate: Some(EN_ES),
            },
            external: ModeSettings {
                layouts: vec![RU, US],
                activate: Some(US),
            },
            external_any: true,
            external_devices: Vec::new(),
            debounce_ms: 700,
            resume_delay_ms: 3000,
            notify: true,
        }
    }

    #[test]
    fn empty_config_gives_empty_model() {
        let m = SettingsModel::from_config(&Config::default());
        assert!(m.laptop.layouts.is_empty());
        assert!(m.external.layouts.is_empty());
        assert!(m.external_any);
        assert!(
            m.validate(&Config::default()).is_err(),
            "zero layouts must not be saved"
        );
        assert!(!is_configured(&Config::default()));
    }

    #[test]
    fn model_round_trip_through_toml() {
        let m = owner_model();
        m.validate(&Config::default()).unwrap();
        let cfg = m.to_config(&Config::default());
        assert!(is_configured(&cfg));
        let text = cfg.to_toml();
        let parsed = Config::parse(&text).unwrap();
        assert_eq!(parsed, cfg);
        assert_eq!(SettingsModel::from_config(&parsed), m);
    }

    #[test]
    fn specific_devices_round_trip() {
        let mut m = owner_model();
        m.external_any = false;
        m.external_devices = vec![
            Matcher {
                vid: Some(0x03F0),
                pid: Some(0x2041),
                ..Default::default()
            },
            Matcher::container("{AAAAAAAA-1111-2222-3333-444455556666}"),
        ];
        m.validate(&Config::default()).unwrap();
        let parsed = Config::parse(&m.to_config(&Config::default()).to_toml()).unwrap();
        assert_eq!(SettingsModel::from_config(&parsed), m);
    }

    #[test]
    fn preserves_other_profiles_and_marks() {
        let base = Config::parse(
            r#"
ignore = [{ vid = "1050" }]
builtin = [{ instance_id = "HID\\VID_06CB&PID_CE2D\\8&1" }]

[[profile]]
name = "Office"
layouts = ["0407:00000407"]
when = { name = "Office keyboard" }

[[profile]]
name = "External keyboard"
layouts = ["0419:00000419"]
when = { any_external = true }

[[profile]]
name = "Later"
layouts = ["040C:0000040C"]
when = { vid = "1234" }
"#,
        )
        .unwrap();
        assert_eq!(other_profiles(&base), vec!["Office".to_string(), "Later".to_string()]);
        let cfg = owner_model().to_config(&base);
        let parsed = Config::parse(&cfg.to_toml()).unwrap();
        let names: Vec<&str> = parsed.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            ["Office", "External keyboard", "Later"],
            "replaced in place, order kept"
        );
        assert_eq!(parsed.profiles[1].set.layouts, vec![RU, US]);
        assert_eq!(parsed.ignore, base.ignore);
        assert_eq!(parsed.builtin, base.builtin);
        assert_eq!(parsed.default.as_ref().unwrap().layouts, vec![RU, EN_ES]);
    }

    #[test]
    fn validation() {
        // Both modes saved before: neither may lose all its layouts.
        let base = owner_model().to_config(&Config::default());
        let mut m = owner_model();
        m.external.layouts.clear();
        assert!(m.validate(&base).is_err());
        let mut m = owner_model();
        m.laptop.layouts.clear();
        m.laptop.activate = None;
        assert!(m.validate(&base).is_err());
        let mut m = owner_model();
        m.laptop.activate = Some(US);
        assert!(m.validate(&base).is_err());
        let mut m = owner_model();
        m.external_any = false;
        assert!(m.validate(&base).is_err());
        // External not set up before: it may stay so, but not with a choice
        // of keyboards and no layouts, and not when nothing else is set up.
        let laptop_only = Config::parse("[default]\nlayouts = [\"0409:00000409\"]\n").unwrap();
        let mut m = SettingsModel::for_window(&laptop_only, &[US, RU]);
        m.validate(&laptop_only).unwrap();
        m.external_any = false;
        m.external_devices = vec![Matcher::container("{AAAAAAAA-1111-2222-3333-444455556666}")];
        assert!(m
            .validate(&laptop_only)
            .unwrap_err()
            .contains("External keyboard connected"));
        let m = SettingsModel::for_window(&Config::default(), &[]);
        assert!(m.validate(&Config::default()).unwrap_err().contains("Laptop only"));
    }

    const FR: LayoutSpec = LayoutSpec::new(0x040C, 0x0000_040C);

    /// The enabled layouts as KeyDock's planner reads them: US English and French.
    struct Enabled;
    impl crate::reconcile::LayoutApi for Enabled {
        fn enabled(&mut self) -> Result<Vec<LayoutSpec>, String> {
            Ok(vec![US, FR])
        }
        fn install(&mut self, _: &[LayoutSpec]) -> Result<(), String> {
            unreachable!("planning only")
        }
        fn uninstall(&mut self, _: &[LayoutSpec]) -> Result<(), String> {
            unreachable!("planning only")
        }
    }

    fn laptop_keyboard() -> crate::device::KeyboardInfo {
        crate::device::KeyboardInfo {
            instance_id: r"ACPI\PNP0303\4&1D401FB5&0".into(),
            ..Default::default()
        }
    }

    fn usb_keyboard() -> crate::device::KeyboardInfo {
        crate::device::KeyboardInfo {
            instance_id: r"HID\VID_046D&PID_C31C&MI_00\8&2A1B3C4D&0&0000".into(),
            hardware_ids: vec![r"HID\VID_046D&PID_C31C&MI_00".into()],
            ..Default::default()
        }
    }

    /// The mode KeyDock selects for `cfg` with `keyboards` present, and the
    /// changes its planner makes from the enabled layouts.
    fn plan_for(cfg: &Config, keyboards: &[crate::device::KeyboardInfo]) -> (String, crate::select::Plan) {
        let sel =
            crate::select::select_profile(cfg, keyboards, &crate::state::State::default()).expect("a mode is selected");
        let (_, plan) = crate::reconcile::plan(&mut Enabled, &sel.set.layouts, &cfg.managed_layouts()).unwrap();
        (sel.name, plan)
    }

    fn laptop_us_only() -> Config {
        Config::parse("[default]\nlayouts = [\"0409:00000409\"]\n").unwrap()
    }

    #[test]
    fn an_options_only_save_keeps_unmanaged_layouts() {
        // Laptop only = US English, no External profile; US English and
        // French enabled and loaded. French is not KeyDock's.
        let base = laptop_us_only();
        assert_eq!(plan_for(&base, &[laptop_keyboard()]).1, Default::default());
        // Settings opened, only "Notify when the mode changes" changed, Apply.
        let mut m = SettingsModel::for_window(&base, &[US, FR]);
        m.notify = !base.notify;
        m.validate(&base)
            .expect("an option can be saved while External is not set up");
        let saved = Config::parse(&m.to_config(&base).to_toml()).unwrap();
        for kb in [laptop_keyboard(), usb_keyboard()] {
            let (mode, plan) = plan_for(&saved, &[kb]);
            assert!(!plan.uninstall.contains(&FR), "{mode}: {plan:?}");
            assert_eq!(plan, Default::default(), "{mode}");
        }
        assert!(!saved.managed_layouts().contains(&FR));
        assert!(
            gui_profile(&saved).is_none(),
            "External keyboard connected stays absent"
        );
        assert_eq!(saved.notify, !base.notify, "the option is saved");
        assert_eq!((&saved.default, &saved.profiles), (&base.default, &base.profiles));

        // The same with only the External profile set up: Laptop only stays absent.
        let base = Config::parse("[[profile]]\nname = \"External keyboard\"\nlayouts = [\"0409:00000409\"]\nwhen = { any_external = true }\n").unwrap();
        let mut m = SettingsModel::for_window(&base, &[US, FR]);
        assert!(m.laptop.layouts.is_empty());
        m.debounce_ms = 900;
        m.validate(&base).unwrap();
        let saved = Config::parse(&m.to_config(&base).to_toml()).unwrap();
        assert_eq!((saved.default.as_ref(), saved.debounce_ms), (None, 900));
        assert_eq!(saved.managed_layouts(), [US]);
    }

    #[test]
    fn configuring_the_absent_mode_saves_it_and_its_layout_changes() {
        let base = laptop_us_only();
        let mut m = SettingsModel::for_window(&base, &[US, FR]);
        // Under "External keyboard connected": tick US English and French, switch to French.
        m.external = ModeSettings {
            layouts: vec![US, FR],
            activate: Some(FR),
        };
        m.validate(&base).unwrap();
        let saved = Config::parse(&m.to_config(&base).to_toml()).unwrap();
        let p = gui_profile(&saved).expect("the External profile is saved");
        assert_eq!(
            (p.set.layouts.as_slice(), p.set.activate),
            ([US, FR].as_slice(), Some(FR))
        );
        assert_eq!(p.when, [Matcher::any_external()]);
        assert_eq!(saved.default, base.default);
        // French is now KeyDock's: kept with an external keyboard, removed without one.
        assert_eq!(
            plan_for(&saved, &[laptop_keyboard(), usb_keyboard()]),
            (GUI_EXTERNAL_PROFILE.to_string(), Default::default())
        );
        let (mode, plan) = plan_for(&saved, &[laptop_keyboard()]);
        assert_eq!((mode.as_str(), plan.uninstall), ("default", vec![FR]));
    }

    #[test]
    fn first_run_starts_both_modes_from_the_loaded_layouts() {
        let base = Config::default();
        let m = SettingsModel::for_window(&base, &[US, FR]);
        assert_eq!(
            (m.laptop.layouts.as_slice(), m.external.layouts.as_slice()),
            ([US, FR].as_slice(), [US, FR].as_slice())
        );
        m.validate(&base).unwrap();
        let saved = m.to_config(&base);
        assert!(is_configured(&saved) && saved.default.is_some() && gui_profile(&saved).is_some());
        assert_eq!(
            plan_for(&saved, &[laptop_keyboard()]).1,
            Default::default(),
            "nothing removed"
        );
        // Settings opened again: what was saved is shown, nothing prefilled.
        assert_eq!(
            SettingsModel::for_window(&saved, &[US]),
            SettingsModel::from_config(&saved)
        );
    }
}
