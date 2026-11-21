//! TOML configuration (`%APPDATA%\KeyDock\config.toml`).
//!
//! The settings window reads and writes this file; hand-editing keeps working.
//! When the settings window saves, the file is rewritten from the parsed model:
//! every setting and every profile is kept, but comments and formatting are not.

use crate::device::{guid_eq, parse_id16, KeyboardInfo};
use crate::langorder::OrderPolicy;
use crate::layout::LayoutSpec;
use serde::Deserialize;
use std::fmt;
use std::fmt::Write as _;

pub const DEFAULT_DEBOUNCE_MS: u32 = 500;
pub const DEFAULT_RESUME_DELAY_MS: u32 = 2000;
pub const DEFAULT_REMOVAL_DELAY_MS: u32 = 2500;

/// Name of the profile the settings window manages for "External keyboard connected".
pub const GUI_EXTERNAL_PROFILE: &str = "External keyboard";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    debounce_ms: Option<u32>,
    resume_delay_ms: Option<u32>,
    removal_delay_ms: Option<u32>,
    remote_sessions: Option<bool>,
    language_order: Option<String>,
    tray: Option<bool>,
    notify: Option<bool>,
    builtin_heuristics: Option<bool>,
    #[serde(default)]
    builtin: Vec<RawMatcher>,
    #[serde(default)]
    external: Vec<RawMatcher>,
    #[serde(default)]
    ignore: Vec<RawMatcher>,
    #[serde(default, rename = "profile")]
    profiles: Vec<RawProfile>,
    default: Option<RawLayoutSet>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawMatcher {
    vid: Option<String>,
    pid: Option<String>,
    container_id: Option<String>,
    name: Option<String>,
    instance_id: Option<String>,
    #[serde(default)]
    any_external: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(RawMatcher),
    Many(Vec<RawMatcher>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    name: String,
    when: OneOrMany,
    layouts: Vec<String>,
    activate: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLayoutSet {
    layouts: Vec<String>,
    activate: Option<String>,
}

/// A keyboard matcher. All criteria that are set must match the same keyboard.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Matcher {
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub container_id: Option<String>,
    pub name: Option<String>,
    pub instance_id: Option<String>,
    pub any_external: bool,
}

impl Matcher {
    pub fn any_external() -> Self {
        Self {
            any_external: true,
            ..Default::default()
        }
    }

    pub fn container(id: &str) -> Self {
        Self {
            container_id: Some(id.to_string()),
            ..Default::default()
        }
    }

    pub fn instance(id: &str) -> Self {
        Self {
            instance_id: Some(id.to_string()),
            ..Default::default()
        }
    }

    /// Matches the device identity criteria (everything except `any_external`).
    pub fn matches_identity(&self, kb: &KeyboardInfo) -> bool {
        if let Some(vid) = self.vid {
            let ok = kb
                .ids()
                .filter_map(crate::device::parse_vid_pid)
                .any(|(v, p)| v == vid && self.pid.is_none_or(|want| want == p));
            if !ok {
                return false;
            }
        }
        if let Some(c) = &self.container_id {
            if !kb.container_id.as_deref().is_some_and(|k| guid_eq(k, c)) {
                return false;
            }
        }
        if let Some(n) = &self.name {
            let n = n.to_lowercase();
            let hit = |s: &Option<String>| s.as_deref().is_some_and(|k| k.to_lowercase().contains(&n));
            if !hit(&kb.name) && !hit(&kb.product) {
                return false;
            }
        }
        if let Some(i) = &self.instance_id {
            if !kb.instance_id.to_ascii_uppercase().contains(&i.to_ascii_uppercase()) {
                return false;
            }
        }
        true
    }

    /// Full match; `is_external` tells whether this keyboard counts as external.
    pub fn matches(&self, kb: &KeyboardInfo, is_external: bool) -> bool {
        (!self.any_external || is_external) && self.matches_identity(kb)
    }

    fn has_any_criterion(&self) -> bool {
        self.vid.is_some()
            || self.container_id.is_some()
            || self.name.is_some()
            || self.instance_id.is_some()
            || self.any_external
    }

    /// Short human-readable description, e.g. `VID 046D PID C52B`.
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.any_external {
            parts.push("any external keyboard".into());
        }
        if let Some(v) = self.vid {
            parts.push(format!("VID {v:04X}"));
        }
        if let Some(p) = self.pid {
            parts.push(format!("PID {p:04X}"));
        }
        if let Some(n) = &self.name {
            parts.push(format!("name \"{n}\""));
        }
        if let Some(c) = &self.container_id {
            parts.push(format!("container {c}"));
        }
        if let Some(i) = &self.instance_id {
            parts.push(format!("instance {i}"));
        }
        parts.join(", ")
    }

    /// TOML inline table, e.g. `{ vid = "046D", pid = "C52B" }`.
    pub fn to_toml_inline(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(v) = self.vid {
            parts.push(format!("vid = \"{v:04X}\""));
        }
        if let Some(p) = self.pid {
            parts.push(format!("pid = \"{p:04X}\""));
        }
        if let Some(c) = &self.container_id {
            parts.push(format!("container_id = {}", toml_str(c)));
        }
        if let Some(n) = &self.name {
            parts.push(format!("name = {}", toml_str(n)));
        }
        if let Some(i) = &self.instance_id {
            parts.push(format!("instance_id = {}", toml_str(i)));
        }
        if self.any_external {
            parts.push("any_external = true".into());
        }
        format!("{{ {} }}", parts.join(", "))
    }
}

