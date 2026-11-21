//! Grouping keyboard interfaces by physical device and manual classification
//! marks (built-in / external / ignored) as used by the settings window.

use crate::config::{Config, Matcher};
use crate::device::{guid_eq, normalize_guid, KeyboardInfo, KeyboardKind, LOCAL_MACHINE_CONTAINER};
use crate::select::classify;
use crate::state::State;

/// A manual classification stored in the configuration lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    BuiltIn,
    External,
    Ignored,
}

/// One physical keyboard (all interfaces sharing a container ID).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceGroup {
    /// Matcher stored when the user marks this device or ticks it under
    /// "Only these keyboards" (see [`device_matcher`]).
    pub matcher: Matcher,
    /// Identifies the physical device while it is connected (normalised
    /// container ID, or the instance ID for internal devices).
    pub key: String,
    /// The device's keyboard interfaces.
    pub members: Vec<KeyboardInfo>,
    pub name: String,
    pub vid_pid: Option<(u16, u16)>,
    pub kind: KeyboardKind,
    /// Set when the user marked the device explicitly.
    pub manual: Option<Mark>,
    /// Built-in only because `state.toml` lists it (written by
    /// `--remember-builtin`, or on first run by KeyDock 0.3.2 and older).
    pub remembered: bool,
    /// Number of keyboard interfaces (HID collections) of the device.
    pub interfaces: usize,
    /// The device also exposes a mouse interface (see `KeyboardInfo::has_mouse`).
    pub has_mouse: bool,
    /// For a USB keyboard without a serial number, whose [`Self::matcher`]
    /// covers every keyboard of its model: a matcher for this unit on its
    /// current port only (its port-specific container ID).
    pub port_matcher: Option<Matcher>,
    /// The manual mark comes from a VID:PID entry that covers every keyboard
    /// of the model, not just this one.
    pub model_wide_mark: bool,
}

impl DeviceGroup {
    /// Text for the "Type" column, e.g. "External (automatic, also mouse)".
    pub fn type_label(&self) -> String {
        let kind = match self.kind {
            KeyboardKind::External => "External",
            KeyboardKind::BuiltIn => "Built-in",
            KeyboardKind::Ignored => "Ignored",
            KeyboardKind::Virtual => "Virtual",
        };
        let model;
        let how = if self.manual.is_some() {
            match (self.model_wide_mark, self.vid_pid) {
                (true, Some((v, p))) => {
                    model = format!("set by you for every {v:04X}:{p:04X}");
                    model.as_str()
                }
                _ => "set by you",
            }
        } else if self.remembered && self.kind == KeyboardKind::BuiltIn {
            "remembered"
        } else {
            "automatic"
        };
        if self.has_mouse {
            format!("{kind} ({how}, also mouse)")
        } else {
            format!("{kind} ({how})")
        }
    }

    /// Whether a configured matcher refers to this device: its own device
    /// matcher, or one whose identity criteria match one of its interfaces
    /// (an older container-ID entry, a hand-written VID/PID rule).
    /// `any_external` entries do not name a device.
    pub fn matched_by(&self, m: &Matcher) -> bool {
        !m.any_external && (same_device(&self.matcher, m) || self.members.iter().any(|k| m.matches_identity(k)))
    }

    /// Label in the "Only these keyboards" list, e.g. "OMEN Reactor (03F0:2041, also mouse)".
    pub fn checklist_label(&self) -> String {
        let mut extra: Vec<String> = Vec::new();
        if let Some((v, p)) = self.vid_pid {
            extra.push(format!("{v:04X}:{p:04X}"));
        }
        if self.has_mouse {
            extra.push("also mouse".into());
        }
        if extra.is_empty() {
            self.name.clone()
        } else {
            format!("{} ({})", self.name, extra.join(", "))
        }
    }
}

/// A VID:PID entry without anything that narrows it to one unit: it covers
/// every keyboard of that model.
pub fn is_model_entry(m: &Matcher) -> bool {
    m.vid.is_some()
        && m.pid.is_some()
        && m.container_id.is_none()
        && m.instance_id.is_none()
        && m.name.is_none()
        && !m.any_external
}

/// "every 3434:0281 keyboard" for a model-wide entry (see [`is_model_entry`]).
pub fn model_scope_note(m: &Matcher) -> Option<String> {
    match (m.vid, m.pid) {
        (Some(v), Some(p)) if is_model_entry(m) => Some(format!("every {v:04X}:{p:04X} keyboard")),
        _ => None,
    }
}

