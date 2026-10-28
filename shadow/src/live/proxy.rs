//! The service's proxy child (pool-service.mjs `child`, `adopt` and the restart half of
//! `checkProxy`), over `pool_core::supervise`'s decisions:
//!
//! 1. Before (re)starting, the takeover plan (`plan_takeover`): adopt the proxy the record
//!    `state/process-proxy.json` names when it is still that process running this install's
//!    binary and config, or one of the install's own builds found on the port; retire one that is
//!    ours but stale; wait while the port is held by something else or cannot be identified.
//! 2. An adopted proxy is watched every 10 s and started again when it is gone; a spawned one is
//!    started again when it exits, after 10 s, doubling to 5 minutes while it keeps exiting within
//!    5 s.
//! 3. A spawned proxy's record is written once its identity reads, in Node's format
//!    (`{"pid","creationDate"}`), so Agent Pools' own supervisor adopts what this one started and
//!    this one adopts what it started: the service can be handed over either way without the
//!    proxy stopping.
//! 4. The service's stop leaves the proxy running for the next supervisor.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use pool_core::supervise::{self, Child, Plan, ProxyRestartPolicy};
use serde_json::{Value, json};

use super::Install;
use super::machine::{self, Host};

const NAME: &str = "proxy";
/// The environment Codex's own runtime sets, which the proxy must not inherit
/// (pool-service.mjs `childEnvironment`).
const DROPPED_ENV: [&str; 6] =
    ["CODEX_APP_TOOLS_PIPE_PATH", "CODEX_SESSION_ID", "CODEX_THREAD_ID", "CODEX_INTERNAL_ORIGINATOR_OVERRIDE", "CODEX_CI", "CODEX_PERMISSION_PROFILE"];
const ADOPTED_EVERY: Duration = Duration::from_secs(10);

/// The proxy as the service runs it.
pub struct Supervisor {
    host: Host,
    exe: PathBuf,
    config: PathBuf,
    port: u16,
    /// The proxy's pid while one is running (spawned or adopted).
    pid: Mutex<Option<u32>>,
    restart: Mutex<ProxyRestartPolicy>,
    respawn_ms: Mutex<Option<u64>>,
    lookup_ms: Mutex<Option<u64>>,
    /// `healthRunning`'s other half: one restart decision at a time.
    deciding: AtomicBool,
    http: reqwest::Client,
}

fn now_ms() -> f64 {
    crate::watch::now_ms()
}

impl Supervisor {
    pub fn new(install: Install, closing: Arc<AtomicBool>) -> Arc<Supervisor> {
        Arc::new(Supervisor {
            exe: install.proxy_binary(),
            config: install.proxy_config(),
            port: install.proxy_port(),
            host: Host { install, closing },
            pid: Mutex::new(None),
            restart: Mutex::new(ProxyRestartPolicy::default()),
            respawn_ms: Mutex::new(None),
            lookup_ms: Mutex::new(None),
            deciding: AtomicBool::new(false),
            http: reqwest::Client::builder().no_proxy().build().expect("an HTTP client with no TLS and no proxy builds"),
        })
    }

    fn closing(&self) -> bool {
        self.host.closing.load(Ordering::SeqCst)
    }

    fn event(&self, kind: &str, details: Value) {
        self.host.install.event("pool-events.jsonl", kind, details);
    }

    fn record_path(&self) -> PathBuf {
        self.host.install.root.join("state").join(format!("process-{NAME}.json"))
    }

    fn write_record(&self, record: &Value) -> std::io::Result<()> {
        std::fs::write(self.record_path(), record.to_string())
    }

