//! Keyboard layout identifiers.
//!
//! A layout entry is written as `<LANGID>:<KLID>`, both in hexadecimal, for
//! example `"0419:00000419"` (Russian language, Russian layout) or
//! `"0C0A:0000040A"` (Spanish (Spain) language, Spanish layout). This is the
//! same `<LangID>:<KLID>` pair that `InstallLayoutOrTip` takes.
//!
//! The KLID (keyboard layout identifier, the registry key name under
//! `HKLM\SYSTEM\CurrentControlSet\Control\Keyboard Layouts`) is *not* an HKL
//! (the input locale handle that `WM_INPUTLANGCHANGEREQUEST` and
//! `GetKeyboardLayoutList` use). The HKL is derived from both the language and
//! the KLID, see [`LayoutSpec::hkl`].

use std::fmt;

/// One `<LANGID>:<KLID>` layout entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LayoutSpec {
    /// Language identifier (LANGID), e.g. `0x0409`.
    pub lang: u16,
    /// Keyboard layout identifier (KLID), e.g. `0x00000409`.
    pub klid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutParseError(pub String);

impl fmt::Display for LayoutParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LayoutParseError {}

fn parse_hex_field(s: &str, max_digits: usize, what: &str, full: &str) -> Result<u32, LayoutParseError> {
    let t = s.trim();
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    if t.is_empty() || t.len() > max_digits || !t.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(LayoutParseError(format!(
            "invalid {what} in layout \"{full}\": expected up to {max_digits} hex digits"
        )));
    }
    u32::from_str_radix(t, 16).map_err(|e| LayoutParseError(format!("invalid {what} in layout \"{full}\": {e}")))
}

impl LayoutSpec {
    pub const fn new(lang: u16, klid: u32) -> Self {
        Self { lang, klid }
    }

    /// Parses `"0419:00000419"` (an optional `0x` prefix on each part is accepted).
    /// Text-service (TIP) profiles of the form `<LangID>:{CLSID}{Profile GUID}`
    /// are not supported by this prototype.
    pub fn parse(s: &str) -> Result<Self, LayoutParseError> {
        let (lang, klid) = s.split_once(':').ok_or_else(|| {
            LayoutParseError(format!(
                "invalid layout \"{s}\": expected \"<LANGID>:<KLID>\", e.g. \"0409:00000409\""
            ))
        })?;
        if klid.trim_start().starts_with('{') {
            return Err(LayoutParseError(format!(
                "layout \"{s}\": text services (TIP GUIDs) are not supported, only keyboard layouts (KLIDs)"
            )));
        }
        let lang = parse_hex_field(lang, 4, "LANGID", s)? as u16;
        let klid = parse_hex_field(klid, 8, "KLID", s)?;
        if lang == 0 {
            return Err(LayoutParseError(format!(
                "invalid layout \"{s}\": LANGID must not be 0"
            )));
        }
        Ok(Self { lang, klid })
    }

    /// The string passed to `InstallLayoutOrTip`, in the form used by the
    /// examples on Microsoft Learn: `"0x0407:0x00000407"`.
    pub fn install_string(&self) -> String {
        format!("0x{:04X}:0x{:08X}", self.lang, self.klid)
    }

    /// The KLID as the 8-hex-digit string `LoadKeyboardLayoutW` and the
    /// registry use, e.g. `"00000409"`.
    pub fn klid_string(&self) -> String {
        format!("{:08X}", self.klid)
    }

    /// True for KLIDs that need the registry `Layout Id` value to compute the
    /// HKL (layout variants such as US-Dvorak `00010409`).
    pub fn is_variant(&self) -> bool {
        !self.is_ime() && (self.klid >> 16) != 0
    }

    /// True for legacy IME KLIDs (`E0xxxxxx`), whose HKL equals the KLID.
    pub fn is_ime(&self) -> bool {
        self.klid & 0xF000_0000 == 0xE000_0000
    }

    /// Computes the low 32 bits of the HKL for this entry.
    ///
    /// * The low word of an HKL is the language identifier from the entry.
    /// * The high word is the device/layout handle:
    ///   - for a base layout (`0000xxxx`) it is the low word of the KLID,
    ///     e.g. `0C0A:0000040A` -> `0x040A0C0A`, `0409:00000409` -> `0x04090409`;
    ///   - for a layout variant (`0001xxxx`, `0002xxxx`, ...) it is
    ///     `0xF000 | LayoutId`, where `LayoutId` is the hex `Layout Id` registry
    ///     value of the KLID, e.g. US-Dvorak `0409:00010409` with `Layout Id`
    ///     `0002` -> `0xF0020409`. Without `layout_id` this returns `None`;
    ///   - a legacy IME KLID (`E0xxxxxx`) is its own HKL.
    ///
    /// On 64-bit Windows an HKL is pointer-sized and sign-extended, so compare
    /// only the low 32 bits.
    pub fn hkl(&self, layout_id: Option<u16>) -> Option<u32> {
        if self.is_ime() {
            return Some(self.klid);
        }
        let device: u32 = if (self.klid >> 16) == 0 {
            self.klid & 0xFFFF
        } else {
            0xF000 | (u32::from(layout_id?) & 0x0FFF)
        };
        Some((device << 16) | u32::from(self.lang))
    }
}

