//! Persistent state (`%APPDATA%\KeyDock\state.toml`): the keyboards that were
//! present on first run and are therefore treated as built-in.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Device instance IDs of the built-in keyboards.
    #[serde(default)]
    pub builtin_instance_ids: Vec<String>,
}

impl State {
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| format!("state.toml: {e}"))
    }

    pub fn to_toml(&self) -> String {
        let body = toml::to_string(self).unwrap_or_default();
        format!(
            "# Written by KeyDock. Keyboards listed here count as built-in for `any_external`.\n\
             # Delete this file (or run `keydock.exe --remember-builtin` with only the\n\
             # built-in keyboard present) to record them again.\n{body}"
        )
    }

    pub fn is_builtin(&self, instance_id: &str) -> bool {
        self.builtin_instance_ids
            .iter()
            .any(|i| i.eq_ignore_ascii_case(instance_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let s = State {
            builtin_instance_ids: vec!["ACPI\\MSF0001\\4&1&0".into(), "HID\\VID_06CB&PID_CE2D\\8&1".into()],
        };
        let t = s.to_toml();
        assert_eq!(State::parse(&t).unwrap(), s);
        assert!(s.is_builtin("acpi\\msf0001\\4&1&0"));
        assert!(!s.is_builtin("HID\\VID_046D"));
        assert_eq!(State::parse("").unwrap(), State::default());
    }
}
