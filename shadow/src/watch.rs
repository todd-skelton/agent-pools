//! The shadow's state and its two tasks: one watches the install's files and checks each new
//! forecast and value report; the other asks the live service for `/api/summary` and checks it.
//! Both only read the install. The results go to `parity.jsonl` in the shadow's own directory.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use corten::Signal;
use pool_core::js;
use serde::Serialize;
use serde_json::Value;

use crate::parity::{self, Check, Kind, Outcome};
use crate::sources::Install;

/// Documents seen, kept to rebuild a report from what was on disk when it was built.
const HISTORY: usize = 64;
/// Checks kept for the page.
const RECENT: usize = 60;
/// How far apart the pool state's save and the forecast's publication can be and still belong to
/// one refresh cycle (the refresh saves, then publishes, within seconds).
const SAME_CYCLE_MS: f64 = 30_000.0;

pub fn now_ms() -> f64 {
    ms(SystemTime::now())
}

fn ms(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

/// A document as it was on disk: its modification time and content.
#[derive(Clone, Debug)]
struct Seen {
    mtime: f64,
    doc: Arc<Value>,
}

/// Counts for one kind of check.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tally {
    pub matched: u64,
    pub differed: u64,
    pub unaligned: u64,
    pub last_match: Option<String>,
    pub last_difference: Option<Check>,
}

#[derive(Default)]
pub struct Inner {
    pub tallies: [Tally; 4],
    /// Newest first.
    pub recent: VecDeque<Check>,
    /// The last `/api/summary` the live service answered, and when.
    pub live_summary: Option<(f64, Arc<Value>)>,
    pub live_error: Option<String>,
    /// What only the live service knows (`processes`, `holds`, `usageLedger`), from its last
    /// `/api/status` answer: the dashboard shows it beside the files.
    pub live_memory: pool_core::status::Live,
    pools: VecDeque<Seen>,
    forecasts: VecDeque<Seen>,
    pool_mtime: Option<f64>,
    forecast_mtime: Option<f64>,
    value_mtime: Option<f64>,
    value_seen: Option<Value>,
}

pub struct Shadow {
    pub install: Install,
    /// Bumped whenever what the dashboard is drawn from changes (a state file is saved, the live
    /// service's memory moves): open pages redraw on it.
    pub changes: tokio::sync::watch::Sender<u64>,
    pub live_url: String,
    pub log: PathBuf,
    pub started_at: f64,
    pub inner: Mutex<Inner>,
    /// The write side's refresh trigger, set only when the shadow runs on a copy (`run-copy`).
    pub writer: std::sync::OnceLock<Arc<crate::writer::Trigger>>,
    http: reqwest::Client,
}

pub type Shared = Arc<Shadow>;

/// What the parity page shows, copied out of the lock.
pub struct Snapshot {
    pub tallies: Vec<(Kind, Tally)>,
    /// Newest first.
    pub recent: Vec<Check>,
    pub live_summary: Option<(f64, Arc<Value>)>,
    pub live_error: Option<String>,
}

