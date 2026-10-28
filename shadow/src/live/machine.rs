//! The supervisor's view of the machine, as pool-service.mjs reads it, with the process's own
//! calls instead of Windows PowerShell: `processInfo` is the record CIM's `Win32_Process` gives
//! (`ProcessId`, `CreationDate` to the millisecond in local time, `ExecutablePath`,
//! `CommandLine`), read with `OpenProcess`, `GetProcessTimes`, `QueryFullProcessImageNameW` and
//! `NtQueryInformationProcess`; `portFree` by binding; `alive` and `kill` as Node's
//! `process.kill(pid, 0)` and `process.kill(pid)` do them on Windows.
//!
//! `portOwner` takes the listener's process from the TCP table (`GetExtendedTcpTable`), not
//! `Get-NetTCPConnection`, which took 27 s on this machine (2026-10-28). The CIM lookup took
//! about a second and needed a 20 s limit; these calls answer in microseconds and cannot hang.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pool_core::supervise::{LookupError, Machine};
use serde_json::Value;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
pub const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(windows)]
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// A child with no console window (`windowsHide`).
pub fn hide_window(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    #[cfg(not(windows))]
    let _ = cmd;
}

/// A process id from a JSON number, when it is one.
pub fn pid_of(pid: f64) -> Option<u32> {
    (pid.is_finite() && pid >= 1.0 && pid <= f64::from(u32::MAX) && pid.fract() == 0.0).then_some(pid as u32)
}

