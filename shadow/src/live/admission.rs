//! model-admission.mjs's probes on the live install: one tiny prompt per (model, effort) through
//! the real `codex` or `claude` on the launch path lanes use (Codex:
//! `model_provider=account_pool`, apps off, `--json`; Claude: `--print --output-format json`),
//! judged by `pool_core::registry`'s judges, Codex's also by the pool log's routing evidence.
//!
//! Safety, as there: executables are resolved to native `.exe` paths on `PATH` and started
//! without a shell; the prompt goes through stdin; ids and efforts are checked before
//! (`models::admit`). Each probe runs in a directory of its own under the temp directory,
//! removed after.
//!
//! One difference: a tool's version (`--version`) is read again only when its executable
//! changes (path, size, modification time). model-admission.mjs read it every run with a 30 s
//! limit, and `--version` timing out failed the whole refresh (13 runs since 2026-10-19); here
//! the limit is 90 s and most runs read nothing.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use pool_core::registry;
use regex::Regex;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::dash::fmt::Zone;
use crate::watch::now_ms;
use crate::writer::models::{PROMPT, Prober, local_ms};

const PROBE_TIMEOUT: Duration = Duration::from_secs(240);
const VERSION_TIMEOUT: Duration = Duration::from_secs(90);
/// model-admission.mjs waits this long for the pool to flush its log line.
const FLUSH: Duration = Duration::from_millis(1500);

/// `resolveExe(name)`: the first `<name>.exe` in a `PATH` directory, as `where.exe` lists it
/// (the working directory is not searched).
pub fn resolve_exe(name: &str) -> Result<PathBuf, String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|d| d.join(format!("{name}.exe")))
        .find(|p| p.is_file())
        .ok_or_else(|| format!("no native {name}.exe on PATH"))
}

/// What a tool prints for `--version`, as its `x.y.z` (the whole trimmed text when it has none).
pub fn version_of(printed: &str) -> String {
    static V: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"(\d+\.\d+\.\d+)").unwrap());
    let v = printed.trim();
    V.captures(v).map_or_else(|| v.to_string(), |m| m[1].to_string())
}

/// A finished child: its exit code (none when it was ended for running too long, Node's `null`)
/// and what it printed.
pub struct Ran {
    pub code: Option<i32>,
    pub stdout: String,
}

/// `run(exe, args, cwd, input)`: no shell, no window, `input` on stdin, ended after `limit`. A
/// child that does not start is code -1.
pub async fn run(exe: &Path, args: &[String], cwd: &Path, input: &str, limit: Duration) -> Ran {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(args).current_dir(cwd).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    super::machine::hide_window(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return Ran { code: Some(-1), stdout: String::new() },
    };
    if let Some(mut stdin) = child.stdin.take() {
        let input = input.to_string();
        tokio::spawn(async move {
            let _ = stdin.write_all(input.as_bytes()).await;
        });
    }
    let (mut out, mut err) = (child.stdout.take(), child.stderr.take());
    let read = async {
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let read_out = async {
            if let Some(s) = out.as_mut() {
                let _ = s.read_to_end(&mut o).await;
            }
        };
        let read_err = async {
            if let Some(s) = err.as_mut() {
                let _ = s.read_to_end(&mut e).await;
            }
        };
        tokio::join!(read_out, read_err);
        (o, child.wait().await)
    };
    match tokio::time::timeout(limit, read).await {
        Ok((o, status)) => Ran { code: status.ok().and_then(|s| s.code()), stdout: String::from_utf8_lossy(&o).into_owned() },
        // Dropping the read ends the child (kill_on_drop), as Node's child.kill() did.
        Err(_) => Ran { code: None, stdout: String::new() },
    }
}

