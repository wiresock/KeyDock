//! Keyboard device descriptions and identifier parsing.

/// Container ID that Windows assigns to devices it considers part of the
/// computer itself (internal devices), as opposed to separate physical devices.
pub const LOCAL_MACHINE_CONTAINER: &str = "{00000000-0000-0000-FFFF-FFFFFFFFFFFF}";

/// One present keyboard device interface and the properties of its devnode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyboardInfo {
    /// Device interface path (symbolic link) of the `GUID_DEVINTERFACE_KEYBOARD` interface.
    pub interface_path: String,
    /// Device instance ID, e.g. `HID\VID_046D&PID_C52B&MI_00\8&...`.
    pub instance_id: String,
    /// Hardware IDs of the devnode.
    pub hardware_ids: Vec<String>,
    /// Container ID in registry format, e.g. `{8C1C...}`; groups all functions of one physical device.
    pub container_id: Option<String>,
    /// Friendly name or device description of the keyboard devnode (often generic, e.g. "HID Keyboard Device").
    pub name: Option<String>,
    /// Product name reported by the bus (USB/Bluetooth), taken from the devnode or its parents.
    pub product: Option<String>,
    /// The same physical device (non-local container) also exposes a mouse
    /// interface, e.g. a gaming mouse with programmable keys, a combo
    /// receiver, or a keyboard with a touchpad.
    pub has_mouse: bool,
    /// A USB device without a usable serial number: Windows gives it a new
    /// device instance, and a new random container ID, on every USB port, so
    /// the container ID does not identify it across ports or docks.
    pub container_per_port: bool,
    /// The interface is present but part of its identity could not be read
    /// (instance ID, device node, container ID or hardware IDs), typically
    /// while a device is still being set up after wake or a dock reset. Says
    /// what is missing. Product name and mouse flag do not count.
    pub incomplete: Option<String>,
}

/// The device instance ID encoded in a device interface path:
/// `\\?\HID#VID_046D&PID_C52B&MI_00#7&2a&0&0000#{884b96c3-...}` ->
/// `HID\VID_046D&PID_C52B&MI_00\7&2a&0&0000`. Used when the instance ID
/// property cannot be read.
pub fn instance_id_from_interface(path: &str) -> Option<String> {
    let rest = path.strip_prefix("\\\\?\\").or_else(|| path.strip_prefix("\\??\\"))?;
    let end = rest.rfind("#{")?;
    let id = rest[..end].replace('#', "\\");
    (!id.is_empty()).then_some(id)
}

/// True for the instance ID of a USB device node itself (not an interface of
/// a composite device): `USB\VID_046D&PID_C31C\...` but not `...&MI_00\...`.
pub fn is_usb_device_node(instance_id: &str) -> bool {
    let up = instance_id.to_ascii_uppercase();
    up.starts_with("USB\\") && !up.contains("&MI_")
}

/// True when Windows generated the last part of a USB device instance ID
/// because the device reports no usable serial number
/// (`USB\VID_046D&PID_C31C\7&2A1B3C4D&0&2`, port-specific), rather than
/// using the serial (`USB\VID_046D&PID_C31C\ABC123`).
pub fn usb_id_without_serial(instance_id: &str) -> bool {
    is_usb_device_node(instance_id) && instance_id.rsplit('\\').next().is_some_and(|s| s.contains('&'))
}

/// Bus-reported description and container of one devnode, used while looking
/// for a keyboard's product name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeInfo {
    pub bus_desc: Option<String>,
    pub container_id: Option<String>,
}

fn is_local_or_missing(container: Option<&str>) -> bool {
    container.is_none_or(|c| guid_eq(c, LOCAL_MACHINE_CONTAINER))
}

/// Whether the product name may be looked up on the parents of a keyboard
/// devnode: only for HID, USB and Bluetooth devices that form their own
/// physical device. ACPI (PS/2) keyboards and internal (local-machine
/// container) devices never are, because their parents are buses and bridges.
pub fn may_walk_parents(instance_id: &str, container_id: Option<&str>) -> bool {
    const BUSES: [&str; 4] = ["HID\\", "USB\\", "BTHENUM\\", "BTHLEDEVICE\\"];
    let id = instance_id.to_ascii_uppercase();
    BUSES.iter().any(|b| id.starts_with(b)) && !is_local_or_missing(container_id)
}

/// True when a parent devnode still belongs to the same physical device.
pub fn same_physical_device(child: Option<&str>, parent: Option<&str>) -> bool {
    match (child, parent) {
        (Some(c), Some(p)) => !is_local_or_missing(Some(c)) && guid_eq(c, p),
        _ => false,
    }
}