#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, NORMAL_PRIORITY_CLASS, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, PROCESS_TERMINATE,
        SetPriorityClass, TerminateProcess,
    };

    /// Runs `f` on a handle to `pid` opened with `access`, or gives `None`.
    fn with<T>(pid: u32, access: u32, f: impl FnOnce(windows_sys::Win32::Foundation::HANDLE) -> T) -> Option<T> {
        // SAFETY: the handle is checked before use and closed once, after `f`.
        unsafe {
            let h = OpenProcess(access, 0, pid);
            if h.is_null() {
                return None;
            }
            let out = f(h);
            CloseHandle(h);
            Some(out)
        }
    }

    pub fn alive(pid: u32) -> bool {
        with(pid, PROCESS_QUERY_LIMITED_INFORMATION, |h| {
            let mut code = 0u32;
            // SAFETY: `h` is an open process handle and `code` outlives the call.
            unsafe { GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE as u32 }
        })
        .unwrap_or(false)
    }

    pub fn kill(pid: u32) -> bool {
        // SAFETY: `h` is an open process handle with terminate access.
        with(pid, PROCESS_TERMINATE, |h| unsafe { TerminateProcess(h, 1) != 0 }).unwrap_or(false)
    }

    pub fn normal_priority(pid: u32) {
        // SAFETY: `h` is an open process handle with set-information access.
        let _ = with(pid, PROCESS_SET_INFORMATION, |h| unsafe { SetPriorityClass(h, NORMAL_PRIORITY_CLASS) });
    }

    /// `Win32_Process`'s record for `pid`, as CIM gives it, or `None` when no such process is
    /// running. A process this one may not open (another user's) still has its id, with the
    /// rest null, as CIM leaves what it may not read.
    pub fn process_info(pid: u32) -> Option<serde_json::Value> {
        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, FILETIME, GetLastError, HANDLE, UNICODE_STRING};
        use windows_sys::Win32::System::Threading::{GetProcessTimes, QueryFullProcessImageNameW};
        use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation};
        if pid == 0 {
            return None;
        }
        let denied = || Some(serde_json::json!({ "ProcessId": pid, "CreationDate": null, "ExecutablePath": null, "CommandLine": null }));
        let read = |h: HANDLE| -> Option<serde_json::Value> {
            let mut code = 0u32;
            // SAFETY: `h` is an open process handle and `code` outlives the call.
            if unsafe { GetExitCodeProcess(h, &mut code) } == 0 || code != STILL_ACTIVE as u32 {
                return None; // exited: a handle someone still holds, not a running process
            }
            let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
            let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
            // SAFETY: four FILETIMEs that outlive the call.
            let created = (unsafe { GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user) } != 0).then(|| local_iso(created));
            let mut path = vec![0u16; 32_768];
            let mut len = path.len() as u32;
            // SAFETY: `path` holds `len` UTF-16 units; the call writes at most that many.
            let path = (unsafe { QueryFullProcessImageNameW(h, 0, path.as_mut_ptr(), &mut len) } != 0).then(|| String::from_utf16_lossy(&path[..len as usize]));
            let mut need = 0u32;
            let mut buf: Vec<u64> = vec![0; 64];
            let mut command = None;
            for _ in 0..3 {
                let size = (buf.len() * 8) as u32;
                // SAFETY: `buf` holds `size` bytes, 8-aligned as UNICODE_STRING needs.
                let status = unsafe { NtQueryInformationProcess(h, ProcessCommandLineInformation, buf.as_mut_ptr().cast(), size, &mut need) };
                if status >= 0 {
                    // SAFETY: on success `buf` starts with a UNICODE_STRING whose buffer lies inside `buf`.
                    command = unsafe {
                        let s = &*buf.as_ptr().cast::<UNICODE_STRING>();
                        Some(if s.Buffer.is_null() { String::new() } else { String::from_utf16_lossy(std::slice::from_raw_parts(s.Buffer, usize::from(s.Length) / 2)) })
                    };
                    break;
                }
                if need as usize <= buf.len() * 8 {
                    break;
                }
                buf = vec![0; (need as usize).div_ceil(8) + 1];
            }
            Some(serde_json::json!({ "ProcessId": pid, "CreationDate": created, "ExecutablePath": path, "CommandLine": command }))
        };
        // SAFETY: the handle is checked before use and closed once, after `read`.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return if GetLastError() == ERROR_ACCESS_DENIED { denied() } else { None };
            }
            let out = read(h);
            CloseHandle(h);
            out
        }
    }

    /// A FILETIME as .NET's `ToString('yyyy-MM-ddTHH:mm:ss.fffK')` writes a local time:
    /// truncated to the millisecond, with the offset the time zone had at that moment.
    fn local_iso(t: windows_sys::Win32::Foundation::FILETIME) -> String {
        use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
        use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime};
        let ticks = |f: &FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
        let zero = SYSTEMTIME { wYear: 0, wMonth: 0, wDayOfWeek: 0, wDay: 0, wHour: 0, wMinute: 0, wSecond: 0, wMilliseconds: 0 };
        let (mut utc, mut local) = (zero, zero);
        let mut local_ft = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        // SAFETY: plain value conversions into locals that outlive the calls.
        let ok = unsafe { FileTimeToSystemTime(&t, &mut utc) != 0 && SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) != 0 && SystemTimeToFileTime(&local, &mut local_ft) != 0 };
        if !ok {
            return String::new();
        }
        // Both carry the same milliseconds, so the difference is whole minutes.
        let minutes = (ticks(&local_ft) as i64 - (ticks(&t) as i64 / 10_000 * 10_000)) / 600_000_000;
        let (sign, m) = if minutes < 0 { ('-', -minutes) } else { ('+', minutes) };
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}{sign}{:02}:{:02}",
            local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond, local.wMilliseconds, m / 60, m % 60
        )
    }

    pub fn processes_named(name: &str) -> Vec<u32> {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
        let mut out = Vec::new();
        // SAFETY: the snapshot is checked, walked with a sized entry, and closed once.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return out;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut more = Process32FirstW(snap, &mut e) != 0;
            while more {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                if String::from_utf16_lossy(&e.szExeFile[..len]).eq_ignore_ascii_case(name) {
                    out.push(e.th32ProcessID);
                }
                more = Process32NextW(snap, &mut e) != 0;
            }
            CloseHandle(snap);
        }
        out
    }

    /// The process listening on `127.0.0.1:<port>` (IPv4), from the TCP table.
    pub fn listener_pid(port: u16) -> std::io::Result<Option<u32>> {
        use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
        use windows_sys::Win32::NetworkManagement::IpHelper::{GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER};
        use windows_sys::Win32::Networking::WinSock::AF_INET;
        let mut size = 0u32;
        let mut buf: Vec<u64> = Vec::new();
        for _ in 0..4 {
            // SAFETY: `buf` holds `size` bytes (8-aligned, as the table's rows need) or is empty
            // with `size` 0, which asks for the size.
            let rc = unsafe { GetExtendedTcpTable(buf.as_mut_ptr().cast(), &mut size, 0, u32::from(AF_INET), TCP_TABLE_OWNER_PID_LISTENER, 0) };
            if rc == ERROR_INSUFFICIENT_BUFFER {
                buf = vec![0u64; (size as usize).div_ceil(8) + 1];
                size = (buf.len() * 8) as u32;
                continue;
            }
            if rc != NO_ERROR {
                return Err(std::io::Error::from_raw_os_error(rc as i32));
            }
            let table = buf.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>();
            // SAFETY: the call filled `buf` with a table of `dwNumEntries` rows after the count.
            let rows = unsafe { std::slice::from_raw_parts(std::ptr::addr_of!((*table).table).cast::<MIB_TCPROW_OWNER_PID>(), (*table).dwNumEntries as usize) };
            let loopback = u32::from_ne_bytes([127, 0, 0, 1]);
            return Ok(rows.iter().find(|r| r.dwLocalAddr == loopback && u16::from_be(r.dwLocalPort as u16) == port).map(|r| r.dwOwningPid));
        }
        Err(std::io::Error::other("the TCP table kept growing"))
    }
}

