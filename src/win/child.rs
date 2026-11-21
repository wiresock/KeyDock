//! Child processes that cannot outlive KeyDock.
//!
//! The child is created directly inside a job object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (`PROC_THREAD_ATTRIBUTE_JOB_LIST`,
//! Windows 10 and later), so there is no moment in which it exists outside
//! the job, not even if KeyDock is killed during `CreateProcessW`. It is also
//! created suspended and only resumed after its membership is confirmed
//! (`IsProcessInJob`); its own children inherit the job. Only its three
//! standard handles are inherited (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`).
//! When KeyDock exits in any way (Exit, sign-out, an installer's Restart
//! Manager, a crash, `TerminateProcess` by a forced MSI close), the last
//! handle to the job closes and Windows terminates everything in it. If the
//! job cannot be attached at creation (older Windows), the suspended child
//! is assigned afterwards and terminated if that fails.
//!
//! Waiting is bounded: a timeout, a cancel event, and a bounded wait for the
//! stderr pipe after the process ended (a descendant could hold it open; the
//! job is terminated first, which closes it).

use std::ffi::c_void;
use std::io::Read;
use std::os::windows::io::FromRawHandle;
use std::time::Duration;
use windows::core::BOOL;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, SetHandleInformation, ERROR_ALREADY_EXISTS, GENERIC_READ, GENERIC_WRITE, HANDLE,
    HANDLE_FLAGS, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, OpenJobObjectW, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

/// Job object access rights (winnt.h).
const JOB_OBJECT_QUERY: u32 = 0x0004;
const JOB_OBJECT_TERMINATE: u32 = 0x0008;
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateEventW, CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, InitializeProcThreadAttributeList,
    ResumeThread, SetEvent, TerminateProcess, UpdateProcThreadAttribute, WaitForMultipleObjects, WaitForSingleObject,
    CREATE_NO_WINDOW, CREATE_SUSPENDED, EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, STARTUPINFOW,
};

use super::registry::wide;

