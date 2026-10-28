//! The supervisor's view of the machine, as pool-service.mjs reads it: `processInfo` through
//! Windows PowerShell's CIM (20 s, at normal priority, since the lookup decides adoption and
//! retirement), `portFree` by binding, and `alive` and `kill` as Node's `process.kill(pid, 0)`
//! and `process.kill(pid)` do them on Windows (`OpenProcess`, `TerminateProcess`).
//!
//! `portOwner` differs: the listener's process comes from the TCP table (`GetExtendedTcpTable`),
//! not `Get-NetTCPConnection`, which took 27 s on this machine (2026-10-28) and so never finished
//! inside the 20 s limit; a takeover without a record then waited forever. The owner's details
//! are still CIM's.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pool_core::supervise::{LookupError, Machine};
use serde_json::Value;

/// pool-service.mjs `LOOKUP_TIMEOUT_MS`.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(20);

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

/// pool-service.mjs `powershell(command)`: Windows PowerShell's output, or the lookup's failure
/// (`timed_out` when it ran past [`LOOKUP_TIMEOUT`]).
pub async fn powershell(command: &str) -> Result<String, LookupError> {
    let mut cmd = tokio::process::Command::new("powershell.exe");
    // Piped: spawn() inherits them otherwise, and wait_with_output() then reads nothing.
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", command])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    hide_window(&mut cmd);
    let child = cmd.spawn().map_err(|e| LookupError { message: e.to_string(), timed_out: false })?;
    if let Some(pid) = child.id() {
        normal_priority(pid);
    }
    match tokio::time::timeout(LOOKUP_TIMEOUT, child.wait_with_output()).await {
        Err(_) => Err(LookupError { message: format!("Command failed: powershell.exe timed out after {} ms", LOOKUP_TIMEOUT.as_millis()), timed_out: true }),
        Ok(Err(e)) => Err(LookupError { message: e.to_string(), timed_out: false }),
        Ok(Ok(out)) if !out.status.success() => {
            Err(LookupError { message: format!("Command failed: {}", String::from_utf8_lossy(&out.stderr).trim()), timed_out: false })
        }
        Ok(Ok(out)) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
    }
}

/// `processInfo(pid)`: `ProcessId`, `CreationDate` (`yyyy-MM-ddTHH:mm:ss.fffK`),
/// `ExecutablePath`, `CommandLine`, or `None` when there is no such process.
pub async fn process_info(pid: u32) -> Result<Option<Value>, LookupError> {
    let out = powershell(&format!(
        "Get-CimInstance Win32_Process -Filter \"ProcessId={pid}\" | Select-Object ProcessId, @{{n='CreationDate';e={{$_.CreationDate.ToString('yyyy-MM-ddTHH:mm:ss.fffK')}}}}, ExecutablePath, CommandLine | ConvertTo-Json -Compress"
    ))
    .await?;
    let out = out.trim();
    if out.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(out).map(Some).map_err(|e| LookupError { message: format!("process lookup: {e}"), timed_out: false })
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