/// Matcher used to remember the physical device a keyboard belongs to:
/// * its container ID when that stays the same on every port (USB devices
///   with a serial number, Bluetooth devices);
/// * VID:PID for a USB device without a serial number, whose container ID
///   is new on every port or dock (this matcher then covers every keyboard
///   of that model);
/// * the instance ID for internal devices (the shared local-machine
///   container) and devices without a container.
pub fn device_matcher(kb: &KeyboardInfo) -> Matcher {
    match kb.container_id.as_deref() {
        Some(c) if !guid_eq(c, LOCAL_MACHINE_CONTAINER) => match kb.vid_pid() {
            Some((vid, pid)) if kb.container_per_port => Matcher {
                vid: Some(vid),
                pid: Some(pid),
                ..Default::default()
            },
            _ => Matcher::container(c),
        },
        _ => Matcher::instance(&kb.instance_id),
    }
}

/// Key that groups the interfaces of one connected physical device.
fn group_key(kb: &KeyboardInfo) -> String {
    match kb.container_id.as_deref() {
        Some(c) if !guid_eq(c, LOCAL_MACHINE_CONTAINER) => normalize_guid(c),
        _ => kb.instance_id.to_ascii_uppercase(),
    }
}

/// True when two matchers name the same device (case-insensitive IDs).
pub fn same_device(a: &Matcher, b: &Matcher) -> bool {
    let plain = |m: &Matcher| m.vid.is_none() && m.name.is_none() && !m.any_external;
    if !plain(a) || !plain(b) {
        return a == b;
    }
    match (&a.container_id, &b.container_id, &a.instance_id, &b.instance_id) {
        (Some(x), Some(y), None, None) => guid_eq(x, y),
        (None, None, Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => a == b,
    }
}

/// The manual mark recorded for a device, if any.
pub fn manual_mark(cfg: &Config, m: &Matcher) -> Option<Mark> {
    let has = |list: &[Matcher]| list.iter().any(|x| same_device(x, m));
    if has(&cfg.ignore) {
        Some(Mark::Ignored)
    } else if has(&cfg.external) {
        Some(Mark::External)
    } else if has(&cfg.builtin) {
        Some(Mark::BuiltIn)
    } else {
        None
    }
}

/// The mark that applies to a connected device: the first list in
/// classification order (ignore, external, builtin) with an entry that
/// refers to it (see [`DeviceGroup::matched_by`]).
pub fn group_mark(cfg: &Config, g: &DeviceGroup) -> Option<Mark> {
    let has = |list: &[Matcher]| list.iter().any(|x| g.matched_by(x));
    if has(&cfg.ignore) {
        Some(Mark::Ignored)
    } else if has(&cfg.external) {
        Some(Mark::External)
    } else if has(&cfg.builtin) {
        Some(Mark::BuiltIn)
    } else {
        None
    }
}

/// An entry that names one device (or one model): container, instance ID,
/// or VID with PID. Broader hand-written rules (VID only, name, `any_external`)
/// are not removed by marking a single device.
fn is_device_entry(m: &Matcher) -> bool {
    !m.any_external && m.name.is_none() && (m.vid.is_none() || m.pid.is_some())
}

/// Sets (or with `None` clears) the mark of a connected device: removes every
/// device entry that refers to it, also older container-ID entries, and adds
/// its matcher to the chosen list. Returns the mark that applies afterwards,
/// which differs from `mark` when a broader hand-written rule still matches.
pub fn set_group_mark(cfg: &mut Config, g: &DeviceGroup, mark: Option<Mark>) -> Option<Mark> {
    for list in [&mut cfg.builtin, &mut cfg.external, &mut cfg.ignore] {
        list.retain(|x| !(is_device_entry(x) && g.matched_by(x)));
    }
    match mark {
        Some(Mark::BuiltIn) => cfg.builtin.push(g.matcher.clone()),
        Some(Mark::External) => cfg.external.push(g.matcher.clone()),
        Some(Mark::Ignored) => cfg.ignore.push(g.matcher.clone()),
        None => {}
    }
    group_mark(cfg, g)
}

/// Result of [`set_group_mark_scoped`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarkOutcome {
    /// The mark that applies to the device afterwards.
    pub effective: Option<Mark>,
    /// Other connected keyboards that a removed model-wide entry covered;
    /// they keep their mark through an entry of their own (their port for a
    /// serial-less unit, their stable container ID or instance otherwise).
    pub kept_for: Vec<String>,
    /// Connected keyboards the removed entry covered whose mark could not be
    /// kept (they have no identity of their own to store); the user is told.
    pub not_kept: Vec<String>,
    /// A model-wide entry was removed: units of the model that are not
    /// connected now no longer get its mark.
    pub dropped_model_rule: bool,
}

