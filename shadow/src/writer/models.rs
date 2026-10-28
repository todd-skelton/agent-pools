//! model-refresh.mjs on the copy: the hourly refresh (live model lists, admission probes, the
//! model registry and lineage) and the five-minute recheck that rebuilds a registry reporting a
//! current model unavailable, under model-store.mjs's lock and atomic writes.
//!
//! Agent Pools probes a model by running the real `codex` or `claude` tool on it, which is a
//! provider call. The copy's [`StandIn`] prober never does: it sends the same prompt to the
//! stand-in proxy's inference (so the copy's ledger counts it and the stand-in logs the pool's
//! routing lines), reads the answer as the tool's output would carry it, and judges it with the
//! real judges and, for Codex, the real routing evidence from the copy's `logs/main.log`. Its rows
//! say `standIn: true`. The benchmark and arena fetches (external sites, one with a key) are not
//! ported: the copy runs as `benchmarks: false` does and keeps the snapshot it was copied with.

use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use pool_core::js;
use pool_core::registry::{self, EFFORTS};
use serde_json::{Value, json};

use crate::dash::fmt::Zone;
use crate::watch::now_ms;

pub const MAX_PROBES_PER_RUN: f64 = 16.0;
/// Probes start only within this window of a run's start.
pub const PROBE_BUDGET_MS: f64 = 20.0 * 60_000.0;
pub const PROMPT: &str = super::fake_proxy::ADMISSION_PROMPT;
const PROBE_TIMEOUT: Duration = Duration::from_secs(240);
/// The scheduled tasks: the refresh hourly, the recheck every five minutes.
pub const EVERY: Duration = Duration::from_secs(3600);
pub const RECHECK_EVERY: Duration = Duration::from_secs(300);
pub const FIRST: Duration = Duration::from_secs(180);

// ---------------------------------------------------------------- model-store.mjs

/// `withLock` options: how long to wait for the lock, when an unrenewed lock is stale, how often
/// the holder renews it.
#[derive(Clone, Copy)]
pub struct Lock {
    pub wait_ms: f64,
    pub stale_ms: f64,
    pub renew_ms: f64,
}

impl Default for Lock {
    fn default() -> Lock {
        Lock { wait_ms: 60_000.0, stale_ms: 30.0 * 60_000.0, renew_ms: 60_000.0 }
    }
}

const RECLAIM_GUARD_STALE_MS: f64 = 60_000.0;

/// EEXIST, EPERM, EACCES, EBUSY: the file exists, or Windows is deleting it.
fn taken(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(e.kind(), AlreadyExists | PermissionDenied | ResourceBusy)
}

/// EPERM, EACCES, EBUSY: a reader holding the file open on Windows.
fn held_open(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(e.kind(), PermissionDenied | ResourceBusy)
}

fn suffixed(file: &Path, suffix: &str) -> PathBuf {
    let mut s = file.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// `Date.now() - stat(p).mtimeMs`, or `None` when it does not stat.
fn age_ms(p: &Path) -> Option<f64> {
    let m = std::fs::metadata(p).and_then(|m| m.modified()).ok()?;
    Some(match SystemTime::now().duration_since(m) {
        Ok(d) => d.as_secs_f64() * 1000.0,
        Err(e) => -(e.duration().as_secs_f64() * 1000.0),
    })
}

fn hex(bytes: usize) -> String {
    uuid::Uuid::new_v4().simple().to_string().chars().take(bytes * 2).collect()
}

/// Reclaiming a stale lock is serialised by a short-lived guard file, and the staleness is
/// checked again under it, so two contenders cannot both take over.
fn reclaim_if_stale(lock: &Path, stale_ms: f64) -> std::io::Result<()> {
    let guard = suffixed(lock, ".reclaim");
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&guard) {
        Ok(g) => drop(g),
        Err(e) if taken(&e) => {
            if age_ms(&guard).unwrap_or(0.0) > RECLAIM_GUARD_STALE_MS {
                let _ = std::fs::remove_file(&guard);
            }
            return Ok(());
        }
        Err(e) => return Err(e),
    }
    if age_ms(lock).is_some_and(|a| a > stale_ms) {
        let _ = std::fs::remove_file(lock);
    }
    let _ = std::fs::remove_file(&guard);
    Ok(())
}

