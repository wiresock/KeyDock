//! Readable names for languages and keyboard layouts, and the list of layouts
//! known on this system (user language list, loaded HKLs, all installed KLIDs).

use super::layouts::{current_hkls, hkl_low32};
use super::registry::{wide, Key};
use keydock::layout::{parse_layout_id, LayoutSpec};
use std::collections::BTreeMap;
use windows::core::PCWSTR;
use windows::Win32::Globalization::{
    GetLocaleInfoEx, LCIDToLocaleName, LocaleNameToLCID, LOCALE_ALLOW_NEUTRAL_NAMES, LOCALE_SLOCALIZEDDISPLAYNAME,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

const LAYOUTS_KEY: &str = "SYSTEM\\CurrentControlSet\\Control\\Keyboard Layouts";
const USER_PROFILE_KEY: &str = "Control Panel\\International\\User Profile";

/// One keyboard layout installed on the system (a KLID).
#[derive(Debug, Clone)]
pub struct KlidInfo {
    pub klid: u32,
    pub name: String,
    pub layout_id: Option<u16>,
}

pub struct Catalog {
    /// All KLIDs from the registry, sorted by name.
    pub klids: Vec<KlidInfo>,
    by_klid: BTreeMap<u32, usize>,
    /// Languages in the user's language list (LANGIDs, in order).
    pub user_languages: Vec<u16>,
    /// Layout entries from the user's language list.
    pub user_layouts: Vec<LayoutSpec>,
}

pub fn language_name(lang: u16) -> String {
    let mut name = [0u16; 85];
    // SAFETY: buffers sized as documented (LOCALE_NAME_MAX_LENGTH = 85).
    unsafe {
        let n = LCIDToLocaleName(u32::from(lang), Some(&mut name), LOCALE_ALLOW_NEUTRAL_NAMES);
        if n > 0 {
            let mut out = [0u16; 128];
            let m = GetLocaleInfoEx(PCWSTR(name.as_ptr()), LOCALE_SLOCALIZEDDISPLAYNAME, Some(&mut out));
            if m > 1 {
                return String::from_utf16_lossy(&out[..(m - 1) as usize]);
            }
        }
    }
    format!("Language {lang:04X}")
}

fn lang_from_tag(tag: &str) -> Option<u16> {
    let w = wide(tag);
    // SAFETY: NUL-terminated locale name.
    let lcid = unsafe { LocaleNameToLCID(PCWSTR(w.as_ptr()), LOCALE_ALLOW_NEUTRAL_NAMES) };
    let lang = (lcid & 0xFFFF) as u16;
    // 0x1000 = LOCALE_CUSTOM_UNSPECIFIED: no fixed LANGID.
    (lcid != 0 && lang != 0x1000).then_some(lang)
}

impl Catalog {
    pub fn load() -> Self {
        let mut klids = Vec::new();
        if let Some(root) = Key::open(HKEY_LOCAL_MACHINE, LAYOUTS_KEY) {
            for sub in root.subkeys() {
                let Ok(klid) = u32::from_str_radix(&sub, 16) else {
                    continue;
                };
                let Some(k) = Key::open(HKEY_LOCAL_MACHINE, &format!("{LAYOUTS_KEY}\\{sub}")) else {
                    continue;
                };
                let name = k
                    .mui_string("Layout Display Name")
                    .or_else(|| k.string("Layout Text"))
                    .unwrap_or_else(|| format!("Layout {klid:08X}"));
                let layout_id = k.string("Layout Id").and_then(|s| parse_layout_id(&s));
                klids.push(KlidInfo { klid, name, layout_id });
            }
        }
        klids.sort_by_key(|k| k.name.to_lowercase());
        let by_klid = klids.iter().enumerate().map(|(i, k)| (k.klid, i)).collect();

        let mut user_languages = Vec::new();
        let mut user_layouts = Vec::new();
        if let Some(up) = Key::open(HKEY_CURRENT_USER, USER_PROFILE_KEY) {
            // The "Languages" value lists the order; subkeys hold the layouts.
            for tag in up.subkeys() {
                let Some(k) = Key::open(HKEY_CURRENT_USER, &format!("{USER_PROFILE_KEY}\\{tag}")) else {
                    continue;
                };
                if let Some(l) = lang_from_tag(&tag) {
                    if !user_languages.contains(&l) {
                        user_languages.push(l);
                    }
                }
                for v in k.value_names() {
                    if let Ok(spec) = LayoutSpec::parse(&v) {
                        if !user_layouts.contains(&spec) {
                            user_layouts.push(spec);
                        }
                    }
                }
            }
        }
        Self {
            klids,
            by_klid,
            user_languages,
            user_layouts,
        }
    }

    pub fn layout_name(&self, klid: u32) -> String {
        self.by_klid
            .get(&klid)
            .map(|&i| self.klids[i].name.clone())
            .unwrap_or_else(|| format!("Layout {klid:08X}"))
    }

    /// "English (United States) — Spanish".
    pub fn label(&self, spec: &LayoutSpec) -> String {
        format!("{} \u{2014} {}", language_name(spec.lang), self.layout_name(spec.klid))
    }

    /// Maps a loaded HKL back to its `<LANGID>:<KLID>` entry.
    pub fn spec_for_hkl(&self, hkl: u32) -> Option<LayoutSpec> {
        let lang = (hkl & 0xFFFF) as u16;
        let hi = hkl >> 16;
        if hkl & 0xF000_0000 == 0xE000_0000 {
            return Some(LayoutSpec::new(lang, hkl));
        }
        if hi & 0xF000 == 0xF000 {
            let id = (hi & 0x0FFF) as u16;
            return self
                .klids
                .iter()
                .find(|k| k.layout_id == Some(id))
                .map(|k| LayoutSpec::new(lang, k.klid));
        }
        Some(LayoutSpec::new(lang, hi))
    }

    /// Layouts loaded in the session right now.
    pub fn loaded_specs(&self) -> Vec<LayoutSpec> {
        current_hkls()
            .into_iter()
            .filter_map(|h| self.spec_for_hkl(hkl_low32(h)))
            .collect()
    }
}