/// Sets (or with `None` clears) the mark of a connected device. With
/// `port_only` and a USB keyboard without a serial number, the new entry
/// names only this unit on its current port (its container ID), so two
/// identical keyboards can be told apart; a model-wide entry that covered
/// it is replaced by port entries for the other connected units in
/// `connected` (they keep their mark). Otherwise as [`set_group_mark`].
pub fn set_group_mark_scoped(
    cfg: &mut Config,
    g: &DeviceGroup,
    mark: Option<Mark>,
    port_only: bool,
    connected: &[DeviceGroup],
) -> MarkOutcome {
    let Some(port) = g.port_matcher.clone().filter(|_| port_only) else {
        return MarkOutcome {
            effective: set_group_mark(cfg, g, mark),
            ..Default::default()
        };
    };
    let mut out = MarkOutcome::default();
    for list in [&mut cfg.builtin, &mut cfg.external, &mut cfg.ignore] {
        let models: Vec<Matcher> = list
            .iter()
            .filter(|x| is_model_entry(x) && g.matched_by(x))
            .cloned()
            .collect();
        for m in &models {
            out.dropped_model_rule = true;
            for o in connected.iter().filter(|o| o.key != g.key && o.matched_by(m)) {
                // Review TP-08: a peer with a stable container (a serial-
                // bearing USB or Bluetooth unit reporting the same IDs) keeps
                // its mark too, through its own device matcher.
                let own = o
                    .port_matcher
                    .clone()
                    .or_else(|| Some(o.matcher.clone()).filter(|x| !is_model_entry(x)));
                match own {
                    Some(op) => {
                        if !list.iter().any(|x| same_device(x, &op)) {
                            list.push(op);
                        }
                        if !out.kept_for.contains(&o.name) {
                            out.kept_for.push(o.name.clone());
                        }
                    }
                    None => {
                        if !out.not_kept.contains(&o.name) {
                            out.not_kept.push(o.name.clone());
                        }
                    }
                }
            }
        }
        list.retain(|x| !(is_device_entry(x) && (models.contains(x) || same_device(x, &port))));
    }
    match mark {
        Some(Mark::BuiltIn) => cfg.builtin.push(port),
        Some(Mark::External) => cfg.external.push(port),
        Some(Mark::Ignored) => cfg.ignore.push(port),
        None => {}
    }
    out.effective = group_mark(cfg, g);
    out
}

/// Sets (or with `None` clears) the manual mark recorded for matcher `m`.
pub fn set_mark(cfg: &mut Config, m: &Matcher, mark: Option<Mark>) {
    for list in [&mut cfg.builtin, &mut cfg.external, &mut cfg.ignore] {
        list.retain(|x| !same_device(x, m));
    }
    match mark {
        Some(Mark::BuiltIn) => cfg.builtin.push(m.clone()),
        Some(Mark::External) => cfg.external.push(m.clone()),
        Some(Mark::Ignored) => cfg.ignore.push(m.clone()),
        None => {}
    }
}

fn rank(k: KeyboardKind) -> u8 {
    match k {
        KeyboardKind::External => 0,
        KeyboardKind::BuiltIn => 1,
        KeyboardKind::Ignored => 2,
        KeyboardKind::Virtual => 3,
    }
}