/// Stops renewing the lock when the holder is done (or panics).
struct Renewing(tokio::task::JoinHandle<()>);

impl Drop for Renewing {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `withLock(file, fn)`: runs `f` holding `<file>.lock`. The holder renews the lock's time every
/// `renew_ms`, so only a lock whose holder stopped renewing ages past `stale_ms` and is taken
/// over. Release removes the lock only if it still carries this holder's token. Waiting past
/// `wait_ms` is `lock busy: <lock>`.
pub async fn with_lock<T, Fut: Future<Output = T>>(file: &Path, o: Lock, f: impl FnOnce() -> Fut) -> Result<T, String> {
    let lock = suffixed(file, ".lock");
    let token = format!("{}-{}", std::process::id(), hex(6));
    let start = now_ms();
    loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(mut h) => {
                let held = json!({ "pid": std::process::id(), "token": token, "at": js::iso(now_ms()) });
                h.write_all(js::stringify(&held).as_bytes()).map_err(|e| e.to_string())?;
                break;
            }
            Err(e) if taken(&e) => {
                if age_ms(&lock).unwrap_or(0.0) > o.stale_ms {
                    reclaim_if_stale(&lock, o.stale_ms).map_err(|e| e.to_string())?;
                    continue;
                }
                if now_ms() - start > o.wait_ms {
                    return Err(format!("lock busy: {}", lock.display()));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let renewing = {
        let lock = lock.clone();
        let every = Duration::from_secs_f64(o.renew_ms.max(1.0) / 1000.0);
        Renewing(tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let _ = std::fs::OpenOptions::new().write(true).open(&lock).and_then(|f| f.set_modified(SystemTime::now()));
            }
        }))
    };
    let out = f().await;
    drop(renewing);
    let held = std::fs::read_to_string(&lock).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).and_then(|v| v.get("token").and_then(Value::as_str).map(String::from));
    if held.as_deref() == Some(token.as_str()) {
        let _ = std::fs::remove_file(&lock);
    }
    Ok(out)
}

/// `writeJSONAtomic(file, value)`: `JSON.stringify(value, null, 2)` and a newline to a unique
/// temporary file, renamed over `file`, retried (up to 20 times) while a reader holds it open.
pub async fn write_json_atomic(file: &Path, value: &Value) -> std::io::Result<()> {
    let tmp = suffixed(file, &format!(".{}.{}.tmp", std::process::id(), hex(4)));
    std::fs::write(&tmp, js::stringify_pretty(value, 2) + "\n")?;
    let mut i = 0u64;
    loop {
        match std::fs::rename(&tmp, file) {
            Ok(()) => return Ok(()),
            Err(e) if held_open(&e) && i < 20 => {
                i += 1;
                tokio::time::sleep(Duration::from_millis(100 * i)).await;
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        }
    }
}

/// `readJSON(file, fallback)`: a missing file is `fallback` (when given); anything else that
/// does not read or parse is an error. A byte-order mark is allowed.
fn read_json(file: &Path, fallback: Option<Value>) -> Result<Value, String> {
    match std::fs::read_to_string(file) {
        Ok(text) => serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| format!("{}: {e}", file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fallback.ok_or_else(|| format!("{}: {e}", file.display())),
        Err(e) => Err(format!("{}: {e}", file.display())),
    }
}

// ---------------------------------------------------------------- model-admission.mjs

/// Runs one probe: a model at an effort, answering `{ ok, reason?, servedModel?, account?,
/// poolRequestId?, durationMs }`.
pub trait Prober: Send + Sync {
    /// The installed tools' versions (`harnessVersion(resolveExe(...))`) the evidence binds to.
    fn harness_versions(&self) -> Value;
    /// `batteryRunning()`: probing would compete with a running load, so it waits for next run.
    fn deferred(&self) -> bool;
    fn probe(&self, provider: &str, id: &str, effort: &str) -> impl Future<Output = Value> + Send;
}

/// `readProbes(root)`: every row of `state/model-admission.jsonl` (none when it is missing).
pub fn read_probes(root: &Path) -> Result<Vec<Value>, String> {
    let text = match std::fs::read_to_string(root.join("state/model-admission.jsonl")) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.to_string()),
    };
    text.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).map_err(|e| format!("state/model-admission.jsonl: {e}")))
        .collect()
}