/// A probe's own working directory under the temp directory (mkdtemp's `model-admission-`),
/// removed when dropped; a tool still holding it (EBUSY) gets a few more tries.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Scratch> {
        let dir = std::env::temp_dir().join(format!("model-admission-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]));
        std::fs::create_dir(&dir)?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for i in 0..5 {
            if std::fs::remove_dir_all(&self.0).is_ok() || !self.0.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200 * (i + 1)));
        }
    }
}

type Seen = (PathBuf, u64, Option<SystemTime>);

/// The real tools, on the live install.
pub struct Native {
    root: PathBuf,
    zone: Zone,
    /// Each tool's version, with the executable it was read from as it was then.
    versions: Mutex<HashMap<&'static str, (Seen, String)>>,
}

impl Native {
    pub fn new(root: PathBuf) -> Native {
        Native { root, zone: Zone::from_env(), versions: Mutex::new(HashMap::new()) }
    }

    /// `harnessVersion(resolveExe(name))`, read again only when the executable changed.
    async fn version(&self, name: &'static str) -> Result<String, String> {
        let exe = resolve_exe(name)?;
        let meta = std::fs::metadata(&exe).map_err(|e| format!("{}: {e}", exe.display()))?;
        let seen: Seen = (exe.clone(), meta.len(), meta.modified().ok());
        if let Some((s, v)) = self.versions.lock().unwrap_or_else(|e| e.into_inner()).get(name)
            && *s == seen
        {
            return Ok(v.clone());
        }
        let cwd = std::env::temp_dir();
        let ran = run(&exe, &["--version".to_string()], &cwd, "", VERSION_TIMEOUT).await;
        match ran.code {
            Some(0) => {}
            Some(code) => return Err(format!("{} --version: exit {code}", exe.display())),
            None => return Err(format!("{} --version: no answer in {} s", exe.display(), VERSION_TIMEOUT.as_secs())),
        }
        let v = version_of(&ran.stdout);
        self.versions.lock().unwrap_or_else(|e| e.into_inner()).insert(name, (seen, v.clone()));
        Ok(v)
    }

    async fn probe_now(&self, provider: &str, id: &str, effort: &str) -> Value {
        let start = now_ms();
        let exe = match resolve_exe(provider) {
            Ok(e) => e,
            Err(e) => return json!({ "ok": false, "reason": e, "durationMs": 0 }),
        };
        let scratch = match Scratch::new() {
            Ok(s) => s,
            Err(e) => return json!({ "ok": false, "reason": format!("temp directory: {e}"), "durationMs": 0 }),
        };
        let s = |v: &[&str]| v.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let failed = |code: Option<i32>| json!({ "ok": false, "reason": format!("exit {}", code.map_or("null".into(), |c| c.to_string())) });
        let mut verdict = if provider == "claude" {
            let r = run(&exe, &s(&["--model", id, "--effort", effort, "--print", "--output-format", "json"]), &scratch.0, PROMPT, PROBE_TIMEOUT).await;
            if r.code == Some(0) { registry::judge_claude(id, &r.stdout).unwrap_or_else(|e| json!({ "ok": false, "reason": e })) } else { failed(r.code) }
        } else {
            let effort_arg = format!("model_reasoning_effort={effort}");
            let args = s(&["exec", "--skip-git-repo-check", "-m", id, "-c", "model_provider=account_pool", "-c", &effort_arg, "-c", "features.apps=false", "--json", "-"]);
            let r = run(&exe, &args, &scratch.0, PROMPT, PROBE_TIMEOUT).await;
            if r.code == Some(0) { registry::judge_codex(&r.stdout) } else { failed(r.code) }
        };
        let end = now_ms();
        drop(scratch);
        if provider == "codex" && verdict.get("ok") == Some(&Value::Bool(true)) {
            tokio::time::sleep(FLUSH).await;
            let log = std::fs::read_to_string(self.root.join("logs/main.log")).unwrap_or_default();
            let routed = registry::pool_routing(&log, id, start, end, |st| local_ms(self.zone, st));
            verdict = if routed.is_null() {
                json!({ "ok": false, "reason": "no-pool-routing-evidence" })
            } else {
                let mut v = verdict;
                v["account"] = routed["account"].clone();
                v["poolRequestId"] = routed["requestId"].clone();
                v
            };
        }
        verdict["durationMs"] = json!(end - start);
        verdict
    }
}

