//! Thin helpers over Win32 controls for the settings window.

use super::registry::wide;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateFontIndirectW, DeleteObject, HFONT, HGDIOBJ};
use windows::Win32::UI::Controls::{
    BST_CHECKED, LIST_VIEW_ITEM_STATE_FLAGS, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVIF_PARAM, LVIF_STATE, LVIF_TEXT,
    LVIS_SELECTED, LVIS_STATEIMAGEMASK, LVITEMW, LVM_DELETEALLITEMS, LVM_ENSUREVISIBLE, LVM_GETITEMCOUNT,
    LVM_GETITEMSTATE, LVM_GETITEMW, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW, LVM_INSERTITEMW, LVM_SETCOLUMNWIDTH,
    LVM_SETITEMSTATE, LVM_SETITEMTEXTW, LVNI_SELECTED,
};
use windows::Win32::UI::HiDpi::SystemParametersInfoForDpi;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, GetWindowTextLengthW, GetWindowTextW, MoveWindow, SendMessageW, SetWindowTextW, BM_GETCHECK,
    BM_SETCHECK, CB_ADDSTRING, CB_GETCURSEL, CB_GETITEMDATA, CB_RESETCONTENT, CB_SETCURSEL, CB_SETITEMDATA, HMENU,
    NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, WINDOW_EX_STYLE, WINDOW_STYLE, WM_SETFONT,
};

pub fn scale(v: i32, dpi: u32) -> i32 {
    ((i64::from(v) * i64::from(dpi) + 48) / 96) as i32
}

fn send(h: HWND, msg: u32, w: usize, l: isize) -> isize {
    // SAFETY: messages to our own child controls with valid arguments.
    unsafe { SendMessageW(h, msg, Some(WPARAM(w)), Some(LPARAM(l))).0 }
}

/// The message font (Segoe UI on current Windows) for a DPI.
pub fn message_font(dpi: u32) -> HFONT {
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    // SAFETY: ncm is correctly sized.
    unsafe {
        let _ = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            ncm.cbSize,
            Some((&mut ncm as *mut NONCLIENTMETRICSW).cast()),
            0,
            dpi,
        );
        CreateFontIndirectW(&ncm.lfMessageFont)
    }
}

pub fn delete_font(f: HFONT) {
    if !f.is_invalid() {
        // SAFETY: font created by message_font.
        unsafe {
            let _ = DeleteObject(HGDIOBJ(f.0));
        }
    }
}

/// A child control with its layout rectangle in 96-DPI units.
pub struct Placed {
    pub hwnd: HWND,
    pub rect: RECT,
}

/// Creates child controls and keeps their logical positions for DPI changes.
pub struct Layout {
    pub parent: HWND,
    pub items: Vec<Placed>,
}