/// An owned kernel handle, closed on drop.
pub struct Owned(pub HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0 .0.is_null() {
            // SAFETY: a handle this value owns.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

// SAFETY: kernel handles may be used from any thread.
unsafe impl Send for Owned {}
// SAFETY: the handles wrapped here are only used with thread-safe kernel calls.
unsafe impl Sync for Owned {}

/// A manual-reset event used to cancel a wait.
pub struct Event(Owned);

impl Event {
    pub fn new() -> Result<Self, String> {
        // SAFETY: unnamed manual-reset event, initially not signalled.
        let h = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(|e| format!("CreateEventW: {e}"))?;
        Ok(Self(Owned(h)))
    }

    pub fn set(&self) {
        // SAFETY: an event handle owned by self.
        unsafe {
            let _ = SetEvent(self.0 .0);
        }
    }

    pub fn handle(&self) -> HANDLE {
        self.0 .0
    }

    pub fn is_set(&self) -> bool {
        // SAFETY: a plain zero-timeout wait on our event.
        unsafe { WaitForSingleObject(self.0 .0, 0) == WAIT_OBJECT_0 }
    }
}

/// A job object whose processes die when its last handle closes.
pub struct Job(Owned);

impl Job {
    /// Creates (or, if a job of that name still exists, opens) a kill-on-close
    /// job. The flag says whether it already existed.
    pub fn create(name: Option<&str>) -> Result<(Self, bool), String> {
        let n = name.map(wide);
        let pname = n.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()));
        // SAFETY: optional NUL-terminated name; default security.
        let h = unsafe { CreateJobObjectW(None, pname) }.map_err(|e| format!("CreateJobObjectW: {e}"))?;
        // SAFETY: plain query right after the call.
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let job = Self(Owned(h));
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        // SAFETY: the structure and its size match the information class.
        unsafe {
            SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .map_err(|e| format!("SetInformationJobObject: {e}"))?;
        Ok((job, existed))
    }

    pub fn handle(&self) -> HANDLE {
        self.0 .0
    }

    /// Terminates every process in the job.
    pub fn terminate(&self) {
        // SAFETY: a job handle owned by self.
        unsafe {
            let _ = TerminateJobObject(self.0 .0, 1);
        }
    }

    /// Processes currently in the job.
    pub fn active_processes(&self) -> Option<u32> {
        active_processes(self.0 .0)
    }
}

fn active_processes(job: HANDLE) -> Option<u32> {
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    // SAFETY: the structure and its size match the information class.
    unsafe {
        QueryInformationJobObject(
            Some(job),
            JobObjectBasicAccountingInformation,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            None,
        )
    }
    .ok()?;
    Some(info.ActiveProcesses)
}

/// Waits up to `timeout` until `job` has no processes; true when empty.
fn wait_empty(job: HANDLE, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        match active_processes(job) {
            Some(0) => return true,
            None => return false,
            Some(_) if start.elapsed() >= timeout => return false,
            Some(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// A stale job that is not known to be empty: its processes did not end in
/// time (`job` is kept open so the caller can hold back every change until
/// it is empty), or it exists but cannot be opened or queried (`job` is
/// `None`: uncertain, not absent; review FP-01).
pub struct Unconfirmed {
    pub job: Option<Owned>,
    /// Processes still in it when the wait gave up (`u32::MAX`: unknown).
    pub left: u32,
    pub reason: String,
}

impl std::fmt::Debug for Unconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Unconfirmed({}; {} left)", self.reason, self.left)
    }
}

impl PartialEq for Unconfirmed {
    fn eq(&self, other: &Self) -> bool {
        self.left == other.left && self.job.is_some() == other.job.is_some()
    }
}

/// Processes in the job behind `h` (`None` if it cannot be queried).
pub fn job_processes(h: HANDLE) -> Option<u32> {
    active_processes(h)
}

/// If a job of this name exists (a previous KeyDock instance in this session
/// is still ending, or was killed while its child ran), terminates its
/// processes and waits until they are gone. `Ok(None)`: no such job;
/// `Ok(Some(n))`: it had `n` processes, all ended; `Err`: termination could
/// not be confirmed, or the job exists but cannot be opened or queried.
pub fn stop_stale(name: &str) -> Result<Option<u32>, Unconfirmed> {
    let n = wide(name);
    // SAFETY: NUL-terminated name.
    let h = match unsafe { OpenJobObjectW(JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE, false, PCWSTR(n.as_ptr())) } {
        Ok(h) => Owned(h),
        // Only "not found" means there is no such job.
        Err(e) if e.code() == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND.to_hresult() => return Ok(None),
        Err(e) => {
            return Err(Unconfirmed {
                job: None,
                left: u32::MAX,
                reason: format!("the job exists but cannot be opened: {e}"),
            })
        }
    };
    let Some(count) = active_processes(h.0) else {
        return Err(Unconfirmed {
            job: Some(h),
            left: u32::MAX,
            reason: "the job cannot be queried".into(),
        });
    };
    if count > 0 {
        // SAFETY: a job handle opened above.
        unsafe {
            let _ = TerminateJobObject(h.0, 1);
        }
        if !wait_empty(h.0, Duration::from_secs(5)) {
            let left = active_processes(h.0).unwrap_or(u32::MAX);
            return Err(Unconfirmed {
                job: Some(h),
                left,
                reason: "its processes did not end".into(),
            });
        }
    }
    Ok(Some(count))
}

/// How a child ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    Exited(u32),
    /// Stopped after the timeout.
    TimedOut,
    /// Stopped because the cancel event was set.
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct Finished {
    pub end: End,
    /// What the child wrote to stderr (trimmed; may be cut short).
    pub stderr: String,
}

/// Quotes one argument for `CreateProcessW` / `CommandLineToArgvW`.
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"', '\n']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// A `PROC_THREAD_ATTRIBUTE_LIST`, deleted on drop.
struct AttrList {
    buf: Vec<u8>,
}

impl AttrList {
    fn new(count: u32) -> Result<Self, String> {
        let mut size = 0usize;
        // SAFETY: size query (fails with ERROR_INSUFFICIENT_BUFFER by design).
        let _ = unsafe { InitializeProcThreadAttributeList(None, count, None, &mut size) };
        if size == 0 {
            return Err("InitializeProcThreadAttributeList reported no size".into());
        }
        let mut a = Self { buf: vec![0u8; size] };
        // SAFETY: a buffer of the reported size.
        unsafe { InitializeProcThreadAttributeList(Some(a.list()), count, None, &mut size) }
            .map_err(|e| format!("InitializeProcThreadAttributeList: {e}"))?;
        Ok(a)
    }

    fn list(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.buf.as_mut_ptr().cast())
    }
}

impl Drop for AttrList {
    fn drop(&mut self) {
        // SAFETY: initialised in `new`.
        unsafe { DeleteProcThreadAttributeList(self.list()) };
    }
}

/// A running, contained child.
pub struct Child {
    process: Owned,
    reader: Option<std::sync::mpsc::Receiver<String>>,
    job: HANDLE,
}

// SAFETY: kernel handles may be used from any thread.
unsafe impl Send for Child {}

fn inheritable_nul(write: bool) -> Result<Owned, String> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    let access = if write { GENERIC_WRITE.0 } else { GENERIC_READ.0 };
    // SAFETY: NUL-terminated device name; valid attributes.
    let h = unsafe {
        CreateFileW(
            windows::core::w!("NUL"),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            Some(&sa),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|e| format!("cannot open NUL: {e}"))?;
    Ok(Owned(h))
}

impl Child {
    /// Starts `exe` with `args` inside `job` (see the module documentation),
    /// hidden, stdin and stdout on NUL, stderr captured.
    pub fn spawn(job: &Job, exe: &std::path::Path, args: &[&str]) -> Result<Child, String> {
        let mut cmd = quote_arg(&exe.to_string_lossy());
        for a in args {
            cmd.push(' ');
            cmd.push_str(&quote_arg(a));
        }
        let mut cmd_w = wide(&cmd);
        let exe_w = wide(&exe.to_string_lossy());

        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        let (mut rd, mut wr) = (HANDLE::default(), HANDLE::default());
        // SAFETY: out pointers to local handles; inheritable pipe.
        unsafe { CreatePipe(&mut rd, &mut wr, Some(&sa), 0) }.map_err(|e| format!("CreatePipe: {e}"))?;
        let rd = Owned(rd);
        let wr = Owned(wr);
        // Only the child's end is inherited.
        // SAFETY: a pipe handle created above.
        unsafe { SetHandleInformation(rd.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) }
            .map_err(|e| format!("SetHandleInformation: {e}"))?;
        let nul_in = inheritable_nul(false)?;
        let nul_out = inheritable_nul(true)?;

        // Attributes: inherit only these three handles, and create the
        // process inside the job.
        let inherit = [nul_in.0, nul_out.0, wr.0];
        let jobs = [job.handle()];
        let mut attrs = AttrList::new(2)?;
        // SAFETY: `inherit` and `jobs` outlive CreateProcessW below; sizes match.
        unsafe {
            UpdateProcThreadAttribute(
                attrs.list(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(inherit.as_ptr().cast()),
                std::mem::size_of_val(&inherit),
                None,
                None,
            )
        }
        .map_err(|e| format!("cannot restrict inherited handles: {e}"))?;
        // SAFETY: as above.
        let job_at_creation = unsafe {
            UpdateProcThreadAttribute(
                attrs.list(),
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                Some(jobs.as_ptr().cast()),
                std::mem::size_of_val(&jobs),
                None,
                None,
            )
        }
        .is_ok();
        let si = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: nul_in.0,
                hStdOutput: nul_out.0,
                hStdError: wr.0,
                ..Default::default()
            },
            lpAttributeList: attrs.list(),
        };
        let mut pi = PROCESS_INFORMATION::default();
        // SAFETY: valid NUL-terminated strings (the command line is mutable as
        // required), extended start-up information and out structure.
        unsafe {
            CreateProcessW(
                PCWSTR(exe_w.as_ptr()),
                Some(PWSTR(cmd_w.as_mut_ptr())),
                None,
                None,
                true,
                CREATE_SUSPENDED | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
                None,
                PCWSTR::null(),
                &si.StartupInfo,
                &mut pi,
            )
        }
        .map_err(|e| format!("cannot run {}: {e}", exe.display()))?;
        drop(attrs);
        let process = Owned(pi.hProcess);
        let thread = Owned(pi.hThread);
        // Our copies of the child's handles: once closed, the pipe reports
        // end-of-file when the child (and anything it started) is gone.
        drop((wr, nul_in, nul_out));

        let assigned = if job_at_creation {
            Ok(())
        } else {
            // SAFETY: handles from CreateProcessW and the job.
            unsafe { AssignProcessToJobObject(job.handle(), process.0) }
        };
        let mut in_job = BOOL(0);
        // SAFETY: valid handles and out pointer.
        let confirmed = assigned.and_then(|_| unsafe { IsProcessInJob(process.0, Some(job.handle()), &mut in_job) });
        if let Err(e) = confirmed.and_then(|_| {
            if in_job.as_bool() {
                Ok(())
            } else {
                Err(windows::core::Error::new(windows::core::HRESULT(-1), "not in the job"))
            }
        }) {
            // Never let it run uncontained.
            // SAFETY: our suspended child.
            unsafe {
                let _ = TerminateProcess(process.0, 1);
                let _ = WaitForSingleObject(process.0, 5000);
            }
            return Err(format!("cannot contain {} in a job: {e}", exe.display()));
        }
        // SAFETY: the primary thread of our suspended child.
        if unsafe { ResumeThread(thread.0) } == u32::MAX {
            job.terminate();
            return Err("ResumeThread failed".into());
        }
        drop(thread);

        // Read stderr on its own thread so a chatty child cannot fill the
        // pipe and stall; the result comes back over a channel so waiting
        // for it can be bounded.
        let (tx, rx) = std::sync::mpsc::channel();
        // SAFETY: we own the read end; File takes it over and closes it.
        let mut file = unsafe { std::fs::File::from_raw_handle(rd.0 .0) };
        std::mem::forget(rd);
        let spawned = std::thread::Builder::new().name("child-stderr".into()).spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut file).take(64 * 1024).read_to_end(&mut buf);
            let _ = tx.send(String::from_utf8_lossy(&buf).trim().to_string());
        });
        let reader = spawned.ok().map(|_| rx);
        Ok(Child {
            process,
            reader,
            job: job.handle(),
        })
    }

    /// Waits until the child exits, `timeout` passes or `cancel` is set. On
    /// timeout or cancel the whole job is terminated and the end confirmed.
    pub fn wait(self, timeout: Duration, cancel: Option<&Event>) -> Result<Finished, String> {
        let mut handles = vec![self.process.0];
        if let Some(c) = cancel {
            handles.push(c.handle());
        }
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: valid handles owned by self or the caller.
        let r = unsafe { WaitForMultipleObjects(&handles, false, ms) };
        let end = if r == WAIT_OBJECT_0 {
            let mut code = 0u32;
            // SAFETY: a process handle that has ended.
            unsafe { GetExitCodeProcess(self.process.0, &mut code) }.map_err(|e| format!("GetExitCodeProcess: {e}"))?;
            End::Exited(code)
        } else if r.0 == WAIT_OBJECT_0.0 + 1 {
            End::Cancelled
        } else if r == WAIT_TIMEOUT {
            End::TimedOut
        } else {
            // WAIT_FAILED: do not leave a live child behind.
            self.stop()?;
            return Err("waiting for the child process failed; it was stopped".into());
        };
        if !matches!(end, End::Exited(_)) {
            self.stop()?;
        }
        // Descendants may still hold the pipe: end them too, then give the
        // reader a short, bounded time.
        // SAFETY: the job handle outlives this call (owned by the caller's Job).
        unsafe {
            let _ = TerminateJobObject(self.job, 1);
        }
        let stderr = self
            .reader
            .as_ref()
            .and_then(|r| r.recv_timeout(Duration::from_secs(2)).ok())
            .unwrap_or_default();
        Ok(Finished { end, stderr })
    }

    /// Terminates the job and confirms that the child ended.
    fn stop(&self) -> Result<(), String> {
        // SAFETY: handles owned by self / the caller's Job.
        unsafe {
            let _ = TerminateJobObject(self.job, 1);
            let _ = TerminateProcess(self.process.0, 1);
            if WaitForSingleObject(self.process.0, 5000) != WAIT_OBJECT_0 {
                return Err("the child process did not end after it was terminated".into());
            }
        }
        Ok(())
    }

    /// The process handle (tests).
    #[cfg(test)]
    pub fn process(&self) -> HANDLE {
        self.process.0
    }

    /// Whether the process has ended (tests).
    #[cfg(test)]
    pub fn has_exited(&self) -> bool {
        // SAFETY: a zero-timeout wait on our process handle.
        unsafe { WaitForSingleObject(self.process.0, 0) == WAIT_OBJECT_0 }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! Real child processes. The helper is this test binary itself, started
    //! with a `kdhelper=<mode>|<ready file>` argument (an extra test-name
    //! filter that matches nothing, so only `helper_entry` runs). It writes a
    //! ready file and then sleeps (no network or other tools involved), so a
    //! test only times out or cancels a child that is known to be running.
    //! A helper that exits before it is ready fails the test with its exit
    //! code and stderr. Every helper runs in a job the test owns; cleanup
    //! only ever targets that job or a handle the test retained.
    use super::*;

    const HELPER: &str = "win::child::tests::helper_entry";

    fn cmd() -> std::path::PathBuf {
        std::path::PathBuf::from(std::env::var("ComSpec").unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".into()))
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("keydock-helper-{name}-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// Waits for a ready file written by a process the caller cannot watch
    /// (a grandchild); returns its content.
    pub(crate) fn wait_ready(path: &std::path::Path) -> String {
        let start = std::time::Instant::now();
        loop {
            if let Ok(s) = std::fs::read_to_string(path) {
                if s.ends_with('\n') {
                    return s.trim().to_string();
                }
            }
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "{} was not written within 60 s",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for `child`'s ready file. If the helper exits first, fails with
    /// its exit code and stderr instead of a bare timeout (review FP-08).
    pub(crate) fn wait_ready_child(child: Child, path: &std::path::Path) -> (String, Child) {
        let start = std::time::Instant::now();
        loop {
            if let Ok(s) = std::fs::read_to_string(path) {
                if s.ends_with('\n') {
                    return (s.trim().to_string(), child);
                }
            }
            if child.has_exited() {
                // It may have written the file just before exiting.
                if let Ok(s) = std::fs::read_to_string(path) {
                    if s.ends_with('\n') {
                        return (s.trim().to_string(), child);
                    }
                }
                let f = child.wait(Duration::from_secs(5), None);
                panic!("the helper exited before it was ready: {f:?}");
            }
            if start.elapsed() > Duration::from_secs(60) {
                let f = child.wait(Duration::from_millis(1), None);
                panic!("the helper did not get ready within 60 s (stopped: {f:?})");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The command-line arguments that start the helper in `mode`.
    pub(crate) fn helper_args(mode: &str, ready: &std::path::Path) -> Vec<String> {
        vec![
            "--exact".into(),
            HELPER.into(),
            "--nocapture".into(),
            format!("kdhelper={mode}|{}", ready.display()),
        ]
    }

    /// Starts the helper in `job`. `mode`: "sleep", "sleep:<secs>", "tree"
    /// (starts a sleeping grandchild first), "orphan-parent[:<secs>]" (starts
    /// a sleeper and exits at once), "lock:<name>", "lowlock:<name>",
    /// "trylock:<name>".
    pub(crate) fn spawn_helper(job: &Job, mode: &str, ready: &std::path::Path) -> Child {
        let args = helper_args(mode, ready);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        Child::spawn(job, &std::env::current_exe().unwrap(), &refs).unwrap()
    }

    /// The helper itself (does nothing in a normal test run).
    #[test]
    fn helper_entry() {
        let Some(spec) = std::env::args().find_map(|a| a.strip_prefix("kdhelper=").map(str::to_string)) else {
            return;
        };
        let (mode, ready) = spec.split_once('|').unwrap();
        let pid = std::process::id();
        match mode {
            "sleep" => {
                std::fs::write(ready, format!("{pid}\n")).unwrap();
                std::thread::sleep(Duration::from_secs(120));
            }
            m if m.starts_with("sleep:") => {
                let secs: u64 = m["sleep:".len()..].parse().unwrap();
                std::fs::write(ready, format!("{pid}\n")).unwrap();
                std::thread::sleep(Duration::from_secs(secs));
            }
            "tree" => {
                // A grandchild, which inherits the job.
                let inner = format!("{ready}.inner");
                let _ = std::fs::remove_file(&inner);
                let mut grand = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(helper_args("sleep", std::path::Path::new(&inner)))
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap();
                let grandchild = wait_ready(std::path::Path::new(&inner));
                std::fs::write(ready, format!("{pid} {grandchild}\n")).unwrap();
                std::thread::sleep(Duration::from_secs(120));
                // Normally the job ends both before this.
                let _ = grand.kill();
                let _ = grand.wait();
            }
            m if m.starts_with("lowlock:") => {
                // Drop this process to Low integrity, then try the lock a
                // higher-integrity process holds; report what happened.
                let name = &m["lowlock:".len()..];
                let r = match super::super::session::tests::lower_to_low_integrity() {
                    Err(e) => format!("cannot lower: {e}"),
                    Ok(()) => super::super::session::tests::try_lock(name),
                };
                // A Low integrity process may not write the ready file
                // (in %TEMP%); the parent reads stderr instead.
                let _ = ready;
                eprintln!("LOWLOCK={r}");
            }
            m if m.starts_with("trylock:") => {
                // Try a writer lock of this name once and report the result
                // on stderr (used to run as another account in the VM).
                let _ = ready;
                eprintln!(
                    "TRYLOCK={}",
                    super::super::session::tests::try_lock(&m["trylock:".len()..])
                );
            }
            m if m.starts_with("lock:") => {
                // Hold a writer lock of this name until killed.
                let name = &m["lock:".len()..];
                let _l = super::super::session::tests::hold(name);
                std::fs::write(ready, format!("{pid}\n")).unwrap();
                std::thread::sleep(Duration::from_secs(120));
            }
            m if m == "orphan-parent" || m.starts_with("orphan-parent:") => {
                let sleep = match m.strip_prefix("orphan-parent:") {
                    Some(secs) => format!("sleep:{secs}"),
                    None => "sleep".to_string(),
                };
                // Start a sleeper (it inherits the job) and exit at once,
                // leaving it without a parent: what an older KeyDock's
                // restore looks like after that KeyDock exited.
                let inner = format!("{ready}.sleeper");
                let _ = std::fs::remove_file(&inner);
                // Deliberately not waited for: it is the orphan (it runs in
                // the test's job, which ends it).
                #[allow(clippy::zombie_processes)]
                let _sleeper = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(helper_args(&sleep, std::path::Path::new(&inner)))
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap();
                let sleeper = wait_ready(std::path::Path::new(&inner));
                std::fs::write(ready, format!("{sleeper}\n")).unwrap();
            }
            other => panic!("unknown helper mode {other}"),
        }
    }

    fn alive(pid: u32) -> bool {
        // SAFETY: a synchronize-only query handle, closed by Owned.
        match unsafe {
            windows::Win32::System::Threading::OpenProcess(
                windows::Win32::System::Threading::PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        } {
            Ok(h) => {
                let h = Owned(h);
                // SAFETY: plain wait on a valid handle.
                unsafe { WaitForSingleObject(h.0, 0) == WAIT_TIMEOUT }
            }
            Err(_) => false,
        }
    }

    /// The job's process count drops a moment before the process object is
    /// signalled; allow that moment.
    fn ends_within(pid: u32) -> bool {
        let start = std::time::Instant::now();
        while alive(pid) {
            if start.elapsed() > Duration::from_secs(5) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_arg("-NoLogo"), "-NoLogo");
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quote_arg("C:\\dir with space\\"), "\"C:\\dir with space\\\\\"");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("$x -split '\\|'"), "\"$x -split '\\|'\"");
    }

    #[test]
    fn exit_code_and_stderr() {
        let (job, _) = Job::create(None).unwrap();
        let c = Child::spawn(&job, &cmd(), &["/d", "/c", "echo oops 1>&2 & exit 7"]).unwrap();
        let f = c.wait(Duration::from_secs(20), None).unwrap();
        assert_eq!(f.end, End::Exited(7));
        assert_eq!(f.stderr, "oops");
    }

    #[test]
    fn child_is_in_the_job_from_creation() {
        let (job, _) = Job::create(None).unwrap();
        let ready = temp("injob");
        let c = spawn_helper(&job, "sleep", &ready);
        let mut in_job = BOOL(0);
        // SAFETY: valid handles and out pointer.
        unsafe { IsProcessInJob(c.process(), Some(job.handle()), &mut in_job) }.unwrap();
        assert!(in_job.as_bool());
        let (_, c) = wait_ready_child(c, &ready);
        job.terminate();
        let _ = c.wait(Duration::from_secs(5), None);
    }

    /// A plain child process that is ended (and waited for) when dropped,
    /// so a failing assertion cannot leave it running.
    struct KillOnDrop(std::process::Child);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A file removed when dropped (on success and on failure).
    struct TempFile(std::path::PathBuf);

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Whether `value`, a handle value in `process`, refers to the kernel
    /// object `ours` (review F5-05). The value is never used in the child:
    /// it is duplicated out of it (only a valid handle can be), and the copy
    /// is compared by object identity, so a value that is invalid there, or
    /// that now names another object, is "not inherited".
    fn inherited_by(process: HANDLE, value: HANDLE, ours: HANDLE) -> Result<bool, String> {
        use windows::Win32::Foundation::{
            CompareObjectHandles, DuplicateHandle, DUPLICATE_SAME_ACCESS, ERROR_INVALID_HANDLE, ERROR_NOT_SUPPORTED,
        };
        use windows::Win32::System::Threading::GetCurrentProcess;
        let mut dup = HANDLE::default();
        // SAFETY: valid process handles (the parent's own, with
        // PROCESS_DUP_HANDLE on the child) and an out pointer.
        let r = unsafe {
            DuplicateHandle(
                process,
                value,
                GetCurrentProcess(),
                &mut dup,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
        };
        match r {
            Ok(()) => {
                let dup = Owned(dup);
                // SAFETY: two valid handles of this process.
                Ok(unsafe { CompareObjectHandles(dup.0, ours) }.as_bool())
            }
            // No such handle there, or one that cannot be duplicated (an
            // event can): not the event.
            Err(e) if e.code() == ERROR_INVALID_HANDLE.to_hresult() || e.code() == ERROR_NOT_SUPPORTED.to_hresult() => {
                Ok(false)
            }
            Err(e) => Err(format!("cannot look into the child: {e}")),
        }
    }

    #[test]
    fn only_the_standard_handles_are_inherited() {
        // Reviews TP-03 and F5-05: an inheritable handle of KeyDock that is
        // not one of the child's three standard handles must not reach it.
        // Both children only sleep; the parent checks each from outside.
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        // SAFETY: an unnamed, inheritable event (nothing else can open it).
        let ev = Owned(unsafe { CreateEventW(Some(&sa), true, false, PCWSTR::null()) }.unwrap());
        // Positive control: a plain child (bInheritHandles, no list) does get
        // it, so "not inherited" below means something.
        let ready = TempFile(temp("inherit-control"));
        let control = KillOnDrop(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(helper_args("sleep:60", &ready.0))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let _ = wait_ready(&ready.0);
        use std::os::windows::io::AsRawHandle;
        let control_process = HANDLE(control.0.as_raw_handle());
        assert_eq!(
            inherited_by(control_process, ev.0, ev.0),
            Ok(true),
            "the control child inherited the event, so the check can see inheritance"
        );
        drop(control);
        // KeyDock's launcher passes only the three standard handles.
        let (job, _) = Job::create(None).unwrap();
        let ready = TempFile(temp("inherit"));
        let c = spawn_helper(&job, "sleep:60", &ready.0);
        let (_, c) = wait_ready_child(c, &ready.0);
        assert_eq!(
            inherited_by(c.process(), ev.0, ev.0),
            Ok(false),
            "the event was not inherited"
        );
        // Only the test's own job is ended.
        job.terminate();
        let _ = c.wait(Duration::from_secs(5), None);
    }

    #[test]
    fn timeout_terminates_the_whole_tree() {
        let (job, _) = Job::create(None).unwrap();
        let ready = temp("tree");
        let c = spawn_helper(&job, "tree", &ready);
        let (r, c) = wait_ready_child(c, &ready);
        let pids: Vec<u32> = r.split(' ').map(|p| p.parse().unwrap()).collect();
        assert!(pids.iter().all(|p| alive(*p)), "both running before the timeout");
        let start = std::time::Instant::now();
        let f = c.wait(Duration::from_millis(300), None).unwrap();
        assert_eq!(f.end, End::TimedOut);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "bounded: {:?}",
            start.elapsed()
        );
        assert!(
            wait_empty(job.handle(), Duration::from_secs(5)),
            "nothing left in the job"
        );
        assert!(pids.iter().all(|p| ends_within(*p)), "child and grandchild ended");
    }

    #[test]
    fn cancel_event_stops_the_child() {
        let (job, _) = Job::create(None).unwrap();
        let ev = Event::new().unwrap();
        let ready = temp("cancel");
        let c = spawn_helper(&job, "sleep", &ready);
        let (r, c) = wait_ready_child(c, &ready);
        let pid: u32 = r.parse().unwrap();
        assert!(alive(pid));
        ev.set();
        let f = c.wait(Duration::from_secs(20), Some(&ev)).unwrap();
        assert_eq!(f.end, End::Cancelled);
        assert_eq!(job.active_processes(), Some(0));
        assert!(ends_within(pid));
    }

    #[test]
    fn closing_the_job_kills_the_child() {
        // What happens when KeyDock itself ends without cleaning up.
        let (job, _) = Job::create(None).unwrap();
        let ready = temp("close");
        let c = spawn_helper(&job, "sleep", &ready);
        let (_, c) = wait_ready_child(c, &ready);
        let p = c.process();
        drop(job);
        // SAFETY: the process handle is still owned by `c`.
        assert_eq!(unsafe { WaitForSingleObject(p, 5000) }, WAIT_OBJECT_0);
        drop(c);
    }

    #[test]
    fn stale_named_job_is_stopped() {
        let name = format!("Local\\KeyDock.Test.{}", std::process::id());
        let (job, existed) = Job::create(Some(&name)).unwrap();
        assert!(!existed);
        let ready = temp("stale");
        let c = spawn_helper(&job, "sleep", &ready);
        let (_, c) = wait_ready_child(c, &ready);
        // A new instance finds the job by name and stops its processes.
        assert_eq!(stop_stale(&name), Ok(Some(1)));
        // SAFETY: the process handle is still owned by `c`.
        assert_eq!(unsafe { WaitForSingleObject(c.process(), 5000) }, WAIT_OBJECT_0);
        let (_again, existed) = Job::create(Some(&name)).unwrap();
        assert!(existed, "same session, the old handle is still open");
        drop(c);
        drop(job);
        assert_eq!(stop_stale("Local\\KeyDock.Test.none"), Ok(None));
    }

    #[test]
    fn inaccessible_stale_job_is_uncertain_not_absent() {
        // Review FP-01: a job that exists but cannot be opened must not be
        // taken for "no previous restore".
        let name = format!("Local\\KeyDock.TestDenied.{}", std::process::id());
        let job = denied_job(&name);
        match stop_stale(&name) {
            Err(u) => {
                assert!(u.job.is_none(), "{u:?}");
                assert!(u.reason.contains("cannot be opened"), "{u:?}");
            }
            other => panic!("an inaccessible job must be uncertain, got {other:?}"),
        }
        drop(job);
        // Gone now: absent.
        assert_eq!(stop_stale(&name), Ok(None));
    }

    /// A named job that everyone (its creator included) is denied query and
    /// terminate access to; it exists while the returned handle is open.
    pub(crate) fn denied_job(name: &str) -> Owned {
        use windows::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows::Win32::Security::PSECURITY_DESCRIPTOR;
        // Everyone is denied query and terminate; the creator keeps its handle.
        let sddl = wide("D:(D;;0x000C;;;WD)(A;;GA;;;WD)");
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: NUL-terminated SDDL; freed with LocalFree.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), SDDL_REVISION_1, &mut sd, None)
        }
        .unwrap();
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let n = wide(name);
        // SAFETY: valid attributes and name.
        let job = Owned(unsafe { CreateJobObjectW(Some(&sa), PCWSTR(n.as_ptr())) }.unwrap());
        // SAFETY: allocated above.
        unsafe {
            let _ = windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(sd.0)));
        }
        job
    }

    /// Review S6-03 diagnosis (run explicitly with `--ignored`): whether
    /// this Windows lets a new process into a job right after a child's
    /// wait has ended it (`Child::wait` terminates the job to end any
    /// descendant). KeyDock no longer depends on it: every restore gets a
    /// fresh job. Prints what happens; asserts only that a fresh job works.
    #[test]
    #[ignore]
    fn diagnose_job_reuse_after_termination() {
        let (job, _) = Job::create(None).unwrap();
        for i in 0..5 {
            let r = Child::spawn(&job, &cmd(), &["/d", "/c", "exit 0"])
                .and_then(|c| c.wait(Duration::from_secs(5), None))
                .map(|f| f.end);
            eprintln!(
                "same job, launch {i}: {r:?} (active processes {:?})",
                job.active_processes()
            );
        }
        let (fresh, _) = Job::create(None).unwrap();
        let r =
            Child::spawn(&fresh, &cmd(), &["/d", "/c", "exit 0"]).and_then(|c| c.wait(Duration::from_secs(5), None));
        assert_eq!(r.map(|f| f.end), Ok(End::Exited(0)), "a fresh job takes a process");
    }

    #[test]
    fn missing_program_is_an_error() {
        let (job, _) = Job::create(None).unwrap();
        assert!(Child::spawn(&job, std::path::Path::new("C:\\nope\\missing.exe"), &[]).is_err());
    }

    /// Review KD-02: a forced end of the owner (what an installer's forced
    /// close or Task Manager does to KeyDock) must take the contained child
    /// with it. This test runs itself again as the "owner", which starts a
    /// contained helper and waits; the test then terminates the owner.
    #[test]
    fn contained_child_dies_with_a_killed_owner() {
        const TEST: &str = "win::child::tests::contained_child_dies_with_a_killed_owner";
        if let Ok(pidfile) = std::env::var("KEYDOCK_OWNER_PIDFILE") {
            // Owner mode.
            let (job, _) = Job::create(None).unwrap();
            let ready = std::path::PathBuf::from(format!("{pidfile}.child"));
            let _ = std::fs::remove_file(&ready);
            let c = spawn_helper(&job, "sleep", &ready);
            let (pid, _c) = wait_ready_child(c, &ready);
            std::fs::write(&pidfile, format!("{pid}\n")).unwrap();
            std::thread::sleep(Duration::from_secs(120));
            return;
        }
        let pidfile = temp("owner");
        let mut owner = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env("KEYDOCK_OWNER_PIDFILE", &pidfile)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid: u32 = wait_ready(&pidfile).parse().unwrap();
        assert!(alive(pid), "the child runs while its owner runs");
        owner.kill().unwrap(); // TerminateProcess
        let _ = owner.wait();
        let start = std::time::Instant::now();
        while alive(pid) && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(pid), "the child ends with its owner");
        let _ = std::fs::remove_file(&pidfile);
    }
}