impl Shadow {
    pub fn new(install: Install, live_url: String, log: PathBuf) -> Shared {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .no_proxy()
            .build()
            .expect("an HTTP client with no TLS and no proxy builds");
        Arc::new(Shadow {
            install,
            changes: tokio::sync::watch::Sender::new(0),
            live_url,
            log,
            started_at: now_ms(),
            inner: Mutex::new(Inner::default()),
            writer: std::sync::OnceLock::new(),
            http,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Counts a check, keeps it for the page and appends it to the log.
    pub fn record(&self, check: Check) {
        if let Ok(line) = serde_json::to_string(&check) {
            let written = (|| -> std::io::Result<()> {
                if let Some(dir) = self.log.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.log)?;
                writeln!(f, "{line}")
            })();
            if let Err(e) = written {
                eprintln!("shadow: parity log {}: {e}", self.log.display());
            }
        }
        let mut inner = self.lock();
        let tally = &mut inner.tallies[Kind::ALL.iter().position(|k| *k == check.kind).unwrap()];
        match check.outcome {
            Outcome::Match => {
                tally.matched += 1;
                tally.last_match = check.subject.clone();
            }
            Outcome::Differs => {
                tally.differed += 1;
                tally.last_difference = Some(check.clone());
            }
            Outcome::Unaligned => tally.unaligned += 1,
        }
        inner.recent.push_front(check);
        inner.recent.truncate(RECENT);
    }

    /// Tells open pages their inputs changed.
    pub fn changed_inputs(&self) {
        self.changes.send_modify(|v| *v = v.wrapping_add(1));
    }

    pub fn live_memory(&self) -> pool_core::status::Live {
        self.lock().live_memory.clone()
    }

    pub fn snapshot(&self) -> Snapshot {
        let inner = self.lock();
        Snapshot {
            tallies: Kind::ALL
                .iter()
                .copied()
                .zip(inner.tallies.iter().cloned())
                .collect(),
            recent: inner.recent.iter().cloned().collect(),
            live_summary: inner.live_summary.clone(),
            live_error: inner.live_error.clone(),
        }
    }

    /// A file's modification time, if it changed since `last`.
    fn changed(&self, rel: &str, last: Option<f64>) -> Option<f64> {
        let mtime = ms(self.install.modified(rel)?);
        (Some(mtime) != last).then_some(mtime)
    }
}

/// The work a watch tick found: checks to run outside the lock.
enum Job {
    Forecast {
        live: Arc<Value>,
        pool: Arc<Value>,
        previous: Arc<Value>,
    },
    Value {
        live: Arc<Value>,
        pool: Arc<Value>,
        forecast: Option<Arc<Value>>,
        ledger_until: f64,
    },
}

/// One pass over the files: records what changed and returns the checks it calls for.
fn observe(shadow: &Shadow) -> Vec<Job> {
    let mut jobs = Vec::new();
    let now = now_ms();
    // The pool state first: a refresh saves it before it publishes the forecast.
    let pool_mtime = shadow.changed("state/pool-state.json", shadow.lock().pool_mtime);
    if let Some(mtime) = pool_mtime
        && let Some(doc) = shadow.install.json("state/pool-state.json")
    {
        let mut inner = shadow.lock();
        inner.pool_mtime = Some(mtime);
        inner.pools.push_back(Seen {
            mtime,
            doc: Arc::new(doc),
        });
        if inner.pools.len() > HISTORY {
            inner.pools.pop_front();
        }
    }
    let forecast_mtime =
        shadow.changed("state/capacity-forecast.json", shadow.lock().forecast_mtime);
    if let Some(mtime) = forecast_mtime
        && let Some(doc) = shadow.install.json("state/capacity-forecast.json")
    {
        let doc = Arc::new(doc);
        let mut inner = shadow.lock();
        inner.forecast_mtime = Some(mtime);
        let previous = inner.forecasts.back().cloned();
        let new = previous
            .as_ref()
            .is_none_or(|p| p.doc.get("generatedAt") != doc.get("generatedAt"));
        if new {
            let pool = inner.pools.iter().rev().find(|p| p.mtime <= mtime).cloned();
            inner.forecasts.push_back(Seen {
                mtime,
                doc: doc.clone(),
            });
            if inner.forecasts.len() > HISTORY {
                inner.forecasts.pop_front();
            }
            drop(inner);
            let subject = doc
                .get("generatedAt")
                .and_then(Value::as_str)
                .map(str::to_string);
            match (previous, pool) {
                    (None, _) => shadow.record(Check::unaligned(
                        Kind::Forecast,
                        subject,
                        "first forecast the shadow saw: the document before it (the publisher's memory) is unknown",
                        now,
                    )),
                    (_, None) => shadow.record(Check::unaligned(Kind::Forecast, subject, "no pool state saved before it was seen", now)),
                    (Some(_), Some(pool)) if mtime - pool.mtime > SAME_CYCLE_MS => shadow.record(Check::unaligned(
                        Kind::Forecast,
                        subject,
                        format!("the pool state seen last was saved {:.0} s before it, not in its refresh cycle", (mtime - pool.mtime) / 1000.0),
                        now,
                    )),
                    (Some(previous), Some(pool)) => jobs.push(Job::Forecast { live: doc, pool: pool.doc, previous: previous.doc }),
                }
        }
    }
    let value_mtime = shadow.changed("state/usage-value.json", shadow.lock().value_mtime);
    if let Some(mtime) = value_mtime
        && let Some(doc) = shadow.install.json("state/usage-value.json")
    {
        let mut inner = shadow.lock();
        inner.value_mtime = Some(mtime);
        let generated = doc.get("generatedAt").cloned().unwrap_or(Value::Null);
        let first = inner.value_seen.is_none();
        if inner.value_seen.as_ref() != Some(&generated) {
            inner.value_seen = Some(generated);
            let t = js::date_parse_value(doc.get("generatedAt"));
            let subject = doc
                .get("generatedAt")
                .and_then(Value::as_str)
                .map(str::to_string);
            // What was on disk when the report read it: the newest saves from before `t`, if the
            // shadow was watching then (it has a save from before and one from after).
            let pool = inner.pools.iter().rev().find(|p| p.mtime <= t).cloned();
            let forecast = inner.forecasts.iter().rev().find(|p| p.mtime <= t).cloned();
            let watched = inner.forecasts.front().is_some_and(|f| f.mtime <= t)
                && inner.pools.front().is_some_and(|p| p.mtime <= t);
            drop(inner);
            match pool {
                _ if first => shadow.record(Check::unaligned(
                    Kind::Value,
                    subject,
                    "first report the shadow saw: the pool state and forecast it read are unknown",
                    now,
                )),
                Some(pool) if watched => jobs.push(Job::Value {
                    live: Arc::new(doc),
                    pool: pool.doc,
                    forecast: forecast.map(|f| f.doc),
                    ledger_until: mtime,
                }),
                _ => shadow.record(Check::unaligned(
                    Kind::Value,
                    subject,
                    "the shadow was not watching when the report was built",
                    now,
                )),
            }
        }
    }
    jobs
}

fn run(shadow: &Shadow, job: Job) -> Check {
    let now = now_ms();
    match job {
        Job::Forecast {
            live,
            pool,
            previous,
        } => {
            let inp = parity::ForecastInputs {
                install: &shadow.install,
                pool: &pool,
                previous: Some(&previous),
            };
            parity::check_forecast(&live, &inp, now)
        }
        Job::Value {
            live,
            pool,
            forecast,
            ledger_until,
        } => {
            // The report reads the sample logs first, within a second of its `now`.
            let samples_until = js::date_parse_value(live.get("generatedAt")) + 1000.0;
            let inp = parity::ValueInputs {
                install: &shadow.install,
                pool: &pool,
                forecast: forecast.as_deref(),
                ledger_until,
                samples_until,
                ledger_rows: None,
            };
            parity::check_value(&live, &inp, now)
        }
    }
}

/// Watches the install's files every `every` and checks each new forecast and value report.
pub async fn watch(shadow: Shared, every: Duration, signal: Signal) {
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = signal.aborted() => return,
        }
        let s = shadow.clone();
        let work = tokio::task::spawn_blocking(move || {
            for job in observe(&s) {
                let check = run(&s, job);
                s.record(check);
            }
        });
        tokio::select! {
            done = work => if let Err(e) = done { eprintln!("shadow: watch pass failed: {e}") },
            // A check in progress only reads; it is dropped with the process.
            _ = signal.aborted() => return,
        }
    }
}