/// Maximum number of parent devnodes examined for the product name.
pub const MAX_PARENT_WALK: usize = 4;

/// Finds the bus-reported product name of a keyboard: on its own devnode, or
/// (for HID/USB/Bluetooth devices only) on parents in the same container.
/// `parents` is evaluated lazily, nearest parent first.
pub fn find_product(instance_id: &str, node: &NodeInfo, parents: impl IntoIterator<Item = NodeInfo>) -> Option<String> {
    let usable = |d: &Option<String>| d.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    if let Some(p) = usable(&node.bus_desc) {
        return Some(p);
    }
    let container = node.container_id.as_deref();
    if !may_walk_parents(instance_id, container) {
        return None;
    }
    for parent in parents.into_iter().take(MAX_PARENT_WALK) {
        if !same_physical_device(container, parent.container_id.as_deref()) {
            return None;
        }
        if let Some(p) = usable(&parent.bus_desc) {
            return Some(p);
        }
    }
    None
}

/// Sets `has_mouse` on keyboards whose (non-local) container also has a mouse
/// interface. The local-machine container is shared by all internal devices
/// (built-in keyboard and touchpad), so it never counts.
pub fn mark_mouse_containers(keyboards: &mut [KeyboardInfo], mouse_containers: &[String]) {
    for kb in keyboards {
        kb.has_mouse = match kb.container_id.as_deref() {
            Some(c) if !is_local_or_missing(Some(c)) => mouse_containers.iter().any(|m| guid_eq(m, c)),
            _ => false,
        };
    }
}

/// How KeyDock classifies a keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardKind {
    /// Remote-desktop or other software keyboard; ignored completely.
    Virtual,
    /// Excluded by an `ignore` rule in the configuration.
    Ignored,
    /// Part of the computer (remembered on first run, or recognised by heuristics).
    BuiltIn,
    /// Everything else.
    External,
}

impl KeyboardKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            KeyboardKind::Virtual => "virtual (ignored)",
            KeyboardKind::Ignored => "ignored by config",
            KeyboardKind::BuiltIn => "built-in",
            KeyboardKind::External => "external",
        }
    }
}

impl KeyboardInfo {
    /// All identifiers that may carry a VID/PID: hardware IDs first, then the instance ID.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.hardware_ids
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(self.instance_id.as_str()))
    }

    /// The most descriptive name available: the bus-reported product name, then the device name.
    pub fn display_name(&self) -> String {
        self.product
            .as_deref()
            .or(self.name.as_deref())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("Keyboard")
            .to_string()
    }

    /// The first VID/PID pair found in the hardware IDs or the instance ID.
    pub fn vid_pid(&self) -> Option<(u16, u16)> {
        self.ids().find_map(parse_vid_pid)
    }

    /// True for keyboards that are not physical devices: Remote Desktop
    /// (`TERMINPUT_BUS`, `RDP_KBD`) and root-enumerated software devices.
    pub fn is_virtual(&self) -> bool {
        let id = self.instance_id.to_ascii_uppercase();
        id.starts_with("ROOT\\")
            || id.contains("TERMINPUT")
            || id.contains("RDP_KBD")
            || self.hardware_ids.iter().any(|h| {
                let h = h.to_ascii_uppercase();
                h.contains("TERMINPUT") || h.contains("RDP_KBD")
            })
    }

    /// Heuristic for keyboards that are part of the computer: ACPI-enumerated
    /// (PS/2 / i8042) keyboards and devices in the local-machine container.
    pub fn looks_built_in(&self) -> bool {
        self.instance_id.to_ascii_uppercase().starts_with("ACPI\\")
            || self
                .container_id
                .as_deref()
                .is_some_and(|c| guid_eq(c, LOCAL_MACHINE_CONTAINER))
    }
}

fn hex_run(s: &[u8]) -> &[u8] {
    let n = s.iter().take_while(|c| c.is_ascii_hexdigit()).count();
    &s[..n]
}

fn hex_u16_tail(digits: &[u8]) -> Option<u16> {
    // Accept 4 hex digits (USB `VID_046D`) or longer forms whose last four
    // digits are the ID (Bluetooth `VID&0002046D`, BLE `VID&02046D`).
    if digits.len() < 4 || digits.len() > 8 {
        return None;
    }
    let tail = std::str::from_utf8(&digits[digits.len() - 4..]).ok()?;
    u16::from_str_radix(tail, 16).ok()
}

