//! The settings window: plain Win32 controls (common controls v6 through the
//! manifest), per-monitor DPI aware, system colours only so it follows the
//! light/dark/high-contrast system settings that classic controls support.

use super::catalog::Catalog;
use super::registry::wide;
use super::ui::{self, Layout};
use super::window::{cached_icon, with_app, Manual, IDI_APP};
use super::{autostart, devices};
use keydock::config::{Config, Matcher};
use keydock::device::KeyboardKind;
use keydock::fsutil::{read_revision, write_atomic_if_with_backup, Revision, WriteError};
use keydock::groups::{group_keyboards, model_scope_note, set_group_mark_scoped, DeviceGroup, Mark};
use keydock::layout::LayoutSpec;
use keydock::run_entry::{autostart_action, AutostartAction, RunEntry};
use keydock::settings::{other_profiles, save_conflict, SettingsModel};
use keydock::state::State;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetSysColorBrush, COLOR_BTNFACE, HFONT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_LISTVIEW_CLASSES, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, LVIF_STATE,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVN_ITEMCHANGED, LVS_EX_CHECKBOXES, LVS_EX_DOUBLEBUFFER, LVS_EX_FULLROWSELECT,
    LVS_EX_LABELTIP, LVS_NOCOLUMNHEADER, LVS_REPORT, LVS_SHOWSELALWAYS, LVS_SINGLESEL, NMHDR, NMLISTVIEW, WC_BUTTONW,
    WC_COMBOBOXW, WC_EDITW, WC_LISTVIEWW, WC_STATICW,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, LoadCursorW, MessageBoxW, RegisterClassW, SendMessageW,
    SetForegroundWindow, SetWindowPos, ShowWindow, BS_AUTOCHECKBOX, BS_AUTORADIOBUTTON, BS_DEFPUSHBUTTON, BS_GROUPBOX,
    BS_PUSHBUTTON, CBS_DROPDOWNLIST, CW_USEDEFAULT, ES_AUTOHSCROLL, ES_NUMBER, ICON_BIG, ICON_SMALL, IDC_ARROW, IDYES,
    MB_DEFBUTTON2, MB_ICONWARNING, MB_OK, MB_YESNO, SM_CXICON, SM_CXSMICON, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER,
    SW_RESTORE, SW_SHOW, WINDOW_EX_STYLE, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_DPICHANGED, WM_NOTIFY, WM_SETICON,
    WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_EX_CLIENTEDGE, WS_EX_CONTROLPARENT, WS_GROUP, WS_MINIMIZEBOX,
    WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};

/// SS_ENDELLIPSIS (static control style).
const SS_ENDELLIPSIS: u32 = 0x4000;
const CLASS: PCWSTR = w!("KeyDockSettingsWindow");
const CLIENT_W: i32 = 820;
const CLIENT_H: i32 = 712;

const ID_KB_LIST: i32 = 1001;
const ID_MARK_BUILTIN: i32 = 1002;
const ID_MARK_EXTERNAL: i32 = 1003;
const ID_MARK_IGNORE: i32 = 1004;
const ID_MARK_AUTO: i32 = 1005;
const ID_PORT_ONLY: i32 = 1006;
const ID_LAP_LIST: i32 = 1010;
const ID_LAP_COMBO: i32 = 1011;
const ID_EXT_ANY: i32 = 1020;
const ID_EXT_ONLY: i32 = 1021;
const ID_EXT_DEVICES: i32 = 1022;
const ID_EXT_LIST: i32 = 1023;
const ID_EXT_COMBO: i32 = 1024;
const ID_ADD_LANG: i32 = 1030;
const ID_ADD_LAYOUT: i32 = 1031;
const ID_ADD: i32 = 1032;
const ID_AUTOSTART: i32 = 1040;
const ID_PAUSED: i32 = 1041;
const ID_NOTIFY: i32 = 1042;
const ID_DEBOUNCE: i32 = 1043;
const ID_RESUME: i32 = 1044;
const ID_SAVE: i32 = 1050;
const ID_CLOSE: i32 = 1051;
const ID_APPLY: i32 = 1052;

/// Column widths of the keyboard list (96-DPI units).
const KB_COLS: [(&str, i32); 4] = [("Keyboard", 220), ("VID:PID", 76), ("Type", 250), ("Interfaces", 70)];

/// What the settings window needs from the running app.
pub struct OpenArgs {
    pub cfg: Config,
    pub state: State,
    pub paused: bool,
    pub config_path: PathBuf,
    /// Why config.toml could not be used, if it cannot.
    pub config_error: Option<String>,
}

struct Controls {
    kb_list: HWND,
    port_only: HWND,
    lap_list: HWND,
    lap_combo: HWND,
    ext_any: HWND,
    ext_only: HWND,
    ext_devices: HWND,
    ext_list: HWND,
    ext_combo: HWND,
    add_lang: HWND,
    add_layout: HWND,
    autostart: HWND,
    paused: HWND,
    notify: HWND,
    debounce: HWND,
    resume: HWND,
    other: HWND,
    status: HWND,
}

