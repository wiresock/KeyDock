//! "Start with Windows" via `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
//!
//! The Run key needs no administrator rights and has none of the Task
//! Scheduler defaults that stop a task on battery power or after three days.
//!
//! Turning the entry off in Task Manager or Settings > Apps > Startup leaves
//! the Run value in place and records the choice under `StartupApproved`
//! (an undocumented value: KeyDock only reads its first byte, odd = off, and
//! deletes it). Only an explicit opt-in by the user ([`enable`], ticking
//! "Start with Windows") removes that record; repairing a stale path
//! ([`repair_stale`]) rewrites the command but keeps the user's choice.

use super::registry::wide;
use keydock::run_entry::{classify, startup_approved, RunEntry};
use std::path::Path;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ,
};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE: PCWSTR = w!("KeyDock");
/// Where Explorer records that a Run entry was turned off in Task Manager or
/// Settings > Apps > Startup.
const APPROVED_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run");

/// `Some(false)` when the entry is turned off in Task Manager or Settings >
/// Apps > Startup; `None` when Windows has no record (which means enabled).
pub fn approved() -> Option<bool> {
    let mut buf = [0u8; 64];
    let mut size = buf.len() as u32;
    // SAFETY: buffer size in bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            APPROVED_KEY,
            VALUE,
            RRF_RT_REG_BINARY,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    startup_approved(&buf[..(size as usize).min(buf.len())])
}

fn exe_path() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// The command line stored in the Run key: start in the background (tray only).
pub fn command() -> String {
    format!("\"{}\" --background", exe_path())
}

fn read() -> Option<String> {
    let mut buf = vec![0u16; 2048];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: buffer size in bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}

/// Where the Run entry points: nowhere, this executable, or another copy.
pub fn status() -> RunEntry {
    classify(read().as_deref(), &exe_path())
}

/// Whether "Start with Windows" is effectively on: a Run entry exists and is
/// not turned off in Windows' Startup apps.
pub fn is_on() -> bool {
    status() != RunEntry::Missing && approved() != Some(false)
}

/// At startup: an entry that points to a copy which no longer exists (for
/// example a portable copy that was replaced by the installed one) is moved
/// to this executable. An entry for another existing copy is left alone. A
/// "turned off" record in Startup apps is kept: the entry stays off.
pub fn repair_stale() {
    if let RunEntry::OtherExe(other) = status() {
        if Path::new(&other).exists() {
            info!("Start with Windows points to another copy: {other}");
        } else {
            match write_command() {
                Ok(()) => info!(
                    "Start with Windows pointed to a missing file ({other}); now points to this copy{}",
                    if approved() == Some(false) {
                        " (still turned off in Startup apps)"
                    } else {
                        ""
                    }
                ),
                Err(e) => warn!("{e}"),
            }
        }
    }
}

/// `--remove-autostart` (used by the uninstaller): removes the entry when it
/// starts this executable or a file that no longer exists.
pub fn remove_if_ours() -> Result<(), String> {
    match status() {
        RunEntry::Missing => Ok(()),
        RunEntry::OtherExe(other) if Path::new(&other).exists() => {
            info!("Start with Windows belongs to another copy ({other}); left unchanged");
            Ok(())
        }
        _ => {
            info!("removing the Start with Windows entry");
            disable()
        }
    }
}

/// Writes the Run value for this executable; leaves the Startup apps choice alone.
pub fn write_command() -> Result<(), String> {
    let data = wide(&command());
    // SAFETY: REG_SZ data including the terminating NUL; size in bytes.
    let rc = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE,
            REG_SZ.0,
            Some(data.as_ptr().cast()),
            (data.len() * 2) as u32,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(format!("cannot write the Run registry value (error {})", rc.0));
    }
    Ok(())
}

/// The user turned "Start with Windows" on: the Run value for this copy,
/// and an earlier "turned off" record in Startup apps is removed.
pub fn enable() -> Result<(), String> {
    write_command()?;
    if approved() == Some(false) {
        // SAFETY: plain value deletion; a missing record means enabled.
        let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, APPROVED_KEY, VALUE) };
        if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
            return Err(format!("cannot turn on the Startup apps entry (error {})", rc.0));
        }
    }
    Ok(())
}

/// The user turned "Start with Windows" off: removes the Run value.
pub fn disable() -> Result<(), String> {
    // SAFETY: plain value deletion.
    let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE) };
    if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
        return Err(format!("cannot remove the Run registry value (error {})", rc.0));
    }
    Ok(())
}
