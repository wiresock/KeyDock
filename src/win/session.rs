//! Which session may change layouts, and one writer per user.
//!
//! The user's layouts and language list are per-user settings, shared by all
//! sessions of that user. KeyDock only sees the keyboards of the computer it
//! runs on, so by default it changes nothing in a Remote Desktop session
//! (`remote_sessions = true` allows it); the same rule applies to the tray
//! and to `--once`, and it is checked with the configuration read fresh for
//! each evaluation. When one user has several sessions with KeyDock (fast
//! user switching back to one's own account is one session; a console and an
//! RDP session of one user are two), a per-user lock in the global namespace
//! lets only one of them change layouts at a time.

use keydock::config::Config;
use std::time::Duration;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, HANDLE, HLOCAL, WAIT_ABANDONED_0, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Registry::HKEY_LOCAL_MACHINE;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    CreateMutexExW, GetCurrentProcess, GetCurrentProcessId, OpenMutexW, OpenProcessToken, ReleaseMutex,
    WaitForMultipleObjects, SYNCHRONIZATION_ACCESS_RIGHTS,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};

use super::registry::wide;

/// Whether this process runs in a remote (or otherwise non-console) session,
/// following Microsoft's "Detecting the Remote Desktop Services
/// environment": `SM_REMOTESESSION`, and for sessions that report 0 there
/// (RemoteFX, the services session) the console session recorded as
/// `GlassSessionId`. If that value cannot be read the session counts as
/// local, as in Microsoft's sample.
pub fn is_remote() -> bool {
    // SAFETY: plain metric query.
    if unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0 {
        return true;
    }
    let glass = super::registry::Key::open(
        HKEY_LOCAL_MACHINE,
        "SYSTEM\\CurrentControlSet\\Control\\Terminal Server",
    )
    .and_then(|k| k.dword("GlassSessionId"));
    let mut current = 0u32;
    // SAFETY: valid out pointer.
    let have_current = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut current) }.is_ok();
    match glass {
        Some(g) if have_current => current != g,
        _ => false,
    }
}

/// Why this session must not change layouts, if it must not.
pub fn blocked(cfg: &Config) -> Option<String> {
    (!cfg.remote_sessions && is_remote())
        .then(|| "Remote Desktop or other non-console session (set remote_sessions = true to allow it)".to_string())
}

/// The user's SID as a string, for per-user object names.
fn user_sid() -> Option<String> {
    // SAFETY: token query with buffers of the reported size; the SID string
    // is freed with LocalFree as documented.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let token = super::child::Owned(token);
        let mut len = 0u32;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut len);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        GetTokenInformation(token.0, TokenUser, Some(buf.as_mut_ptr().cast()), len, &mut len).ok()?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s).ok()?;
        let out = s.to_string().ok();
        let _ = LocalFree(Some(HLOCAL(s.0.cast())));
        out
    }
}

/// Held while KeyDock changes the user's layouts or language list. Thread
/// affine (a mutex): taken and released by the same thread.
///
/// The lock is a named mutex in the global namespace, one per user SID, with
/// a security descriptor that grants only that user (and SYSTEM) access and
/// a low mandatory label, so the user's elevated and non-elevated processes,
/// in any session, can all open it. It is opened with only the rights a
/// wait and a release need. Anything that prevents KeyDock from knowing it
/// is the only writer (no SID, the object cannot be created or opened, the
/// wait fails) is a failure: the caller changes nothing (review TP-02).
pub struct WriterLock(HANDLE);

/// The result of [`WriterLock::acquire`].
pub enum Acquire {
    /// The lock is held.
    Owned(WriterLock),
    /// The lock is held, but its previous owner ended without releasing it:
    /// a change of the layouts or the language list may have been cut short.
    /// Callers proceed only from a fresh observation of the current state.
    Abandoned(WriterLock),
    /// Another KeyDock of this user holds it.
    Busy,
    /// The cancel event was set while waiting.
    Cancelled,
    /// Ownership cannot be established.
    Failed(String),
}

/// MUTEX_MODIFY_STATE (winnt.h): needed for ReleaseMutex.
const MUTEX_MODIFY_STATE: u32 = 0x0001;
/// SYNCHRONIZE: needed for a wait.
const SYNCHRONIZE: u32 = 0x0010_0000;

fn writer_name() -> Result<(String, String), String> {
    let sid = user_sid().ok_or("cannot determine the user's SID")?;
    Ok((format!("Global\\KeyDock.Writer.{sid}"), sid))
}

