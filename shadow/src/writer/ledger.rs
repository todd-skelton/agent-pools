//! usage-ledger.mjs's drain on the copy: the proxy's per-request usage queue
//! (`usage-queue?count=`, which removes what it returns) is popped in batches every
//! `drainIntervalSeconds` while the proxy answers, each record becomes a row
//! (`pool_core::ledger::ledger_row`), and rows are appended to
//! `logs/usage-ledger/raw-YYYY-MM-DD.jsonl` by the UTC day of the request. Rows that cannot be
//! written are held (up to `maxPendingRows`) for the next drain, old files are pruned once a day,
//! and a problem is logged once each time it changes. On a copy the queue is the stand-in's.
//!
//! One difference: a failed write's message is the operating system's text as Rust reports it,
//! not Node's (`EEXIST: ...`).

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pool_core::js;
use pool_core::ledger::{LEDGER_DIR, LedgerConfig, ledger_config, ledger_row};
use serde_json::{Map, Value, json};

const DAY_MS: f64 = 86_400_000.0;

/// What a drain reads and tells: the queue, install.json, and the event log.
pub trait Source: Send + Sync {
    /// `popQueue(count)`: the next records, removed from the queue.
    fn pop(&self, count: usize) -> impl Future<Output = Result<Value, String>> + Send;
    /// `readConfig()`: install.json, `None` when it does not read.
    fn install(&self) -> Option<Value>;
    /// `event(type, details)`; a failed write is dropped.
    fn event(&self, kind: &str, details: Value);
}

impl Source for super::io::FileIo {
    async fn pop(&self, count: usize) -> Result<Value, String> {
        self.call(&format!("usage-queue?count={count}"), None, None).await.map_err(|e| e.message)
    }

    fn install(&self) -> Option<Value> {
        super::io::FileIo::install(self)
    }

    fn event(&self, kind: &str, details: Value) {
        self.log_event(kind, details)
    }
}

/// The ledger's stats as `/api/status` shows them (`usageLedger`).
#[derive(Clone, Debug, Default)]
struct Stats {
    drained_at: Option<String>,
    rows: u64,
    dropped: u64,
    pending: usize,
    last_error: Option<String>,
}

struct Inner {
    pending: Vec<Value>,
    last_problem: String,
    last_prune_day: Option<String>,
}

/// `createUsageLedger({ root, popQueue, accounts, readConfig, event, now, batch, maxBatchesPerDrain })`.
pub struct Ledger {
    root: PathBuf,
    batch: usize,
    max_batches: usize,
    /// `accounts()`: the pool's accounts as the refresh cycle last left them.
    accounts: Mutex<Vec<Value>>,
    config: Mutex<LedgerConfig>,
    stats: Mutex<Stats>,
    inner: tokio::sync::Mutex<Inner>,
    /// Drains finished; a caller that waited through one shares it (single flight).
    drains: AtomicU64,
}

impl Ledger {
    pub fn new(root: PathBuf) -> Ledger {
        Ledger::with_batches(root, 1000, 50)
    }

    pub fn with_batches(root: PathBuf, batch: usize, max_batches: usize) -> Ledger {
        Ledger {
            root,
            batch: batch.max(1),
            max_batches,
            accounts: Mutex::new(Vec::new()),
            config: Mutex::new(LedgerConfig::default()),
            stats: Mutex::new(Stats::default()),
            inner: tokio::sync::Mutex::new(Inner { pending: Vec::new(), last_problem: String::new(), last_prune_day: None }),
            drains: AtomicU64::new(0),
        }
    }

    /// Keeps the pool's accounts (`state.accounts`) for attributing rows.
    pub fn set_accounts(&self, state: &Value) {
        let accounts = js::values(state.get("accounts")).into_iter().cloned().collect();
        *self.accounts.lock().unwrap_or_else(|e| e.into_inner()) = accounts;
    }

    /// `stats()`: `{ drainedAt, rows, dropped, pending, lastError, enabled }`.
    pub fn stats(&self) -> Value {
        let s = self.stats.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let enabled = self.config.lock().unwrap_or_else(|e| e.into_inner()).enabled;
        json!({ "drainedAt": s.drained_at, "rows": s.rows, "dropped": s.dropped, "pending": s.pending, "lastError": s.last_error, "enabled": enabled })
    }

    /// `intervalMs()`, as a duration.
    pub fn interval(&self) -> Duration {
        Duration::from_secs_f64(self.config.lock().unwrap_or_else(|e| e.into_inner()).drain_interval_seconds)
    }