impl fmt::Display for LayoutSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04X}:{:08X}", self.lang, self.klid)
    }
}

/// Parses the registry `Layout Id` value (a hex string such as `"0002"`).
pub fn parse_layout_id(s: &str) -> Option<u16> {
    let t = s.trim().trim_end_matches('\0');
    if t.is_empty() || t.len() > 4 {
        return None;
    }
    u16::from_str_radix(t, 16).ok()
}

/// Joins entries with `;` as `InstallLayoutOrTip` expects for a list.
pub fn install_list(specs: &[LayoutSpec]) -> String {
    specs
        .iter()
        .map(LayoutSpec::install_string)
        .collect::<Vec<_>>()
        .join(";")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let s = LayoutSpec::parse("0419:00000419").unwrap();
        assert_eq!(s, LayoutSpec::new(0x0419, 0x0000_0419));
        let s = LayoutSpec::parse(" 0x0c0a : 0x0000040a ").unwrap();
        assert_eq!(s, LayoutSpec::new(0x0C0A, 0x0000_040A));
        assert_eq!(s.to_string(), "0C0A:0000040A");
        assert_eq!(s.install_string(), "0x0C0A:0x0000040A");
        assert_eq!(s.klid_string(), "0000040A");
    }

    #[test]
    fn parse_errors() {
        assert!(LayoutSpec::parse("00000409").is_err());
        assert!(LayoutSpec::parse("0409:").is_err());
        assert!(LayoutSpec::parse("04090:00000409").is_err());
        assert!(LayoutSpec::parse("0409:000004090").is_err());
        assert!(LayoutSpec::parse("zz09:00000409").is_err());
        assert!(LayoutSpec::parse("0000:00000409").is_err());
        assert!(
            LayoutSpec::parse("0412:{A028AE76-01B1-46C2-99C4-ACD9858AE02F}{B5FE1F02-D5F2-4445-9C03-C568F23C99A1}")
                .is_err()
        );
    }

    #[test]
    fn hkl_base_layouts() {
        // KLID is not the HKL: the language goes into the low word.
        assert_eq!(LayoutSpec::new(0x0409, 0x0409).hkl(None), Some(0x0409_0409));
        assert_eq!(LayoutSpec::new(0x0419, 0x0419).hkl(None), Some(0x0419_0419));
        // Spanish (Spain) language with the Spanish layout.
        assert_eq!(LayoutSpec::new(0x0C0A, 0x040A).hkl(None), Some(0x040A_0C0A));
        // US layout under the Spanish (Spain) language.
        assert_eq!(LayoutSpec::new(0x0C0A, 0x0409).hkl(None), Some(0x0409_0C0A));
        // English (US) language with the Spanish layout.
        assert_eq!(LayoutSpec::new(0x0409, 0x040A).hkl(None), Some(0x040A_0409));
    }

    #[test]
    fn hkl_variants_and_ime() {
        let dvorak = LayoutSpec::new(0x0409, 0x0001_0409);
        assert!(dvorak.is_variant());
        assert_eq!(dvorak.hkl(None), None);
        assert_eq!(dvorak.hkl(Some(0x0002)), Some(0xF002_0409));
        let ime = LayoutSpec::new(0x0411, 0xE001_0411);
        assert!(ime.is_ime());
        assert!(!ime.is_variant());
        assert_eq!(ime.hkl(None), Some(0xE001_0411));
    }

    #[test]
    fn layout_id_parse() {
        assert_eq!(parse_layout_id("0002"), Some(2));
        assert_eq!(parse_layout_id("00c0\0"), Some(0xC0));
        assert_eq!(parse_layout_id(""), None);
        assert_eq!(parse_layout_id("xyz"), None);
    }

    #[test]
    fn list_join() {
        let l = [LayoutSpec::new(0x0419, 0x0419), LayoutSpec::new(0x0409, 0x0409)];
        assert_eq!(install_list(&l), "0x0419:0x00000419;0x0409:0x00000409");
    }
}