    /// `/api/status`'s `processes`: `[{ name, pid }]` for the running proxy, else `[]`.
    pub fn processes(&self) -> Value {
        match *self.pid.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(pid) => json!([{ "name": NAME, "pid": pid }]),
            None => json!([]),
        }
    }

    /// `nextRespawnDelay(name, uptimeMs)`.
    pub fn next_respawn_ms(&self, uptime_ms: u64) -> u64 {
        let mut d = self.respawn_ms.lock().unwrap_or_else(|e| e.into_inner());
        let next = if uptime_ms < 5000 { (d.unwrap_or(10_000) * 2).min(300_000) } else { 10_000 };
        *d = Some(next);
        next
    }

    /// `nextLookupRetryDelay(name)`.
    pub fn next_lookup_ms(&self) -> u64 {
        let mut d = self.lookup_ms.lock().unwrap_or_else(|e| e.into_inner());
        let next = (d.unwrap_or(2500) * 2).min(30_000);
        *d = Some(next);
        next
    }

    /// `child(name, ...)` again after `ms`.
    fn again(self: &Arc<Self>, ms: u64) {
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            me.start().await;
        });
    }

    /// `child("proxy", proxyBinary, ["-config", proxy.yaml], root)`, boxed so it can schedule itself.
    pub fn start(self: Arc<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move { self.child().await })
    }

    async fn child(self: Arc<Self>) {
        if self.closing() {
            return;
        }
        let record = super::read_json(&self.record_path()).ok();
        let (exe, config, root) = (self.exe.to_string_lossy().into_owned(), self.config.to_string_lossy().into_owned(), self.host.install.root.to_string_lossy().into_owned());
        let plan = supervise::plan_takeover(&Child { name: NAME, exe: &exe, config: &config, root: &root, port: Some(self.port) }, record.as_ref(), &self.host).await;
        if self.closing() {
            return;
        }
        if !matches!(plan, Plan::Wait { reason, .. } if reason != "port-busy") {
            *self.lookup_ms.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        match plan {
            Plan::Adopt { pid, record, owner } => {
                let Some(pid) = machine::pid_of(pid) else { return };
                // Watched first: a failed record write must not leave it unwatched.
                self.adopt(pid);
                machine::normal_priority(pid);
                if let Some(record) = record {
                    let _ = self.write_record(&record);
                    self.event("process-adopted", json!({ "name": NAME, "pid": pid, "executable": owner.as_ref().and_then(|o| o.get("ExecutablePath")) }));
                }
            }
            Plan::Wait { reason, owner } => {
                // Spawning against a held port only crash-loops; wait for the port instead.
                let busy = reason == "port-busy";
                let ms = if busy { self.next_respawn_ms(0) } else { self.next_lookup_ms() };
                let owner = owner.map(|o| json!({ "pid": o.get("ProcessId"), "executable": o.get("ExecutablePath") }));
                self.event(if busy { "port-busy" } else { "port-held-unidentified" }, json!({ "name": NAME, "port": self.port, "retryInMs": ms, "owner": owner }));
                self.again(ms);
            }
            Plan::Spawn => self.spawn().await,
        }
    }

    /// `adopt(name, pid, ...)`: watched every 10 s, and started again once it is gone.
    fn adopt(self: &Arc<Self>, pid: u32) {
        *self.pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(pid);
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(ADOPTED_EVERY).await;
                if me.closing() {
                    return;
                }
                if !machine::alive(pid) {
                    me.forget(pid);
                    me.event("process-exit", json!({ "name": NAME, "adopted": true }));
                    me.start().await;
                    return;
                }
            }
        });
    }

    fn forget(&self, pid: u32) {
        let mut p = self.pid.lock().unwrap_or_else(|e| e.into_inner());
        if *p == Some(pid) {
            *p = None;
        }
    }

    /// Spawns the proxy, detached and windowless, its output to `logs/proxy.log`.
    async fn spawn(self: &Arc<Self>) {
        let started = Instant::now();
        let root = &self.host.install.root;
        let log = root.join("logs").join(format!("{NAME}.log"));
        let _ = std::fs::create_dir_all(root.join("logs"));
        let spawned = std::fs::OpenOptions::new().create(true).append(true).open(&log).and_then(|out| {
            let err = out.try_clone()?;
            let mut cmd = tokio::process::Command::new(&self.exe);
            cmd.arg("-config").arg(&self.config).current_dir(root).stdin(std::process::Stdio::null()).stdout(out).stderr(err);
            for name in DROPPED_ENV {
                cmd.env_remove(name);
            }
            #[cfg(windows)]
            cmd.creation_flags(machine::DETACHED_PROCESS | machine::CREATE_NEW_PROCESS_GROUP);
            cmd.spawn()
        });
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                self.event("process-error", json!({ "name": NAME, "message": e.to_string() }));
                // Node's "close" fires for a failed spawn too, so it gets the same retry.
                self.exited(None, None, started);
                return;
            }
        };
        let Some(pid) = child.id() else { return };
        *self.pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(pid);
        self.restart.lock().unwrap_or_else(|e| e.into_inner()).started(now_ms());
        machine::normal_priority(pid);
        let me = self.clone();
        tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|s| s.code());
            me.exited(Some(pid), code, started);
        });
        // A lost record leaves the next supervisor to find this process from the port alone, so
        // the read is retried like the takeover's lookups.
        let host = &self.host;
        let info = supervise::retry_lookup(
            || machine::process_info(pid),
            supervise::LOOKUP_ATTEMPTS,
            |ms| tokio::time::sleep(Duration::from_millis(ms)),
            |_, _| async {},
        )
        .await;
        let written = match info {
            Ok(Some(info)) => self.write_record(&json!({ "pid": pid, "creationDate": info.get("CreationDate") })).map_err(|e| e.to_string()),
            Ok(None) => Err("exited before its record was written".into()),
            Err(e) => Err(e.message),
        };
        if let Err(message) = written {
            let message: String = message.chars().take(200).collect();
            host.install.event("pool-events.jsonl", "process-record-error", json!({ "name": NAME, "pid": pid, "message": message }));
        }
    }

    /// The child's `close`: forgotten, logged, and started again after the backoff.
    fn exited(self: &Arc<Self>, pid: Option<u32>, code: Option<i32>, started: Instant) {
        if let Some(pid) = pid {
            self.forget(pid);
        }
        let uptime = started.elapsed().as_millis() as u64;
        let ms = self.next_respawn_ms(uptime);
        let kind = if uptime < 5000 && ms > 10_000 { "process-crashloop" } else { "process-exit" };
        self.event(kind, json!({ "name": NAME, "code": code, "uptimeMs": uptime, "retryInMs": ms }));
        if !self.closing() {
            self.again(ms);
        }
    }

    /// `proxyAlive()`: `/healthz` answers (it takes no locks and calls nothing upstream).
    async fn proxy_alive(&self) -> bool {
        let url = format!("http://127.0.0.1:{}/healthz", self.port);
        self.http.get(url).timeout(Duration::from_millis(supervise::PROXY_LIVENESS_TIMEOUT_MS as u64)).send().await.is_ok_and(|r| r.status().is_success())
    }

    /// The restart half of `checkProxy`, after the management probe read `managed`: a restart
    /// drops every turn in flight, so a proxy that still answers `/healthz` is only flagged until
    /// the long degraded limit. Only the recorded process, still this install's, is ended; the
    /// child's exit (or the adopted watch) starts the next.
    pub async fn after_check(&self, managed: bool) {
        if self.closing() || self.deciding.swap(true, Ordering::SeqCst) {
            return;
        }
        let alive = managed || self.proxy_alive().await;
        let decision = self.restart.lock().unwrap_or_else(|e| e.into_inner()).check(managed, alive, now_ms());
        if decision.degraded {
            self.event("proxy-health-degraded", json!({ "reason": "management probe failing, liveness ok" }));
        }
        if decision.restart
            && let Err(message) = self.restart_recorded(alive).await
        {
            self.event("proxy-health-error", json!({ "message": message }));
        }
        self.deciding.store(false, Ordering::SeqCst);
    }

    async fn restart_recorded(&self, alive: bool) -> Result<(), String> {
        let previous = super::read_json(&self.record_path()).map_err(|e| e.to_string())?;
        let pid = pool_core::js::to_number(previous.get("pid"));
        let actual = match machine::pid_of(pid) {
            Some(p) => machine::process_info(p).await.map_err(|e| e.message)?,
            None => None,
        };
        if supervise::same_process(&previous, actual.as_ref(), &self.exe.to_string_lossy(), &self.config.to_string_lossy()) {
            if let Some(p) = machine::pid_of(pid) {
                machine::kill(p);
            }
            self.restart.lock().unwrap_or_else(|e| e.into_inner()).started(now_ms());
            self.event("proxy-health-restart", json!({ "pid": previous.get("pid"), "alive": alive }));
        }
        Ok(())
    }
}
