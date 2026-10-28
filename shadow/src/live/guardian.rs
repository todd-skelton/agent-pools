//! pool-guardian.mjs: keeps `serve-service` running. It starts the service (this program again,
//! its output to `logs/supervisor.out.log`), reads `/api/health` every 10 s after a 3 s
//! allowance, and restarts it after three bad readings in a row (healthy: 200, `service` is
//! `agent-pools`, `pid` is the child's). It stops when `state/guardian-stop.request` exists or
//! it is told to (Ctrl-C), stopping the service gracefully (`POST /api/shutdown` with the
//! management key, then up to 20 s, then ended), which leaves the proxy running. A guardian that
//! finds another install's service already answering on the port steps aside
//! (`guardian-duplicate`) rather than crash-loop against it.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pool_core::supervise::HealthPolicy;
use serde_json::{Value, json};

use super::{Install, machine};

const STOP_FILE: &str = "state/guardian-stop.request";

struct Guardian {
    install: Install,
    origin: String,
    key: String,
    http: reqwest::Client,
    stopping: Arc<AtomicBool>,
}

impl Guardian {
    fn event(&self, kind: &str, details: Value) {
        self.install.event("pool-events.jsonl", kind, details);
    }

    fn stop_file(&self) -> PathBuf {
        self.install.root.join(STOP_FILE)
    }

    fn requested_stop(&self) -> bool {
        self.stopping.load(Ordering::SeqCst) || self.stop_file().exists()
    }

    async fn health(&self, timeout: Duration) -> Option<Value> {
        let r = self.http.get(format!("{}/api/health", self.origin)).timeout(timeout).send().await.ok()?;
        let ok = r.status().is_success();
        let body: Value = r.json().await.ok()?;
        ok.then_some(body)
    }

    /// `foreignSupervisor()`: the pid of a service already answering on the port.
    async fn foreign(&self) -> Option<u64> {
        let h = self.health(Duration::from_secs(1)).await?;
        (h.get("service").and_then(Value::as_str) == Some("agent-pools")).then(|| h.get("pid").and_then(Value::as_u64)).flatten()
    }

    /// `stopChild()`.
    async fn stop_child(&self, child: &mut tokio::process::Child) {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        let pid = child.id().map(u64::from);
        // Only this guardian's own child is asked; the port may belong to another process.
        if pid.is_some() && self.health(Duration::from_secs(1)).await.and_then(|h| h.get("pid").and_then(Value::as_u64)) == pid {
            let _ = self.http.post(format!("{}/api/shutdown", self.origin)).bearer_auth(&self.key).timeout(Duration::from_secs(3)).send().await;
        }
        if tokio::time::timeout(Duration::from_secs(20), child.wait()).await.is_err() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }

    /// Up to `secs` seconds, ending early once a stop is asked for or `child` exits.
    async fn sleep_unless_stopped(&self, secs: u64, child: &mut tokio::process::Child) {
        for _ in 0..secs {
            if self.requested_stop() || !matches!(child.try_wait(), Ok(None)) {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    fn spawn(&self) -> std::io::Result<tokio::process::Child> {
        let logs = self.install.root.join("logs");
        std::fs::create_dir_all(&logs)?;
        let file = logs.join("supervisor.out.log");
        if std::fs::metadata(&file).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
            let _ = std::fs::rename(&file, file.with_extension("log.1"));
        }
        let out = std::fs::OpenOptions::new().create(true).append(true).open(&file)?;
        let err = out.try_clone()?;
        let mut cmd = tokio::process::Command::new(std::env::current_exe()?);
        cmd.arg("serve-service").args(self.install.args()).current_dir(&self.install.root).stdin(std::process::Stdio::null()).stdout(out).stderr(err);
        machine::hide_window(&mut cmd);
        let child = cmd.spawn()?;
        if let Some(pid) = child.id() {
            machine::normal_priority(pid);
        }
        Ok(child)
    }
}

/// `pools-shadow guardian <root> [--live]`.
pub async fn run(install: Install) -> std::io::Result<()> {
    super::normal_priority();
    let key = install.secret("managementKey")?;
    let g = Guardian {
        origin: install.service_url(),
        key,
        http: reqwest::Client::builder().no_proxy().build().map_err(std::io::Error::other)?,
        stopping: Arc::new(AtomicBool::new(false)),
        install,
    };
    let stopping = g.stopping.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        stopping.store(true, Ordering::SeqCst);
    });
    let mut last: Option<tokio::process::Child> = None;
    while !g.requested_stop() {
        // Another guardian's service already has the port: starting one here would only
        // crash-loop on the bind and flood the event log.
        if let Some(pid) = g.foreign().await {
            g.event("guardian-duplicate", json!({ "supervisorPid": pid }));
            break;
        }
        let mut child = match g.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Supervisor spawn failed: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let pid = child.id();
        let mut restart = HealthPolicy::default();
        tokio::time::sleep(Duration::from_secs(3)).await;
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                g.event("supervisor-exit", json!({ "pid": pid, "code": status.code(), "signal": Value::Null }));
                break;
            }
            if g.requested_stop() {
                break;
            }
            let healthy = g.health(Duration::from_secs(3)).await.is_some_and(|h| {
                h.get("service").and_then(Value::as_str) == Some("agent-pools") && h.get("pid").and_then(Value::as_u64) == pid.map(u64::from)
            });
            if restart.check(healthy) {
                g.event("supervisor-unresponsive", json!({ "pid": pid }));
                g.stop_child(&mut child).await;
                break;
            }
            g.sleep_unless_stopped(10, &mut child).await;
        }
        last = Some(child);
        if g.requested_stop() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if let Some(child) = last.as_mut() {
        g.stop_child(child).await;
    }
    match std::fs::remove_file(g.stop_file()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}
