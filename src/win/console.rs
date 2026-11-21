//! Console output for a GUI-subsystem process.

use windows::core::w;
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Console::{
    AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};

fn valid(h: HANDLE) -> bool {
    !h.is_invalid() && !h.0.is_null()
}

/// Makes `std::io::stdout()` usable. Returns true when output goes somewhere:
/// either stdout was already redirected by the parent (file or pipe), or the
/// parent's console could be attached with `AttachConsole(ATTACH_PARENT_PROCESS)`.
pub fn attach() -> bool {
    // SAFETY: plain Win32 calls with valid arguments; handles are owned by the
    // process for its lifetime once installed as standard handles.
    unsafe {
        if GetStdHandle(STD_OUTPUT_HANDLE).map(valid).unwrap_or(false) {
            return true;
        }
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return false;
        }
        if GetStdHandle(STD_OUTPUT_HANDLE).map(valid).unwrap_or(false) {
            return true;
        }
        match CreateFileW(
            w!("CONOUT$"),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        ) {
            Ok(h) => {
                let _ = SetStdHandle(STD_OUTPUT_HANDLE, h);
                let _ = SetStdHandle(STD_ERROR_HANDLE, h);
                // The shell has already printed its prompt; start on a fresh line.
                println!();
                true
            }
            Err(_) => false,
        }
    }
}