/// Asks the live service for `/api/status` every `memory_every`, keeping what only it knows
/// (`processes`, `holds`, `usageLedger`) for the dashboard, and checks the answer against the
/// files every `check_every`.
pub async fn compare_status(shadow: Shared, memory_every: Duration, check_every: Duration, signal: Signal) {
    let mut tick = tokio::time::interval(memory_every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let url = format!("{}/api/status", shadow.live_url.trim_end_matches('/'));
    // Offset from the summary check, so the two do not read the files at the same moment.
    let mut next_check = tokio::time::Instant::now() + check_every / 2;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = signal.aborted() => return,
        }
        let answer = tokio::select! {
            a = fetch(&shadow.http, &url) => a,
            _ = signal.aborted() => return,
        };
        let at = now_ms();
        let live = match answer {
            Ok(live) => live,
            Err(e) => {
                shadow.lock().live_error = Some(format!("{}: /api/status: {e}", js::iso(at)));
                continue;
            }
        };
        let memory = pool_core::status::Live {
            processes: live.get("processes").cloned(),
            holds: live.get("holds").cloned(),
            usage_ledger: live.get("usageLedger").cloned(),
        };
        let moved = {
            let mut inner = shadow.lock();
            let old = &inner.live_memory;
            let moved = old.processes != memory.processes || old.holds != memory.holds || old.usage_ledger != memory.usage_ledger;
            inner.live_memory = memory;
            moved
        };
        if moved {
            shadow.changed_inputs();
        }
        if tokio::time::Instant::now() < next_check {
            continue;
        }
        next_check = tokio::time::Instant::now() + check_every;
        let s = shadow.clone();
        let check = tokio::task::spawn_blocking(move || {
            parity::check_status(&live, &s.install, at, now_ms())
        })
        .await;
        match check {
            Ok(check) => shadow.record(check),
            Err(e) => eprintln!("shadow: status check failed: {e}"),
        }
    }
}

