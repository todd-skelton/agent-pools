//! Each owned worker waits for a one-byte launch handshake. The supervisor assigns it to
//! a kill-on-close job BEFORE releasing that handshake, so all subsequent CLI descendants
//! are owned too. No PID adoption, process-name killing, or orphaned inference on a crash.
#[cfg(windows)]
pub struct Tree(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
// SAFETY: the guard is the sole owner; moving it between runtime threads does not share
// the handle or change Windows job ownership semantics.
unsafe impl Send for Tree {}

#[cfg(windows)]
impl Tree {
    pub fn attach(child: &tokio::process::Child) -> std::io::Result<Self> {
        use windows_sys::Win32::System::JobObjects::*;
        // SAFETY: a fresh non-inheritable job; only this Child's live process handle is
        // assigned, while it waits for its handshake. The owned job is closed once.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let tree = Self(job);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let process = child
                .raw_handle()
                .ok_or_else(|| std::io::Error::other("Worker has no process handle"))?;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&info) as u32,
            ) == 0
                || AssignProcessToJobObject(job, process.cast()) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(tree)
        }
    }
}
#[cfg(windows)]
impl Drop for Tree {
    fn drop(&mut self) {
        // SAFETY: this non-inherited job handle belongs only to this guard.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(not(windows))]
pub struct Tree;
#[cfg(not(windows))]
impl Tree {
    pub fn attach(_: &tokio::process::Child) -> std::io::Result<Self> {
        Err(std::io::Error::other(
            "Managed Claude RC process trees currently require Windows",
        ))
    }
}
