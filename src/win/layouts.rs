//! Keyboard layout management: `InstallLayoutOrTip` (input.dll) to add and
//! remove layouts for the current user, `GetKeyboardLayoutList` to see what is
//! loaded, and `WM_INPUTLANGCHANGEREQUEST` to switch the foreground window.

use keydock::layout::{install_list, parse_layout_id, LayoutSpec};
use std::ffi::c_void;
use windows::core::PWSTR;
use windows::core::{s, w, BOOL, GUID, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, FreeLibrary, HMODULE, HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ActivateKeyboardLayout, GetKeyboardLayout, GetKeyboardLayoutList, LoadKeyboardLayoutW, UnloadKeyboardLayout, HKL,
    KLF_NOTELLSHELL, KLF_SETFORPROCESS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, PostMessageW, SystemParametersInfoW,
    GUITHREADINFO, SPI_GETDEFAULTINPUTLANG, SPI_SETDEFAULTINPUTLANG, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    WM_INPUTLANGCHANGEREQUEST,
};

/// `ILOT_UNINSTALL` ("Same as ILOT_DISABLED"). Not defined in a public header;
/// value from Microsoft Learn, InstallLayoutOrTip function:
/// https://learn.microsoft.com/en-us/windows/win32/tsf/installlayoutortip
pub const ILOT_UNINSTALL: u32 = 0x0000_0001;
/// Flags 0 = install/enable the listed layouts for the current user and apply
/// them to the current session (ILOT_NOAPPLYTOCURRENTSESSION = 0x20 is not set).
pub const ILOT_INSTALL: u32 = 0;

/// `BOOL CALLBACK InstallLayoutOrTip(_In_ LPCWSTR psz, _In_ DWORD dwFlags);`
/// (the Learn page's sample typedef says HRESULT, but the documented syntax and
/// return-value table are BOOL: TRUE on success, FALSE on failure).
type InstallLayoutOrTipFn = unsafe extern "system" fn(psz: PCWSTR, flags: u32) -> BOOL;

/// input.dll loaded from System32 only (no import library exists for it).
pub struct InputDll {
    module: HMODULE,
    install_layout_or_tip: InstallLayoutOrTipFn,
}

