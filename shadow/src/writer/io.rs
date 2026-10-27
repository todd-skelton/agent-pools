//! The refresh cycle's I/O on a copy of the install (pool-service.mjs's `management`, `event`,
//! `sample`, `save`, `authFile`, `clientVersion` and `loadResetPolicies`), with the capacity
//! publisher's hooks. Desktop toasts are written to `logs/toasts.jsonl` instead of shown: a
//! copy never alerts the desktop.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pool_core::js;
use pool_core::refresh::{CyclePolicy, Failure, ForecastInput, Io};
use pool_core::resets::{ResetPolicy, reset_policies};
use serde_json::{Map, Value, json};

use super::publisher::{Owned, Publisher};
use crate::dash::fmt::Zone;

/// Log files rotate to `.1` past this size (events.mjs).
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const MANAGEMENT_TIMEOUT: Duration = Duration::from_secs(18);
const VERSIONS_TTL_MS: f64 = 5.0 * 60_000.0;

pub struct FileIo {
    pub root: PathBuf,
    proxy: String,
    key: String,
    http: reqwest::Client,
    pub publisher: Arc<Publisher>,
    zone: Zone,
    versions: Mutex<(f64, HashMap<String, String>)>,
    policies: Mutex<(Option<Vec<ResetPolicy>>, String)>,
    /// Serializes rotate-then-append so concurrent events never split or lose a line.
    append_lock: Mutex<()>,
}

pub fn now_ms() -> f64 {
    crate::watch::now_ms()
}

impl FileIo {
    pub fn new(root: PathBuf, proxy: String, key: String) -> FileIo {
        let http = reqwest::Client::builder().no_proxy().build().expect("an HTTP client with no TLS and no proxy builds");
        FileIo {
            publisher: Arc::new(Publisher::new(root.clone())),
            root,
            proxy: proxy.trim_end_matches('/').to_string(),
            key,
            http,
            zone: Zone::from_env(),
            versions: Mutex::new((f64::NEG_INFINITY, HashMap::new())),
            policies: Mutex::new((None, String::new())),
            append_lock: Mutex::new(()),
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// The copy's install.json (a byte-order mark allowed), or `None` when it does not read.
    pub fn install(&self) -> Option<Value> {
        let text = std::fs::read_to_string(self.path("state/install.json")).ok()?;
        serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
    }

    /// One line in the copy's `logs/pool-events.jsonl`, for parts outside the refresh cycle (the
    /// desktop relay); a failed write is dropped, as `event(...).catch(() => {})` does.
    pub fn log_event(&self, kind: &str, details: Value) {
        let _ = self.append("logs/pool-events.jsonl", kind, details);
    }

    /// One line in another of the copy's event logs (`logs/inference-events.jsonl`, ...).
    pub fn log_to(&self, rel: &str, kind: &str, details: Value) {
        let _ = self.append(rel, kind, details);
    }

    /// `createEventLog(file)(type, details)`: rotate past 5 MiB, then append one line.
    fn append(&self, rel: &str, kind: &str, details: Value) -> Result<(), Failure> {
        let file = self.path(rel);
        let _guard = self.append_lock.lock().unwrap_or_else(|e| e.into_inner());
        if std::fs::metadata(&file).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
            let _ = std::fs::rename(&file, file.with_extension("jsonl.1"));
        }
        let mut line = Map::new();
        line.insert("at".into(), json!(js::iso(now_ms())));
        line.insert("type".into(), json!(kind));
        if let Value::Object(d) = details {
            line.extend(d);
        }
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Failure::new(e.to_string()))?;
        }
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
            .and_then(|mut f| f.write_all(format!("{}\n", js::stringify(&Value::Object(line))).as_bytes()))
            .map_err(|e| Failure::new(e.to_string()))
    }

    /// pool-service.mjs `management(endpoint, body, method)`.
    pub async fn call(&self, endpoint: &str, body: Option<Value>, method: Option<String>) -> Result<Value, Failure> {
        let url = format!("{}/v0/management/{endpoint}", self.proxy);
        let method = method.unwrap_or_else(|| if body.is_some() { "POST".into() } else { "GET".into() });
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| Failure::new(e.to_string()))?;
        let mut req = self
            .http
            .request(method, url)
            .bearer_auth(&self.key)
            .header("Content-Type", "application/json")
            .timeout(MANAGEMENT_TIMEOUT);
        if let Some(b) = &body {
            req = req.body(js::stringify(b));
        }
        let r = req.send().await.map_err(transport)?;
        let status = r.status();
        if !status.is_success() {
            let name = endpoint.split('?').next().unwrap_or(endpoint);
            return Err(Failure::with_status(format!("Proxy management {name} returned HTTP {}", status.as_u16()), status.as_u16()));
        }
        let text = r.text().await.map_err(transport)?;
        serde_json::from_str(&text).map_err(|_| Failure { syntax: true, name: Some("SyntaxError".into()), ..Failure::new("Unexpected token in JSON") })
    }
}

/// A failed fetch as Node reports one: a timeout is a `TimeoutError`, anything else `fetch failed`.
fn transport(e: reqwest::Error) -> Failure {
    if e.is_timeout() {
        return Failure { name: Some("TimeoutError".into()), ..Failure::new("The operation was aborted due to timeout") };
    }
    Failure::new(format!("fetch failed ({e})"))
}

fn inside(path: &Path, dir: &Path) -> bool {
    let lower = |p: &Path| p.to_string_lossy().to_lowercase();
    lower(path).starts_with(&(lower(dir) + "\\"))
}