struct SettingsWin {
    hwnd: HWND,
    layout: Layout,
    font: HFONT,
    dpi: u32,
    c: Controls,
    catalog: Catalog,
    /// Working copy: device marks and everything the window does not show.
    work: Config,
    state: State,
    config_path: PathBuf,
    config_error: Option<String>,
    /// config.toml as it was on disk when the window opened (or last saved
    /// it), to notice edits and deletion meanwhile, e.g. in a text editor.
    disk_snapshot: Revision,
    /// State of "Start with Windows" as shown when the window opened.
    autostart_initial: bool,
    /// Rows of both layout lists (same order in both).
    known: Vec<LayoutSpec>,
    /// Rows of the keyboard list.
    groups: Vec<DeviceGroup>,
    /// Rows of the "only these keyboards" list.
    ext_rows: Vec<Matcher>,
}

thread_local! {
    static SETTINGS: RefCell<Option<SettingsWin>> = const { RefCell::new(None) };
    /// The settings window while it exists. Kept apart from `SETTINGS`, which
    /// is borrowed while a handler runs (also while one shows a message box,
    /// whose modal loop can deliver "open Settings" again): the window must
    /// still be found then, or a second one would be created.
    static SETTINGS_HWND: Cell<Option<HWND>> = const { Cell::new(None) };
    /// Each Settings window gets the next number; the open one's is kept
    /// while it exists. A Save/Apply result is shown only in the window that
    /// submitted it (review: a reopened window is another session).
    static SESSIONS: Cell<u64> = const { Cell::new(0) };
    static OPEN_SESSION: Cell<Option<u64>> = const { Cell::new(None) };
}

/// The session number of the open Settings window, if one is open.
pub fn session() -> Option<u64> {
    OPEN_SESSION.with(Cell::get)
}

/// Whether a result for session `target` may be shown while `open` is the
/// open window's session.
pub(crate) fn is_recipient(open: Option<u64>, target: u64) -> bool {
    open == Some(target)
}