impl InputDll {
    pub fn load() -> Result<Self, String> {
        // SAFETY: loading a system DLL from System32 only, as the Learn page advises.
        let module = unsafe { LoadLibraryExW(w!("input.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
            .map_err(|e| format!("LoadLibraryExW(input.dll) failed: {e}"))?;
        // SAFETY: valid module handle and NUL-terminated ANSI name.
        let Some(proc) = (unsafe { GetProcAddress(module, s!("InstallLayoutOrTip")) }) else {
            // SAFETY: module was loaded above.
            unsafe {
                let _ = FreeLibrary(module);
            }
            return Err("input.dll does not export InstallLayoutOrTip".into());
        };
        // SAFETY: the export has the documented signature above.
        let install_layout_or_tip: InstallLayoutOrTipFn = unsafe { std::mem::transmute(proc) };
        Ok(Self {
            module,
            install_layout_or_tip,
        })
    }

    fn call(&self, specs: &[LayoutSpec], flags: u32) -> Result<(), String> {
        if specs.is_empty() {
            return Ok(());
        }
        let list = install_list(specs);
        let wide: Vec<u16> = list.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: NUL-terminated string that outlives the call.
        let ok = unsafe { (self.install_layout_or_tip)(PCWSTR(wide.as_ptr()), flags) };
        if ok.as_bool() {
            Ok(())
        } else {
            Err(format!("InstallLayoutOrTip(\"{list}\", 0x{flags:X}) returned FALSE"))
        }
    }

    pub fn install(&self, specs: &[LayoutSpec]) -> Result<(), String> {
        self.call(specs, ILOT_INSTALL)
    }

    pub fn uninstall(&self, specs: &[LayoutSpec]) -> Result<(), String> {
        self.call(specs, ILOT_UNINSTALL)
    }
}

/// `LAYOUTORTIPPROFILE` (Microsoft Learn, EnumEnabledLayoutOrTip).
#[repr(C)]
struct LayoutOrTipProfile {
    profile_type: u32,
    langid: u16,
    clsid: GUID,
    guid_profile: GUID,
    catid: GUID,
    substitute_layout: u32,
    flags: u32,
    id: [u16; 260],
}

const _: () = assert!(std::mem::size_of::<LayoutOrTipProfile>() == 584);

/// `LOTP_KEYBOARDLAYOUT`: the entry is a keyboard layout, not a text service.
const LOTP_KEYBOARDLAYOUT: u32 = 2;

/// `UINT EnumEnabledLayoutOrTip(LPCWSTR pszUserReg, LPCWSTR pszSystemReg,
/// LPCWSTR pszSoftwareReg, LAYOUTORTIPPROFILE *pLayoutOrTipProfile, UINT uBufLength)`.
type EnumEnabledLayoutOrTipFn = unsafe extern "system" fn(PCWSTR, PCWSTR, PCWSTR, *mut LayoutOrTipProfile, u32) -> u32;

/// The keyboard layouts enabled in the current user's settings (the list
/// `InstallLayoutOrTip` edits), via the documented `EnumEnabledLayoutOrTip`.
/// Unlike [`current_hkls`] this ignores layouts that are only loaded in the
/// session (a removed layout still in use, a layout an application loaded).
///
/// `Err` unless the answer is authoritative: the function is unavailable,
/// reports no input profile at all (every user has at least one), or
/// returns a keyboard-layout record that cannot be parsed. A user with only
/// text services (IMEs) gets an empty list, which is a valid answer.
pub fn enabled_layouts() -> Result<Vec<LayoutSpec>, String> {
    // SAFETY: loading a system DLL from System32 only.
    let module = unsafe { LoadLibraryExW(w!("input.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
        .map_err(|e| format!("LoadLibraryExW(input.dll) failed: {e}"))?;
    struct Module(HMODULE);
    impl Drop for Module {
        fn drop(&mut self) {
            // SAFETY: loaded above.
            unsafe {
                let _ = FreeLibrary(self.0);
            }
        }
    }
    let module = Module(module);
    // SAFETY: valid module handle and NUL-terminated ANSI name.
    let proc = unsafe { GetProcAddress(module.0, s!("EnumEnabledLayoutOrTip")) }
        .ok_or("input.dll does not export EnumEnabledLayoutOrTip")?;
    // SAFETY: the export has the documented signature above.
    let f: EnumEnabledLayoutOrTipFn = unsafe { std::mem::transmute(proc) };
    let null = PCWSTR::null();
    // SAFETY: a NULL buffer asks for the number of enabled items.
    let n = unsafe { f(null, null, null, std::ptr::null_mut(), 0) };
    if n == 0 {
        return Err("EnumEnabledLayoutOrTip reported no input profiles".into());
    }
    // A few spare entries in case one is added between the two calls. The
    // length is passed as a number of entries: if the function took bytes,
    // it would only write less.
    let cap = n as usize + 4;
    // SAFETY: plain data; all-zero is a valid value.
    let mut buf: Vec<LayoutOrTipProfile> = (0..cap).map(|_| unsafe { std::mem::zeroed() }).collect();
    // SAFETY: `buf` holds `cap` entries.
    let got = unsafe { f(null, null, null, buf.as_mut_ptr(), cap as u32) } as usize;
    if got == 0 {
        return Err("EnumEnabledLayoutOrTip returned no input profiles".into());
    }
    let mut out: Vec<LayoutSpec> = Vec::new();
    for p in buf.iter().take(got.min(cap)) {
        if p.profile_type != LOTP_KEYBOARDLAYOUT {
            continue;
        }
        let len = p.id.iter().position(|&c| c == 0).unwrap_or(p.id.len());
        let id = String::from_utf16_lossy(&p.id[..len]);
        // "<LANGID>:<KLID>"; a bare KLID takes the entry's language.
        let spec = LayoutSpec::parse(&id).ok().or_else(|| {
            u32::from_str_radix(id.trim(), 16)
                .ok()
                .map(|klid| LayoutSpec::new(p.langid, klid))
        });
        match spec {
            Some(s) if !out.contains(&s) => out.push(s),
            Some(_) => {}
            None => {
                return Err(format!(
                    "EnumEnabledLayoutOrTip returned a keyboard layout record that cannot be read: \"{id}\""
                ))
            }
        }
    }
    Ok(out)
}

/// [`keydock::reconcile::LayoutApi`] on the real Windows functions.
pub struct WinLayouts {
    dll: Option<InputDll>,
}

impl WinLayouts {
    pub fn new() -> Self {
        Self { dll: None }
    }

    fn dll(&mut self) -> Result<&InputDll, String> {
        if self.dll.is_none() {
            self.dll = Some(InputDll::load()?);
        }
        Ok(self.dll.as_ref().expect("loaded above"))
    }
}

impl keydock::reconcile::LayoutApi for WinLayouts {
    fn enabled(&mut self) -> Result<Vec<LayoutSpec>, String> {
        enabled_layouts()
    }
    fn install(&mut self, specs: &[LayoutSpec]) -> Result<(), String> {
        self.dll()?.install(specs)
    }
    fn uninstall(&mut self, specs: &[LayoutSpec]) -> Result<(), String> {
        self.dll()?.uninstall(specs)
    }
}

impl Drop for InputDll {
    fn drop(&mut self) {
        // SAFETY: module was loaded in `load`.
        unsafe {
            let _ = FreeLibrary(self.module);
        }
    }
}

/// Low 32 bits of an HKL (HKLs are sign-extended on 64-bit Windows).
pub fn hkl_low32(h: HKL) -> u32 {
    h.0 as usize as u32
}

/// The input locales currently loaded in this session.
pub fn current_hkls() -> Vec<HKL> {
    // SAFETY: size query with no buffer, then a buffer of that size.
    unsafe {
        let n = GetKeyboardLayoutList(None);
        if n <= 0 {
            return Vec::new();
        }
        let mut v = vec![HKL::default(); n as usize];
        let got = GetKeyboardLayoutList(Some(&mut v));
        v.truncate(got.max(0) as usize);
        v
    }
}

/// Reads `HKLM\SYSTEM\CurrentControlSet\Control\Keyboard Layouts\<KLID>\Layout Id`.
pub fn registry_layout_id(spec: &LayoutSpec) -> Option<u16> {
    let key: Vec<u16> = format!(
        "SYSTEM\\CurrentControlSet\\Control\\Keyboard Layouts\\{}",
        spec.klid_string()
    )
    .encode_utf16()
    .chain(std::iter::once(0))
    .collect();
    let mut buf = [0u16; 32];
    let mut size = std::mem::size_of_val(&buf) as u32;
    // SAFETY: valid NUL-terminated key/value names; buffer size in bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            w!("Layout Id"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
    };
    if rc.is_err() {
        return None;
    }
    let chars = (size as usize / 2).min(buf.len());
    parse_layout_id(&String::from_utf16_lossy(&buf[..chars]))
}

/// Expected HKL (low 32 bits) for a layout entry, consulting the registry for variants.
pub fn expected_hkl(spec: &LayoutSpec) -> Option<u32> {
    let layout_id = if spec.is_variant() {
        registry_layout_id(spec)
    } else {
        None
    };
    spec.hkl(layout_id)
}

/// Finds the HKL to activate: exactly the HKL computed for the layout (its
/// language in the low word, its layout or variant in the high word). Uses
/// the loaded one, or loads it with `LoadKeyboardLayoutW(KLID)`; any other
/// HKL is rejected, also one of the same language: `LoadKeyboardLayoutW` can
/// return another layout (the default) when it finds no match, and derives
/// the language from the KLID (for "0C0A:0000040A" it gives 0x040A040A, not
/// 0x040A0C0A).
pub fn resolve_hkl(spec: &LayoutSpec) -> Result<HKL, String> {
    let present = current_hkls();
    let want = expected_hkl(spec).ok_or_else(|| {
        format!(
            "cannot work out the input locale of {spec} (no \"Layout Id\" for KLID {} in the registry)",
            spec.klid_string()
        )
    })?;
    if let Some(h) = present.iter().find(|h| hkl_low32(**h) == want) {
        return Ok(*h);
    }
    let klid: Vec<u16> = spec.klid_string().encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: NUL-terminated KLID string. KLF_NOTELLSHELL avoids a shell
    // notification; the layout is already installed at this point.
    let h = unsafe { LoadKeyboardLayoutW(PCWSTR(klid.as_ptr()), KLF_NOTELLSHELL) }.map_err(|e| {
        if e.code().is_ok() {
            // No error code: typical for a session without an input
            // desktop (a service or remote PowerShell session).
            format!(
                "LoadKeyboardLayoutW({}) returned no layout; this session cannot load input locales",
                spec.klid_string()
            )
        } else {
            format!("LoadKeyboardLayoutW({}) failed: {e}", spec.klid_string())
        }
    })?;
    if hkl_low32(h) == want {
        Ok(h)
    } else {
        // Do not leave a layout loaded that we loaded only to find it is the
        // wrong one: it would show up in the language switcher.
        let unload = if present.iter().any(|p| hkl_low32(*p) == hkl_low32(h)) {
            String::new()
        } else {
            // SAFETY: an HKL returned by LoadKeyboardLayoutW above.
            match unsafe { UnloadKeyboardLayout(h) } {
                Ok(()) => String::new(),
                Err(e) => format!("; it could not be unloaded again ({e})"),
            }
        };
        Err(format!(
            "layout {spec} is not loaded (expected HKL 0x{want:08X}, LoadKeyboardLayoutW gave 0x{:08X}){unload}",
            hkl_low32(h)
        ))
    }
}

/// The request to the window the user is typing in. Posting is best effort:
/// an application may ignore the request, and Windows refuses it for a
/// window of a more privileged (elevated) process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Foreground {
    /// No foreground window (locked screen, desktop switch).
    NoWindow,
    /// The request was delivered to this process's window; whether it
    /// switched is up to the application.
    Requested(String),
    /// Windows did not accept the request.
    Refused(String),
}

/// What [`activate`] achieved, each part on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    /// The default input language for new windows, read back.
    pub default_input: Result<(), String>,
    pub foreground: Foreground,
}

impl Activation {
    /// What went wrong, if anything the user should know about.
    pub fn problem(&self) -> Option<String> {
        let mut p = Vec::new();
        if let Err(e) = &self.default_input {
            p.push(e.clone());
        }
        if let Foreground::Refused(e) = &self.foreground {
            p.push(format!("the active window did not accept the switch ({e})"));
        }
        (!p.is_empty()).then(|| p.join("; "))
    }
}

/// Makes `hkl` the default input language for new windows and reads it back.
pub fn set_default_input(hkl: HKL) -> Result<(), String> {
    let mut h = hkl;
    // SAFETY: pvParam points to an HKL as SPI_SETDEFAULTINPUTLANG requires. No
    // WM_SETTINGCHANGE broadcast (fWinIni = 0) to avoid re-entrancy.
    unsafe {
        SystemParametersInfoW(
            SPI_SETDEFAULTINPUTLANG,
            0,
            Some(&mut h as *mut HKL as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .map_err(|e| format!("SystemParametersInfoW(SPI_SETDEFAULTINPUTLANG) failed: {e}"))?;
    match default_input_lang() {
        Some(d) if hkl_low32(d) == hkl_low32(hkl) => Ok(()),
        Some(d) => Err(format!(
            "the default input language is 0x{:08X}, not 0x{:08X}",
            hkl_low32(d),
            hkl_low32(hkl)
        )),
        None => Err("cannot read the default input language back".into()),
    }
}

/// Makes `hkl` the default input language for new windows (and checks it)
/// and asks the window with the keyboard focus to switch to it.
pub fn activate(hkl: HKL) -> Activation {
    let default_input = set_default_input(hkl);
    // SAFETY: plain window queries; the message is posted, not sent.
    let foreground = unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() || fg.0.is_null() {
            Foreground::NoWindow
        } else {
            let mut pid = 0u32;
            let tid = GetWindowThreadProcessId(fg, Some(&mut pid));
            // The documented target is the window with the keyboard focus.
            let mut gti = GUITHREADINFO {
                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            let target = if tid != 0 && GetGUIThreadInfo(tid, &mut gti).is_ok() && !gti.hwndFocus.0.is_null() {
                gti.hwndFocus
            } else {
                fg
            };
            match PostMessageW(
                Some(target),
                WM_INPUTLANGCHANGEREQUEST,
                WPARAM(0),
                LPARAM(hkl.0 as isize),
            ) {
                Ok(()) => Foreground::Requested(process_name(pid).unwrap_or_else(|| format!("pid {pid}"))),
                Err(e) => Foreground::Refused(e.to_string()),
            }
        }
    };
    Activation {
        default_input,
        foreground,
    }
}

/// The loaded HKL of a layout, if it is loaded. Never loads anything.
pub fn find_loaded(spec: &LayoutSpec, present: &[HKL]) -> Option<HKL> {
    let want = expected_hkl(spec)?;
    present.iter().copied().find(|h| hkl_low32(*h) == want)
}

/// The default input language for new threads (SPI_GETDEFAULTINPUTLANG).
pub fn default_input_lang() -> Option<HKL> {
    let mut h = HKL::default();
    // SAFETY: pvParam points to an HKL as SPI_GETDEFAULTINPUTLANG requires.
    unsafe {
        SystemParametersInfoW(
            SPI_GETDEFAULTINPUTLANG,
            0,
            Some(&mut h as *mut HKL as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .ok()
    .map(|_| h)
}

/// A GUI thread (identified through one of its top-level windows) whose
/// active input locale is one of the layouts being removed.
#[derive(Debug, Clone)]
pub struct ThreadUse {
    pub hwnd: HWND,
    pub tid: u32,
    pub pid: u32,
    pub hkl: u32,
}

/// Who still uses one of the given layouts (HKL low 32 bits).
#[derive(Debug, Clone, Default)]
pub struct Usage {
    /// KeyDock's own thread.
    pub own_thread: Option<u32>,
    /// The default input language for new threads.
    pub default_lang: Option<u32>,
    /// Other GUI threads, one entry per thread.
    pub threads: Vec<ThreadUse>,
}

impl Usage {
    pub fn any(&self) -> bool {
        self.own_thread.is_some() || self.default_lang.is_some() || !self.threads.is_empty()
    }

    /// One line for the log: what still uses the layouts.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(h) = self.own_thread {
            parts.push(format!("KeyDock itself (0x{h:08X})"));
        }
        if let Some(h) = self.default_lang {
            parts.push(format!("the default input language (0x{h:08X})"));
        }
        for t in &self.threads {
            parts.push(format!(
                "{} pid {} thread {} (0x{:08X})",
                process_name(t.pid).unwrap_or_else(|| "?".into()),
                t.pid,
                t.tid,
                t.hkl
            ));
        }
        parts.join(", ")
    }
}

fn process_name(pid: u32) -> Option<String> {
    // SAFETY: limited query handle, closed below; buffer length in characters.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        Some(full.rsplit('\\').next().unwrap_or(&full).to_string())
    }
}

struct EnumCtx<'a> {
    removed: &'a [u32],
    own_tid: u32,
    out: Vec<ThreadUse>,
}

extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    // SAFETY: lparam is the &mut EnumCtx passed to EnumWindows below, which is
    // alive for the whole (synchronous) enumeration.
    let ctx = unsafe { &mut *(lparam.0 as *mut EnumCtx) };
    let mut pid = 0u32;
    // SAFETY: plain queries on a window handle from EnumWindows.
    let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if tid != 0 && tid != ctx.own_tid && !ctx.out.iter().any(|t| t.tid == tid) {
        // SAFETY: GetKeyboardLayout accepts any thread identifier.
        let h = hkl_low32(unsafe { GetKeyboardLayout(tid) });
        if ctx.removed.contains(&h) {
            ctx.out.push(ThreadUse { hwnd, tid, pid, hkl: h });
        }
    }
    BOOL(1)
}

/// Finds what still uses one of `removed` (HKL low 32 bits): KeyDock's own
/// thread, the default input language, and the threads of all top-level
/// windows on this desktop (GetKeyboardLayout per window thread).
pub fn usage(removed: &[u32]) -> Usage {
    let mut u = Usage::default();
    if removed.is_empty() {
        return u;
    }
    // SAFETY: 0 = the calling thread.
    let own = hkl_low32(unsafe { GetKeyboardLayout(0) });
    if removed.contains(&own) {
        u.own_thread = Some(own);
    }
    if let Some(d) = default_input_lang().map(hkl_low32) {
        if removed.contains(&d) {
            u.default_lang = Some(d);
        }
    }
    // SAFETY: plain thread id query.
    let own_tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    let mut ctx = EnumCtx {
        removed,
        own_tid,
        out: Vec::new(),
    };
    // SAFETY: ctx outlives the synchronous enumeration.
    let _ = unsafe { EnumWindows(Some(enum_proc), LPARAM(&mut ctx as *mut EnumCtx as isize)) };
    u.threads = ctx.out;
    u
}

/// Moves everything found by [`usage`] to `to`: the default input language
/// (SPI_SETDEFAULTINPUTLANG, no broadcast), KeyDock's own process
/// (ActivateKeyboardLayout with KLF_SETFORPROCESS) and, for every other
/// thread, a posted WM_INPUTLANGCHANGEREQUEST to one of its top-level windows
/// (DefWindowProc accepts the request). Nothing is sent synchronously, so a
/// hung application cannot block KeyDock. Returns the number of requests posted.
pub fn move_off(u: &Usage, to: HKL) -> usize {
    if u.default_lang.is_some() {
        let mut h = to;
        // SAFETY: pvParam points to an HKL as SPI_SETDEFAULTINPUTLANG requires.
        let _ = unsafe {
            SystemParametersInfoW(
                SPI_SETDEFAULTINPUTLANG,
                0,
                Some(&mut h as *mut HKL as *mut c_void),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
    }
    if u.own_thread.is_some() {
        // SAFETY: `to` is a loaded HKL.
        let _ = unsafe { ActivateKeyboardLayout(to, KLF_SETFORPROCESS) };
    }
    let mut posted = 0;
    for t in &u.threads {
        // SAFETY: posted, not sent; a stale window handle only makes this fail.
        if unsafe {
            PostMessageW(
                Some(t.hwnd),
                WM_INPUTLANGCHANGEREQUEST,
                WPARAM(0),
                LPARAM(to.0 as isize),
            )
        }
        .is_ok()
        {
            posted += 1;
        }
    }
    posted
}

/// Waits up to `timeout` for [`usage`] to become empty; returns what is left.
pub fn wait_unused(removed: &[u32], timeout: std::time::Duration) -> Usage {
    let start = std::time::Instant::now();
    loop {
        let u = usage(removed);
        if !u.any() || start.elapsed() >= timeout {
            return u;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Unloads a layout from the session. Fails while any thread still uses it,
/// or for a preloaded layout (Microsoft Learn, UnloadKeyboardLayout).
pub fn unload(hkl: HKL) -> Result<(), String> {
    // SAFETY: plain call; an invalid HKL only makes it fail.
    unsafe { UnloadKeyboardLayout(hkl) }
        .map_err(|e| format!("UnloadKeyboardLayout(0x{:08X}) failed: {e}", hkl_low32(hkl)))
}