impl WriterLock {
    /// Takes the lock, waiting up to `timeout` (and returning early when
    /// `cancel` is set).
    pub fn acquire(timeout: Duration, cancel: Option<HANDLE>) -> Acquire {
        match writer_name() {
            // Tests (the restore worker's among them, review S6-06) go through
            // the same descriptor and logic under a name of their own, so they
            // never contend with a KeyDock running as the same user.
            #[cfg(test)]
            Ok((_, sid)) => Self::acquire_name(&tests::process_writer_name(), &sid, timeout, cancel),
            #[cfg(not(test))]
            Ok((name, sid)) => Self::acquire_name(&name, &sid, timeout, cancel),
            Err(e) => Acquire::Failed(e),
        }
    }

    fn acquire_name(name: &str, sid: &str, timeout: Duration, cancel: Option<HANDLE>) -> Acquire {
        let h = match open_or_create(name, sid) {
            Ok(h) => h,
            Err(e) => {
                return Acquire::Failed(format!(
                    "cannot open the writer lock {name}: {e} (this process: {}; the lock: {})",
                    token_facts(),
                    object_sddl(name)
                ))
            }
        };
        let mut handles = vec![h];
        if let Some(c) = cancel {
            handles.push(c);
        }
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: valid handles; the mutex handle is ours.
        let r = unsafe { WaitForMultipleObjects(&handles, false, ms) };
        if r == WAIT_OBJECT_0 {
            Acquire::Owned(WriterLock(h))
        } else if r == WAIT_ABANDONED_0 {
            Acquire::Abandoned(WriterLock(h))
        } else {
            let out = if r.0 == WAIT_OBJECT_0.0 + 1 {
                Acquire::Cancelled
            } else if r == WAIT_TIMEOUT {
                Acquire::Busy
            } else {
                // SAFETY: plain query right after the failed wait.
                Acquire::Failed(format!("waiting for the writer lock failed ({:?})", unsafe {
                    GetLastError()
                }))
            };
            // SAFETY: created above, not owned.
            unsafe {
                let _ = CloseHandle(h);
            }
            out
        }
    }
}

/// Creates the per-user mutex with its security descriptor, or opens the
/// existing one, with SYNCHRONIZE | MUTEX_MODIFY_STATE only.
fn open_or_create(name: &str, sid: &str) -> Result<HANDLE, String> {
    let n = wide(name);
    // Only this user and SYSTEM; a low "no write up" label so that a medium
    // and a high integrity process of the user can both use it.
    let sddl = wide(&format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)S:(ML;;NW;;;LW)"));
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: NUL-terminated SDDL; the descriptor is freed with LocalFree below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), SDDL_REVISION_1, &mut sd, None)
    }
    .map_err(|e| format!("cannot build its security descriptor: {e}"))?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: valid name and attributes; minimal access.
    let created = unsafe { CreateMutexExW(Some(&sa), PCWSTR(n.as_ptr()), 0, SYNCHRONIZE | MUTEX_MODIFY_STATE) };
    // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    match created {
        Ok(h) => Ok(h),
        // An existing object that this token may not create over: open it.
        // SAFETY: valid name; minimal access.
        Err(e) => unsafe {
            OpenMutexW(
                SYNCHRONIZATION_ACCESS_RIGHTS(SYNCHRONIZE | MUTEX_MODIFY_STATE),
                false,
                PCWSTR(n.as_ptr()),
            )
        }
        .map_err(|o| format!("{e}; opening it: {o}")),
    }
}