/// A TOML basic string literal with proper escaping.
fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LayoutSet {
    pub layouts: Vec<LayoutSpec>,
    pub activate: Option<LayoutSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    /// The profile applies when any of these matchers matches a present keyboard.
    pub when: Vec<Matcher>,
    pub set: LayoutSet,
}

impl Profile {
    pub fn matches(&self, kb: &KeyboardInfo, is_external: bool) -> bool {
        self.when.iter().any(|m| m.matches(kb, is_external))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub debounce_ms: u32,
    pub resume_delay_ms: u32,
    /// After a keyboard is removed, how long the keyboard set must stay
    /// unchanged before KeyDock switches (docks drop devices for a moment).
    pub removal_delay_ms: u32,
    /// Also change layouts in a Remote Desktop session. Off by default: the
    /// keyboards KeyDock sees are the remote computer's, not the ones the
    /// remote user types on.
    pub remote_sessions: bool,
    /// What to do when a layout change reorders the user's language list
    /// (`language_order = "report"`, the default since 0.3.5, or the opt-in
    /// `"restore"`, see `crate::langorder`).
    pub language_order: OrderPolicy,
    pub tray: bool,
    pub notify: bool,
    pub builtin_heuristics: bool,
    /// Keyboards marked as built-in.
    pub builtin: Vec<Matcher>,
    /// Keyboards marked as external (overrides built-in detection).
    pub external: Vec<Matcher>,
    /// Keyboards to disregard entirely.
    pub ignore: Vec<Matcher>,
    pub profiles: Vec<Profile>,
    pub default: Option<LayoutSet>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            resume_delay_ms: DEFAULT_RESUME_DELAY_MS,
            removal_delay_ms: DEFAULT_REMOVAL_DELAY_MS,
            remote_sessions: false,
            language_order: OrderPolicy::Report,
            tray: true,
            notify: false,
            builtin_heuristics: true,
            builtin: Vec::new(),
            external: Vec::new(),
            ignore: Vec::new(),
            profiles: Vec::new(),
            default: None,
        }
    }
}