/// `admit(root, targets, { log, deadline })`: one evidence row per (model, effort) appended to
/// `state/model-admission.jsonl`; no probe starts after `deadline`. An unsafe id or unknown
/// effort refuses the whole batch.
pub async fn admit<P: Prober>(root: &Path, targets: &[Value], prober: &P, versions: &Value, log: &(dyn Fn(&str) + Sync), deadline: f64) -> Result<Vec<Value>, String> {
    let out = root.join("state/model-admission.jsonl");
    let mut rows = Vec::new();
    for t in targets {
        let (id, provider) = (js::to_string(t.get("id")), js::to_string(t.get("provider")));
        if !registry::safe_id(&id) || !["claude", "codex"].contains(&provider.as_str()) {
            return Err(format!("refusing unsafe target {id}"));
        }
        for effort in js::values(t.get("efforts")) {
            let effort = js::to_string(Some(effort));
            if !EFFORTS.contains(&effort.as_str()) {
                return Err(format!("refusing effort {effort}"));
            }
            if now_ms() >= deadline {
                log(&format!("admission budget spent; {id} {effort} left for the next run"));
                continue;
            }
            let res = prober.probe(&provider, &id, &effort).await;
            let ok = res.get("ok") == Some(&Value::Bool(true));
            let or_null = |k: &str| res.get(k).cloned().unwrap_or(Value::Null);
            let mut row = json!({
                "at": js::iso(now_ms()), "model": id, "provider": provider, "effort": effort,
                "harnessVersion": versions.get(&provider).cloned().unwrap_or(Value::Null),
                "ok": ok, "reason": if ok { Value::Null } else { or_null("reason") }, "servedModel": or_null("servedModel"),
                "account": or_null("account"), "poolRequestId": or_null("poolRequestId"), "durationMs": or_null("durationMs"),
            });
            if let Some(s) = res.get("standIn") {
                row["standIn"] = s.clone();
            }
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&out).map_err(|e| e.to_string())?;
            f.write_all(format!("{}\n", js::stringify(&row)).as_bytes()).map_err(|e| e.to_string())?;
            log(&format!("{id} {effort}: {}", if ok { "ok".to_string() } else { format!("FAILED {}", js::to_string(row.get("reason"))) }));
            rows.push(row);
        }
    }
    Ok(rows)
}

/// The copy's prober: the stand-in proxy answers the probe, nothing runs `codex` or `claude`.
pub struct StandIn {
    root: PathBuf,
    proxy: String,
    client_key: String,
    http: reqwest::Client,
    versions: Value,
    zone: Zone,
    /// model-admission.mjs waits this long for the pool to flush its log line.
    pub flush: Duration,
}

impl StandIn {
    /// The stand-in at `proxy` with the pool's client key. The harness versions are the ones the
    /// copy's registry recorded, so the evidence copied with it keeps counting.
    pub fn new(root: PathBuf, proxy: &str, client_key: String) -> StandIn {
        let versions = read_json(&root.join("state/model-registry.json"), None)
            .ok()
            .and_then(|r| r.get("harnessVersions").filter(|v| v.is_object()).cloned())
            .unwrap_or_else(|| json!({ "claude": "stand-in", "codex": "stand-in" }));
        StandIn {
            root,
            proxy: proxy.trim_end_matches('/').to_string(),
            client_key,
            http: reqwest::Client::builder().no_proxy().build().expect("an HTTP client with no TLS and no proxy builds"),
            versions,
            zone: Zone::from_env(),
            flush: Duration::from_millis(1500),
        }
    }

    /// `Date.parse` of a pool log time (`YYYY-MM-DDTHH:mm:ss`, local).
    fn local_ms(&self, stamp: &str) -> f64 {
        let Some(utc) = js::date_parse(&format!("{stamp}Z")) else { return f64::NAN };
        let guess = utc - f64::from(self.zone.standard) * 3_600_000.0;
        utc - f64::from(self.zone.offset_hours(guess)) * 3_600_000.0
    }