/// This process's token, for diagnosing a lock that cannot be opened
/// (review FP-02): user SID, integrity level, elevation, whether it is a
/// restricted token (sandboxes) or an AppContainer.
pub fn token_facts() -> String {
    use windows::Win32::Security::{
        IsTokenRestricted, TokenElevation, TokenIntegrityLevel, TokenIsAppContainer, TOKEN_ELEVATION,
        TOKEN_MANDATORY_LABEL,
    };
    // SAFETY: queries on our own token with buffers of the reported size.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return "token cannot be opened".into();
        }
        let token = super::child::Owned(token);
        let user = user_sid().unwrap_or_else(|| "?".into());
        let mut len = 0u32;
        let _ = GetTokenInformation(token.0, TokenIntegrityLevel, None, 0, &mut len);
        let mut buf = vec![0u8; len.max(64) as usize];
        let il = if GetTokenInformation(
            token.0,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        )
        .is_ok()
        {
            let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
            let mut s = PWSTR::null();
            if ConvertSidToStringSidW(label.Label.Sid, &mut s).is_ok() {
                let out = s.to_string().unwrap_or_default();
                let _ = LocalFree(Some(HLOCAL(s.0.cast())));
                out
            } else {
                "?".into()
            }
        } else {
            "?".into()
        };
        let mut elev = TOKEN_ELEVATION::default();
        let mut l2 = 0u32;
        let elevated = GetTokenInformation(
            token.0,
            TokenElevation,
            Some((&mut elev as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut l2,
        )
        .is_ok()
            && elev.TokenIsElevated != 0;
        let mut ac = 0u32;
        let appcontainer = GetTokenInformation(
            token.0,
            TokenIsAppContainer,
            Some((&mut ac as *mut u32).cast()),
            4,
            &mut l2,
        )
        .is_ok()
            && ac != 0;
        let restricted = IsTokenRestricted(token.0).is_ok();
        // A sandboxed token can carry the user SID as deny-only: it then
        // cannot use this user's per-user lock (by design).
        let mut ul = 0u32;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut ul);
        let mut ub = vec![0u8; ul.max(64) as usize];
        let deny_only = GetTokenInformation(token.0, TokenUser, Some(ub.as_mut_ptr().cast()), ul, &mut ul).is_ok()
            && (*(ub.as_ptr() as *const TOKEN_USER)).User.Attributes & 0x10 != 0; // SE_GROUP_USE_FOR_DENY_ONLY
        format!(
            "user {user}{}, integrity {il}, elevated {elevated}, restricted {restricted}, AppContainer {appcontainer}",
            if deny_only { " (deny-only)" } else { "" }
        )
    }
}

/// The logon SID of this process's token, if any.
#[cfg(test)]
pub fn logon_sid() -> Option<String> {
    use windows::Win32::Security::{TokenLogonSid, TOKEN_GROUPS};
    // SAFETY: a query on our own token with a buffer of the reported size.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let token = super::child::Owned(token);
        let mut len = 0u32;
        let _ = GetTokenInformation(token.0, TokenLogonSid, None, 0, &mut len);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        GetTokenInformation(token.0, TokenLogonSid, Some(buf.as_mut_ptr().cast()), len, &mut len).ok()?;
        let groups = &*(buf.as_ptr() as *const TOKEN_GROUPS);
        if groups.GroupCount == 0 {
            return None;
        }
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(groups.Groups[0].Sid, &mut s).ok()?;
        let out = s.to_string().ok();
        let _ = LocalFree(Some(HLOCAL(s.0.cast())));
        out
    }
}

