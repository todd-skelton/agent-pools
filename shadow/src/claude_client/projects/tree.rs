//! Each worker puts itself in a kill-on-close job before it launches Claude, so all of its
//! CLI descendants are owned: when the worker ends (stopped, crashed or killed), so do they.
//! The supervisor holds no job of its own over workers, so a service restart leaves them running.
#[cfg(windows)]
pub struct Tree(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
// SAFETY: the guard is the sole owner; moving it between runtime threads does not share
// the handle or change Windows job ownership semantics.
unsafe impl Send for Tree {}

#[cfg(windows)]
impl Tree {
    /// A fresh kill-on-close job holding the calling process; its future children join it.
    pub fn current() -> std::io::Result<Self> {
        use windows_sys::Win32::System::JobObjects::*;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh non-inheritable job; only this process (a pseudo-handle) is assigned.
        // The owned job is closed once, when the process ends or the guard drops.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let tree = Self(job);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&info) as u32,
            ) == 0
                || AssignProcessToJobObject(job, GetCurrentProcess()) == 0
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
    pub fn current() -> std::io::Result<Self> {
        Err(std::io::Error::other(
            "Managed Claude RC process trees currently require Windows",
        ))
    }
}