/// The files the dashboard is drawn from.
const INPUTS: [&str; 3] = ["state/pool-state.json", "state/capacity-forecast.json", "state/usage-value.json"];

/// Looks at the dashboard's files' modification times every `every` (metadata only) and tells
/// open pages when one was saved.
pub async fn notice_saves(shadow: Shared, every: Duration, signal: Signal) {
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut seen: [Option<SystemTime>; 3] = [None; 3];
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = signal.aborted() => return,
        }
        let now: [Option<SystemTime>; 3] = INPUTS.map(|rel| shadow.install.modified(rel));
        if now != seen {
            seen = now;
            shadow.changed_inputs();
        }
    }
}

/// Asks the live service for `/api/summary` every `every` and checks it.
pub async fn compare_summary(shadow: Shared, every: Duration, signal: Signal) {
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let url = format!("{}/api/summary", shadow.live_url.trim_end_matches('/'));
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = signal.aborted() => return,
        }
        let answer = tokio::select! {
            a = fetch(&shadow.http, &url) => a,
            _ = signal.aborted() => return,
        };
        let live = match answer {
            Ok(live) => Arc::new(live),
            Err(e) => {
                shadow.lock().live_error = Some(format!("{}: {e}", js::iso(now_ms())));
                continue;
            }
        };
        {
            let mut inner = shadow.lock();
            inner.live_summary = Some((now_ms(), live.clone()));
            inner.live_error = None;
        }
        // Read at once, before the live service saves or publishes again.
        let s = shadow.clone();
        let check = tokio::task::spawn_blocking(move || {
            let pool = s.install.json("state/pool-state.json");
            let forecast = s.install.json("state/capacity-forecast.json");
            let value = s.install.json("state/usage-value.json");
            parity::check_summary(
                &live,
                pool.as_ref(),
                forecast.as_ref(),
                value.as_ref(),
                now_ms(),
            )
        })
        .await;
        match check {
            Ok(check) => shadow.record(check),
            Err(e) => eprintln!("shadow: summary check failed: {e}"),
        }
    }
}

async fn fetch(http: &reqwest::Client, url: &str) -> Result<Value, String> {
    let res = http.get(url).send().await.map_err(|e| e.to_string())?;
    let status = res.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    res.json::<Value>().await.map_err(|e| e.to_string())
}