    /// `drain()`: one drain, shared by every call that overlaps it.
    pub async fn drain(&self, source: &impl Source, now: impl Fn() -> f64) -> Value {
        let seen = self.drains.load(Ordering::SeqCst);
        let mut inner = self.inner.lock().await;
        if self.drains.load(Ordering::SeqCst) != seen {
            return self.stats();
        }
        self.drain_once(&mut inner, source, &now).await;
        self.drains.fetch_add(1, Ordering::SeqCst);
        self.stats()
    }

    async fn drain_once(&self, inner: &mut Inner, source: &impl Source, now: &impl Fn() -> f64) {
        let install = source.install();
        let (config, problems) = ledger_config(install.as_ref().and_then(|i| i.get("usageLedger")));
        *self.config.lock().unwrap_or_else(|e| e.into_inner()) = config.clone();
        // Disabled: leave the queue alone (records expire in the proxy).
        if !config.enabled {
            self.problem(inner, source, problems.join("; "), Map::new());
            return;
        }
        let map: HashMap<String, Value> = self
            .accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|a| js::truthy(a.get("auth_index")))
            .map(|a| (js::to_string(a.get("auth_index")), a.get("id").cloned().unwrap_or(Value::Null)))
            .collect();
        let mut read_error = None;
        let mut dropped = 0;
        for _ in 0..self.max_batches {
            let items = match source.pop(self.batch).await {
                Ok(v) => v,
                Err(e) => {
                    read_error = Some(e);
                    break;
                }
            };
            let Value::Array(items) = items else { break };
            if items.is_empty() {
                break;
            }
            for item in &items {
                match ledger_row(item, &map) {
                    Some(row) => inner.pending.push(row),
                    None => dropped += 1,
                }
            }
            if items.len() < self.batch {
                break;
            }
        }
        if inner.pending.len() > config.max_pending_rows {
            let over = inner.pending.len() - config.max_pending_rows;
            dropped += over as u64;
            inner.pending.drain(..over);
        }
        let mut write_error = None;
        let mut written = 0;
        if !inner.pending.is_empty() {
            match self.write(&inner.pending) {
                Ok(()) => {
                    written = inner.pending.len() as u64;
                    inner.pending.clear();
                }
                Err(e) => write_error = Some(e.to_string()),
            }
        }
        self.prune(inner, config.raw_retention_days, now());
        {
            let mut s = self.stats.lock().unwrap_or_else(|e| e.into_inner());
            s.rows += written;
            s.dropped += dropped;
            s.drained_at = Some(js::iso(now()));
            s.pending = inner.pending.len();
        }
        let mut message: Vec<String> = problems;
        message.extend(read_error.map(|e| format!("queue read failed: {e}")));
        let failed_write = write_error.is_some();
        message.extend(write_error.map(|e| format!("write failed: {e}")));
        let mut details = Map::new();
        if failed_write {
            details.insert("pending".into(), json!(inner.pending.len()));
        }
        self.problem(inner, source, message.join("; "), details);
    }

    /// `problem(message, details)`: the last error, and one event per distinct problem.
    fn problem(&self, inner: &mut Inner, source: &impl Source, message: String, details: Map<String, Value>) {
        self.stats.lock().unwrap_or_else(|e| e.into_inner()).last_error = (!message.is_empty()).then(|| message.clone());
        if message == inner.last_problem {
            return;
        }
        inner.last_problem = message.clone();
        if !message.is_empty() {
            let mut d = Map::new();
            d.insert("problem".into(), json!(message));
            d.extend(details);
            source.event("usage-ledger-problem", Value::Object(d));
        }
    }

    /// Appends each row to its UTC day's file, days in the order first seen.
    fn write(&self, rows: &[Value]) -> std::io::Result<()> {
        use std::io::Write;
        let dir = self.root.join(LEDGER_DIR);
        std::fs::create_dir_all(&dir)?;
        let mut days: Vec<(String, String)> = Vec::new();
        for row in rows {
            let day: String = js::to_string(row.get("at")).chars().take(10).collect();
            let line = js::stringify(row) + "\n";
            match days.iter_mut().find(|(d, _)| *d == day) {
                Some((_, text)) => text.push_str(&line),
                None => days.push((day, line)),
            }
        }
        for (day, text) in days {
            std::fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("raw-{day}.jsonl")))?.write_all(text.as_bytes())?;
        }
        Ok(())
    }

    /// Once a UTC day: removes `raw-YYYY-MM-DD.jsonl` files dated before the retention.
    fn prune(&self, inner: &mut Inner, retention_days: f64, now: f64) {
        let today: String = js::iso(now).chars().take(10).collect();
        if inner.last_prune_day.as_deref() == Some(today.as_str()) {
            return;
        }
        inner.last_prune_day = Some(today.clone());
        let Some(midnight) = js::date_parse(&today) else { return };
        let cutoff: String = js::iso(midnight - retention_days * DAY_MS).chars().take(10).collect();
        let dir = self.root.join(LEDGER_DIR);
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if ledger_day(&name).is_some_and(|day| day < cutoff.as_str()) {
                let _ = std::fs::remove_file(dir.join(&name));
            }
        }
    }
}