/// `batteryRunning()`: a pwsh or Windows PowerShell running controller-release-battery.ps1 (its
/// command line matched without regard to case, as `-match` does). A process whose command line
/// cannot be read is not counted, as CIM's empty one was not.
pub fn battery_running() -> bool {
    ["pwsh.exe", "powershell.exe"].into_iter().flat_map(super::machine::processes_named).any(|pid| {
        super::machine::win_process_info(pid)
            .and_then(|p| p.get("CommandLine").and_then(Value::as_str).map(|c| c.to_ascii_lowercase().contains("controller-release-battery.ps1")))
            .unwrap_or(false)
    })
}

impl Prober for Native {
    #[allow(clippy::manual_async_fn)]
    fn harness_versions(&self) -> impl Future<Output = Result<Value, String>> + Send {
        async { Ok(json!({ "claude": self.version("claude").await?, "codex": self.version("codex").await? })) }
    }

    fn deferred(&self) -> bool {
        battery_running()
    }

    fn probe(&self, provider: &str, id: &str, effort: &str) -> impl Future<Output = Value> + Send {
        let (provider, id, effort) = (provider.to_string(), id.to_string(), effort.to_string());
        async move { self.probe_now(&provider, &id, &effort).await }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_is_the_x_y_z_a_tool_prints() {
        assert_eq!(version_of("codex-cli 0.165.0\r\n"), "0.165.0");
        assert_eq!(version_of("2.1.310 (Claude Code)\n"), "2.1.310");
        assert_eq!(version_of("codex-cli 0.165.0-alpha.1.2"), "0.165.0");
        assert_eq!(version_of("  dev build \n"), "dev build");
    }

    /// The child's exit code and output, stdin delivered; one that runs too long is ended with no
    /// code (Node's `exit null`); one that does not start is -1.
    #[tokio::test]
    async fn a_child_gets_stdin_and_one_running_too_long_is_ended() {
        let cmd = PathBuf::from(std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into()));
        let cwd = std::env::temp_dir();
        let s = |v: &[&str]| v.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let echoed = run(&cmd, &s(&["/d", "/c", "sort"]), &cwd, "Reply with exactly: OK", Duration::from_secs(20)).await;
        assert_eq!((echoed.code, echoed.stdout.trim()), (Some(0), "Reply with exactly: OK"));
        let failed = run(&cmd, &s(&["/d", "/c", "exit", "3"]), &cwd, "", Duration::from_secs(20)).await;
        assert_eq!(failed.code, Some(3));
        let started = std::time::Instant::now();
        let slow = run(&cmd, &s(&["/d", "/c", "ping", "-n", "30", "127.0.0.1"]), &cwd, "", Duration::from_millis(500)).await;
        assert_eq!(slow.code, None);
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        let missing = run(&cwd.join("no-such-tool.exe"), &[], &cwd, "", Duration::from_secs(5)).await;
        assert_eq!(missing.code, Some(-1));
    }

    #[test]
    fn a_tool_is_found_as_an_exe_on_path_only() {
        assert!(resolve_exe("cmd").is_ok_and(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("cmd.exe"))));
        assert_eq!(resolve_exe("no-such-tool-here").unwrap_err(), "no native no-such-tool-here.exe on PATH");
    }

    #[test]
    fn a_probes_directory_is_removed_after() {
        let s = Scratch::new().unwrap();
        let dir = s.0.clone();
        std::fs::write(dir.join("left.txt"), "x").unwrap();
        assert!(dir.file_name().unwrap().to_string_lossy().starts_with("model-admission-"));
        drop(s);
        assert!(!dir.exists());
    }
}
