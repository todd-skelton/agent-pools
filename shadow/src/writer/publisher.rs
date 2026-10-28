//! The capacity forecast's publisher (capacity-publisher.mjs): the usage-sample history in
//! memory, a forecast rebuilt on every refresh and published atomically to
//! `state/capacity-forecast.json`, whose last document is the forecast's hysteresis memory and
//! the credit planner's input; then the routing ladders (routing-ladder.mjs) from that forecast,
//! published to `state/routing-ladders.json`.

use std::path::PathBuf;
use std::sync::Mutex;

use pool_core::forecast::{self, HOUR};
use pool_core::js;
use pool_core::refresh::ForecastInput;
use serde_json::{Map, Value, json};

pub struct Publisher {
    root: PathBuf,
    inner: Mutex<Inner>,
    /// Where `current()` and `ladders()` are answered from.
    memory: std::sync::Arc<super::memory::Memory>,
}

struct Inner {
    samples: Option<Vec<Value>>,
    previous: Option<Value>,
    last_problem: String,
    /// The ladders' file inputs, read again after an hour.
    ladder_inputs: Option<Value>,
    ladder_inputs_at: f64,
    last_ladder_problem: String,
}

impl Default for Inner {
    fn default() -> Inner {
        Inner {
            samples: None,
            previous: None,
            last_problem: String::new(),
            ladder_inputs: None,
            ladder_inputs_at: f64::NEG_INFINITY,
            last_ladder_problem: String::new(),
        }
    }
}