/// `LEDGER_FILE` (`/^raw-(\d{4}-\d{2}-\d{2})\.jsonl$/`): the day a ledger file holds.
pub fn ledger_day(name: &str) -> Option<&str> {
    let day = name.strip_prefix("raw-")?.strip_suffix(".jsonl")?;
    let b = day.as_bytes();
    let shape = b.len() == 10 && b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() });
    shape.then_some(day)
}

/// `scheduleLedger`: a first drain 5 s after the start, then one every `drainIntervalSeconds`
/// after the last ends, only while the proxy answers (records wait in its queue meanwhile). On
/// shutdown, a last drain of at most 5 s when the proxy answers, as pool-service.mjs's `stop`.
/// `changed` is called when a drain changes what the dashboard shows (not only `drainedAt`).
pub async fn run(
    ledger: std::sync::Arc<Ledger>,
    source: std::sync::Arc<super::io::FileIo>,
    health: std::sync::Arc<super::health::Health>,
    first: Duration,
    changed: impl Fn() + Send,
    signal: corten::Signal,
) {
    let shown = |mut stats: Value| {
        stats.as_object_mut().map(|m| m.shift_remove("drainedAt"));
        stats
    };
    let mut wait = first;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = signal.aborted() => break,
        }
        if health.healthy() {
            let before = shown(ledger.stats());
            ledger.drain(&*source, super::io::now_ms).await;
            if shown(ledger.stats()) != before {
                changed();
            }
        }
        wait = ledger.interval();
    }
    if health.healthy() {
        let _ = tokio::time::timeout(Duration::from_secs(5), ledger.drain(&*source, super::io::now_ms)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A queue record as the proxy's usage queue gives it (usage-ledger.test.mjs's `record`).
    fn record(extra: Value) -> Value {
        let mut r = json!({
            "timestamp": "2026-10-15T08:47:34.9727204-05:00", "latency_ms": 6417, "auth_index": "aaaa",
            "user_agent": "claude-cli/2.1.299 (external, sdk-cli)",
            "tokens": { "input_tokens": 2, "output_tokens": 666, "cache_read_tokens": 78668, "cache_creation_tokens": 1185 },
            "failed": false, "fail": { "status_code": 200, "body": "" }, "provider": "claude", "model": "claude-opus-5-5",
            "endpoint": "POST /v1/messages", "request_id": "8241b157", "session_id": "45dbe21a", "reasoning_effort": "high",
        });
        if let (Value::Object(r), Value::Object(e)) = (&mut r, extra) {
            r.extend(e);
        }
        r
    }

    struct Fake {
        queue: Mutex<Vec<Value>>,
        calls: Mutex<Vec<usize>>,
        install: Value,
        events: Mutex<Vec<(String, Value)>>,
        gate: Option<tokio::sync::Semaphore>,
    }

    impl Fake {
        fn new(queue: Vec<Value>, install: Value) -> Fake {
            Fake { queue: Mutex::new(queue), calls: Mutex::new(Vec::new()), install, events: Mutex::new(Vec::new()), gate: None }
        }
    }

    impl Source for Fake {
        async fn pop(&self, count: usize) -> Result<Value, String> {
            self.calls.lock().unwrap().push(count);
            if let Some(gate) = &self.gate {
                let _ = gate.acquire().await;
            }
            let mut q = self.queue.lock().unwrap();
            let n = count.min(q.len());
            Ok(Value::Array(q.drain(..n).collect()))
        }
        fn install(&self) -> Option<Value> {
            Some(self.install.clone())
        }
        fn event(&self, kind: &str, details: Value) {
            self.events.lock().unwrap().push((kind.into(), details));
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pools-ledger-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn lines(path: PathBuf) -> Vec<Value> {
        std::fs::read_to_string(path).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[tokio::test]
    async fn a_drain_pops_every_batch_writes_rows_by_utc_day_and_drops_what_it_cannot_read() {
        let root = temp("days");
        let queue = vec![record(json!({})), record(json!({ "timestamp": "2026-10-15T23:59:59Z" })), record(json!({ "timestamp": "2026-10-16T00:00:01Z" })), json!("{bad")];
        let fake = Fake::new(queue, json!({}));
        let ledger = Ledger::with_batches(root.clone(), 2, 50);
        ledger.set_accounts(&json!({ "accounts": [{ "id": "claude-1.json", "auth_index": "aaaa" }] }));
        let stats = ledger.drain(&fake, || js::date_parse("2026-10-16T00:00:05Z").unwrap()).await;
        assert_eq!(*fake.calls.lock().unwrap(), vec![2, 2, 2]);
        assert_eq!((&stats["rows"], &stats["dropped"], &stats["pending"]), (&json!(3), &json!(1), &json!(0)));
        assert_eq!(stats["drainedAt"], "2026-10-16T00:00:05.000Z");
        let day1 = lines(root.join(LEDGER_DIR).join("raw-2026-10-15.jsonl"));
        let day2 = lines(root.join(LEDGER_DIR).join("raw-2026-10-16.jsonl"));
        assert_eq!((day1.len(), day2.len()), (2, 1));
        assert_eq!(day2[0]["acct"], "claude-1.json");
        assert_eq!(day1[0]["at"], "2026-10-15T13:47:34.972Z");
        assert!(fake.events.lock().unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_failed_write_keeps_the_popped_rows_for_the_next_drain_and_says_so_once() {
        let root = temp("blocked");
        std::fs::create_dir_all(root.join("logs")).unwrap();
        // A file where the ledger directory should be makes creating it fail.
        std::fs::write(root.join(LEDGER_DIR), "blocker").unwrap();
        let fake = Fake::new(vec![record(json!({}))], json!({}));
        let ledger = Ledger::new(root.clone());
        let stats = ledger.drain(&fake, || 0.0).await;
        assert_eq!(stats["pending"], 1);
        assert!(stats["lastError"].as_str().unwrap().starts_with("write failed: "), "{stats}");
        let stats = ledger.drain(&fake, || 0.0).await;
        assert_eq!(stats["pending"], 1);
        {
            let events = fake.events.lock().unwrap();
            assert_eq!(events.len(), 1, "the same problem is logged once");
            assert_eq!(events[0].0, "usage-ledger-problem");
            assert_eq!(events[0].1["pending"], 1);
        }
        std::fs::remove_file(root.join(LEDGER_DIR)).unwrap();
        let stats = ledger.drain(&fake, || 0.0).await;
        assert_eq!((&stats["pending"], &stats["rows"], &stats["lastError"]), (&json!(0), &json!(1), &Value::Null));
        assert_eq!(std::fs::read_dir(root.join(LEDGER_DIR)).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn disabled_leaves_the_queue_alone_old_files_are_pruned_and_drains_are_single_flight() {
        let root = temp("prune");
        let disabled = Fake::new(vec![record(json!({}))], json!({ "usageLedger": { "enabled": false } }));
        let ledger = Ledger::new(root.clone());
        let stats = ledger.drain(&disabled, || 0.0).await;
        assert!(disabled.calls.lock().unwrap().is_empty());
        assert_eq!(ledger.stats()["enabled"], false);
        assert_eq!(stats["drainedAt"], Value::Null);

        let dir = root.join(LEDGER_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["raw-2026-01-01.jsonl", "raw-2026-10-14.jsonl", "raw-2026-9-1.jsonl"] {
            std::fs::write(dir.join(name), "{}\n").unwrap();
        }
        std::fs::write(dir.join("notes.txt"), "keep").unwrap();
        let gated = Arc::new(Fake { gate: Some(tokio::sync::Semaphore::new(0)), ..Fake::new(Vec::new(), json!({})) });
        let ledger = Arc::new(Ledger::new(root.clone()));
        let now = || js::date_parse("2026-10-15T12:00:00Z").unwrap();
        let (l1, f1, l2, f2) = (ledger.clone(), gated.clone(), ledger.clone(), gated.clone());
        let a = tokio::spawn(async move { l1.drain(&*f1, now).await });
        let b = tokio::spawn(async move { l2.drain(&*f2, now).await });
        while gated.calls.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        gated.gate.as_ref().unwrap().add_permits(10);
        let (a, b) = (a.await.unwrap(), b.await.unwrap());
        assert_eq!(gated.calls.lock().unwrap().len(), 1, "the second drain joined the first");
        assert_eq!(a, b);
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["notes.txt", "raw-2026-10-14.jsonl", "raw-2026-9-1.jsonl"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_ledger_file_is_named_by_its_day() {
        assert_eq!(ledger_day("raw-2026-10-15.jsonl"), Some("2026-10-15"));
        for name in ["raw-2026-10-15.jsonl.1", "raw-2026-9-15.jsonl", "notes.txt", "raw-2026x10-15.jsonl"] {
            assert_eq!(ledger_day(name), None, "{name}");
        }
    }
}
