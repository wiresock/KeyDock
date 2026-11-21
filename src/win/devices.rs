//! Enumeration of present keyboard device interfaces via the Configuration
//! Manager API (cfgmgr32).

use keydock::device::{
    find_product, format_guid_bytes, instance_id_from_interface, is_usb_device_node, mark_mouse_containers,
    same_physical_device, split_multi_sz, usb_id_without_serial, KeyboardInfo, NodeInfo, LOCAL_MACHINE_CONTAINER,
};
use windows::core::GUID;
use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_PropertyW, CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_PropertyW, CM_Get_Parent, CM_Locate_DevNodeW, CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
    CM_LOCATE_DEVNODE_NORMAL, CONFIGRET, CR_BUFFER_SMALL, CR_SUCCESS,
};
use windows::Win32::Devices::HumanInterfaceDevice::{GUID_DEVINTERFACE_KEYBOARD, GUID_DEVINTERFACE_MOUSE};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_BusReportedDeviceDesc, DEVPKEY_Device_ContainerId, DEVPKEY_Device_HardwareIds,
    DEVPKEY_Device_InstanceId, DEVPKEY_NAME, DEVPROPTYPE, DEVPROP_TYPE_GUID, DEVPROP_TYPE_STRING,
    DEVPROP_TYPE_STRING_LIST,
};
use windows::Win32::Foundation::DEVPROPKEY;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn bytes_to_u16(b: &[u8]) -> Vec<u16> {
    b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect()
}

/// Calls a CM_Get_*_PropertyW style function twice (size query, then read).
fn read_prop(
    mut f: impl FnMut(Option<*mut u8>, &mut u32, &mut DEVPROPTYPE) -> CONFIGRET,
) -> Option<(DEVPROPTYPE, Vec<u8>)> {
    let mut ty = DEVPROPTYPE(0);
    let mut size = 0u32;
    let cr = f(None, &mut size, &mut ty);
    if cr != CR_BUFFER_SMALL || size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let cr = f(Some(buf.as_mut_ptr()), &mut size, &mut ty);
    if cr != CR_SUCCESS {
        return None;
    }
    buf.truncate(size as usize);
    Some((ty, buf))
}

fn prop_string((ty, buf): (DEVPROPTYPE, Vec<u8>)) -> Option<String> {
    (ty == DEVPROP_TYPE_STRING).then(|| {
        split_multi_sz(&bytes_to_u16(&buf))
            .into_iter()
            .next()
            .unwrap_or_default()
    })
}

fn prop_string_list((ty, buf): (DEVPROPTYPE, Vec<u8>)) -> Vec<String> {
    if ty == DEVPROP_TYPE_STRING_LIST {
        split_multi_sz(&bytes_to_u16(&buf))
    } else {
        Vec::new()
    }
}

fn prop_guid((ty, buf): (DEVPROPTYPE, Vec<u8>)) -> Option<String> {
    if ty != DEVPROP_TYPE_GUID || buf.len() != 16 {
        return None;
    }
    let mut b = [0u8; 16];
    b.copy_from_slice(&buf);
    Some(format_guid_bytes(&b))
}

fn interface_prop(iface: &[u16], key: &DEVPROPKEY) -> Option<(DEVPROPTYPE, Vec<u8>)> {
    read_prop(|buf, size, ty| {
        // SAFETY: `iface` is NUL-terminated; buffer/size pairs come from read_prop.
        unsafe { CM_Get_Device_Interface_PropertyW(PCWSTR(iface.as_ptr()), key, ty, buf, size, 0) }
    })
}

fn devnode_prop(devinst: u32, key: &DEVPROPKEY) -> Option<(DEVPROPTYPE, Vec<u8>)> {
    read_prop(|buf, size, ty| {
        // SAFETY: buffer/size pairs come from read_prop.
        unsafe { CM_Get_DevNode_PropertyW(devinst, key, ty, buf, size, 0) }
    })
}

fn bus_desc(devinst: u32) -> Option<String> {
    devnode_prop(devinst, &DEVPKEY_Device_BusReportedDeviceDesc).and_then(prop_string)
}

fn parent_of(devinst: u32) -> Option<u32> {
    let mut parent = 0u32;
    // SAFETY: valid out pointer.
    (unsafe { CM_Get_Parent(&mut parent, devinst, 0) } == CR_SUCCESS).then_some(parent)
}

fn locate(instance_id: &str) -> Option<u32> {
    let inst = wide(instance_id);
    let mut devinst = 0u32;
    // SAFETY: `inst` is NUL-terminated.
    let cr = unsafe { CM_Locate_DevNodeW(&mut devinst, PCWSTR(inst.as_ptr()), CM_LOCATE_DEVNODE_NORMAL) };
    (cr == CR_SUCCESS).then_some(devinst)
}