/// model-store.mjs `readJSON(file, fallback)`: a missing file is `fallback` when there is one;
/// anything else unreadable is an error. A byte-order mark is not skipped, as `JSON.parse`
/// does not skip it.
fn read_json(path: &std::path::Path, fallback: Option<Value>) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && fallback.is_some() => Ok(fallback.unwrap_or_default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// capacity-publisher.mjs `readLadderInputs(root)`, with model-calibrate.mjs `loadInputs`.
pub fn read_ladder_inputs(root: &std::path::Path) -> Result<Value, String> {
    let bench = root.join("state/benchmarks");
    let base = |v: &Value| -> Option<String> {
        let f = v.get("file").and_then(Value::as_str)?;
        std::path::Path::new(f).file_name().and_then(|n| n.to_str()).map(String::from)
    };
    let latest = read_json(&bench.join("latest.json"), None)?;
    let arena_latest = read_json(&bench.join("latest-arena.json"), Some(Value::Null))?;
    let snapshot_file = base(&latest).ok_or("latest.json names no file")?;
    let arena_file = if arena_latest.is_null() { None } else { Some(base(&arena_latest).ok_or("latest-arena.json names no file")?) };
    let manual = read_json(&bench.join("manual-scores.json"), Some(Value::Null))?;
    if !manual.is_null() {
        pool_core::ladder::validate_manual_scores(&manual)?;
    }
    let mut m = Map::new();
    m.insert("rules".into(), read_json(&root.join("calibration-rules.json"), None)?);
    // loadInputs reads the policy too (and fails without it); the ladders never use it.
    read_json(&root.join("state/routing-policy.json"), None)?;
    m.insert("registry".into(), read_json(&root.join("state/model-registry.json"), None)?);
    m.insert("snapshot".into(), read_json(&bench.join(&snapshot_file), None)?);
    m.insert("arena".into(), match &arena_file { Some(f) => read_json(&bench.join(f), None)?, None => Value::Null });
    m.insert("manual".into(), manual);
    m.insert("snapshotFile".into(), json!(snapshot_file));
    m.insert("arenaFile".into(), json!(arena_file));
    m.insert("value".into(), read_json(&root.join("state/usage-value.json"), None)?);
    m.insert("attempts".into(), read_json(&root.join("state/routing-attempts.json"), Some(json!({ "attempts": [] })))?);
    m.insert("sessionUsage".into(), read_json(&root.join("state/session-usage.json"), Some(Value::Null))?);
    Ok(Value::Object(m))
}

/// model-store.mjs `writeJSONAtomic`: a unique temporary file, then a rename, retried while a
/// reader holds the file open on Windows.
fn write_json_atomic(file: &std::path::Path, doc: &Value) -> std::io::Result<()> {
    let tmp = file.with_extension(format!("json.{}.{:08x}.tmp", std::process::id(), rand_u32()));
    std::fs::write(&tmp, js::stringify_pretty(doc, 2) + "\n")?;
    let mut i = 0u64;
    loop {
        match std::fs::rename(&tmp, file) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && i < 20 => {
                i += 1;
                std::thread::sleep(std::time::Duration::from_millis(100 * i));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        }
    }
}

fn rand_u32() -> u32 {
    uuid::Uuid::new_v4().as_u128() as u32
}

/// An event the publisher logs, for the caller to write.
pub type Logged = Vec<(String, Value)>;

impl Publisher {
    /// Seeded with the last published document, when it is a forecast (as `memory` loaded it).
    pub fn new(root: PathBuf, memory: std::sync::Arc<super::memory::Memory>) -> Publisher {
        let previous = memory.forecast().map(|d| (*d).clone());
        Publisher { root, inner: Mutex::new(Inner { previous, ..Inner::default() }), memory }
    }

    /// `rebuildLadders(forecast, t)`: the ladders from this forecast, the file inputs read again
    /// after an hour; a problem is logged once until it changes.
    fn rebuild_ladders(&self, forecast: &Value, now: f64, logged: &mut Logged) {
        let result = (|| -> Result<(), String> {
            let cached = {
                let inner = self.inner.lock().unwrap();
                inner.ladder_inputs.clone().filter(|_| now - inner.ladder_inputs_at < HOUR)
            };
            let mut inputs = match cached {
                Some(i) => i,
                None => {
                    let i = read_ladder_inputs(&self.root)?;
                    let mut inner = self.inner.lock().unwrap();
                    inner.ladder_inputs = Some(i.clone());
                    inner.ladder_inputs_at = now;
                    i
                }
            };
            inputs["forecast"] = forecast.clone();
            let doc = pool_core::ladder::build_ladders(&inputs, now)?;
            write_json_atomic(&self.root.join("state/routing-ladders.json"), &doc).map_err(|e| e.to_string())?;
            // `ladders = doc` once written.
            self.memory.set_ladders(std::sync::Arc::new(doc));
            Ok(())
        })();
        let mut inner = self.inner.lock().unwrap();
        match result {
            // `/api/ladders` answers from the file written.
            Ok(_) => inner.last_ladder_problem.clear(),
            Err(problem) => {
                if problem != inner.last_ladder_problem {
                    inner.last_ladder_problem = problem.clone();
                    logged.push(("routing-ladder-problem".into(), json!({ "problem": problem })));
                }
            }
        }
    }

    pub fn plan(&self, provider: &str, now: f64) -> Option<std::collections::HashSet<String>> {
        forecast::lapsing_holders(self.inner.lock().unwrap().previous.as_ref(), provider, now)
    }

    pub fn refill_demand(&self, provider: &str, now: f64) -> Option<Value> {
        forecast::refill_demand_of(self.inner.lock().unwrap().previous.as_ref(), provider, now)
    }

    pub fn drain_first(&self, provider: &str, now: f64) -> Option<std::collections::HashMap<String, f64>> {
        forecast::drain_first_of(self.inner.lock().unwrap().previous.as_ref(), provider, now)
    }

    fn samples_from_logs(&self, keep_from: f64) -> Vec<Value> {
        let mut out: Vec<(f64, Value)> = Vec::new();
        for rel in ["logs/usage-samples.jsonl.1", "logs/usage-samples.jsonl"] {
            let Ok(text) = std::fs::read_to_string(self.root.join(rel)) else { continue };
            for line in text.split('\n').filter(|l| !l.is_empty()) {
                if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(line) {
                    let at = js::date_parse_value(v.get("at"));
                    // `Date.parse(s.at) >= keepFrom`: an unreadable time is dropped.
                    if at >= keep_from {
                        out.push((at, v));
                    }
                }
            }
        }
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        out.into_iter().map(|(_, v)| v).collect()
    }

    fn publish(&self, doc: &Value) -> std::io::Result<()> {
        let file = self.root.join("state/capacity-forecast.json");
        let tmp = self.root.join("state/capacity-forecast.json.tmp");
        // A reader holding the file can make the rename fail on Windows: retry once.
        let mut attempt = 0;
        loop {
            let r = std::fs::write(&tmp, js::stringify_pretty(doc, 2)).and_then(|_| std::fs::rename(&tmp, &file));
            match r {
                Ok(()) => return Ok(()),
                Err(e) if attempt >= 1 => return Err(e),
                Err(_) => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
        }
    }

    /// `update(input)`: rebuild, publish and return the forecast, or `None` (the last document
    /// stays published). Blocking: run it off the async threads.
    pub fn update(&self, install: &Value, input: Owned, now: f64) -> (Option<Value>, Logged) {
        let mut logged = Vec::new();
        let (config, problems) = forecast::forecast_config(install.get("capacityForecast"));
        let keep_from = now - (config.lookback_hours + 48.0) * HOUR;
        let loaded = {
            let inner = self.inner.lock().unwrap();
            inner.samples.is_none()
        };
        let fresh = if loaded { Some(self.samples_from_logs(keep_from)) } else { None };
        let (samples, previous) = {
            let mut inner = self.inner.lock().unwrap();
            match (&mut inner.samples, fresh) {
                (s @ None, Some(f)) => *s = Some(f),
                (Some(s), _) => {
                    s.extend(input.new_samples.iter().cloned());
                    if s.first().is_some_and(|f| js::date_parse_value(f.get("at")) < keep_from) {
                        s.retain(|x| js::date_parse_value(x.get("at")) >= keep_from);
                    }
                }
                (None, None) => inner.samples = Some(Vec::new()),
            }
            (inner.samples.clone().unwrap_or_default(), inner.previous.clone())
        };
        let doc = forecast::build_forecast(&input.accounts, &samples, &config, now, previous.as_ref(), &input.policies, input.reset_automation);
        let digest = |d: Option<&Value>| d.and_then(|d| d.get("decisionDigest")).cloned();
        let changed = digest(previous.as_ref()) != digest(Some(&doc));
        self.inner.lock().unwrap().previous = Some(doc.clone());
        // Memory advances even if the file write fails (`previous = forecast`).
        self.memory.set_forecast(std::sync::Arc::new(doc.clone()));
        if changed {
            let mut e = Map::new();
            e.insert("digest".into(), doc.get("decisionDigest").cloned().unwrap_or(Value::Null));
            if let Some(Value::Object(d)) = doc.get("decision") {
                e.extend(d.clone());
            }
            logged.push(("capacity-forecast-decision".into(), Value::Object(e)));
        }
        let mut problem = problems.join("; ");
        if let Err(e) = self.publish(&doc) {
            problem = [problem, format!("write failed: {e}")].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("; ");
        }
        {
            let mut inner = self.inner.lock().unwrap();
            if problem != inner.last_problem {
                inner.last_problem = problem.clone();
                if !problem.is_empty() {
                    logged.push(("capacity-forecast-problem".into(), json!({ "problem": problem })));
                }
            }
        }
        self.rebuild_ladders(&doc, now, &mut logged);
        (Some(doc), logged)
    }
}

/// What `update` reads from a cycle, owned so it can move to a blocking thread.
pub struct Owned {
    pub accounts: Vec<Value>,
    pub new_samples: Vec<Value>,
    pub policies: Vec<pool_core::resets::ResetPolicy>,
    pub reset_automation: bool,
}

impl From<ForecastInput<'_>> for Owned {
    fn from(i: ForecastInput<'_>) -> Owned {
        Owned {
            accounts: i.accounts.to_vec(),
            new_samples: i.new_samples.to_vec(),
            policies: i.reset_policies.iter().map(|p| p.reset.clone()).collect(),
            reset_automation: i.reset_automation,
        }
    }
}