fn convert_matcher(raw: RawMatcher, ctx: &str, allow_any_external: bool) -> Result<Matcher, ConfigError> {
    let vid = raw
        .vid
        .as_deref()
        .map(|s| {
            parse_id16(s).ok_or_else(|| ConfigError(format!("{ctx}: invalid vid \"{s}\" (expected 4 hex digits)")))
        })
        .transpose()?;
    let pid = raw
        .pid
        .as_deref()
        .map(|s| {
            parse_id16(s).ok_or_else(|| ConfigError(format!("{ctx}: invalid pid \"{s}\" (expected 4 hex digits)")))
        })
        .transpose()?;
    if pid.is_some() && vid.is_none() {
        return Err(ConfigError(format!("{ctx}: pid requires vid")));
    }
    if raw.any_external && !allow_any_external {
        return Err(ConfigError(format!("{ctx}: any_external is not allowed here")));
    }
    let empty_to_none = |o: Option<String>| o.filter(|s| !s.trim().is_empty());
    let m = Matcher {
        vid,
        pid,
        container_id: empty_to_none(raw.container_id),
        name: empty_to_none(raw.name),
        instance_id: empty_to_none(raw.instance_id),
        any_external: raw.any_external,
    };
    if !m.has_any_criterion() {
        return Err(ConfigError(format!(
            "{ctx}: needs at least one of vid, container_id, name, instance_id, any_external"
        )));
    }
    Ok(m)
}

fn convert_list(raw: Vec<RawMatcher>, key: &str) -> Result<Vec<Matcher>, ConfigError> {
    raw.into_iter()
        .enumerate()
        .map(|(i, m)| convert_matcher(m, &format!("{key}[{i}]"), false))
        .collect()
}

fn convert_set(layouts: Vec<String>, activate: Option<String>, ctx: &str) -> Result<LayoutSet, ConfigError> {
    let mut out: Vec<LayoutSpec> = Vec::new();
    for l in &layouts {
        let spec = LayoutSpec::parse(l).map_err(|e| ConfigError(format!("{ctx}: {e}")))?;
        if !out.contains(&spec) {
            out.push(spec);
        }
    }
    if out.is_empty() {
        return Err(ConfigError(format!("{ctx}: `layouts` must list at least one layout")));
    }
    let activate = activate
        .as_deref()
        .map(|a| LayoutSpec::parse(a).map_err(|e| ConfigError(format!("{ctx}: activate: {e}"))))
        .transpose()?;
    if let Some(a) = activate {
        if !out.contains(&a) {
            return Err(ConfigError(format!("{ctx}: activate \"{a}\" is not in `layouts`")));
        }
    }
    Ok(LayoutSet { layouts: out, activate })
}

fn write_set(out: &mut String, set: &LayoutSet) {
    let layouts: Vec<String> = set.layouts.iter().map(|l| format!("\"{l}\"")).collect();
    let _ = writeln!(out, "layouts = [{}]", layouts.join(", "));
    if let Some(a) = set.activate {
        let _ = writeln!(out, "activate = \"{a}\"");
    }
}