fn with_settings<R>(f: impl FnOnce(&mut SettingsWin) -> R) -> Option<R> {
    SETTINGS.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

pub fn hwnd() -> Option<HWND> {
    SETTINGS_HWND.with(Cell::get)
}

fn message(hwnd: HWND, text: &str) {
    let t = wide(text);
    // SAFETY: NUL-terminated strings.
    unsafe {
        MessageBoxW(Some(hwnd), PCWSTR(t.as_ptr()), w!("KeyDock"), MB_OK | MB_ICONWARNING);
    }
}

/// Yes/No question (No is the default); true for Yes.
fn confirm(hwnd: HWND, text: &str) -> bool {
    let t = wide(text);
    // SAFETY: NUL-terminated strings.
    unsafe {
        MessageBoxW(
            Some(hwnd),
            PCWSTR(t.as_ptr()),
            w!("KeyDock"),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
        ) == IDYES
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

impl SettingsWin {
    fn create_controls(&mut self) {
        let l = &mut self.layout;
        let child = WS_CHILD.0 | WS_VISIBLE.0;
        let tab = child | WS_TABSTOP.0;
        let button = |l: &mut Layout, text: &str, id: i32, x: i32, y: i32, w: i32, h: i32| {
            l.add(WC_BUTTONW, text, tab | BS_PUSHBUTTON as u32, 0, id, x, y, w, h)
        };
        let label = |l: &mut Layout, text: &str, x: i32, y: i32, w: i32, h: i32| {
            l.add(WC_STATICW, text, child | SS_ENDELLIPSIS, 0, -1, x, y, w, h)
        };
        let lv = |l: &mut Layout, id: i32, header: bool, x: i32, y: i32, w: i32, h: i32| {
            let mut style = tab | LVS_REPORT | LVS_SHOWSELALWAYS | WS_BORDER.0;
            if !header {
                style |= LVS_NOCOLUMNHEADER;
            }
            if id == ID_KB_LIST {
                style |= LVS_SINGLESEL;
            }
            l.add(WC_LISTVIEWW, "", style, 0, id, x, y, w, h)
        };
        let group = |l: &mut Layout, text: &str, x: i32, y: i32, w: i32, h: i32| {
            l.add(WC_BUTTONW, text, child | BS_GROUPBOX as u32, 0, -1, x, y, w, h)
        };
        let combo = |l: &mut Layout, id: i32, x: i32, y: i32, w: i32| {
            l.add(
                WC_COMBOBOXW,
                "",
                tab | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32,
                0,
                id,
                x,
                y,
                w,
                320,
            )
        };

        group(l, "Connected keyboards", 12, 8, 796, 186);
        group(l, "Laptop only (no external keyboard)", 12, 202, 392, 330);
        group(l, "External keyboard connected", 416, 202, 392, 330);
        group(l, "Add a layout to the lists", 12, 540, 796, 56);
        group(l, "Options", 12, 602, 796, 58);

        let kb_list = lv(l, ID_KB_LIST, true, 24, 30, 640, 152);
        button(l, "Built-in", ID_MARK_BUILTIN, 676, 30, 120, 28);
        button(l, "External", ID_MARK_EXTERNAL, 676, 64, 120, 28);
        button(l, "Ignore", ID_MARK_IGNORE, 676, 98, 120, 28);
        button(l, "Automatic", ID_MARK_AUTO, 676, 132, 120, 28);
        let port_only = l.add(
            WC_BUTTONW,
            "This port only",
            tab | BS_AUTOCHECKBOX as u32,
            0,
            ID_PORT_ONLY,
            676,
            164,
            124,
            22,
        );

        label(l, "Keep these layouts:", 24, 224, 368, 18);
        let lap_list = lv(l, ID_LAP_LIST, false, 24, 244, 368, 236);
        label(l, "Switch to:", 24, 492, 78, 20);
        let lap_combo = combo(l, ID_LAP_COMBO, 104, 488, 288);

        let ext_any = l.add(
            WC_BUTTONW,
            "Any external keyboard",
            tab | WS_GROUP.0 | BS_AUTORADIOBUTTON as u32,
            0,
            ID_EXT_ANY,
            428,
            222,
            368,
            20,
        );
        let ext_only = l.add(
            WC_BUTTONW,
            "Only these keyboards:",
            child | BS_AUTORADIOBUTTON as u32,
            0,
            ID_EXT_ONLY,
            428,
            244,
            368,
            20,
        );
        let ext_devices = lv(l, ID_EXT_DEVICES, false, 428, 266, 368, 64);
        label(l, "Keep these layouts:", 428, 336, 368, 18);
        let ext_list = lv(l, ID_EXT_LIST, false, 428, 356, 368, 124);
        label(l, "Switch to:", 428, 492, 78, 20);
        let ext_combo = combo(l, ID_EXT_COMBO, 508, 488, 288);

        label(l, "Language:", 24, 562, 78, 20);
        let add_lang = combo(l, ID_ADD_LANG, 104, 558, 250);
        let add_layout = combo(l, ID_ADD_LAYOUT, 362, 558, 330);
        button(l, "Add", ID_ADD, 700, 557, 96, 26);

        let autostart = l.add(
            WC_BUTTONW,
            "Start with Windows",
            tab | BS_AUTOCHECKBOX as u32,
            0,
            ID_AUTOSTART,
            24,
            624,
            150,
            22,
        );
        let paused = l.add(
            WC_BUTTONW,
            "Paused",
            tab | BS_AUTOCHECKBOX as u32,
            0,
            ID_PAUSED,
            180,
            624,
            100,
            22,
        );
        let notify = l.add(
            WC_BUTTONW,
            "Notify when the mode changes",
            tab | BS_AUTOCHECKBOX as u32,
            0,
            ID_NOTIFY,
            284,
            624,
            210,
            22,
        );
        label(l, "Delay, ms:", 498, 627, 66, 20);
        let edit_style = tab | ES_NUMBER as u32 | ES_AUTOHSCROLL as u32;
        let debounce = l.add(
            WC_EDITW,
            "",
            edit_style,
            WS_EX_CLIENTEDGE.0,
            ID_DEBOUNCE,
            566,
            623,
            58,
            23,
        );
        label(l, "after resume:", 632, 627, 90, 20);
        let resume = l.add(
            WC_EDITW,
            "",
            edit_style,
            WS_EX_CLIENTEDGE.0,
            ID_RESUME,
            726,
            623,
            70,
            23,
        );

        let other = label(l, "", 12, 668, 484, 18);
        let status = label(l, "", 12, 688, 484, 18);
        l.add(
            WC_BUTTONW,
            "Save",
            tab | BS_DEFPUSHBUTTON as u32,
            0,
            ID_SAVE,
            508,
            670,
            92,
            30,
        );
        button(l, "Apply", ID_APPLY, 612, 670, 92, 30);
        button(l, "Close", ID_CLOSE, 716, 670, 92, 30);

        let ext_style = LVS_EX_CHECKBOXES | LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP;
        for (h, ex) in [
            (kb_list, LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP),
            (lap_list, ext_style),
            (ext_list, ext_style),
            (ext_devices, ext_style),
        ] {
            // SAFETY: our own list views.
            unsafe {
                SendMessageW(
                    h,
                    LVM_SETEXTENDEDLISTVIEWSTYLE,
                    Some(WPARAM(ex as usize)),
                    Some(LPARAM(ex as isize)),
                );
            }
        }
        for (i, (name, w)) in KB_COLS.iter().enumerate() {
            ui::lv_add_column(kb_list, i as i32, name, *w);
        }
        for h in [lap_list, ext_list, ext_devices] {
            ui::lv_add_column(h, 0, "", 340);
        }
        self.c = Controls {
            kb_list,
            port_only,
            lap_list,
            lap_combo,
            ext_any,
            ext_only,
            ext_devices,
            ext_list,
            ext_combo,
            add_lang,
            add_layout,
            autostart,
            paused,
            notify,
            debounce,
            resume,
            other,
            status,
        };
    }

    /// Positions controls, fonts and column widths for the current DPI.
    fn relayout(&mut self) {
        let old = self.font;
        self.font = ui::message_font(self.dpi);
        self.layout.apply(self.dpi, self.font);
        ui::delete_font(old);
        for (i, (_, w)) in KB_COLS.iter().enumerate() {
            ui::lv_set_column_width(self.c.kb_list, i as i32, ui::scale(*w, self.dpi));
        }
        for h in [self.c.lap_list, self.c.ext_list, self.c.ext_devices] {
            ui::lv_set_column_width(h, 0, ui::scale(340, self.dpi));
        }
    }

    fn populate(&mut self, model: &SettingsModel, paused: bool) {
        // Known layouts: configured, user language list, loaded now.
        let loaded = self.catalog.loaded_specs();
        let mut known: Vec<LayoutSpec> = Vec::new();
        for s in model
            .laptop
            .layouts
            .iter()
            .chain(&model.external.layouts)
            .chain(&self.work.managed_layouts())
            .chain(&self.catalog.user_layouts)
            .chain(&loaded)
        {
            if !known.contains(s) {
                known.push(*s);
            }
        }
        self.known = known;
        // What is ticked comes from the model (SettingsModel::for_window).
        for (list, checked) in [
            (self.c.lap_list, &model.laptop.layouts),
            (self.c.ext_list, &model.external.layouts),
        ] {
            ui::lv_clear(list);
            for (i, spec) in self.known.iter().enumerate() {
                let row = ui::lv_add(list, &format!("{}   ({spec})", self.catalog.label(spec)), i as isize);
                ui::lv_set_checked(list, row, checked.contains(spec));
            }
        }
        self.rebuild_combo(self.c.lap_list, self.c.lap_combo, model.laptop.activate);
        self.rebuild_combo(self.c.ext_list, self.c.ext_combo, model.external.activate);

        ui::set_checked(self.c.ext_any, model.external_any);
        ui::set_checked(self.c.ext_only, !model.external_any);
        self.ext_rows = model.external_devices.clone();

        // Checked when any copy starts with Windows; saving with the box ticked
        // points the entry to this copy.
        let run = autostart::status();
        let turned_off = self.refresh_autostart();
        if turned_off {
            ui::set_text(
                self.c.status,
                "Start with Windows is turned off in Windows' Startup apps; tick it and save to turn it on.",
            );
        } else if let RunEntry::OtherExe(other) = &run {
            ui::set_text(
                self.c.status,
                &format!("Start with Windows runs another copy ({other}); Save or Apply points it to this one."),
            );
        }
        ui::set_checked(self.c.paused, paused);
        ui::set_checked(self.c.notify, model.notify);
        ui::set_text(self.c.debounce, &model.debounce_ms.to_string());
        ui::set_text(self.c.resume, &model.resume_delay_ms.to_string());

        self.show_other_line();

        // Add-a-layout choices: user languages first, then languages of base layouts.
        let mut langs: Vec<u16> = self.catalog.user_languages.clone();
        let mut more: Vec<(String, u16)> = self
            .catalog
            .klids
            .iter()
            .filter(|k| k.klid >> 16 == 0)
            .map(|k| (k.klid & 0xFFFF) as u16)
            .filter(|l| !langs.contains(l))
            .map(|l| (super::catalog::language_name(l), l))
            .collect();
        more.sort();
        more.dedup();
        langs.extend(more.into_iter().map(|(_, l)| l));
        ui::combo_clear(self.c.add_lang);
        for l in &langs {
            ui::combo_add(self.c.add_lang, &super::catalog::language_name(*l), *l as isize);
        }
        // Sorted control would reorder; ours is unsorted so index 0 is the first user language.
        ui::combo_select(self.c.add_lang, 0);
        ui::combo_clear(self.c.add_layout);
        for k in &self.catalog.klids {
            ui::combo_add(
                self.c.add_layout,
                &format!("{}  ({:08X})", k.name, k.klid),
                k.klid as isize,
            );
        }
        ui::combo_select(self.c.add_layout, 0);

        self.refresh_keyboards();
        self.update_enabled();
    }

    /// Shows "Start with Windows" as it is in the registry now; true when the
    /// entry exists but is turned off in Windows' Startup apps.
    fn refresh_autostart(&mut self) -> bool {
        let run = autostart::status();
        let turned_off = run != RunEntry::Missing && autostart::approved() == Some(false);
        let on = run != RunEntry::Missing && !turned_off;
        ui::set_checked(self.c.autostart, on);
        self.autostart_initial = on;
        turned_off
    }

    /// Enables "This port only" for a keyboard that can be told apart only by its port.
    fn update_port_only(&self) {
        let port = ui::lv_selected(self.c.kb_list)
            .and_then(|r| self.groups.get(r as usize))
            .is_some_and(|g| g.port_matcher.is_some());
        // SAFETY: our own control.
        unsafe {
            let _ = EnableWindow(self.c.port_only, port);
        }
    }

    /// The line above the status line: a configuration error if there is
    /// one, otherwise the profiles the window does not manage.
    fn show_other_line(&self) {
        let text = match &self.config_error {
            Some(e) => format!("config.toml cannot be used: {}", first_line(e)),
            None => {
                let others = other_profiles(&self.work);
                if others.is_empty() {
                    String::new()
                } else {
                    format!("Also in config.toml and kept as they are: {}", others.join(", "))
                }
            }
        };
        ui::set_text(self.c.other, &text);
    }

    /// Guidance shown when the window opens.
    fn show_start_hint(&self) {
        if self.config_error.is_some() {
            ui::set_text(
                self.c.status,
                "Save or Apply replaces the file; the current one is kept as config.toml.bak.",
            );
        } else if !keydock::settings::is_configured(&self.work) {
            ui::set_text(self.c.other, "Welcome! Nothing changes until you click Save or Apply.");
            let external_auto = self
                .groups
                .iter()
                .any(|g| g.kind == KeyboardKind::External && g.manual.is_none());
            ui::set_text(
                self.c.status,
                if external_auto {
                    "Laptop keyboard listed as External? Select it and click Built-in."
                } else {
                    "Pick the layouts to keep in each mode."
                },
            );
        } else if ui::get_text(self.c.status).is_empty() {
            let absent = if self.work.default.is_none() {
                Some("Laptop only")
            } else if keydock::settings::gui_profile(&self.work).is_none() {
                Some("External keyboard connected")
            } else {
                None
            };
            if let Some(mode) = absent {
                ui::set_text(
                    self.c.status,
                    &format!(
                        "\"{mode}\" is not set up: tick its layouts to set it up. Other changes are saved without it."
                    ),
                );
            }
        }
    }

    fn checked_specs(&self, list: HWND) -> Vec<LayoutSpec> {
        (0..ui::lv_count(list))
            .filter(|&r| ui::lv_checked(list, r))
            .filter_map(|r| self.known.get(ui::lv_param(list, r) as usize).copied())
            .collect()
    }

    /// Refills a "Switch to" combo from the checked layouts, keeping the selection.
    fn rebuild_combo(&self, list: HWND, combo: HWND, want: Option<LayoutSpec>) {
        let current = want.or_else(|| {
            ui::combo_selected_data(combo).and_then(|d| (d >= 0).then(|| self.known.get(d as usize).copied()).flatten())
        });
        ui::combo_clear(combo);
        ui::combo_add(combo, "(don't switch)", -1);
        let mut sel = 0;
        for spec in self.checked_specs(list) {
            let idx = self.known.iter().position(|k| *k == spec).unwrap_or(0);
            let i = ui::combo_add(combo, &self.catalog.label(&spec), idx as isize);
            if Some(spec) == current {
                sel = i;
            }
        }
        ui::combo_select(combo, sel);
    }

    fn combo_spec(&self, combo: HWND) -> Option<LayoutSpec> {
        ui::combo_selected_data(combo)
            .filter(|d| *d >= 0)
            .and_then(|d| self.known.get(d as usize).copied())
    }

    /// Re-enumerates keyboards and refreshes the device lists.
    fn refresh_keyboards(&mut self) {
        let keyboards = match devices::enumerate().map(devices::complete) {
            Ok(k) => k,
            Err(e) => {
                ui::set_text(self.c.status, &format!("Cannot list keyboards: {e}"));
                Vec::new()
            }
        };
        let selected = ui::lv_selected(self.c.kb_list)
            .and_then(|r| self.groups.get(r as usize))
            .map(|g| g.key.clone());
        self.groups = group_keyboards(&keyboards, &self.work, &self.state);
        ui::lv_clear(self.c.kb_list);
        for (i, g) in self.groups.iter().enumerate() {
            let row = ui::lv_add(self.c.kb_list, &g.name, i as isize);
            let id = g
                .vid_pid
                .map(|(v, p)| format!("{v:04X}:{p:04X}"))
                .unwrap_or_else(|| "\u{2014}".into());
            ui::lv_set_text(self.c.kb_list, row, 1, &id);
            ui::lv_set_text(self.c.kb_list, row, 2, &g.type_label());
            ui::lv_set_text(self.c.kb_list, row, 3, &g.interfaces.to_string());
            if selected.as_deref() == Some(g.key.as_str()) {
                ui::lv_select(self.c.kb_list, row);
            }
        }

        // "Only these keyboards": configured ones (checked state kept) plus
        // connected external keyboards.
        let mut checked: Vec<Matcher> = (0..ui::lv_count(self.c.ext_devices))
            .filter(|&r| ui::lv_checked(self.c.ext_devices, r))
            .filter_map(|r| self.ext_rows.get(ui::lv_param(self.c.ext_devices, r) as usize).cloned())
            .collect();
        if ui::lv_count(self.c.ext_devices) == 0 {
            checked = self.ext_rows.clone();
        }
        for g in self.groups.iter().filter(|g| g.kind == KeyboardKind::External) {
            // A configured row may name this device by an older container ID.
            if !self.ext_rows.iter().any(|m| g.matched_by(m)) {
                self.ext_rows.push(g.matcher.clone());
            }
        }
        ui::lv_clear(self.c.ext_devices);
        for (i, m) in self.ext_rows.iter().enumerate() {
            let mut label = match self.groups.iter().find(|g| g.matched_by(m)) {
                Some(g) => g.checklist_label(),
                None => format!("{} (not connected)", m.describe()),
            };
            // A VID:PID entry covers every keyboard of the model: say so.
            if let Some(note) = model_scope_note(m) {
                label = format!("{label} \u{2014} {note}");
            }
            let row = ui::lv_add(self.c.ext_devices, &label, i as isize);
            ui::lv_set_checked(self.c.ext_devices, row, checked.iter().any(|c| c == m));
        }
        self.update_port_only();
    }

    fn update_enabled(&self) {
        let only = ui::is_checked(self.c.ext_only);
        // SAFETY: our own control.
        unsafe {
            let _ = EnableWindow(self.c.ext_devices, only);
        }
    }

    fn mark_selected(&mut self, mark: Option<Mark>) {
        let Some(g) = ui::lv_selected(self.c.kb_list)
            .and_then(|r| self.groups.get(r as usize))
            .cloned()
        else {
            ui::set_text(self.c.status, "Select a keyboard in the list first.");
            return;
        };
        let port_only = g.port_matcher.is_some() && ui::is_checked(self.c.port_only);
        let groups = self.groups.clone();
        let r = set_group_mark_scoped(&mut self.work, &g, mark, port_only, &groups);
        self.refresh_keyboards();
        let model = g.vid_pid.map(|(v, p)| format!("{v:04X}:{p:04X}")).unwrap_or_default();
        let text = if r.effective != mark {
            format!(
                "\"{}\" is still matched by a broader rule in config.toml; edit the file to change that.",
                g.name
            )
        } else if port_only {
            let mut t = format!("\"{}\" updated for this USB port only.", g.name);
            if !r.kept_for.is_empty() {
                t.push_str(&format!(" {} keep their mark.", r.kept_for.join(", ")));
            }
            if !r.not_kept.is_empty() {
                t.push_str(&format!(
                    " {} lost their mark (no identity of their own to store); mark them again.",
                    r.not_kept.join(", ")
                ));
            }
            if r.dropped_model_rule {
                t.push_str(&format!(
                    " Other {model} keyboards that are not connected are no longer covered."
                ));
            }
            t.push_str(" Click Save or Apply to keep it.");
            t
        } else if g.port_matcher.is_some() {
            format!(
                "\"{}\" updated for every {model} keyboard (it has no serial number; tick \"This port only\" to tell identical keyboards apart). Click Save or Apply to keep it.",
                g.name
            )
        } else {
            format!("\"{}\" updated. Click Save or Apply to keep it.", g.name)
        };
        ui::set_text(self.c.status, &text);
    }

    fn add_layout(&mut self) {
        let (Some(lang), Some(klid)) = (
            ui::combo_selected_data(self.c.add_lang),
            ui::combo_selected_data(self.c.add_layout),
        ) else {
            return;
        };
        let spec = LayoutSpec::new(lang as u16, klid as u32);
        if let Some(i) = self.known.iter().position(|k| *k == spec) {
            ui::set_text(
                self.c.status,
                &format!("{} is already in the lists.", self.catalog.label(&spec)),
            );
            ui::lv_select(self.c.lap_list, i as i32);
            return;
        }
        self.known.push(spec);
        let idx = (self.known.len() - 1) as isize;
        let text = format!("{}   ({spec})", self.catalog.label(&spec));
        for list in [self.c.lap_list, self.c.ext_list] {
            let row = ui::lv_add(list, &text, idx);
            ui::lv_set_checked(list, row, false);
            ui::lv_select(list, row);
        }
        ui::set_text(
            self.c.status,
            &format!("Added {}. Tick it where you want it.", self.catalog.label(&spec)),
        );
    }

    fn read_model(&self) -> Result<SettingsModel, String> {
        let num = |h: HWND, what: &str| {
            ui::get_text(h)
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("{what} must be a number of milliseconds."))
        };
        let ext_devices: Vec<Matcher> = (0..ui::lv_count(self.c.ext_devices))
            .filter(|&r| ui::lv_checked(self.c.ext_devices, r))
            .filter_map(|r| self.ext_rows.get(ui::lv_param(self.c.ext_devices, r) as usize).cloned())
            .collect();
        let external_any = ui::is_checked(self.c.ext_any);
        Ok(SettingsModel {
            laptop: keydock::settings::ModeSettings {
                layouts: self.checked_specs(self.c.lap_list),
                activate: self.combo_spec(self.c.lap_combo),
            },
            external: keydock::settings::ModeSettings {
                layouts: self.checked_specs(self.c.ext_list),
                activate: self.combo_spec(self.c.ext_combo),
            },
            external_any,
            external_devices: if external_any { Vec::new() } else { ext_devices },
            debounce_ms: num(self.c.debounce, "The delay")?,
            resume_delay_ms: num(self.c.resume, "The delay after resume")?,
            notify: ui::is_checked(self.c.notify),
        })
    }
}

/// What a save produced; applied to the app after the settings borrow ends.
struct SaveResult {
    cfg: Config,
    paused: bool,
    autostart: bool,
    /// "Start with Windows" as shown before this save.
    autostart_was: bool,
}

fn save(s: &mut SettingsWin) -> Option<SaveResult> {
    let model = match s.read_model() {
        Ok(m) => m,
        Err(e) => {
            message(s.hwnd, &e);
            return None;
        }
    };
    if let Err(e) = model.validate(&s.work) {
        message(s.hwnd, &e);
        return None;
    }
    let cfg = model.to_config(&s.work);
    let text = cfg.to_toml();
    // Never silently replace a file that KeyDock cannot use (a hand edit with
    // a typo, another encoding, a locked file), that changed since this
    // window read it, or that was deleted meanwhile. The revision the user
    // approves replacing is the one that is backed up, and the write only
    // happens if the file is still that revision immediately before the
    // rename (see keydock::fsutil); otherwise the user is asked again.
    let mut current = read_revision(&s.config_path);
    let mut again = false;
    loop {
        if let Some(problem) = save_conflict(&s.disk_snapshot, &current) {
            let backup_note = match &current {
                Revision::Content(_) => " The current file is kept as config.toml.bak.",
                _ => "",
            };
            let question = format!(
                "{} {problem}{}\n\nReplace it with the settings shown here?{backup_note}",
                s.config_path.display(),
                if again {
                    " (It changed again while this question was open.)"
                } else {
                    ""
                }
            );
            if !confirm(s.hwnd, &question) {
                return None;
            }
            match &current {
                // Backed up together with the replace below, so an aborted
                // or failed save keeps the previous config.toml.bak.
                Revision::Content(_) => {}
                Revision::Unreadable(e) => {
                    message(
                        s.hwnd,
                        &format!(
                            "{} cannot be read ({e}), so it is not replaced.",
                            s.config_path.display()
                        ),
                    );
                    return None;
                }
                Revision::Missing => {}
            }
        }
        // Back up only a revision the user agreed to replace.
        let backup = save_conflict(&s.disk_snapshot, &current).and(current.bytes());
        match write_atomic_if_with_backup(&s.config_path, &text, &current, backup) {
            Ok(()) => {
                if backup.is_some() {
                    info!("kept the replaced configuration as config.toml.bak");
                }
                break;
            }
            Err(WriteError::Conflict(now)) => {
                // Changed between the check (or the question) and the write:
                // what the user approved is no longer what is on disk.
                info!("{} changed while saving; asking again", s.config_path.display());
                if save_conflict(&s.disk_snapshot, &now).is_none() {
                    // Changed back to what this window read: still ask, it moved.
                    s.disk_snapshot = Revision::Unreadable("changed while saving".into());
                }
                current = now;
                again = true;
            }
            Err(WriteError::Io(e)) => {
                message(s.hwnd, &format!("Cannot write {}: {e}", s.config_path.display()));
                return None;
            }
        }
    }
    s.disk_snapshot = Revision::Content(text.into_bytes());
    info!("settings saved to {}", s.config_path.display());
    s.work = cfg.clone();
    s.config_error = None;
    s.show_other_line();
    let autostart = ui::is_checked(s.c.autostart);
    let autostart_was = std::mem::replace(&mut s.autostart_initial, autostart);
    Some(SaveResult {
        cfg,
        paused: ui::is_checked(s.c.paused),
        autostart,
        autostart_was,
    })
}

fn on_command(hwnd: HWND, id: i32) {
    match id {
        ID_MARK_BUILTIN => drop(with_settings(|s| s.mark_selected(Some(Mark::BuiltIn)))),
        ID_MARK_EXTERNAL => drop(with_settings(|s| s.mark_selected(Some(Mark::External)))),
        ID_MARK_IGNORE => drop(with_settings(|s| s.mark_selected(Some(Mark::Ignored)))),
        ID_MARK_AUTO => drop(with_settings(|s| s.mark_selected(None))),
        ID_PORT_ONLY => {}
        ID_EXT_ANY | ID_EXT_ONLY => drop(with_settings(|s| s.update_enabled())),
        ID_ADD => drop(with_settings(|s| s.add_layout())),
        // Save: write, apply and close. Apply: write and apply, keep the window open.
        ID_SAVE | ID_APPLY => {
            let Some(r) = with_settings(save).flatten() else { return };
            // Only a change the user made to the box turns the entry on or
            // off; a choice made in Windows' Startup apps meanwhile is kept.
            let action = autostart_action(
                r.autostart_was,
                r.autostart,
                autostart::is_on(),
                matches!(autostart::status(), RunEntry::OtherExe(_)),
            );
            let result = match action {
                AutostartAction::Enable => autostart::enable(),
                AutostartAction::Disable => autostart::disable(),
                AutostartAction::Repoint => autostart::write_command(),
                AutostartAction::None => Ok(()),
            };
            if let Err(e) = result {
                message(hwnd, &e);
            }
            // Show what the registry says now (after a failure, or a change
            // made in Windows while the window was open).
            let changed_outside = action == AutostartAction::None && r.autostart != autostart::is_on();
            with_settings(|s| {
                s.refresh_autostart();
            });
            // Apply (outside the settings borrow: evaluation refreshes this
            // window and reports its result there).
            let res = with_app(|a| a.apply_saved_settings(r.cfg, r.paused, session()));
            if id == ID_SAVE {
                // SAFETY: our own window.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            } else {
                let note = if changed_outside {
                    " Start with Windows was changed in Windows meanwhile; the box shows it now."
                } else {
                    ""
                };
                match res {
                    Some(Manual::Deferred(why)) => show_status(&format!("Saved; {why}.{note}")),
                    Some(Manual::Ran) if !note.is_empty() => {
                        let cur = with_settings(|s| ui::get_text(s.c.status)).unwrap_or_default();
                        show_status(&format!("{cur}{note}"));
                    }
                    Some(Manual::Ran) => {}
                    None => show_status("Saved; KeyDock is busy and applies it shortly."),
                }
            }
        }
        // IDCANCEL (Esc via IsDialogMessage) or Close.
        2 | ID_CLOSE => {
            // SAFETY: our own window.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        _ => {}
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            let code = ((wparam.0 >> 16) & 0xFFFF) as u32;
            if code == 0 || id == ID_EXT_ANY || id == ID_EXT_ONLY {
                on_command(hwnd, id);
            }
            return LRESULT(0);
        }
        WM_NOTIFY => {
            // SAFETY: lParam of WM_NOTIFY points to an NMHDR (an NMLISTVIEW for LVN_ITEMCHANGED).
            let hdr = unsafe { &*(lparam.0 as *const NMHDR) };
            if hdr.code == LVN_ITEMCHANGED && hdr.idFrom == ID_KB_LIST as usize {
                with_settings(|s| s.update_port_only());
            }
            if hdr.code == LVN_ITEMCHANGED && (hdr.idFrom == ID_LAP_LIST as usize || hdr.idFrom == ID_EXT_LIST as usize)
            {
                // SAFETY: see above.
                let nm = unsafe { &*(lparam.0 as *const NMLISTVIEW) };
                if nm.uChanged.0 & LVIF_STATE.0 != 0 && (nm.uNewState ^ nm.uOldState) & 0xF000 != 0 {
                    let lap = hdr.idFrom == ID_LAP_LIST as usize;
                    with_settings(|s| {
                        let (list, combo) = if lap {
                            (s.c.lap_list, s.c.lap_combo)
                        } else {
                            (s.c.ext_list, s.c.ext_combo)
                        };
                        s.rebuild_combo(list, combo, None);
                    });
                }
            }
            return LRESULT(0);
        }
        WM_DPICHANGED => {
            let dpi = (wparam.0 & 0xFFFF) as u32;
            // SAFETY: lParam points to the suggested window rectangle.
            let r = unsafe { *(lparam.0 as *const RECT) };
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            with_settings(|s| {
                s.dpi = dpi;
                s.relayout();
            });
            return LRESULT(0);
        }
        WM_CLOSE => {
            // SAFETY: our own window.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return LRESULT(0);
        }
        WM_DESTROY => {
            SETTINGS_HWND.with(|c| c.set(None));
            // A result still pending for this window is not shown elsewhere.
            OPEN_SESSION.with(|c| c.set(None));
            let old = SETTINGS.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.take()));
            if let Some(s) = old {
                ui::delete_font(s.font);
            }
            return LRESULT(0);
        }
        _ => {}
    }
    // SAFETY: default processing.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Called by the app after every evaluation (device change etc.).
pub fn refresh_devices() {
    with_settings(|s| s.refresh_keyboards());
}

/// Shows a status line in the open Settings window.
pub fn show_status(text: &str) {
    with_settings(|s| ui::set_text(s.c.status, text));
}

/// Shows the result of a Save/Apply that the app reports later, only in the
/// window (session) that submitted it.
pub fn show_status_for(target: u64, text: &str) {
    if is_recipient(session(), target) {
        show_status(text);
    }
}

/// Opens the settings window, or brings the existing one to the front.
pub fn open(args: OpenArgs) {
    if let Some(h) = hwnd() {
        // SAFETY: our own window.
        unsafe {
            let _ = ShowWindow(h, SW_RESTORE);
            let _ = SetForegroundWindow(h);
        }
        return;
    }
    // SAFETY: standard class registration and window creation.
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
        let Ok(inst) = GetModuleHandleW(None) else { return };
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: inst.into(),
            lpszClassName: CLASS,
            hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: cached_icon(IDI_APP, SM_CXICON),
            ..Default::default()
        };
        RegisterClassW(&wc); // fails harmlessly when already registered
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let ex = WS_EX_CONTROLPARENT;
        let Ok(hwnd) = CreateWindowExW(
            ex,
            CLASS,
            w!("KeyDock Settings"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CLIENT_W,
            CLIENT_H,
            None,
            None,
            Some(inst.into()),
            None,
        ) else {
            error!("cannot create the settings window");
            return;
        };
        SETTINGS_HWND.with(|c| c.set(Some(hwnd)));
        let id = SESSIONS.with(|c| {
            c.set(c.get() + 1);
            c.get()
        });
        OPEN_SESSION.with(|c| c.set(Some(id)));
        let dpi = GetDpiForWindow(hwnd).max(96);
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: ui::scale(CLIENT_W, dpi),
            bottom: ui::scale(CLIENT_H, dpi),
        };
        let _ = AdjustWindowRectExForDpi(&mut rc, style, false, WINDOW_EX_STYLE(ex.0), dpi);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            rc.right - rc.left,
            rc.bottom - rc.top,
            SWP_NOMOVE | SWP_NOZORDER,
        );
        SendMessageW(
            hwnd,
            WM_SETICON,
            Some(WPARAM(ICON_BIG as usize)),
            Some(LPARAM(cached_icon(IDI_APP, SM_CXICON).0 as isize)),
        );
        SendMessageW(
            hwnd,
            WM_SETICON,
            Some(WPARAM(ICON_SMALL as usize)),
            Some(LPARAM(cached_icon(IDI_APP, SM_CXSMICON).0 as isize)),
        );

        // Show what is on disk now: the app re-reads config.toml only when it
        // evaluates, so a hand edit may not have reached it yet.
        let disk_snapshot = read_revision(&args.config_path);
        let cfg = disk_snapshot
            .bytes()
            .and_then(|b| std::str::from_utf8(b).ok())
            .and_then(|t| Config::parse(t).ok())
            .unwrap_or(args.cfg);
        let catalog = Catalog::load();
        let model = SettingsModel::for_window(&cfg, &catalog.loaded_specs());
        let empty = HWND::default();
        let mut s = SettingsWin {
            hwnd,
            layout: Layout::new(hwnd),
            font: HFONT::default(),
            dpi,
            c: Controls {
                kb_list: empty,
                port_only: empty,
                lap_list: empty,
                lap_combo: empty,
                ext_any: empty,
                ext_only: empty,
                ext_devices: empty,
                ext_list: empty,
                ext_combo: empty,
                add_lang: empty,
                add_layout: empty,
                autostart: empty,
                paused: empty,
                notify: empty,
                debounce: empty,
                resume: empty,
                other: empty,
                status: empty,
            },
            catalog,
            work: cfg,
            state: args.state,
            config_path: args.config_path,
            config_error: args.config_error,
            disk_snapshot,
            autostart_initial: false,
            known: Vec::new(),
            groups: Vec::new(),
            ext_rows: Vec::new(),
        };
        s.create_controls();
        s.relayout();
        s.populate(&model, args.paused);
        s.show_start_hint();
        let stored = SETTINGS.with(|c| match c.try_borrow_mut() {
            Ok(mut g) => {
                *g = Some(s);
                true
            }
            Err(_) => false,
        });
        if !stored {
            // Cannot happen while SETTINGS_HWND guards `open`; never panic in a window procedure.
            error!("settings window state is busy; closing the new window");
            let _ = DestroyWindow(hwnd);
            return;
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
}
