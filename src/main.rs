#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    std::process::exit(win::run());
}

#[cfg(not(windows))]
fn main() {
    eprintln!("KeyDock runs on Windows only; on this platform only the library tests are useful (cargo test).");
    std::process::exit(1);
}