impl Layout {
    pub fn new(parent: HWND) -> Self {
        Self {
            parent,
            items: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add(
        &mut self,
        class: PCWSTR,
        text: &str,
        style: u32,
        ex: u32,
        id: i32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> HWND {
        let t = wide(text);
        // SAFETY: creating a child of our own window.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(ex),
                class,
                PCWSTR(t.as_ptr()),
                WINDOW_STYLE(style),
                x,
                y,
                w,
                h,
                Some(self.parent),
                Some(HMENU(id as isize as *mut _)),
                None,
                None,
            )
        }
        .unwrap_or_default();
        self.items.push(Placed {
            hwnd,
            rect: RECT {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            },
        });
        hwnd
    }

    /// Positions every control for `dpi` and applies `font`.
    pub fn apply(&self, dpi: u32, font: HFONT) {
        for p in &self.items {
            let r = p.rect;
            // SAFETY: our own child windows.
            unsafe {
                let _ = MoveWindow(
                    p.hwnd,
                    scale(r.left, dpi),
                    scale(r.top, dpi),
                    scale(r.right - r.left, dpi),
                    scale(r.bottom - r.top, dpi),
                    true,
                );
            }
            send(p.hwnd, WM_SETFONT, font.0 as usize, 1);
        }
    }
}

pub fn set_text(h: HWND, s: &str) {
    let w = wide(s);
    // SAFETY: NUL-terminated text.
    unsafe {
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}

pub fn get_text(h: HWND) -> String {
    // SAFETY: buffer sized from GetWindowTextLengthW.
    unsafe {
        let n = GetWindowTextLengthW(h);
        let mut buf = vec![0u16; n.max(0) as usize + 1];
        let got = GetWindowTextW(h, &mut buf);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    }
}

pub fn is_checked(h: HWND) -> bool {
    send(h, BM_GETCHECK, 0, 0) == BST_CHECKED.0 as isize
}

pub fn set_checked(h: HWND, on: bool) {
    send(h, BM_SETCHECK, usize::from(on), 0);
}

// ---- combo box ----

pub fn combo_clear(h: HWND) {
    send(h, CB_RESETCONTENT, 0, 0);
}

pub fn combo_add(h: HWND, text: &str, data: isize) -> i32 {
    let w = wide(text);
    let i = send(h, CB_ADDSTRING, 0, w.as_ptr() as isize);
    send(h, CB_SETITEMDATA, i as usize, data);
    i as i32
}

pub fn combo_select(h: HWND, index: i32) {
    send(h, CB_SETCURSEL, index as usize, 0);
}

/// Item data of the selected entry.
pub fn combo_selected_data(h: HWND) -> Option<isize> {
    let i = send(h, CB_GETCURSEL, 0, 0);
    (i >= 0).then(|| send(h, CB_GETITEMDATA, i as usize, 0))
}

// ---- list view ----

pub fn lv_add_column(h: HWND, index: i32, text: &str, width: i32) {
    let mut w = wide(text);
    let col = LVCOLUMNW {
        mask: LVCF_TEXT | LVCF_WIDTH,
        cx: width,
        pszText: PWSTR(w.as_mut_ptr()),
        ..Default::default()
    };
    send(h, LVM_INSERTCOLUMNW, index as usize, &col as *const LVCOLUMNW as isize);
}

pub fn lv_set_column_width(h: HWND, index: i32, width: i32) {
    send(h, LVM_SETCOLUMNWIDTH, index as usize, width as isize);
}

pub fn lv_clear(h: HWND) {
    send(h, LVM_DELETEALLITEMS, 0, 0);
}

pub fn lv_count(h: HWND) -> i32 {
    send(h, LVM_GETITEMCOUNT, 0, 0) as i32
}

/// Appends a row; returns its index.
pub fn lv_add(h: HWND, text: &str, param: isize) -> i32 {
    let mut w = wide(text);
    let item = LVITEMW {
        mask: LVIF_TEXT | LVIF_PARAM,
        iItem: lv_count(h),
        pszText: PWSTR(w.as_mut_ptr()),
        lParam: LPARAM(param),
        ..Default::default()
    };
    send(h, LVM_INSERTITEMW, 0, &item as *const LVITEMW as isize) as i32
}

pub fn lv_set_text(h: HWND, row: i32, col: i32, text: &str) {
    let mut w = wide(text);
    let item = LVITEMW {
        iSubItem: col,
        pszText: PWSTR(w.as_mut_ptr()),
        ..Default::default()
    };
    send(h, LVM_SETITEMTEXTW, row as usize, &item as *const LVITEMW as isize);
}

pub fn lv_param(h: HWND, row: i32) -> isize {
    let mut item = LVITEMW {
        mask: LVIF_PARAM,
        iItem: row,
        ..Default::default()
    };
    send(h, LVM_GETITEMW, 0, &mut item as *mut LVITEMW as isize);
    item.lParam.0
}

pub fn lv_checked(h: HWND, row: i32) -> bool {
    let st = send(h, LVM_GETITEMSTATE, row as usize, LVIS_STATEIMAGEMASK.0 as isize) as u32;
    (st >> 12) == 2
}

pub fn lv_set_checked(h: HWND, row: i32, on: bool) {
    let item = LVITEMW {
        mask: LVIF_STATE,
        stateMask: LVIS_STATEIMAGEMASK,
        state: LIST_VIEW_ITEM_STATE_FLAGS(if on { 2 << 12 } else { 1 << 12 }),
        ..Default::default()
    };
    send(h, LVM_SETITEMSTATE, row as usize, &item as *const LVITEMW as isize);
}

pub fn lv_selected(h: HWND) -> Option<i32> {
    let i = send(h, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
    (i >= 0).then_some(i as i32)
}

pub fn lv_select(h: HWND, row: i32) {
    let item = LVITEMW {
        mask: LVIF_STATE,
        stateMask: LVIS_SELECTED,
        state: LVIS_SELECTED,
        ..Default::default()
    };
    send(h, LVM_SETITEMSTATE, row as usize, &item as *const LVITEMW as isize);
    send(h, LVM_ENSUREVISIBLE, row as usize, 0);
}