fn find_field(upper: &[u8], key: &[u8]) -> Option<u16> {
    // key is e.g. b"VID"; accept "VID_" and "VID&" separators.
    let mut i = 0;
    while i + key.len() < upper.len() {
        if &upper[i..i + key.len()] == key {
            let sep = upper[i + key.len()];
            if sep == b'_' || sep == b'&' {
                let digits = hex_run(&upper[i + key.len() + 1..]);
                if let Some(v) = hex_u16_tail(digits) {
                    return Some(v);
                }
            }
        }
        i += 1;
    }
    None
}

/// Extracts the vendor and product ID from a hardware or instance ID.
///
/// Supported forms (case-insensitive):
/// * USB / USB HID: `USB\VID_046D&PID_C52B`, `HID\VID_046D&PID_C52B&MI_00`
/// * Bluetooth classic HID: `HID\{00001124-0000-1000-8000-00805F9B34FB}_VID&0002046D_PID&B35F`
/// * Bluetooth LE HID: `HID\{00001812-0000-1000-8000-00805F9B34FB}_DEV_VID&02046D_PID&B35F_REV&0001`
pub fn parse_vid_pid(id: &str) -> Option<(u16, u16)> {
    let upper = id.to_ascii_uppercase();
    let b = upper.as_bytes();
    let vid = find_field(b, b"VID")?;
    let pid = find_field(b, b"PID")?;
    Some((vid, pid))
}

/// Parses a 4-digit hex VID or PID from the configuration (`"046D"`, `"0x046d"`).
pub fn parse_id16(s: &str) -> Option<u16> {
    let t = s.trim();
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    if t.is_empty() || t.len() > 4 {
        return None;
    }
    u16::from_str_radix(t, 16).ok()
}

/// Normalises a GUID string for comparison: upper case, no braces or spaces.
pub fn normalize_guid(s: &str) -> String {
    s.trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .to_ascii_uppercase()
}

pub fn guid_eq(a: &str, b: &str) -> bool {
    normalize_guid(a) == normalize_guid(b)
}