/// The owner, DACL and label of the named mutex as Windows assigned them
/// (SDDL), or why they cannot be read.
pub fn object_sddl(name: &str) -> String {
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SE_KERNEL_OBJECT,
    };
    use windows::Win32::Security::{DACL_SECURITY_INFORMATION, LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION};
    const READ_CONTROL: u32 = 0x0002_0000;
    let n = wide(name);
    // SAFETY: open for READ_CONTROL; the descriptor and string are freed
    // with LocalFree.
    unsafe {
        let h = match OpenMutexW(SYNCHRONIZATION_ACCESS_RIGHTS(READ_CONTROL), false, PCWSTR(n.as_ptr())) {
            Ok(h) => super::child::Owned(h),
            Err(e) => return format!("(cannot open for READ_CONTROL: {e})"),
        };
        let info = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let rc = GetSecurityInfo(h.0, SE_KERNEL_OBJECT, info, None, None, None, None, Some(&mut sd));
        if rc.is_err() {
            return format!("(GetSecurityInfo failed: {})", rc.0);
        }
        let mut s = PWSTR::null();
        let out =
            if ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, SDDL_REVISION_1, info, &mut s, None).is_ok() {
                let t = s.to_string().unwrap_or_default();
                let _ = LocalFree(Some(HLOCAL(s.0.cast())));
                t
            } else {
                "(cannot format)".into()
            };
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        out
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        if self.0.is_invalid() || self.0 .0.is_null() {
            return;
        }
        // SAFETY: a mutex this thread owns.
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Lowers this process's integrity level to Low (always allowed).
    pub(crate) fn lower_to_low_integrity() -> Result<(), String> {
        use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
        use windows::Win32::Security::{
            SetTokenInformation, TokenIntegrityLevel, PSID, SID_AND_ATTRIBUTES, TOKEN_ADJUST_DEFAULT,
            TOKEN_MANDATORY_LABEL,
        };
        // SAFETY: own token; the SID is freed with LocalFree.
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_DEFAULT | TOKEN_QUERY, &mut token)
                .map_err(|e| e.to_string())?;
            let token = super::super::child::Owned(token);
            let s = wide("S-1-16-4096");
            let mut sid = PSID::default();
            ConvertStringSidToSidW(PCWSTR(s.as_ptr()), &mut sid).map_err(|e| e.to_string())?;
            let label = TOKEN_MANDATORY_LABEL {
                Label: SID_AND_ATTRIBUTES {
                    Sid: sid,
                    Attributes: 0x20, // SE_GROUP_INTEGRITY
                },
            };
            let r = SetTokenInformation(
                token.0,
                TokenIntegrityLevel,
                &label as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32,
            );
            let _ = LocalFree(Some(HLOCAL(sid.0)));
            r.map_err(|e| e.to_string())
        }
    }

    /// The name [`WriterLock::acquire`] uses in tests: unique to this test
    /// process, never the production per-user name.
    pub(crate) fn process_writer_name() -> String {
        format!("Global\\KeyDock.WriterTestProcess.{}", std::process::id())
    }

    fn assert_writer_descriptor(name: &str, sid: &str) {
        use windows::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
        use windows::Win32::Security::DACL_SECURITY_INFORMATION;

        // Windows may render the same SID as an alias, such as LA on a
        // runner using the built-in Administrator account. Format the expected
        // DACL with the same Windows API before comparing the complete DACL.
        let expected = wide(&format!("D:P(A;;0x1f0001;;;{sid})(A;;0x1f0001;;;SY)"));
        let expected = unsafe {
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(expected.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
            .expect("parse expected DACL");
            let mut text = PWSTR::null();
            let result = ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                None,
            );
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
            result.expect("format expected DACL");
            let decoded = text.to_string();
            let _ = LocalFree(Some(HLOCAL(text.0.cast())));
            decoded.expect("decode expected DACL")
        };
        let sddl = super::object_sddl(name);
        let (_, sections) = sddl.split_once("D:").unwrap_or_else(|| panic!("DACL missing: {sddl}"));
        let (dacl, label) = sections
            .split_once("S:")
            .unwrap_or_else(|| panic!("label missing: {sddl}"));
        assert_eq!(format!("D:{dacl}"), expected, "{sddl}");
        assert!(label.contains("(ML;;NW;;;LW)"), "{sddl}");
    }

    #[test]
    fn acquire_uses_a_test_name_with_the_production_descriptor() {
        // Review S6-06: what the restore worker takes in tests. It holds that
        // lock, so it shares the worker tests' guard (review S7-04).
        let _g = super::super::langorder::tests::previous_guard();
        if appcontainer_fails_closed("acquire_uses_a_test_name_with_the_production_descriptor") {
            return;
        }
        let held = owned(WriterLock::acquire(Duration::from_secs(5), None));
        let (production, sid) = writer_name().unwrap();
        assert_ne!(process_writer_name(), production);
        assert_writer_descriptor(&process_writer_name(), &sid);
        // The production lock is untouched: it does not exist because of
        // this test (another KeyDock may hold it; then it is just busy).
        drop(held);
    }

    /// Tries the lock `name` once; "busy", "owned", "abandoned" or "failed: ...".
    pub(crate) fn try_lock(name: &str) -> String {
        let (_, sid) = writer_name().unwrap();
        match WriterLock::acquire_name(name, &sid, Duration::from_millis(200), None) {
            Acquire::Busy => "busy".into(),
            Acquire::Owned(_) => "owned".into(),
            Acquire::Abandoned(_) => "abandoned".into(),
            Acquire::Cancelled => "cancelled".into(),
            Acquire::Failed(e) => format!("failed: {e}"),
        }
    }

    #[test]
    fn a_lower_integrity_process_sees_the_lock_as_busy() {
        // Review TP-02 (mixed integrity): a process of the same user at a
        // lower integrity level must be able to open the lock a higher one
        // holds, and see it as busy (it used to be "cannot open", which the
        // old code treated as permission to write).
        if appcontainer_fails_closed("a_lower_integrity_process_sees_the_lock_as_busy") {
            return;
        }
        use super::super::child::{tests as c, Job};
        let name = format!("Global\\KeyDock.WriterTestIL.{}", std::process::id());
        let _held = hold(&name);
        let (job, _) = Job::create(None).unwrap();
        let ready = std::env::temp_dir().join(format!("keydock-il-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&ready);
        let child = c::spawn_helper(&job, &format!("lowlock:{name}"), &ready);
        let f = child.wait(Duration::from_secs(60), None).unwrap();
        let r = f
            .stderr
            .lines()
            .find_map(|l| l.strip_prefix("LOWLOCK="))
            .unwrap_or("(no result)")
            .to_string();
        assert_eq!(r, "busy", "{}", f.stderr);
    }

    /// Takes the writer lock `name` (for the helper process).
    pub(crate) fn hold(name: &str) -> WriterLock {
        let (_, sid) = writer_name().unwrap();
        owned(WriterLock::acquire_name(name, &sid, Duration::from_secs(5), None))
    }

    #[test]
    fn another_process_holding_the_lock_is_busy_then_abandoned() {
        // Concurrent tray and CLI (or another session): a different process
        // holds the per-user lock; when it is killed, the next owner learns
        // that a change may have been cut short.
        if appcontainer_fails_closed("another_process_holding_the_lock_is_busy_then_abandoned") {
            return;
        }
        use super::super::child::{tests as c, Job};
        let name = format!("Global\\KeyDock.WriterTestX.{}", std::process::id());
        let (job, _) = Job::create(None).unwrap();
        let ready = std::env::temp_dir().join(format!("keydock-lock-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&ready);
        let child = c::spawn_helper(&job, &format!("lock:{name}"), &ready);
        let (_, child) = c::wait_ready_child(child, &ready);
        let (_, sid) = writer_name().unwrap();
        assert!(matches!(
            WriterLock::acquire_name(&name, &sid, Duration::from_millis(100), None),
            Acquire::Busy
        ));
        // A waiter is in place when the owner is killed (abandonment is seen
        // by whoever holds a handle at that moment; with no handle left the
        // object is gone and the next acquisition simply creates it again).
        let (n2, s2) = (name.clone(), sid.clone());
        let waiter = std::thread::spawn(move || {
            matches!(
                WriterLock::acquire_name(&n2, &s2, Duration::from_secs(30), None),
                Acquire::Abandoned(_)
            )
        });
        std::thread::sleep(Duration::from_millis(200));
        job.terminate();
        let _ = child.wait(Duration::from_secs(5), None);
        assert!(
            waiter.join().unwrap(),
            "the waiting process learns the owner died mid-change"
        );
        let _ = std::fs::remove_file(&ready);
    }

    fn owned(a: Acquire) -> WriterLock {
        match a {
            Acquire::Owned(l) | Acquire::Abandoned(l) => l,
            Acquire::Busy => panic!("busy"),
            Acquire::Cancelled => panic!("cancelled"),
            Acquire::Failed(e) => panic!("failed: {e}"),
        }
    }

    fn test_lock(timeout: Duration, cancel: Option<HANDLE>) -> Acquire {
        let (_, sid) = writer_name().unwrap();
        let name = format!("Global\\KeyDock.WriterTest.{}", std::process::id());
        WriterLock::acquire_name(&name, &sid, timeout, cancel)
    }

    #[test]
    fn writer_lock_states() {
        if appcontainer_fails_closed("writer_lock_states") {
            return;
        }
        // A name of its own, so other tests cannot take the lock meanwhile.
        let a = owned(test_lock(Duration::from_secs(5), None));
        // Contention: another thread sees Busy.
        let t = std::thread::spawn(|| matches!(test_lock(Duration::from_millis(50), None), Acquire::Busy));
        assert!(t.join().unwrap(), "held by the other thread");
        // Cancellation while waiting (review TP-03): Cancelled, not a launch.
        let ev = super::super::child::Event::new().unwrap();
        ev.set();
        let h = ev.handle().0 as isize;
        let t = std::thread::spawn(move || {
            matches!(
                test_lock(Duration::from_secs(30), Some(HANDLE(h as *mut _))),
                Acquire::Cancelled
            )
        });
        assert!(t.join().unwrap());
        drop(a);
        // Abandoned: a thread takes the lock and ends without releasing it.
        let t = std::thread::spawn(|| {
            let l = owned(test_lock(Duration::from_secs(5), None));
            std::mem::forget(l);
        });
        t.join().unwrap();
        assert!(
            matches!(test_lock(Duration::from_secs(5), None), Acquire::Abandoned(_)),
            "the next owner learns the previous one ended mid-change"
        );
        let t = std::thread::spawn(|| matches!(test_lock(Duration::from_secs(5), None), Acquire::Owned(_)));
        assert!(t.join().unwrap(), "free again");
        // The test lock is created exactly as the production lock (review
        // F5-05: the real per-user lock is never taken by a test, so a
        // running KeyDock is not contended): the same descriptor, and the
        // production name is the per-user one.
        let held = owned(test_lock(Duration::from_secs(5), None));
        let name = format!("Global\\KeyDock.WriterTest.{}", std::process::id());
        let (_, sid) = writer_name().unwrap();
        assert_writer_descriptor(&name, &sid);
        drop(held);
        assert_eq!(writer_name().unwrap().0, format!("Global\\KeyDock.Writer.{sid}"));
    }

    /// In an AppContainer (some sandboxed hosts) the same token cannot
    /// reopen the per-user lock (review F5-05): KeyDock must then fail
    /// closed. Returns true, after checking that, when this runs in one;
    /// the callers' ordinary-Windows checks do not apply there.
    fn appcontainer_fails_closed(test: &str) -> bool {
        if !token_facts().contains("AppContainer true") {
            return false;
        }
        let (_, sid) = writer_name().unwrap();
        let name = format!("Global\\KeyDock.WriterTestAC.{}", std::process::id());
        let first = WriterLock::acquire_name(&name, &sid, Duration::from_millis(100), None);
        let n2 = name.clone();
        // Another thread (the lock is per thread); only its description
        // comes back.
        let second = std::thread::spawn(move || {
            describe(&WriterLock::acquire_name(&n2, &sid, Duration::from_millis(100), None))
        })
        .join()
        .unwrap_or_else(|_| "panicked".into());
        let second_owned = second == "owned" || second == "abandoned";
        eprintln!(
            "{test}: AppContainer token ({}); first acquisition {}, second {second}; ordinary-Windows checks skipped",
            token_facts(),
            describe(&first)
        );
        assert!(!second_owned, "two owners at once in an AppContainer");
        drop(first);
        true
    }

    fn describe(a: &Acquire) -> String {
        match a {
            Acquire::Owned(_) => "owned".into(),
            Acquire::Abandoned(_) => "abandoned".into(),
            Acquire::Busy => "busy".into(),
            Acquire::Cancelled => "cancelled".into(),
            Acquire::Failed(e) => format!("failed (fail closed): {e}"),
        }
    }

    /// A token's facts for the diagnosis of review FP-02.
    fn token_facts() -> String {
        super::token_facts()
    }

    /// Runs `f` on a new thread impersonating a restricted copy of this
    /// process's token whose restricting SID is `restricting` (what some
    /// sandboxes and agent hosts run under).
    fn with_restricted<R: Send + 'static>(
        restricting: &str,
        disable: bool,
        f: impl FnOnce() -> R + Send + 'static,
    ) -> Result<R, String> {
        use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
        use windows::Win32::Security::{
            CreateRestrictedToken, ImpersonateLoggedOnUser, RevertToSelf, CREATE_RESTRICTED_TOKEN_FLAGS, PSID,
            SID_AND_ATTRIBUTES, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_IMPERSONATE,
        };
        let restricting = restricting.to_string();
        std::thread::spawn(move || -> Result<R, String> {
            // SAFETY: own token; SID freed with LocalFree; impersonation is
            // reverted before the thread ends.
            unsafe {
                let mut token = HANDLE::default();
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_IMPERSONATE | TOKEN_ASSIGN_PRIMARY,
                    &mut token,
                )
                .map_err(|e| e.to_string())?;
                let token = super::super::child::Owned(token);
                let s = wide(&restricting);
                let mut sid = PSID::default();
                ConvertStringSidToSidW(PCWSTR(s.as_ptr()), &mut sid).map_err(|e| e.to_string())?;
                let list = [SID_AND_ATTRIBUTES {
                    Sid: sid,
                    Attributes: 0,
                }];
                let mut restricted = HANDLE::default();
                // Either restrict with the SID, or make it deny-only.
                let (disabled, restricting): (Option<&[SID_AND_ATTRIBUTES]>, Option<&[SID_AND_ATTRIBUTES]>) = if disable
                {
                    (Some(&list), None)
                } else {
                    (None, Some(&list))
                };
                let r = CreateRestrictedToken(
                    token.0,
                    CREATE_RESTRICTED_TOKEN_FLAGS(0),
                    disabled,
                    None,
                    restricting,
                    &mut restricted,
                );
                let _ = LocalFree(Some(HLOCAL(sid.0)));
                r.map_err(|e| e.to_string())?;
                let restricted = super::super::child::Owned(restricted);
                ImpersonateLoggedOnUser(restricted.0).map_err(|e| e.to_string())?;
                let out = f();
                let _ = RevertToSelf();
                Ok(out)
            }
        })
        .join()
        .map_err(|_| "thread panicked".to_string())?
    }

    fn try_open(name: &str) -> Result<(), String> {
        let n = wide(name);
        // SAFETY: open by name with the rights KeyDock uses.
        unsafe {
            OpenMutexW(
                SYNCHRONIZATION_ACCESS_RIGHTS(SYNCHRONIZE | MUTEX_MODIFY_STATE),
                false,
                PCWSTR(n.as_ptr()),
            )
        }
        .map(|h| {
            // SAFETY: just opened.
            unsafe {
                let _ = CloseHandle(h);
            }
        })
        .map_err(|e| e.to_string())
    }

    /// Review FP-02 diagnosis: prints this token, the descriptor Windows
    /// assigned to the lock object, and which tokens can reopen it. Asserts
    /// only what must hold everywhere: the same token reopens its own lock.
    #[test]
    fn writer_lock_descriptor_probe() {
        let (_, sid) = writer_name().unwrap();
        eprintln!("token: {}", token_facts());
        if appcontainer_fails_closed("writer_lock_descriptor_probe") {
            return;
        }
        let name = format!("Global\\KeyDock.WriterProbe.{}", std::process::id());
        let h = open_or_create(&name, &sid).unwrap();
        let h = super::super::child::Owned(h);
        eprintln!("assigned descriptor: {}", super::object_sddl(&name));
        // Same token, same process (what failed on the reviewer's host).
        assert_eq!(try_open(&name), Ok(()), "the same token reopens the lock");
        // Default descriptor for comparison.
        let dname = format!("Global\\KeyDock.WriterProbeDefault.{}", std::process::id());
        let dn = wide(&dname);
        // SAFETY: an unnamed-security mutex for comparison only.
        let d = super::super::child::Owned(
            unsafe { CreateMutexExW(None, PCWSTR(dn.as_ptr()), 0, SYNCHRONIZE | MUTEX_MODIFY_STATE) }.unwrap(),
        );
        eprintln!("default descriptor: {}", super::object_sddl(&dname));
        let logon = super::logon_sid().unwrap_or_default();
        for (what, restricting, disable) in [
            ("restricted to the user SID", sid.clone(), false),
            ("restricted to the logon SID", logon.clone(), false),
            ("restricted to RESTRICTED", "S-1-5-12".to_string(), false),
            ("with the user SID deny-only", sid.clone(), true),
        ] {
            if restricting.is_empty() {
                continue;
            }
            let (n1, n2) = (name.clone(), dname.clone());
            let r = with_restricted(&restricting, disable, move || (try_open(&n1), try_open(&n2)));
            eprintln!(
                "token {what} ({restricting}): KeyDock descriptor {:?}, default descriptor {:?}",
                r.as_ref().map(|x| &x.0),
                r.as_ref().map(|x| &x.1)
            );
            if disable {
                // The reviewer's host (FP-02): a token whose user SID is
                // deny-only cannot open the per-user lock, while the default
                // descriptor (which also grants Administrators) lets it. That
                // token is not acting as the user: fail closed is intended.
                let (ours, default) = r.unwrap();
                assert!(ours.is_err(), "per-user descriptor denies a deny-only user SID");
                assert!(default.is_ok() || !token_facts().contains("elevated true"));
            }
        }
        drop((h, d));
    }

    #[test]
    fn remote_sessions_opt_in_lifts_the_block() {
        let cfg = Config {
            remote_sessions: true,
            ..Default::default()
        };
        assert_eq!(blocked(&cfg), None);
        let sid = user_sid().expect("the user's SID");
        assert!(sid.starts_with("S-1-"), "{sid}");
        assert!(writer_name().unwrap().0.starts_with("Global\\KeyDock.Writer.S-1-"));
    }
}