/// Groups the present keyboards by physical device. Virtual keyboards are left out.
pub fn group_keyboards(keyboards: &[KeyboardInfo], cfg: &Config, state: &State) -> Vec<DeviceGroup> {
    let mut groups: Vec<DeviceGroup> = Vec::new();
    for kb in keyboards {
        let kind = classify(kb, cfg, state);
        if kind == KeyboardKind::Virtual {
            continue;
        }
        let key = group_key(kb);
        let remembered = kind == KeyboardKind::BuiltIn
            && !cfg.builtin.iter().any(|b| b.matches_identity(kb))
            && state.is_builtin(&kb.instance_id)
            && !(cfg.builtin_heuristics && kb.looks_built_in());
        let generic = |n: &str| n.eq_ignore_ascii_case("HID Keyboard Device");
        if let Some(g) = groups.iter_mut().find(|g| g.key == key) {
            g.interfaces += 1;
            g.members.push(kb.clone());
            if rank(kind) < rank(g.kind) {
                g.kind = kind;
            }
            if g.vid_pid.is_none() {
                g.vid_pid = kb.vid_pid();
            }
            g.has_mouse |= kb.has_mouse;
            g.remembered |= remembered;
            if let Some(p) = kb.product.as_deref().filter(|p| !p.trim().is_empty()) {
                if generic(&g.name) || g.name == "Keyboard" {
                    g.name = p.to_string();
                }
            }
            continue;
        }
        groups.push(DeviceGroup {
            manual: None,
            matcher: device_matcher(kb),
            key,
            members: vec![kb.clone()],
            name: kb.display_name(),
            vid_pid: kb.vid_pid(),
            kind,
            interfaces: 1,
            has_mouse: kb.has_mouse,
            remembered,
            port_matcher: match (kb.container_per_port, kb.container_id.as_deref()) {
                (true, Some(c)) => Some(Matcher::container(c)),
                _ => None,
            },
            model_wide_mark: false,
        });
    }
    for g in &mut groups {
        g.manual = group_mark(cfg, g);
        let list = match g.manual {
            Some(Mark::Ignored) => &cfg.ignore,
            Some(Mark::External) => &cfg.external,
            Some(Mark::BuiltIn) | None => &cfg.builtin,
        };
        g.model_wide_mark = g.manual.is_some()
            && !list.iter().any(|x| g.matched_by(x) && !is_model_entry(x))
            && list.iter().any(|x| g.matched_by(x) && is_model_entry(x));
    }
    groups.sort_by(|a, b| rank(a.kind).cmp(&rank(b.kind)).then_with(|| a.name.cmp(&b.name)));
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb(instance: &str, container: Option<&str>, product: Option<&str>) -> KeyboardInfo {
        KeyboardInfo {
            instance_id: instance.into(),
            container_id: container.map(Into::into),
            name: Some("HID Keyboard Device".into()),
            product: product.map(Into::into),
            ..Default::default()
        }
    }

    const HP: &str = "{AAAAAAAA-1111-2222-3333-444455556666}";

    fn sample() -> Vec<KeyboardInfo> {
        vec![
            kb("ACPI\\MSFT0001\\4&1&0", Some(LOCAL_MACHINE_CONTAINER), None),
            kb("HID\\VID_03F0&PID_2041&MI_00\\8&1&0&0000", Some(HP), None),
            kb(
                "HID\\VID_03F0&PID_2041&MI_01&COL01\\8&2&0&0000",
                Some(HP),
                Some("HP USB Keyboard"),
            ),
            kb("TERMINPUT_BUS\\UMB\\2&1&0&RDP_KBD", None, None),
        ]
    }

    #[test]
    fn groups_by_container() {
        let cfg = Config::default();
        let g = group_keyboards(&sample(), &cfg, &State::default());
        assert_eq!(g.len(), 2, "virtual keyboard left out, HP collections merged");
        assert_eq!(g[0].kind, KeyboardKind::External);
        assert_eq!(g[0].name, "HP USB Keyboard");
        assert_eq!(g[0].interfaces, 2);
        assert_eq!(g[0].vid_pid, Some((0x03F0, 0x2041)));
        assert_eq!(g[0].matcher, Matcher::container(HP));
        assert_eq!(g[1].kind, KeyboardKind::BuiltIn);
        assert_eq!(g[1].matcher, Matcher::instance("ACPI\\MSFT0001\\4&1&0"));
    }

    #[test]
    fn marks_change_classification() {
        let mut cfg = Config::default();
        let hp = Matcher::container(&HP.to_lowercase());
        set_mark(&mut cfg, &hp, Some(Mark::BuiltIn));
        let g = group_keyboards(&sample(), &cfg, &State::default());
        assert!(g.iter().all(|g| g.kind == KeyboardKind::BuiltIn));
        assert_eq!(
            g.iter().find(|g| g.interfaces == 2).unwrap().manual,
            Some(Mark::BuiltIn)
        );

        set_mark(&mut cfg, &hp, Some(Mark::Ignored));
        assert!(cfg.builtin.is_empty(), "previous mark replaced");
        assert_eq!(cfg.ignore.len(), 1);
        let g = group_keyboards(&sample(), &cfg, &State::default());
        assert_eq!(g.last().unwrap().kind, KeyboardKind::Ignored);

        set_mark(&mut cfg, &hp, None);
        assert!(cfg.ignore.is_empty() && cfg.builtin.is_empty() && cfg.external.is_empty());

        // Marking the built-in keyboard external.
        let acpi = Matcher::instance("acpi\\msft0001\\4&1&0");
        set_mark(&mut cfg, &acpi, Some(Mark::External));
        let g = group_keyboards(&sample(), &cfg, &State::default());
        assert!(g.iter().all(|g| g.kind == KeyboardKind::External));
    }

    #[test]
    fn mouse_flag_propagates_to_group_and_labels() {
        let mut kbs = sample();
        // Only the second HP interface reports the shared mouse container.
        kbs[2].has_mouse = true;
        let g = group_keyboards(&kbs, &Config::default(), &State::default());
        let hp = g.iter().find(|g| g.interfaces == 2).unwrap();
        assert!(hp.has_mouse);
        assert_eq!(hp.type_label(), "External (automatic, also mouse)");
        assert_eq!(hp.checklist_label(), format!("{} (03F0:2041, also mouse)", hp.name));
        let builtin = g.iter().find(|g| g.kind == KeyboardKind::BuiltIn).unwrap();
        assert!(!builtin.has_mouse);
        assert_eq!(builtin.type_label(), "Built-in (automatic)");

        let mut cfg = Config::default();
        set_mark(&mut cfg, &hp.matcher, Some(Mark::Ignored));
        let g = group_keyboards(&kbs, &cfg, &State::default());
        let hp = g.iter().find(|g| g.interfaces == 2).unwrap();
        assert_eq!(hp.type_label(), "Ignored (set by you, also mouse)");
    }

    #[test]
    fn local_container_touchpad_never_flags_builtin() {
        let mut kbs = sample();
        crate::device::mark_mouse_containers(&mut kbs, &[LOCAL_MACHINE_CONTAINER.to_string()]);
        let g = group_keyboards(&kbs, &Config::default(), &State::default());
        assert!(g.iter().all(|g| !g.has_mouse));
        let plain = DeviceGroup {
            matcher: Matcher::instance("x"),
            key: "X".into(),
            members: Vec::new(),
            name: "Keyboard".into(),
            vid_pid: None,
            kind: KeyboardKind::External,
            manual: None,
            remembered: false,
            interfaces: 1,
            has_mouse: false,
            port_matcher: None,
            model_wide_mark: false,
        };
        assert_eq!(plain.checklist_label(), "Keyboard");
    }

    #[test]
    fn remembered_builtin_is_labelled() {
        let kbs = sample();
        // An external keyboard that an old state.toml recorded as built-in.
        let state = State {
            builtin_instance_ids: vec![kbs[1].instance_id.clone(), kbs[2].instance_id.clone()],
        };
        let g = group_keyboards(&kbs, &Config::default(), &state);
        let hp = g.iter().find(|g| g.interfaces == 2).unwrap();
        assert_eq!(hp.kind, KeyboardKind::BuiltIn);
        assert!(hp.remembered);
        assert_eq!(hp.type_label(), "Built-in (remembered)");
        // Only one interface remembered: the device counts as external, and
        // the label must not claim it was remembered.
        let state = State {
            builtin_instance_ids: vec![kbs[1].instance_id.clone()],
        };
        let g = group_keyboards(&kbs, &Config::default(), &state);
        let hp = g.iter().find(|g| g.interfaces == 2).unwrap();
        assert_eq!(hp.type_label(), "External (automatic)");
        // The ACPI keyboard is built-in by heuristics, not because of state.toml.
        let state = State {
            builtin_instance_ids: vec![kbs[0].instance_id.clone()],
        };
        let g = group_keyboards(&kbs, &Config::default(), &state);
        assert_eq!(g[1].type_label(), "Built-in (automatic)");
        // A manual mark wins over the remembered state.
        let mut cfg = Config::default();
        set_mark(&mut cfg, &Matcher::container(HP), Some(Mark::BuiltIn));
        let state = State {
            builtin_instance_ids: vec![kbs[1].instance_id.clone(), kbs[2].instance_id.clone()],
        };
        let g = group_keyboards(&kbs, &cfg, &state);
        assert_eq!(
            g.iter().find(|g| g.interfaces == 2).unwrap().type_label(),
            "Built-in (set by you)"
        );
    }

    #[test]
    fn serial_less_usb_device_is_remembered_by_model() {
        // The same serial-less keyboard on two ports: two containers.
        let on_port = |container: &str| KeyboardInfo {
            instance_id: format!("HID\\VID_3434&PID_0281&MI_00\\8&{container}&0&0000"),
            container_id: Some(container.into()),
            name: Some("HID Keyboard Device".into()),
            container_per_port: true,
            ..Default::default()
        };
        let a = on_port("{11111111-0000-0000-0000-000000000001}");
        let b = on_port("{22222222-0000-0000-0000-000000000002}");
        let cfg0 = Config::default();
        let g = group_keyboards(std::slice::from_ref(&a), &cfg0, &State::default());
        let model = Matcher {
            vid: Some(0x3434),
            pid: Some(0x0281),
            ..Default::default()
        };
        assert_eq!(g[0].matcher, model);

        // Ignore it on one port; it stays ignored on the other.
        let mut cfg = Config::default();
        assert_eq!(
            set_group_mark(&mut cfg, &g[0], Some(Mark::Ignored)),
            Some(Mark::Ignored)
        );
        let g = group_keyboards(std::slice::from_ref(&b), &cfg, &State::default());
        assert_eq!(g[0].kind, KeyboardKind::Ignored);
        assert_eq!(g[0].manual, Some(Mark::Ignored));

        // An entry from an older version (container of port A) is recognised
        // and removed by "Automatic".
        let mut cfg = Config::default();
        cfg.ignore
            .push(Matcher::container("{11111111-0000-0000-0000-000000000001}"));
        let g = group_keyboards(std::slice::from_ref(&a), &cfg, &State::default());
        assert_eq!(g[0].manual, Some(Mark::Ignored));
        assert_eq!(set_group_mark(&mut cfg, &g[0], None), None);
        assert!(cfg.ignore.is_empty());

        // A vendor-wide hand-written rule is kept and still applies.
        let mut cfg = Config::default();
        cfg.ignore.push(Matcher {
            vid: Some(0x3434),
            ..Default::default()
        });
        let g = group_keyboards(std::slice::from_ref(&a), &cfg, &State::default());
        assert_eq!(set_group_mark(&mut cfg, &g[0], None), Some(Mark::Ignored));
        assert_eq!(cfg.ignore.len(), 1);

        // Two connected units of the model are still two rows.
        let g = group_keyboards(&[a, b], &cfg0, &State::default());
        assert_eq!(g.len(), 2);
    }

    #[test]
    fn identical_units_can_be_told_apart_by_port() {
        // Review KD-08: two units of the same serial-less model.
        let on_port = |container: &str, port: u8| KeyboardInfo {
            instance_id: format!("HID\\VID_3434&PID_0281&MI_00\\8&{port}&0&0000"),
            container_id: Some(container.into()),
            product: Some(format!("Keychron {port}")),
            container_per_port: true,
            ..Default::default()
        };
        let a = on_port("{11111111-0000-0000-0000-000000000001}", 1);
        let b = on_port("{22222222-0000-0000-0000-000000000002}", 2);
        let both = [a, b];
        let mut cfg = Config::default();
        let g = group_keyboards(&both, &cfg, &State::default());
        let ga = g.iter().find(|x| x.name == "Keychron 1").unwrap().clone();
        let gb = g.iter().find(|x| x.name == "Keychron 2").unwrap().clone();

        // Model-wide by default, and the label says so.
        let r = set_group_mark_scoped(&mut cfg, &ga, Some(Mark::Ignored), false, &g);
        assert_eq!(r.effective, Some(Mark::Ignored));
        let g = group_keyboards(&both, &cfg, &State::default());
        assert!(g.iter().all(|x| x.kind == KeyboardKind::Ignored));
        assert_eq!(g[0].type_label(), "Ignored (set by you for every 3434:0281)");
        assert_eq!(
            model_scope_note(&cfg.ignore[0]).as_deref(),
            Some("every 3434:0281 keyboard")
        );

        // Now make unit B external on its port only: A stays ignored through
        // a port entry of its own, and the model-wide rule is gone.
        let r = set_group_mark_scoped(&mut cfg, &gb, Some(Mark::External), true, &g);
        assert_eq!(r.effective, Some(Mark::External));
        assert_eq!(r.kept_for, vec!["Keychron 1".to_string()]);
        assert!(r.dropped_model_rule);
        assert!(!cfg.ignore.iter().any(is_model_entry));
        let g = group_keyboards(&both, &cfg, &State::default());
        let kind = |n: &str| g.iter().find(|x| x.name == n).unwrap().kind;
        assert_eq!(kind("Keychron 1"), KeyboardKind::Ignored);
        assert_eq!(kind("Keychron 2"), KeyboardKind::External);
        assert_eq!(
            g.iter().find(|x| x.name == "Keychron 2").unwrap().type_label(),
            "External (set by you)"
        );

        // Automatic on port only removes just that unit's entry.
        let gb = g.iter().find(|x| x.name == "Keychron 2").unwrap().clone();
        let r = set_group_mark_scoped(&mut cfg, &gb, None, true, &g);
        assert_eq!(r.effective, None);
        assert_eq!(cfg.ignore.len(), 1, "A's port entry kept");
        assert!(cfg.external.is_empty());

        // A keyboard with a serial number has no port scope.
        let hp = group_keyboards(&sample(), &Config::default(), &State::default());
        assert!(hp.iter().all(|x| x.port_matcher.is_none()));
    }

    #[test]
    fn port_override_keeps_a_stable_peers_mark() {
        // Review TP-08: a VID/PID Ignore rule covers a serial-less unit and a
        // connected unit with a serial number (stable container) reporting
        // the same IDs.
        let serial_less = KeyboardInfo {
            instance_id: "HID\\VID_3434&PID_0281&MI_00\\8&1&0&0000".into(),
            container_id: Some("{11111111-0000-0000-0000-000000000001}".into()),
            product: Some("Keychron (no serial)".into()),
            container_per_port: true,
            ..Default::default()
        };
        let stable = KeyboardInfo {
            instance_id: "HID\\VID_3434&PID_0281&MI_00\\9&2&0&0000".into(),
            container_id: Some("{33333333-0000-0000-0000-000000000003}".into()),
            product: Some("Keychron (serial)".into()),
            container_per_port: false,
            ..Default::default()
        };
        let both = [serial_less, stable];
        let mut cfg = Config::default();
        cfg.ignore.push(Matcher {
            vid: Some(0x3434),
            pid: Some(0x0281),
            ..Default::default()
        });
        let g = group_keyboards(&both, &cfg, &State::default());
        assert!(g.iter().all(|x| x.kind == KeyboardKind::Ignored));
        let sl = g.iter().find(|x| x.name == "Keychron (no serial)").unwrap().clone();
        let r = set_group_mark_scoped(&mut cfg, &sl, Some(Mark::External), true, &g);
        assert_eq!(r.effective, Some(Mark::External));
        assert_eq!(r.kept_for, vec!["Keychron (serial)".to_string()]);
        assert!(r.not_kept.is_empty());
        let g = group_keyboards(&both, &cfg, &State::default());
        let kind = |n: &str| g.iter().find(|x| x.name == n).unwrap().kind;
        assert_eq!(
            kind("Keychron (serial)"),
            KeyboardKind::Ignored,
            "the peer keeps its mark"
        );
        assert_eq!(kind("Keychron (no serial)"), KeyboardKind::External);
        assert!(cfg
            .ignore
            .contains(&Matcher::container("{33333333-0000-0000-0000-000000000003}")));
    }

    #[test]
    fn same_device_rules() {
        assert!(same_device(&Matcher::container("{abc}"), &Matcher::container("ABC")));
        assert!(same_device(&Matcher::instance("hid\\x"), &Matcher::instance("HID\\X")));
        assert!(!same_device(&Matcher::container("{abc}"), &Matcher::instance("abc")));
        let vid = Matcher {
            vid: Some(1),
            ..Default::default()
        };
        assert!(same_device(&vid, &vid.clone()));
        assert!(!same_device(&vid, &Matcher::container("{abc}")));
    }
}