    async fn ask(&self, path: &str, body: Value) -> Result<Value, i32> {
        let r = self.http.post(format!("{}{path}", self.proxy)).bearer_auth(&self.client_key).json(&body).timeout(PROBE_TIMEOUT).send().await.map_err(|_| -1)?;
        if !r.status().is_success() {
            return Err(1);
        }
        r.json().await.map_err(|_| 1)
    }
}

impl Prober for StandIn {
    fn harness_versions(&self) -> Value {
        self.versions.clone()
    }

    fn deferred(&self) -> bool {
        false
    }

    /// `probe(...)`: Claude as `claude --print --output-format json` would print the answer
    /// (its result and model usage); Codex as `codex exec --json` would (the agent message, the
    /// turn completed), then the pool log's routing evidence inside the probe's window. A failed
    /// request is the tool's failed exit (`exit 1`; `exit -1` when nothing answered).
    fn probe(&self, provider: &str, id: &str, effort: &str) -> impl Future<Output = Value> + Send {
        let (provider, id, effort) = (provider.to_string(), id.to_string(), effort.to_string());
        async move {
            let start = now_ms();
            let mut verdict = if provider == "claude" {
                let body = json!({ "model": id, "max_tokens": 64, "output_config": { "effort": effort }, "messages": [{ "role": "user", "content": PROMPT }] });
                match self.ask("/v1/messages", body).await {
                    Ok(a) => {
                        let text = js::values(a.get("content")).first().map(|c| js::to_string(c.get("text"))).unwrap_or_default();
                        let mut usage = serde_json::Map::new();
                        usage.insert(js::to_string(a.get("model")), json!({}));
                        let printed = json!({ "type": "result", "subtype": "success", "is_error": false, "result": text, "modelUsage": usage });
                        registry::judge_claude(&id, &js::stringify(&printed)).unwrap_or_else(|e| json!({ "ok": false, "reason": e }))
                    }
                    Err(code) => json!({ "ok": false, "reason": format!("exit {code}") }),
                }
            } else {
                let body = json!({ "model": id, "reasoning": { "effort": effort }, "input": PROMPT, "stream": false });
                match self.ask("/v1/responses", body).await {
                    Ok(a) => {
                        let text = js::values(a.get("output")).iter().flat_map(|o| js::values(o.get("content"))).map(|c| js::to_string(c.get("text"))).collect::<String>();
                        let events = [
                            json!({ "type": "thread.started" }),
                            json!({ "type": "turn.started" }),
                            json!({ "type": "item.completed", "item": { "id": "item_0", "type": "agent_message", "text": text } }),
                            json!({ "type": "turn.completed", "usage": a.get("usage").cloned().unwrap_or(Value::Null) }),
                        ];
                        registry::judge_codex(&events.iter().map(js::stringify).collect::<Vec<_>>().join("\n"))
                    }
                    Err(code) => json!({ "ok": false, "reason": format!("exit {code}") }),
                }
            };
            let end = now_ms();
            if provider == "codex" && verdict.get("ok") == Some(&Value::Bool(true)) {
                tokio::time::sleep(self.flush).await;
                let log = std::fs::read_to_string(self.root.join("logs/main.log")).unwrap_or_default();
                let routed = registry::pool_routing(&log, &id, start, end, |s| self.local_ms(s));
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
            verdict["standIn"] = json!(true);
            verdict
        }
    }
}

// ---------------------------------------------------------------- model-refresh.mjs

/// `refresh(root, { probe, benchmarks: false })`: under the registry's lock, builds the registry
/// from the live model lists, the pool's status (`status`, as `/api/status` answers; `None` when
/// it does not), the families, the evidence and the lineage; probes the efforts the plan lacks
/// (16 at most, least recently tried first, within the probe budget) and builds again; then
/// writes the lineage and the registry (with the benchmark snapshot on disk). Answers the registry.
pub async fn refresh<P: Prober>(root: &Path, prober: &P, status: impl FnOnce() -> Option<Value>, probe: bool, log: &(dyn Fn(&str) + Sync)) -> Result<Value, String> {
    let file = root.join("state/model-registry.json");
    let target = file.clone();
    with_lock(&file, Lock::default(), || async move {
        let file = target;
        let started = now_ms();
        let families = read_json(&root.join("model-families.json"), None)?;
        let live_models = read_json(&root.join("state/live-models.json"), None)?;
        let status = status().unwrap_or(Value::Null);
        let lineage_path = root.join("state/model-lineage.json");
        let lineage = read_json(&lineage_path, Some(registry::empty_lineage()))?;
        let harness = prober.harness_versions();
        let mut probes = read_probes(root)?;
        let build = |probes: &[Value]| {
            registry::build_registry(&json!({
                "liveModels": live_models, "status": status, "families": families, "probes": probes,
                "lineage": lineage, "harnessVersions": harness, "now": now_ms(),
            }))
        };
        let mut reg = build(&probes)?["registry"].clone();
        let policy = read_json(&root.join("state/routing-policy.json"), Some(Value::Null))?;
        let pointer = read_json(&root.join("state/calibration/latest.json"), Some(Value::Null))?;
        let preview = match pointer.get("file").map(|f| js::to_string(Some(f))) {
            Some(f) if js::truthy(Some(&pointer)) => {
                let name = Path::new(&f.replace('\\', "/")).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                read_json(&root.join("state/calibration").join(name), None).ok()
            }
            _ => None,
        };
        let plan = registry::probe_plan(&reg, &families, &Value::Array(probes.clone()), &harness, now_ms(), &registry::wanted_efforts(Some(&policy), preview.as_ref()));
        if !plan.is_empty() && probe {
            if prober.deferred() {
                log("admission deferred: controller battery running");
                if let Some(n) = reg.get_mut("notices").and_then(Value::as_array_mut) {
                    n.push(json!({ "notice": "admission-deferred-battery" }));
                }
            } else {
                let batch = registry::fair_batch(&Value::Array(plan), &Value::Array(probes.clone()), MAX_PROBES_PER_RUN, now_ms());
                admit(root, &batch, prober, &harness, log, started + PROBE_BUDGET_MS).await?;
                probes = read_probes(root)?;
            }
        }
        // As in model-refresh.mjs, the second build replaces the first (a deferral's notice too).
        let built = build(&probes)?;
        let mut reg = built["registry"].clone();
        reg["benchmarks"] = read_json(&root.join("state/benchmarks/latest.json"), Some(Value::Null))?;
        write_json_atomic(&lineage_path, &built["lineage"]).await.map_err(|e| e.to_string())?;
        write_json_atomic(&file, &reg).await.map_err(|e| e.to_string())?;
        for (family, f) in js::entries(reg.get("families")) {
            let candidate = f.get("candidate").filter(|c| js::truthy(Some(*c))).map(|c| format!(" candidate={}", js::to_string(Some(c)))).unwrap_or_default();
            let unavailable: Vec<String> = js::values(f.get("unavailableEfforts")).into_iter().map(|e| js::to_string(Some(e))).collect();
            let unavailable = if unavailable.is_empty() { String::new() } else { format!(" unavailable={}", unavailable.join(",")) };
            log(&format!("{family:<7} current={}{candidate}{unavailable}", js::to_string(f.get("current"))));
        }
        Ok(reg)
    })
    .await?
}

/// `recheck(root)`: rebuilds (no probes, no benchmarks) only while the registry is missing or a
/// family reports an unavailable effort; a run holding the lock means it is skipped. Answers
/// whether it rebuilt.
pub async fn recheck<P: Prober>(root: &Path, prober: &P, status: impl FnOnce() -> Option<Value>, log: &(dyn Fn(&str) + Sync)) -> Result<bool, String> {
    let current = read_json(&root.join("state/model-registry.json"), None).ok();
    if !registry::degraded(current.as_ref()) {
        return Ok(false);
    }
    log(&format!("--- model-refresh --recheck {} pid={}", js::iso(now_ms()), std::process::id()));
    match refresh(root, prober, status, false, log).await {
        Ok(_) => Ok(true),
        Err(e) if e.starts_with("lock busy") => {
            log("recheck skipped: refresh already running");
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// A line in the copy's `logs/model-refresh.log` (where Run-Model-Refresh.vbs sends the output).
pub fn log_line(root: &Path, line: &str) {
    let _ = std::fs::create_dir_all(root.join("logs"));
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(root.join("logs/model-refresh.log")) {
        let _ = f.write_all(format!("{line}\n").as_bytes());
    }
}

/// The two scheduled tasks on the copy, in one: the refresh `first` after the start and then
/// every `every`, the recheck every `recheck_every`. One at a time, as Task Scheduler's
/// IgnoreNew and the lock keep them. `changed` is told after each run that wrote.
pub async fn run<P: Prober + 'static>(
    root: PathBuf,
    prober: Arc<P>,
    status: impl Fn() -> Option<Value> + Send + Sync,
    changed: impl Fn() + Send,
    (first, every, recheck_every): (Duration, Duration, Duration),
    signal: corten::Signal,
) {
    let log = |l: &str| log_line(&root, l);
    let mut next_refresh = tokio::time::Instant::now() + first;
    let mut next_recheck = tokio::time::Instant::now() + recheck_every;
    loop {
        let refresh_due = next_refresh <= next_recheck;
        tokio::select! {
            _ = tokio::time::sleep_until(if refresh_due { next_refresh } else { next_recheck }) => {}
            _ = signal.aborted() => return,
        }
        if refresh_due {
            log(&format!("--- model-refresh {} pid={}", js::iso(now_ms()), std::process::id()));
            match refresh(&root, &*prober, &status, true, &log).await {
                Ok(_) => changed(),
                Err(e) => log(&format!("model-refresh failed: {e}")),
            }
            next_refresh = tokio::time::Instant::now() + every;
        } else {
            match recheck(&root, &*prober, &status, &log).await {
                Ok(true) => changed(),
                Ok(false) => {}
                Err(e) => log(&format!("model-refresh --recheck failed: {e}")),
            }
            next_recheck = tokio::time::Instant::now() + recheck_every;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pools-shadow-models-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn clean(dir: &Path) {
        for f in std::fs::read_dir(dir).unwrap().flatten() {
            let _ = std::fs::remove_file(f.path());
        }
        let _ = std::fs::remove_dir(dir);
    }

    /// Agent Pools' "F8: lock serializes writers and atomic write leaves no temp files".
    #[tokio::test]
    async fn the_lock_serializes_writers_and_an_atomic_write_leaves_no_temp_files() {
        let dir = scratch("serial");
        let file = dir.join("r.json");
        let order = Mutex::new(Vec::new());
        let writer = |i: u32| {
            let (file, order) = (&file, &order);
            async move {
                with_lock(file, Lock::default(), || async move {
                    order.lock().unwrap().push(format!("in{i}"));
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    order.lock().unwrap().push(format!("out{i}"));
                    write_json_atomic(file, &json!({ "i": i })).await.unwrap();
                })
                .await
                .unwrap()
            }
        };
        tokio::join!(writer(1), writer(2), writer(3));
        let order = order.into_inner().unwrap();
        for pair in order.chunks(2) {
            assert_eq!(pair[0][2..], pair[1][3..], "{order:?}");
        }
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert!([1, 2, 3].contains(&doc["i"].as_u64().unwrap()));
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["r.json"], "no lock or temporary file is left");
        clean(&dir);
    }

    /// Agent Pools' "review r3: a renewed lock is never taken over, and release never removes a
    /// successor's lock".
    #[tokio::test(flavor = "multi_thread")]
    async fn a_renewed_lock_is_never_taken_over_and_release_keeps_a_successors_lock() {
        let dir = scratch("renewed");
        let file = dir.join("x.json");
        let order = Arc::new(Mutex::new(Vec::new()));
        let holder = {
            let (file, order) = (file.clone(), order.clone());
            tokio::spawn(async move {
                with_lock(&file, Lock { renew_ms: 50.0, ..Lock::default() }, || async {
                    order.lock().unwrap().push("a-start");
                    tokio::time::sleep(Duration::from_millis(700)).await;
                    order.lock().unwrap().push("a-end");
                })
                .await
                .unwrap()
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        with_lock(&file, Lock { stale_ms: 300.0, wait_ms: 5000.0, ..Lock::default() }, || async {
            order.lock().unwrap().push("b");
        })
        .await
        .unwrap();
        holder.await.unwrap();
        assert_eq!(*order.lock().unwrap(), ["a-start", "a-end", "b"], "a lock older than stale_ms but still renewed is respected");
        let lock = suffixed(&file, ".lock");
        with_lock(&file, Lock::default(), || async {
            std::fs::remove_file(&lock).unwrap();
            std::fs::write(&lock, r#"{"token":"successor"}"#).unwrap();
        })
        .await
        .unwrap();
        let held: Value = serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        assert_eq!(held["token"], "successor", "the successor's lock survives the first holder's release");
        clean(&dir);
    }

    /// Agent Pools' "review r4: concurrent takeover of a stale lock yields one owner at a time".
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stale_lock_taken_over_by_many_has_one_owner_at_a_time() {
        let dir = scratch("race");
        let file = dir.join("x.json");
        let lock = suffixed(&file, ".lock");
        for round in 0..5 {
            std::fs::write(&lock, r#"{"token":"dead"}"#).unwrap();
            let old = SystemTime::now() - Duration::from_secs(3600);
            std::fs::OpenOptions::new().write(true).open(&lock).unwrap().set_modified(old).unwrap();
            let (inside, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let mut all = tokio::task::JoinSet::new();
            for _ in 0..6 {
                let (file, inside, most) = (file.clone(), inside.clone(), most.clone());
                all.spawn(async move {
                    with_lock(&file, Lock { stale_ms: 1000.0, wait_ms: 10_000.0, ..Lock::default() }, || async {
                        let n = inside.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(n, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        inside.fetch_sub(1, Ordering::SeqCst);
                    })
                    .await
                    .unwrap()
                });
            }
            while let Some(r) = all.join_next().await {
                r.unwrap();
            }
            assert_eq!(most.load(Ordering::SeqCst), 1, "round {round}: two holders overlapped");
        }
        clean(&dir);
    }

    struct Never;

    impl Prober for Never {
        fn harness_versions(&self) -> Value {
            json!({})
        }
        fn deferred(&self) -> bool {
            false
        }
        #[allow(clippy::manual_async_fn)]
        fn probe(&self, _: &str, _: &str, _: &str) -> impl Future<Output = Value> + Send {
            async { panic!("a recheck never probes") }
        }
    }

    /// Agent Pools' "recheck rebuilds only while a family reports an unavailable effort".
    #[tokio::test]
    async fn recheck_rebuilds_only_while_a_family_reports_an_unavailable_effort() {
        assert!(registry::degraded(None), "a missing registry is rebuilt");
        assert!(!registry::degraded(Some(&json!({ "families": { "sol": { "unavailableEfforts": [] }, "opus": { "unavailableEfforts": [] } } }))));
        assert!(registry::degraded(Some(&json!({ "families": { "sol": { "unavailableEfforts": ["high"] }, "opus": { "unavailableEfforts": [] } } }))));
        let root = scratch("recheck");
        std::fs::create_dir_all(root.join("state")).unwrap();
        let file = root.join("state/model-registry.json");
        let healthy = r#"{"families":{"sol":{"unavailableEfforts":[]}}}"#;
        std::fs::write(&file, healthy).unwrap();
        let lines = Mutex::new(Vec::<String>::new());
        let log = |l: &str| lines.lock().unwrap().push(l.to_string());
        assert_eq!(recheck(&root, &Never, || None, &log).await, Ok(false));
        assert!(lines.lock().unwrap().is_empty(), "a healthy registry is left alone without logging");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), healthy);

        // A degraded one is rebuilt (here it cannot be: there are no families to build from).
        std::fs::write(&file, r#"{"families":{"sol":{"unavailableEfforts":["high"]}}}"#).unwrap();
        let rebuilt = recheck(&root, &Never, || None, &log).await;
        assert!(rebuilt.as_ref().is_err_and(|e| e.contains("model-families.json")), "{rebuilt:?}");
        assert!(lines.lock().unwrap()[0].starts_with("--- model-refresh --recheck "));
        // Another run's lock: waiting past wait_ms is "lock busy", which recheck takes as skipped.
        std::fs::write(suffixed(&file, ".lock"), r#"{"token":"other"}"#).unwrap();
        let busy = with_lock(&file, Lock { wait_ms: 0.0, ..Lock::default() }, || async {}).await;
        assert!(busy.is_err_and(|e| e.starts_with("lock busy")));
        std::fs::remove_file(suffixed(&file, ".lock")).unwrap();
        std::fs::remove_file(&file).unwrap();
        std::fs::remove_dir(root.join("state")).unwrap();
        clean(&root);
    }
}