/// Formats a GUID from its 16-byte in-memory (little-endian Data1..3) layout
/// as `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`.
pub fn format_guid_bytes(b: &[u8; 16]) -> String {
    let d1 = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let d2 = u16::from_le_bytes([b[4], b[5]]);
    let d3 = u16::from_le_bytes([b[6], b[7]]);
    format!(
        "{{{d1:08X}-{d2:04X}-{d3:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// Splits a REG_MULTI_SZ style UTF-16 buffer into strings.
pub fn split_multi_sz(buf: &[u16]) -> Vec<String> {
    buf.split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OMEN: &str = "{6D1A0F2E-1111-2222-3333-444455556666}";

    fn node(desc: Option<&str>, container: Option<&str>) -> NodeInfo {
        NodeInfo {
            bus_desc: desc.map(Into::into),
            container_id: container.map(Into::into),
        }
    }

    #[test]
    fn instance_id_from_interface_path() {
        assert_eq!(
            instance_id_from_interface(
                "\\\\?\\HID#VID_046D&PID_C52B&MI_00#7&2a1b&0&0000#{884b96c3-56ef-11d1-bc8c-00a0c91405dd}"
            )
            .as_deref(),
            Some("HID\\VID_046D&PID_C52B&MI_00\\7&2a1b&0&0000")
        );
        assert_eq!(
            instance_id_from_interface("\\\\?\\ACPI#MSF0001#4&1&0#{884b96c3-56ef-11d1-bc8c-00a0c91405dd}\\KBD")
                .as_deref(),
            Some("ACPI\\MSF0001\\4&1&0")
        );
        assert_eq!(instance_id_from_interface("garbage"), None);
    }

    #[test]
    fn acpi_keyboard_never_walks_to_bridge() {
        let own = node(None, Some(LOCAL_MACHINE_CONTAINER));
        let parents = vec![node(Some("PCI to ISA Bridge"), Some(LOCAL_MACHINE_CONTAINER))];
        assert_eq!(find_product("ACPI\\MSFT0001\\4&1cdef223&0", &own, parents), None);
        let kb = KeyboardInfo {
            instance_id: "ACPI\\MSFT0001\\4&1cdef223&0".into(),
            name: Some("Standard PS/2 Keyboard".into()),
            product: None,
            ..Default::default()
        };
        assert_eq!(kb.display_name(), "Standard PS/2 Keyboard");
        assert!(!may_walk_parents("ACPI\\MSFT0001\\4&1&0", Some(OMEN)));
    }

    #[test]
    fn hid_keyboard_uses_same_container_parent() {
        let own = node(None, Some(OMEN));
        let parents = vec![
            node(None, Some(&OMEN.to_lowercase())),
            node(Some("OMEN Encoder"), Some(OMEN)),
            node(Some("Generic USB Hub"), Some(LOCAL_MACHINE_CONTAINER)),
        ];
        assert_eq!(
            find_product("HID\\VID_03F0&PID_3341&MI_00\\8&1&0&0000", &own, parents).as_deref(),
            Some("OMEN Encoder")
        );
        let parents = vec![node(Some("OMEN Reactor"), Some(OMEN))];
        assert_eq!(
            find_product("HID\\VID_03F0&PID_2041&MI_01&COL01\\8&2&0&0000", &own, parents).as_deref(),
            Some("OMEN Reactor")
        );
    }

    #[test]
    fn walk_stops_at_container_boundary() {
        let own = node(None, Some(OMEN));
        // The parent is a hub/controller of the computer: stop, do not take its name.
        let parents = vec![node(Some("USB Root Hub (USB 3.0)"), Some(LOCAL_MACHINE_CONTAINER))];
        assert_eq!(find_product("USB\\VID_03F0&PID_3341\\1", &own, parents), None);
        let parents = vec![node(
            Some("Other device"),
            Some("{11111111-0000-0000-0000-000000000000}"),
        )];
        assert_eq!(find_product("HID\\VID_03F0&PID_3341\\1", &own, parents), None);
        let parents = vec![node(Some("No container"), None)];
        assert_eq!(find_product("BTHENUM\\x", &own, parents), None);
        // HID device in the local-machine container (e.g. an internal I2C keyboard): no walk.
        let local = node(None, Some(LOCAL_MACHINE_CONTAINER));
        let parents = vec![node(
            Some("Intel Serial IO I2C Host Controller"),
            Some(LOCAL_MACHINE_CONTAINER),
        )];
        assert_eq!(find_product("HID\\VEN_ELAN&DEV_0001\\1", &local, parents), None);
        // Own bus-reported name always wins; blank names are skipped.
        assert_eq!(
            find_product("ACPI\\X\\1", &node(Some("Own"), None), vec![]).as_deref(),
            Some("Own")
        );
        let parents = vec![node(Some("  "), Some(OMEN)), node(Some("Real"), Some(OMEN))];
        assert_eq!(find_product("HID\\X\\1", &own, parents).as_deref(), Some("Real"));
    }

    #[test]
    fn walk_is_bounded() {
        let own = node(None, Some(OMEN));
        let mut parents = vec![node(None, Some(OMEN)); MAX_PARENT_WALK];
        parents.push(node(Some("Too far"), Some(OMEN)));
        assert_eq!(find_product("HID\\X\\1", &own, parents), None);
        assert!(may_walk_parents("bthledevice\\{x}", Some(OMEN)));
        assert!(!may_walk_parents("HID\\X", None));
    }

    #[test]
    fn mouse_containers_mark_only_external_devices() {
        let mut kbs = vec![
            KeyboardInfo {
                instance_id: "ACPI\\MSFT0001\\1".into(),
                container_id: Some(LOCAL_MACHINE_CONTAINER.into()),
                ..Default::default()
            },
            KeyboardInfo {
                instance_id: "HID\\VID_03F0&PID_2041&MI_01\\1".into(),
                container_id: Some(OMEN.into()),
                ..Default::default()
            },
            KeyboardInfo {
                instance_id: "HID\\VID_03F0&PID_3341&MI_00\\1".into(),
                container_id: Some("{22222222-0000-0000-0000-000000000000}".into()),
                ..Default::default()
            },
            KeyboardInfo {
                instance_id: "HID\\NOCONTAINER\\1".into(),
                ..Default::default()
            },
        ];
        // The touchpad lives in the local-machine container too.
        let mice = vec![
            LOCAL_MACHINE_CONTAINER.to_lowercase(),
            OMEN.trim_matches(['{', '}']).to_lowercase(),
        ];
        mark_mouse_containers(&mut kbs, &mice);
        assert_eq!(
            kbs.iter().map(|k| k.has_mouse).collect::<Vec<_>>(),
            [false, true, false, false]
        );
    }

    #[test]
    fn usb_serial_detection() {
        assert!(usb_id_without_serial("USB\\VID_046D&PID_C31C\\7&2A1B3C4D&0&2"));
        assert!(!usb_id_without_serial("USB\\VID_046D&PID_C31C\\ABC123"));
        // A composite device's interface node is not the device node.
        assert!(!is_usb_device_node("USB\\VID_046D&PID_C52B&MI_00\\7&1&0&0000"));
        assert!(!usb_id_without_serial("USB\\VID_046D&PID_C52B&MI_00\\7&1&0&0000"));
        assert!(is_usb_device_node("usb\\vid_046d&pid_c52b\\6&1"));
        assert!(!usb_id_without_serial("HID\\VID_046D&PID_C31C\\8&1&0&0000"));
        assert!(!usb_id_without_serial(
            "BTHENUM\\{00001124-0000-1000-8000-00805f9b34fb}\\7&1"
        ));
    }

    #[test]
    fn usb_formats() {
        assert_eq!(parse_vid_pid("USB\\VID_046D&PID_C52B&REV_1201"), Some((0x046D, 0xC52B)));
        assert_eq!(parse_vid_pid("HID\\VID_046D&PID_C52B&MI_00"), Some((0x046D, 0xC52B)));
        assert_eq!(
            parse_vid_pid("hid\\vid_05ac&pid_024f&mi_00&col01"),
            Some((0x05AC, 0x024F))
        );
    }

    #[test]
    fn bluetooth_formats() {
        // Classic Bluetooth HID: vendor ID source 0002 (USB-IF) + VID.
        assert_eq!(
            parse_vid_pid("HID\\{00001124-0000-1000-8000-00805f9b34fb}_VID&0002046d_PID&b35f"),
            Some((0x046D, 0xB35F))
        );
        // Bluetooth LE HID (HOGP).
        assert_eq!(
            parse_vid_pid("HID\\{00001812-0000-1000-8000-00805f9b34fb}_Dev_VID&02046d_PID&b35f_REV&0006"),
            Some((0x046D, 0xB35F))
        );
    }

    #[test]
    fn no_vid_pid() {
        assert_eq!(parse_vid_pid("ACPI\\MSF0001\\4&1234&0"), None);
        assert_eq!(parse_vid_pid("HID\\VID_046D"), None);
        assert_eq!(parse_vid_pid("TERMINPUT_BUS\\UMB\\2&1234"), None);
        // Too few hex digits.
        assert_eq!(parse_vid_pid("HID\\VID_04&PID_12"), None);
    }

    #[test]
    fn classify() {
        let rdp = KeyboardInfo {
            instance_id: "TERMINPUT_BUS\\UMB\\2&1c2b3a&0&RDP_KBD".into(),
            ..Default::default()
        };
        assert!(rdp.is_virtual());
        let root = KeyboardInfo {
            instance_id: "ROOT\\RDP_KBD\\0000".into(),
            ..Default::default()
        };
        assert!(root.is_virtual());
        let ps2 = KeyboardInfo {
            instance_id: "ACPI\\MSF0001\\4&2a&0".into(),
            ..Default::default()
        };
        assert!(!ps2.is_virtual());
        assert!(ps2.looks_built_in());
        let internal_hid = KeyboardInfo {
            instance_id: "HID\\VID_06CB&PID_CE2D&MI_00\\8&1&0&0000".into(),
            container_id: Some("{00000000-0000-0000-ffff-ffffffffffff}".into()),
            ..Default::default()
        };
        assert!(internal_hid.looks_built_in());
        let ext = KeyboardInfo {
            instance_id: "HID\\VID_046D&PID_C52B&MI_00\\8&2&0&0000".into(),
            container_id: Some("{8C1C3E4A-1111-2222-3333-444455556666}".into()),
            ..Default::default()
        };
        assert!(!ext.looks_built_in());
        assert_eq!(ext.vid_pid(), Some((0x046D, 0xC52B)));
    }

    #[test]
    fn guid_helpers() {
        let bytes = [
            0xc3, 0x96, 0x4b, 0x88, 0xef, 0x56, 0xd1, 0x11, 0xbc, 0x8c, 0x00, 0xa0, 0xc9, 0x14, 0x05, 0xdd,
        ];
        assert_eq!(format_guid_bytes(&bytes), "{884B96C3-56EF-11D1-BC8C-00A0C91405DD}");
        assert!(guid_eq(
            "{884b96c3-56ef-11d1-bc8c-00a0c91405dd}",
            "884B96C3-56EF-11D1-BC8C-00A0C91405DD"
        ));
    }

    #[test]
    fn multi_sz() {
        let s: Vec<u16> = "A\0BC\0\0".encode_utf16().collect();
        assert_eq!(split_multi_sz(&s), vec!["A".to_string(), "BC".to_string()]);
    }

    #[test]
    fn id16() {
        assert_eq!(parse_id16("046d"), Some(0x046D));
        assert_eq!(parse_id16("0x046D"), Some(0x046D));
        assert_eq!(parse_id16("12345"), None);
        assert_eq!(parse_id16(""), None);
    }
}
