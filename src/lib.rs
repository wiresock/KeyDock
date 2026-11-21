//! KeyDock: switch the available Windows keyboard layouts when an external
//! keyboard is connected or disconnected.
//!
//! This library crate holds the platform-independent logic (configuration,
//! device identification, profile selection, layout planning and KLID/HKL
//! arithmetic) so that it can be unit-tested on any host. All Win32 code lives
//! in the binary under `src/win` and is compiled only on Windows.

pub mod config;
pub mod device;
pub mod fsutil;
pub mod groups;
pub mod langorder;
pub mod layout;
pub mod reconcile;
pub mod run_entry;
pub mod schedule;
pub mod select;
pub mod settings;
pub mod state;
pub mod status;