fn describe(path: String) -> KeyboardInfo {
    let iface = wide(&path);
    let mut kb = KeyboardInfo {
        interface_path: path,
        ..Default::default()
    };
    match interface_prop(&iface, &DEVPKEY_Device_InstanceId).and_then(prop_string) {
        Some(id) if !id.is_empty() => kb.instance_id = id,
        _ => {
            // Still identifiable from the interface path; see enumerate().
            kb.instance_id = instance_id_from_interface(&kb.interface_path).unwrap_or_default();
            kb.incomplete = Some("instance ID unreadable".into());
            return kb;
        }
    }
    let Some(devinst) = locate(&kb.instance_id) else {
        kb.incomplete = Some("device node not found".into());
        return kb;
    };
    kb.hardware_ids = devnode_prop(devinst, &DEVPKEY_Device_HardwareIds)
        .map(prop_string_list)
        .unwrap_or_default();
    kb.container_id = devnode_prop(devinst, &DEVPKEY_Device_ContainerId).and_then(prop_guid);
    if kb.container_id.is_none() {
        kb.incomplete = Some("container ID unreadable".into());
    } else if kb.hardware_ids.is_empty() && !kb.instance_id.to_ascii_uppercase().starts_with("ROOT\\") {
        kb.incomplete = Some("hardware IDs unreadable".into());
    }
    kb.name = devnode_prop(devinst, &DEVPKEY_NAME).and_then(prop_string);
    // The product string reported by the bus usually lives on the HID or USB
    // parent of the keyboard collection. Parents are only examined for
    // HID/USB/Bluetooth devices and only while they stay in the same physical
    // device (container), so ACPI keyboards never pick up the name of their
    // LPC/ISA bridge and USB keyboards never that of a hub or controller.
    let own = NodeInfo {
        bus_desc: bus_desc(devinst),
        container_id: kb.container_id.clone(),
    };
    let parents = std::iter::successors(parent_of(devinst), |&n| parent_of(n)).map(|n| NodeInfo {
        bus_desc: bus_desc(n),
        container_id: devnode_prop(n, &DEVPKEY_Device_ContainerId).and_then(prop_guid),
    });
    kb.product = find_product(&kb.instance_id, &own, parents);
    kb.container_per_port = usb_without_serial(devinst, kb.container_id.as_deref());
    kb
}

/// Whether the USB device a keyboard interface belongs to has no usable
/// serial number (see `KeyboardInfo::container_per_port`). Walks up from the
/// keyboard devnode within the same physical device to the USB device node
/// and checks its instance ID.
fn usb_without_serial(devinst: u32, container: Option<&str>) -> bool {
    if container.is_none_or(|c| keydock::device::guid_eq(c, LOCAL_MACHINE_CONTAINER)) {
        return false;
    }
    let mut node = devinst;
    for _ in 0..6 {
        let id = devnode_prop(node, &DEVPKEY_Device_InstanceId)
            .and_then(prop_string)
            .unwrap_or_default();
        if is_usb_device_node(&id) {
            return usb_id_without_serial(&id);
        }
        let Some(parent) = parent_of(node) else {
            return false;
        };
        let parent_container = devnode_prop(parent, &DEVPKEY_Device_ContainerId).and_then(prop_guid);
        if !same_physical_device(container, parent_container.as_deref()) {
            return false;
        }
        node = parent;
    }
    false
}

/// Lists the present interfaces of a device interface class.
fn interface_list(class: &GUID, what: &str) -> Result<Vec<String>, String> {
    for _ in 0..5 {
        let mut len = 0u32;
        // SAFETY: valid out pointer and GUID; no device ID filter.
        let cr = unsafe {
            CM_Get_Device_Interface_List_SizeW(&mut len, class, PCWSTR::null(), CM_GET_DEVICE_INTERFACE_LIST_PRESENT)
        };
        if cr != CR_SUCCESS {
            return Err(format!("CM_Get_Device_Interface_List_SizeW failed: CONFIGRET {}", cr.0));
        }
        let mut buf = vec![0u16; len.max(1) as usize];
        // SAFETY: buffer length is passed via the slice.
        let cr = unsafe {
            CM_Get_Device_Interface_ListW(class, PCWSTR::null(), &mut buf, CM_GET_DEVICE_INTERFACE_LIST_PRESENT)
        };
        if cr == CR_BUFFER_SMALL {
            // A device arrived between the two calls; try again.
            continue;
        }
        if cr != CR_SUCCESS {
            return Err(format!("CM_Get_Device_Interface_ListW failed: CONFIGRET {}", cr.0));
        }
        return Ok(split_multi_sz(&buf));
    }
    Err(format!("{what} interface list kept changing during enumeration"))
}

/// Container IDs of all present mouse interfaces.
fn mouse_containers() -> Vec<String> {
    let paths = interface_list(&GUID_DEVINTERFACE_MOUSE, "mouse").unwrap_or_default();
    paths
        .iter()
        .filter_map(|p| {
            let id = interface_prop(&wide(p), &DEVPKEY_Device_InstanceId).and_then(prop_string)?;
            devnode_prop(locate(&id)?, &DEVPKEY_Device_ContainerId).and_then(prop_guid)
        })
        .collect()
}

/// Lists all present `GUID_DEVINTERFACE_KEYBOARD` interfaces with their
/// properties, and flags devices that also expose a mouse interface.
///
/// An interface whose identity cannot be read completely (a device that is
/// going away or still being set up) is included with
/// `KeyboardInfo::incomplete` set: the engine decides whether the missing
/// data could change the profile (then it waits and samples again) or not
/// (then it leaves the keyboard out). Callers that only display devices
/// filter such entries with [`complete`].
pub fn enumerate() -> Result<Vec<KeyboardInfo>, String> {
    let mut keyboards: Vec<KeyboardInfo> = interface_list(&GUID_DEVINTERFACE_KEYBOARD, "keyboard")?
        .into_iter()
        .map(describe)
        .collect();
    for k in keyboards.iter().filter(|k| k.incomplete.is_some()) {
        info!(
            "keyboard interface {} is incomplete ({})",
            k.interface_path,
            k.incomplete.as_deref().unwrap_or_default()
        );
    }
    mark_mouse_containers(&mut keyboards, &mouse_containers());
    Ok(keyboards)
}

/// The keyboards whose identity could be read completely.
pub fn complete(keyboards: Vec<KeyboardInfo>) -> Vec<KeyboardInfo> {
    keyboards.into_iter().filter(|k| k.incomplete.is_none()).collect()
}