impl Io for FileIo {
    fn now(&self) -> f64 {
        now_ms()
    }

    fn management(&self, endpoint: &str, body: Option<&Value>, method: Option<&str>) -> impl Future<Output = Result<Value, Failure>> + Send {
        self.call(endpoint, body.cloned(), method.map(str::to_string))
    }

    async fn event(&self, kind: &str, details: Value) -> Result<(), Failure> {
        self.append("logs/pool-events.jsonl", kind, details)
    }

    async fn sample(&self, kind: &str, details: Value) -> Result<(), Failure> {
        self.append("logs/usage-samples.jsonl", kind, details)
    }

    async fn save(&self, state: &Value) -> Result<(), Failure> {
        let file = self.path("state/pool-state.json");
        let tmp = self.path("state/pool-state.json.tmp");
        std::fs::write(&tmp, js::stringify_pretty(state, 2))
            .and_then(|_| std::fs::rename(&tmp, &file))
            .map_err(|e| Failure::new(e.to_string()))
    }

    async fn toast(&self, title: &str, body: &str) -> Result<(), Failure> {
        self.append("logs/toasts.jsonl", "toast", json!({ "title": title, "body": body }))
    }

    /// `loadResetPolicies()`: re-read each cycle; an unreadable file keeps the last good policy,
    /// and a change in problems is logged once.
    async fn load_policies(&self) -> Vec<ResetPolicy> {
        let config = match self.install() {
            Some(c) => c,
            None => {
                if let Some(p) = self.policies.lock().unwrap().0.clone() {
                    return p;
                }
                let _ = self.append("logs/pool-events.jsonl", "reset-policy-error", json!({ "message": "state/install.json did not read" }));
                json!({})
            }
        };
        let policies = reset_policies(&config);
        let problems = policies.iter().flat_map(|p| p.problems.iter().cloned()).collect::<Vec<_>>().join("; ");
        let report = {
            let mut last = self.policies.lock().unwrap();
            let changed = problems != last.1;
            *last = (Some(policies.clone()), problems.clone());
            changed && !problems.is_empty()
        };
        if report {
            let _ = self.append("logs/pool-events.jsonl", "reset-policy-invalid", json!({ "problems": problems }));
        }
        policies
    }

    /// `authFile(file)`: only a file inside the copy's `auth/` (after resolving links) is read.
    async fn auth_file(&self, file: &Value) -> Option<Value> {
        let dir = std::fs::canonicalize(self.path("auth")).ok()?;
        let path = std::fs::canonicalize(js::to_string(file.get("path"))).ok()?;
        if !inside(&path, &dir) {
            return None;
        }
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
    }

    /// The client versions the proxy presents upstream, read every five minutes; a failed read
    /// keeps the last answer.
    async fn client_version(&self, provider: &str) -> Option<String> {
        let id = match provider {
            "codex" => "codex",
            "claude" => "claude-code",
            _ => return None,
        };
        let stale = {
            let v = self.versions.lock().unwrap();
            now_ms() - v.0 >= VERSIONS_TTL_MS
        };
        if stale {
            let read = self.call("client-versions", None, None).await.ok();
            let fresh: HashMap<String, String> = read
                .as_ref()
                .and_then(|r| r.get("clients"))
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, c)| {
                            let v = c.get("effective")?.as_str()?;
                            let semver = v.split('.').count() == 3 && v.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
                            semver.then(|| (k.clone(), v.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let mut v = self.versions.lock().unwrap();
            v.0 = now_ms();
            if !fresh.is_empty() {
                v.1 = fresh;
            }
        }
        self.versions.lock().unwrap().1.get(id).cloned()
    }

    async fn sleep(&self, ms: f64) {
        tokio::time::sleep(Duration::from_millis(ms.max(0.0) as u64)).await
    }

    async fn timer(&self, ms: f64) {
        tokio::time::sleep(Duration::from_millis(ms.max(0.0) as u64)).await
    }

    fn random_uuid(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn local_offset_minutes(&self, ms: f64) -> f64 {
        f64::from(self.zone.offset_hours(ms)) * 60.0
    }

    fn forecast(&self, input: ForecastInput<'_>) -> impl Future<Output = Option<Result<Option<Value>, Failure>>> + Send {
        let owned = Owned::from(input);
        let publisher = self.publisher.clone();
        let install = self.install().unwrap_or_else(|| json!({}));
        async move {
            let now = now_ms();
            let work = tokio::task::spawn_blocking(move || publisher.update(&install, owned, now)).await;
            let (doc, logged) = match work {
                Ok(r) => r,
                Err(e) => (None, vec![("capacity-forecast-problem".to_string(), json!({ "problem": format!("error: {e}") }))]),
            };
            for (kind, details) in logged {
                let _ = self.append("logs/pool-events.jsonl", &kind, details);
            }
            Some(Ok(doc))
        }
    }

    fn credit_plan(&self, _accounts: &[Value], policy: &CyclePolicy, now: f64) -> Option<Result<Option<HashSet<String>>, Failure>> {
        Some(Ok(self.publisher.plan(&policy.reset.provider, now)))
    }

    fn refill_demand(&self, policy: &ResetPolicy, now: f64) -> Option<Result<Option<Value>, Failure>> {
        Some(Ok(self.publisher.refill_demand(&policy.provider, now)))
    }

    fn drain_first(&self, provider: &str, now: f64) -> Option<Result<Option<HashMap<String, f64>>, Failure>> {
        Some(Ok(self.publisher.drain_first(provider, now)))
    }
}