#[cfg(not(windows))]
mod win {
    pub fn alive(_: u32) -> bool {
        false
    }
    pub fn kill(_: u32) -> bool {
        false
    }
    pub fn normal_priority(_: u32) {}
    pub fn listener_pid(_: u16) -> std::io::Result<Option<u32>> {
        Ok(None)
    }
    pub fn process_info(_: u32) -> Option<serde_json::Value> {
        None
    }
    pub fn processes_named(_: &str) -> Vec<u32> {
        Vec::new()
    }
}

/// `process.kill(pid, 0)` succeeds: the process is running and this one may see it.
pub fn alive(pid: u32) -> bool {
    win::alive(pid)
}

/// `process.kill(pid)`: ends it. False when it could not be opened or ended.
pub fn kill(pid: u32) -> bool {
    win::kill(pid)
}

/// `os.setPriority(pid, PRIORITY_NORMAL)`; a failure is harmless.
pub fn normal_priority(pid: u32) {
    win::normal_priority(pid)
}

/// The record of `pid` (see [`process_info`]), now.
pub fn win_process_info(pid: u32) -> Option<Value> {
    win::process_info(pid)
}

/// The ids of the running processes whose image is `name` (`cloudflared.exe`), any case.
pub fn processes_named(name: &str) -> Vec<u32> {
    win::processes_named(name)
}

/// `processInfo(pid)`: `ProcessId`, `CreationDate` (`yyyy-MM-ddTHH:mm:ss.fffK`),
/// `ExecutablePath`, `CommandLine`, or `None` when there is no such process.
pub async fn process_info(pid: u32) -> Result<Option<Value>, LookupError> {
    Ok(win::process_info(pid))
}

/// `portOwner(port)`: the process listening on `127.0.0.1:<port>`, or `None`.
pub async fn port_owner(port: u16) -> Result<Option<Value>, LookupError> {
    match win::listener_pid(port) {
        Ok(Some(pid)) if pid > 0 => process_info(pid).await,
        Ok(_) => Ok(None),
        Err(e) => Err(LookupError { message: format!("TCP table: {e}"), timed_out: false }),
    }
}

/// `portFree(port)`: `127.0.0.1:<port>` can be bound now.
pub async fn port_free(port: u16) -> bool {
    tokio::net::TcpListener::bind(("127.0.0.1", port)).await.is_ok()
}

/// The machine as the proxy takeover sees it, logging to the install's `logs/pool-events.jsonl`.
pub struct Host {
    pub install: super::Install,
    /// pool-service.mjs `closing`: the service is stopping.
    pub closing: Arc<AtomicBool>,
}

impl Machine for Host {
    fn alive(&self, pid: f64) -> bool {
        pid_of(pid).is_some_and(alive)
    }

    fn kill(&self, pid: f64) {
        if let Some(pid) = pid_of(pid) {
            kill(pid);
        }
    }

    async fn port_free(&self, port: u16) -> bool {
        port_free(port).await
    }

    async fn process_info(&self, pid: f64) -> Result<Option<Value>, LookupError> {
        match pid_of(pid) {
            Some(pid) => process_info(pid).await,
            None => Ok(None),
        }
    }

    async fn port_owner(&self, port: u16) -> Result<Option<Value>, LookupError> {
        port_owner(port).await
    }

    async fn sleep(&self, ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await
    }

    async fn event(&self, kind: &str, details: Value) {
        self.install.event("pool-events.jsonl", kind, details);
    }

    fn closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }
}
