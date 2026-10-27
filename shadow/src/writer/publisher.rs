//! The capacity forecast's publisher (capacity-publisher.mjs): the usage-sample history in
//! memory, a forecast rebuilt on every refresh and published atomically to
//! `state/capacity-forecast.json`, whose last document is the forecast's hysteresis memory and
//! the credit planner's input. Routing ladders (routing-ladder.mjs) are not ported yet.

use std::path::PathBuf;
use std::sync::Mutex;

use pool_core::forecast::{self, HOUR};
use pool_core::js;
use pool_core::refresh::ForecastInput;
use serde_json::{Map, Value, json};

pub struct Publisher {
    root: PathBuf,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    samples: Option<Vec<Value>>,
    previous: Option<Value>,
    last_problem: String,
}

/// An event the publisher logs, for the caller to write.
pub type Logged = Vec<(String, Value)>;

impl Publisher {
    /// Seeded with the last published document, when it is a forecast.
    pub fn new(root: PathBuf) -> Publisher {
        let previous = std::fs::read_to_string(root.join("state/capacity-forecast.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(t.trim_start_matches('\u{feff}')).ok())
            .filter(|d| d.get("format").and_then(Value::as_str) == Some(forecast::FORECAST_FORMAT));
        Publisher { root, inner: Mutex::new(Inner { previous, ..Inner::default() }) }
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
        let mut inner = self.inner.lock().unwrap();
        if problem != inner.last_problem {
            inner.last_problem = problem.clone();
            if !problem.is_empty() {
                logged.push(("capacity-forecast-problem".into(), json!({ "problem": problem })));
            }
        }
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