fn write_list(out: &mut String, key: &str, list: &[Matcher]) {
    if list.is_empty() {
        return;
    }
    let _ = writeln!(out, "{key} = [");
    for m in list {
        let _ = writeln!(out, "    {},", m.to_toml_inline());
    }
    let _ = writeln!(out, "]");
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError(format!("config.toml: {e}")))?;
        let mut cfg = Config::default();
        if let Some(v) = raw.debounce_ms {
            cfg.debounce_ms = v.clamp(50, 60_000);
        }
        if let Some(v) = raw.resume_delay_ms {
            cfg.resume_delay_ms = v.clamp(50, 120_000);
        }
        if let Some(v) = raw.removal_delay_ms {
            cfg.removal_delay_ms = v.clamp(0, 60_000);
        }
        if let Some(v) = raw.remote_sessions {
            cfg.remote_sessions = v;
        }
        if let Some(v) = raw.language_order {
            cfg.language_order = match v.trim().to_ascii_lowercase().as_str() {
                "restore" => OrderPolicy::Restore,
                "report" => OrderPolicy::Report,
                _ => {
                    return Err(ConfigError(format!(
                        "config.toml: language_order must be \"restore\" or \"report\", not \"{v}\""
                    )))
                }
            };
        }
        if let Some(v) = raw.tray {
            cfg.tray = v;
        }
        if let Some(v) = raw.notify {
            cfg.notify = v;
        }
        if let Some(v) = raw.builtin_heuristics {
            cfg.builtin_heuristics = v;
        }
        cfg.builtin = convert_list(raw.builtin, "builtin")?;
        cfg.external = convert_list(raw.external, "external")?;
        cfg.ignore = convert_list(raw.ignore, "ignore")?;
        let mut names: Vec<String> = Vec::new();
        for (i, p) in raw.profiles.into_iter().enumerate() {
            let name = p.name.trim().to_string();
            if name.is_empty() {
                return Err(ConfigError(format!("profile #{}: name must not be empty", i + 1)));
            }
            if name.eq_ignore_ascii_case("default") || names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                return Err(ConfigError(format!("profile \"{name}\": duplicate or reserved name")));
            }
            let ctx = format!("profile \"{name}\"");
            let raw_when = match p.when {
                OneOrMany::One(m) => vec![m],
                OneOrMany::Many(v) => v,
            };
            if raw_when.is_empty() {
                return Err(ConfigError(format!("{ctx}: `when` must not be empty")));
            }
            let when = raw_when
                .into_iter()
                .enumerate()
                .map(|(j, m)| convert_matcher(m, &format!("{ctx} when[{j}]"), true))
                .collect::<Result<Vec<_>, _>>()?;
            let set = convert_set(p.layouts, p.activate, &ctx)?;
            names.push(name.clone());
            cfg.profiles.push(Profile { name, when, set });
        }
        if let Some(d) = raw.default {
            cfg.default = Some(convert_set(d.layouts, d.activate, "[default]")?);
        }
        Ok(cfg)
    }

    /// Serialises the configuration. `Config::parse(&c.to_toml()) == c` holds
    /// for every valid configuration.
    pub fn to_toml(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "# KeyDock configuration, written by the KeyDock settings window.\n\
             # Hand edits are fine, but comments are not kept when the settings window saves.\n\
             # See README.md for all keys.\n\n",
        );
        let _ = writeln!(out, "debounce_ms = {}", self.debounce_ms);
        let _ = writeln!(out, "resume_delay_ms = {}", self.resume_delay_ms);
        let _ = writeln!(out, "removal_delay_ms = {}", self.removal_delay_ms);
        let _ = writeln!(out, "remote_sessions = {}", self.remote_sessions);
        if self.language_order == OrderPolicy::Restore {
            // Written only when opted in, so a file saved with the default
            // still loads in KeyDock 0.3.3, which does not know the key.
            let _ = writeln!(out, "language_order = \"restore\"");
        }
        let _ = writeln!(out, "tray = {}", self.tray);
        let _ = writeln!(out, "notify = {}", self.notify);
        let _ = writeln!(out, "builtin_heuristics = {}", self.builtin_heuristics);
        write_list(&mut out, "builtin", &self.builtin);
        write_list(&mut out, "external", &self.external);
        write_list(&mut out, "ignore", &self.ignore);
        for p in &self.profiles {
            out.push_str("\n[[profile]]\n");
            let _ = writeln!(out, "name = {}", toml_str(&p.name));
            write_set(&mut out, &p.set);
            if p.when.len() == 1 {
                let _ = writeln!(out, "when = {}", p.when[0].to_toml_inline());
            } else {
                let items: Vec<String> = p.when.iter().map(Matcher::to_toml_inline).collect();
                let _ = writeln!(out, "when = [{}]", items.join(", "));
            }
        }
        if let Some(d) = &self.default {
            out.push_str("\n[default]\n");
            write_set(&mut out, d);
        }
        out
    }

    /// Every layout mentioned anywhere in the configuration. KeyDock only ever
    /// installs or removes layouts from this set.
    pub fn managed_layouts(&self) -> Vec<LayoutSpec> {
        let mut out: Vec<LayoutSpec> = Vec::new();
        let sets = self.profiles.iter().map(|p| &p.set).chain(self.default.iter());
        for s in sets {
            for l in &s.layouts {
                if !out.contains(l) {
                    out.push(*l);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
debounce_ms = 800
removal_delay_ms = 4000
remote_sessions = true
tray = false
notify = true
builtin = [{ container_id = "{11111111-2222-3333-4444-555555555555}" }]
external = [{ instance_id = "HID\\VID_06CB&PID_CE2D\\8&1" }]
ignore = [{ vid = "1050" }]

[[profile]]
name = "us"
layouts = ["0419:00000419", "0409:00000409", "0409:00000409"]
activate = "0409:00000409"
when = { vid = "046d", pid = "C52B" }

[[profile]]
name = "ext"
layouts = ["0419:00000419", "0409:00000409"]
when = [{ any_external = true }, { name = "Keychron \"K2\"" }]

[default]
layouts = ["0419:00000419", "0409:0000040A"]
activate = "0419:00000419"
"#;

    #[test]
    fn empty_config_is_inert() {
        let c = Config::parse("").unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.removal_delay_ms, DEFAULT_REMOVAL_DELAY_MS);
        assert!(!c.remote_sessions, "remote sessions are left alone by default");
        assert_eq!(c.language_order, OrderPolicy::Report, "restoring is opt-in");
        assert!(c.managed_layouts().is_empty());
        assert!(!c.to_toml().contains("language_order"), "default not written");
    }

    #[test]
    fn language_order_policy() {
        let c = Config::parse("language_order = \"report\"").unwrap();
        assert_eq!(c.language_order, OrderPolicy::Report);
        assert_eq!(Config::parse(&c.to_toml()).unwrap(), c);
        let c = Config::parse("language_order = \"restore\"").unwrap();
        assert_eq!(c.language_order, OrderPolicy::Restore);
        assert!(
            c.to_toml().contains("language_order = \"restore\""),
            "the opt-in is kept on save"
        );
        assert_eq!(Config::parse(&c.to_toml()).unwrap(), c);
        assert_eq!(
            Config::parse("language_order = \"Restore\"").unwrap().language_order,
            OrderPolicy::Restore
        );
        assert!(Config::parse("language_order = \"never\"").is_err());
    }

    #[test]
    fn full_config() {
        let c = Config::parse(FULL).unwrap();
        assert_eq!(c.debounce_ms, 800);
        assert_eq!(c.removal_delay_ms, 4000);
        assert!(c.remote_sessions);
        assert!(!c.tray);
        assert!(c.notify);
        assert_eq!(c.builtin.len(), 1);
        assert_eq!(c.external.len(), 1);
        assert_eq!(c.profiles.len(), 2);
        assert_eq!(c.profiles[0].set.layouts.len(), 2, "duplicates are removed");
        assert_eq!(
            c.profiles[0].when,
            vec![Matcher {
                vid: Some(0x046D),
                pid: Some(0xC52B),
                ..Default::default()
            }]
        );
        assert_eq!(c.profiles[1].when.len(), 2);
        assert!(c.profiles[1].when[0].any_external);
        assert_eq!(c.managed_layouts().len(), 3);
    }

    #[test]
    fn toml_round_trip() {
        let c = Config::parse(FULL).unwrap();
        let text = c.to_toml();
        let back = Config::parse(&text).unwrap_or_else(|e| panic!("{e}\n---\n{text}"));
        assert_eq!(back, c);
        // And it is stable.
        assert_eq!(back.to_toml(), text);
        let d = Config::default();
        assert_eq!(Config::parse(&d.to_toml()).unwrap(), d);
    }

    #[test]
    fn invalid_configs() {
        let bad = [
            "foo = 1",
            "[[profile]]\nname='a'\nlayouts=['0409:00000409']\nwhen={pid='1234'}",
            "[[profile]]\nname='a'\nlayouts=['0409:00000409']\nwhen={}",
            "[[profile]]\nname='a'\nlayouts=['0409:00000409']\nwhen=[]",
            "[[profile]]\nname='a'\nlayouts=[]\nwhen={any_external=true}",
            "[default]\nlayouts=['0409:00000409']\nactivate='0419:00000419'",
            "[default]\nlayouts=['0409']",
            "[[profile]]\nname='a'\nlayouts=['0409:00000409']\nwhen={any_external=true}\n[[profile]]\nname='A'\nlayouts=['0409:00000409']\nwhen={any_external=true}",
            "[[profile]]\nname='default'\nlayouts=['0409:00000409']\nwhen={any_external=true}",
            "ignore=[{any_external=true}]",
            "builtin=[{any_external=true}]",
            "ignore=[{vid='XYZ'}]",
        ];
        for b in bad {
            assert!(Config::parse(b).is_err(), "should fail: {b}");
        }
    }

    #[test]
    fn matcher_vid_pid_usb_and_bluetooth() {
        let m = Matcher {
            vid: Some(0x046D),
            pid: Some(0xB35F),
            ..Default::default()
        };
        let usb = KeyboardInfo {
            instance_id: "HID\\VID_046D&PID_B35F&MI_00\\8&1&0&0000".into(),
            hardware_ids: vec!["HID\\VID_046D&PID_B35F&REV_0001&MI_00".into()],
            ..Default::default()
        };
        let bt = KeyboardInfo {
            instance_id: "HID\\{00001124-0000-1000-8000-00805f9b34fb}_VID&0002046d_PID&b35f\\9&1&0&0000".into(),
            hardware_ids: vec!["HID\\{00001124-0000-1000-8000-00805f9b34fb}_VID&0002046d_PID&b35f".into()],
            ..Default::default()
        };
        let ble = KeyboardInfo {
            instance_id: "HID\\{00001812-0000-1000-8000-00805f9b34fb}_Dev_VID&02046d_PID&b35f_REV&0006_xxx\\9&2&0&0000"
                .into(),
            ..Default::default()
        };
        let other = KeyboardInfo {
            instance_id: "HID\\VID_046D&PID_C52B&MI_00\\8&1".into(),
            ..Default::default()
        };
        assert!(m.matches(&usb, true));
        assert!(m.matches(&bt, true));
        assert!(m.matches(&ble, true));
        assert!(!m.matches(&other, true));
        let vid_only = Matcher {
            vid: Some(0x046D),
            ..Default::default()
        };
        assert!(vid_only.matches(&other, true));
    }

    #[test]
    fn matcher_other_criteria() {
        let kb = KeyboardInfo {
            instance_id: "HID\\VID_3434&PID_0281&MI_00\\8&1".into(),
            container_id: Some("{8C1C3E4A-1111-2222-3333-444455556666}".into()),
            name: Some("HID Keyboard Device".into()),
            product: Some("Keychron K2".into()),
            ..Default::default()
        };
        assert!(Matcher {
            name: Some("keychron".into()),
            ..Default::default()
        }
        .matches(&kb, true));
        assert!(Matcher {
            name: Some("hid keyboard".into()),
            ..Default::default()
        }
        .matches(&kb, true));
        assert!(Matcher::container("8c1c3e4a-1111-2222-3333-444455556666").matches(&kb, true));
        assert!(Matcher {
            instance_id: Some("vid_3434".into()),
            ..Default::default()
        }
        .matches(&kb, true));
        let and_fails = Matcher {
            name: Some("keychron".into()),
            vid: Some(0x1234),
            ..Default::default()
        };
        assert!(!and_fails.matches(&kb, true));
        assert!(Matcher::any_external().matches(&kb, true));
        assert!(!Matcher::any_external().matches(&kb, false));
    }

    #[test]
    fn describe_matchers() {
        let m = Matcher {
            vid: Some(0x03F0),
            pid: Some(0x2041),
            ..Default::default()
        };
        assert_eq!(m.describe(), "VID 03F0, PID 2041");
        assert_eq!(m.to_toml_inline(), "{ vid = \"03F0\", pid = \"2041\" }");
    }
}
