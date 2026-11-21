//! Parsing of the "Start with Windows" command line stored in
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\KeyDock`.

/// Where the Run entry points relative to the running executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEntry {
    /// No Run entry (or an empty one).
    Missing,
    /// The entry starts this executable.
    ThisExe,
    /// The entry starts another copy, e.g. an older portable copy before the
    /// installed one was used. Holds that copy's path.
    OtherExe(String),
}

/// Extracts the executable path from a Run command line: the quoted part, or
/// up to and including the first `.exe` for unquoted values.
pub fn command_exe(command: &str) -> Option<String> {
    let c = command.trim();
    if let Some(rest) = c.strip_prefix('"') {
        let end = rest.find('"').unwrap_or(rest.len());
        let p = rest[..end].trim();
        return (!p.is_empty()).then(|| p.to_string());
    }
    let lower = c.to_ascii_lowercase();
    let end = match lower.find(".exe") {
        Some(i) => i + 4,
        None => c.find(' ').unwrap_or(c.len()),
    };
    let p = c[..end].trim();
    (!p.is_empty()).then(|| p.to_string())
}

fn normalize(path: &str) -> String {
    let p = path.trim().replace('/', "\\");
    let p = p.strip_prefix("\\\\?\\").unwrap_or(&p);
    p.to_lowercase()
}

/// Paths are compared case-insensitively, ignoring `/` vs `\` and a `\\?\` prefix.
pub fn same_path(a: &str, b: &str) -> bool {
    normalize(a) == normalize(b)
}

/// Interprets the `StartupApproved\Run` value Explorer keeps next to a Run
/// entry when it is turned on or off in Task Manager or Settings > Apps >
/// Startup: the first byte is even (02, 06) when enabled and odd (03, 07)
/// when disabled. `None` for an empty value. Not documented, but stable
/// since Windows 8; a missing value means enabled.
pub fn startup_approved(data: &[u8]) -> Option<bool> {
    data.first().map(|b| b & 1 == 0)
}

/// What a Save/Apply in the settings window does with "Start with Windows".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutostartAction {
    /// Leave the registry alone.
    None,
    /// The user ticked the box: write the entry and clear a "turned off"
    /// record in Startup apps.
    Enable,
    /// The user unticked the box: remove the entry.
    Disable,
    /// The box was left ticked and the entry starts another copy: point it
    /// to this one, keeping the Startup apps choice.
    Repoint,
}

/// Decides [`AutostartAction`]. `shown` is the state the box had when the
/// window opened (or was last saved), `ticked` its state now, `now_on`
/// whether the entry is effectively on in the registry now, `other_copy`
/// whether the entry starts another executable. Only a change the user made
/// to the box turns the entry on or off; when the box was not touched and
/// the registry changed meanwhile (for example turned off in Task Manager),
/// that newer choice is kept.
pub fn autostart_action(shown: bool, ticked: bool, now_on: bool, other_copy: bool) -> AutostartAction {
    match (shown, ticked) {
        (false, true) => AutostartAction::Enable,
        (true, false) => AutostartAction::Disable,
        (true, true) if now_on && other_copy => AutostartAction::Repoint,
        _ => AutostartAction::None,
    }
}

/// Classifies a Run value (`None` when it does not exist) for `current_exe`.
pub fn classify(value: Option<&str>, current_exe: &str) -> RunEntry {
    match value.and_then(command_exe) {
        None => RunEntry::Missing,
        Some(p) if same_path(&p, current_exe) => RunEntry::ThisExe,
        Some(p) => RunEntry::OtherExe(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PF: &str = r"C:\Program Files\KeyDock\keydock.exe";

    #[test]
    fn parses_quoted_and_unquoted() {
        assert_eq!(command_exe(&format!("\"{PF}\" --background")).as_deref(), Some(PF));
        assert_eq!(
            command_exe(r"D:\test\keydock-x86_64.exe --background").as_deref(),
            Some(r"D:\test\keydock-x86_64.exe")
        );
        assert_eq!(
            command_exe(r"C:\Program Files\KeyDock\keydock.EXE").as_deref(),
            Some(r"C:\Program Files\KeyDock\keydock.EXE")
        );
        assert_eq!(command_exe("  "), None);
        assert_eq!(command_exe("\"\" --x"), None);
    }

    #[test]
    fn autostart_changes_only_on_explicit_choice() {
        use AutostartAction::*;
        // Review KD-10: the box was left as it was.
        assert_eq!(autostart_action(true, true, true, false), None);
        assert_eq!(autostart_action(false, false, false, false), None);
        // Turned off in Task Manager after the window opened: kept off.
        assert_eq!(autostart_action(true, true, false, false), None);
        assert_eq!(autostart_action(true, true, false, true), None);
        // The user's own changes.
        assert_eq!(autostart_action(false, true, false, false), Enable);
        assert_eq!(autostart_action(true, false, true, false), Disable);
        // Left ticked, entry starts another copy: repoint, keep the choice.
        assert_eq!(autostart_action(true, true, true, true), Repoint);
    }

    #[test]
    fn startup_approved_values() {
        assert_eq!(startup_approved(&[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]), Some(true));
        assert_eq!(startup_approved(&[0x06, 0, 0, 0]), Some(true));
        assert_eq!(startup_approved(&[0x03, 0, 0, 0, 0x10, 0x20]), Some(false));
        assert_eq!(startup_approved(&[0x07]), Some(false));
        assert_eq!(startup_approved(&[]), None);
    }

    #[test]
    fn classifies_entries() {
        assert_eq!(classify(None, PF), RunEntry::Missing);
        assert_eq!(classify(Some(""), PF), RunEntry::Missing);
        let this = format!("\"{}\" --background", PF.to_uppercase());
        assert_eq!(classify(Some(&this), PF), RunEntry::ThisExe);
        assert_eq!(classify(Some(&this), &format!("\\\\?\\{PF}")), RunEntry::ThisExe);
        assert_eq!(
            classify(Some("\"D:\\test\\KeyDock\\keydock-x86_64.exe\" --background"), PF),
            RunEntry::OtherExe(r"D:\test\KeyDock\keydock-x86_64.exe".into())
        );
        // A path that merely contains this one is another copy.
        assert_eq!(
            classify(Some(r#""D:\backup\C:\Program Files\KeyDock\keydock.exe""#), PF),
            RunEntry::OtherExe(r"D:\backup\C:\Program Files\KeyDock\keydock.exe".into())
        );
    }
}
