//! The refresh cycle against Agent Pools' own: each scenario in
//! golden/outputs/refresh/traces.json (golden/refresh-traces.mjs) runs through the port with a
//! fake proxy that answers as the JavaScript harness's did, and every cycle's result, proxy
//! calls, events, samples, toasts, saves, hook calls and final state must match.
//!
//! Accounts are read concurrently on both sides, so calls and events are compared as multisets;
//! everything else is compared in order.

use pool_core::js;
use pool_core::refresh::{migrate_state, CyclePolicy, Failure, ForecastInput, Io, Options, Refresh};
use pool_core::resets::{reset_policies, ResetPolicy};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Mutex;

fn traces() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/outputs/refresh/traces.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("run golden/refresh-traces.mjs")).unwrap()
}

#[derive(Default)]
struct Trace {
    calls: Vec<Value>,
    events: Vec<Value>,
    samples: Vec<Value>,
    toasts: Vec<Value>,
    saves: Vec<Value>,
    hooks: Vec<Value>,
}

struct Fake {
    clock: Mutex<f64>,
    files: Mutex<Value>,
    routes: Mutex<Map<String, Value>>,
    counts: Mutex<HashMap<String, usize>>,
    hooks: Mutex<Map<String, Value>>,
    trace: Mutex<Trace>,
    uuids: Mutex<u64>,
    config: Value,
    versions: Value,
    org: Option<String>,
}

fn at<'a>(v: &'a Value, k: &str) -> Option<&'a Value> {
    v.as_object().and_then(|m| m.get(k))
}

fn central_offset_minutes(ms: f64) -> f64 {
    // US Central: CDT from the second Sunday in March 2:00 CST to the first Sunday in November 2:00 CDT.
    let day = 86_400_000.0;
    let (year, _, _) = js::civil_from_days((ms / day).floor() as i64);
    let nth_sunday = |month: i64, n: i64| {
        let mut d = 1;
        loop {
            let days = days_from_civil(year, month, d);
            if (days + 4).rem_euclid(7) == 0 {
                return (days + 7 * (n - 1)) as f64 * day;
            }
            d += 1;
        }
    };
    let start = nth_sunday(3, 2) + 8.0 * 3_600_000.0;
    let end = nth_sunday(11, 1) + 7.0 * 3_600_000.0;
    if ms >= start && ms < end { -300.0 } else { -360.0 }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

enum Reply {
    Value(Value),
    Fail(Failure),
    Hang,
}

impl Fake {
    fn reply(&self, key: &str, r: &Value) -> Value {
        if let Some(Value::Array(seq)) = at(r, "$seq") {
            let mut counts = self.counts.lock().unwrap();
            let i = counts.entry(key.to_string()).or_insert(0);
            *i += 1;
            return seq[(*i).min(seq.len()) - 1].clone();
        }
        r.clone()
    }

    fn answer(&self, endpoint: &str, body: Option<&Value>) -> Reply {
        let routes = self.routes.lock().unwrap().clone();
        let throw = |r: &Value| {
            at(r, "$throw").map(|t| {
                let message = at(t, "message").and_then(Value::as_str).unwrap_or_default().to_string();
                match at(t, "status").and_then(Value::as_u64) {
                    Some(s) => Failure::with_status(message, s as u16),
                    None => Failure::new(message),
                }
            })
        };
        if let Some(special) = routes.get(&format!("mgmt:{endpoint}")) {
            let r = self.reply(&format!("mgmt:{endpoint}"), special);
            return match throw(&r) {
                Some(f) => Reply::Fail(f),
                None => Reply::Value(r),
            };
        }
        match endpoint {
            "auth-files" => return Reply::Value(json!({ "files": self.files.lock().unwrap().clone() })),
            "auth-files/fields" | "recover-quota" => return Reply::Value(json!({ "changed": false, "routing": {} })),
            "api-call" => {}
            other => return Reply::Fail(Failure::new(format!("unexpected endpoint {other}"))),
        }
        let body = body.unwrap();
        let index = js::to_string(at(body, "auth_index"));
        let url = js::to_string(at(body, "url"));
        let indexed = format!("{index} {url}");
        let key = if routes.contains_key(&indexed) { indexed } else { url.clone() };
        let Some(r) = routes.get(&key) else {
            return Reply::Fail(Failure::new(format!("no route for {index} {url}")));
        };
        let r = self.reply(&key, r);
        if at(&r, "$hang").is_some() {
            return Reply::Hang;
        }
        if let Some(f) = throw(&r) {
            return Reply::Fail(f);
        }
        if r.as_object().is_some_and(|m| m.contains_key("status_code")) {
            return Reply::Value(r);
        }
        Reply::Value(json!({ "status_code": 200, "body": js::stringify(&r) }))
    }

    fn hook(&self, name: &str) -> Option<Value> {
        self.hooks.lock().unwrap().get(name).cloned()
    }
}

impl Io for Fake {
    fn now(&self) -> f64 {
        *self.clock.lock().unwrap()
    }

    fn management(&self, endpoint: &str, body: Option<&Value>, method: Option<&str>) -> impl Future<Output = Result<Value, Failure>> + Send {
        let mut call = Map::new();
        call.insert("endpoint".into(), json!(endpoint));
        if let Some(b) = body {
            call.insert("body".into(), b.clone());
        }
        if let Some(m) = method {
            call.insert("method".into(), json!(m));
        }
        self.trace.lock().unwrap().calls.push(Value::Object(call));
        let reply = self.answer(endpoint, body);
        async move {
            match reply {
                Reply::Value(v) => Ok(v),
                Reply::Fail(f) => Err(f),
                Reply::Hang => futures_pending().await,
            }
        }
    }

    fn event(&self, kind: &str, details: Value) -> impl Future<Output = Result<(), Failure>> + Send {
        let mut e = Map::new();
        e.insert("type".into(), json!(kind));
        e.extend(details.as_object().cloned().unwrap_or_default());
        self.trace.lock().unwrap().events.push(Value::Object(e));
        async { Ok(()) }
    }

    fn sample(&self, kind: &str, details: Value) -> impl Future<Output = Result<(), Failure>> + Send {
        let mut e = Map::new();
        e.insert("type".into(), json!(kind));
        e.extend(details.as_object().cloned().unwrap_or_default());
        self.trace.lock().unwrap().samples.push(Value::Object(e));
        async { Ok(()) }
    }

    fn save(&self, state: &Value) -> impl Future<Output = Result<(), Failure>> + Send {
        self.trace.lock().unwrap().saves.push(state.clone());
        async { Ok(()) }
    }

    fn toast(&self, title: &str, body: &str) -> impl Future<Output = Result<(), Failure>> + Send {
        self.trace.lock().unwrap().toasts.push(json!({ "title": title, "body": body }));
        async { Ok(()) }
    }

    fn load_policies(&self) -> impl Future<Output = Vec<ResetPolicy>> + Send {
        let p = reset_policies(&self.config);
        async move { p }
    }

    fn auth_file(&self, file: &Value) -> impl Future<Output = Option<Value>> + Send {
        let v = (at(file, "provider").and_then(Value::as_str) == Some("claude"))
            .then(|| self.org.clone().map(|o| json!({ "organization_uuid": o })))
            .flatten();
        async move { v }
    }

    fn client_version(&self, provider: &str) -> impl Future<Output = Option<String>> + Send {
        let v = at(&self.versions, provider).and_then(Value::as_str).map(str::to_string);
        async move { v }
    }

    async fn sleep(&self, _ms: f64) {}

    fn timer(&self, ms: f64) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(std::time::Duration::from_millis(ms as u64))
    }

    fn random_uuid(&self) -> String {
        let mut n = self.uuids.lock().unwrap();
        *n += 1;
        format!("00000000-0000-4000-8000-{:012}", *n)
    }

    fn local_offset_minutes(&self, ms: f64) -> f64 {
        central_offset_minutes(ms)
    }

    fn forecast(&self, input: ForecastInput<'_>) -> impl Future<Output = Option<Result<Option<Value>, Failure>>> + Send {
        let result = self.hook("forecast").map(|h| {
            let policies: Map<String, Value> = input.reset_policies.iter().map(|p| (p.reset.provider.clone(), p.to_value())).collect();
            self.trace.lock().unwrap().hooks.push(json!({
                "hook": "forecast", "accounts": input.accounts, "newSamples": input.new_samples,
                "resetPolicies": policies, "resetAutomation": input.reset_automation,
            }));
            match at(&h, "throw") {
                Some(m) => Err(Failure::new(js::to_string(Some(m)))),
                None => Ok(at(&h, "doc").filter(|d| !d.is_null()).cloned()),
            }
        });
        async move { result }
    }

    fn credit_plan(&self, _accounts: &[Value], policy: &CyclePolicy, now: f64) -> Option<Result<Option<HashSet<String>>, Failure>> {
        let h = self.hook("creditPlan")?;
        self.trace.lock().unwrap().hooks.push(json!({ "hook": "creditPlan", "provider": policy.reset.provider, "now": now }));
        Some(match at(&h, "throw") {
            Some(m) => Err(Failure::new(js::to_string(Some(m)))),
            None => Ok(at(&h, "ids").and_then(Value::as_array).map(|ids| ids.iter().map(|i| js::to_string(Some(i))).collect())),
        })
    }

    fn refill_demand(&self, policy: &ResetPolicy, now: f64) -> Option<Result<Option<Value>, Failure>> {
        let h = self.hook("refillDemand")?;
        self.trace.lock().unwrap().hooks.push(json!({ "hook": "refillDemand", "provider": policy.provider, "now": now }));
        Some(match at(&h, "throw") {
            Some(m) => Err(Failure::new(js::to_string(Some(m)))),
            None => Ok(at(&h, "value").filter(|v| !v.is_null()).cloned()),
        })
    }

    fn drain_first(&self, provider: &str, now: f64) -> Option<Result<Option<HashMap<String, f64>>, Failure>> {
        let h = self.hook("drainFirst")?;
        self.trace.lock().unwrap().hooks.push(json!({ "hook": "drainFirst", "provider": provider, "now": now }));
        Some(match at(&h, "throw") {
            Some(m) => Err(Failure::new(js::to_string(Some(m)))),
            None => Ok(at(&h, "byProvider").and_then(|b| at(b, provider)).and_then(Value::as_object).map(|m| {
                m.iter().map(|(k, v)| (k.clone(), v.as_f64().unwrap_or(f64::NAN))).collect()
            })),
        })
    }
}

async fn futures_pending<T>() -> T {
    std::future::pending().await
}

/// Keys sorted, numbers as JavaScript writes them, so two documents compare by content.
fn canonical(v: &Value) -> Value {
    match v {
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 9e15 => json!(f as i64),
            Some(f) => json!(f),
            None => v.clone(),
        },
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            Value::Object(keys.into_iter().map(|k| (k.clone(), canonical(&m[k]))).collect())
        }
        v => v.clone(),
    }
}

fn texts(vs: &[Value]) -> Vec<String> {
    vs.iter().map(|v| canonical(v).to_string()).collect()
}

fn sorted(vs: &[Value]) -> Vec<String> {
    let mut t = texts(vs);
    t.sort();
    t
}

/// The first place two lists differ, for the failure message.
fn first_difference(a: &[String], b: &[String]) -> String {
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i), b.get(i));
        if x != y {
            return format!("at {i}:\n  port: {}\n  js:   {}", x.map_or("(none)", |s| s), y.map_or("(none)", |s| s));
        }
    }
    String::new()
}

fn value_difference(port: &Value, js: &Value, path: &str, out: &mut Vec<String>) {
    match (port, js) {
        (Value::Object(a), Value::Object(b)) => {
            for k in a.keys().chain(b.keys().filter(|k| !a.contains_key(*k))) {
                match (a.get(k), b.get(k)) {
                    (Some(x), Some(y)) => value_difference(x, y, &format!("{path}.{k}"), out),
                    (x, y) => out.push(format!("{path}.{k}: port {} js {}", x.map_or("(absent)".into(), |v| v.to_string()), y.map_or("(absent)".into(), |v| v.to_string()))),
                }
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                value_difference(x, y, &format!("{path}[{i}]"), out);
            }
        }
        (a, b) if canonical(a) != canonical(b) => out.push(format!("{path}: port {a} js {b}")),
        _ => {}
    }
}

#[tokio::test(flavor = "current_thread")]
async fn each_cycle_matches_agent_pools() {
    let doc = traces();
    let mut failures = Vec::new();
    let mut cycles_checked = 0;
    for t in doc["traces"].as_array().unwrap() {
        let name = t["name"].as_str().unwrap();
        let s = &t["scenario"];
        let config = at(s, "config").cloned().unwrap_or_else(|| {
            json!({ "expectedAccounts": at(s, "expected").cloned().unwrap_or(json!({ "codex": 2, "claude": 3 })) })
        });
        let org = match at(s, "org") {
            Some(Value::Null) => None,
            Some(v) => Some(js::to_string(Some(v))),
            None => Some("a0b1c2d3-e4f5-4a6b-8c7d-9e0f1a2b3c4d".into()),
        };
        let fake = Fake {
            clock: Mutex::new(doc["t0"].as_f64().unwrap()),
            files: Mutex::new(s["files"].clone()),
            routes: Mutex::new(at(s, "routes").and_then(Value::as_object).cloned().unwrap_or_default()),
            counts: Mutex::new(HashMap::new()),
            hooks: Mutex::new(at(s, "hooks").and_then(Value::as_object).cloned().unwrap_or_default()),
            trace: Mutex::new(Trace::default()),
            uuids: Mutex::new(0),
            config,
            versions: at(s, "versions").cloned().unwrap_or(json!({ "claude": "2.1.284" })),
            org,
        };
        let mut state = json!({ "accounts": [], "resetAutomation": true });
        if let Some(Value::Object(saved)) = at(s, "state") {
            for (k, v) in saved {
                state[k] = v.clone();
            }
        }
        migrate_state(&mut state);
        let mut options = Options { read_plans: at(s, "readPlans") == Some(&json!(true)), ..Options::default() };
        if let Some(ms) = at(s, "planTimeoutMs").and_then(Value::as_f64) {
            options.plan_timeout_ms = ms;
        }
        let mut refresh = Refresh::new(options, &state);
        let steps = at(s, "cycles").and_then(Value::as_array).cloned().unwrap_or_else(|| vec![json!({})]);
        for (i, (step, expected)) in steps.iter().zip(t["cycles"].as_array().unwrap()).enumerate() {
            *fake.clock.lock().unwrap() += at(step, "advance").and_then(Value::as_f64).unwrap_or(0.0);
            if let Some(files) = at(step, "files") {
                *fake.files.lock().unwrap() = files.clone();
            }
            if let Some(Value::Object(routes)) = at(step, "routes") {
                let mut r = fake.routes.lock().unwrap();
                for (k, v) in routes {
                    if v.is_null() { r.shift_remove(k); } else { r.insert(k.clone(), v.clone()); }
                    fake.counts.lock().unwrap().remove(k);
                }
            }
            if let Some(Value::Object(hooks)) = at(step, "hooks") {
                fake.hooks.lock().unwrap().extend(hooks.clone());
            }
            *fake.trace.lock().unwrap() = Trace::default();
            let outcome = refresh.refresh(&mut state, &fake).await;
            cycles_checked += 1;
            let trace = std::mem::take(&mut *fake.trace.lock().unwrap());
            let at_cycle = format!("{name} cycle {i}");
            let result = json!({ "degraded": outcome.degraded, "followUp": outcome.follow_up });
            if result != expected["result"] {
                failures.push(format!("{at_cycle} result: port {result} js {}", expected["result"]));
            }
            let field = |k: &str| expected[k].as_array().cloned().unwrap_or_default();
            for (kind, port, js, ordered) in [
                ("calls", &trace.calls, field("calls"), false),
                ("events", &trace.events, field("events"), false),
                ("samples", &trace.samples, field("samples"), true),
                ("toasts", &trace.toasts, field("toasts"), true),
                ("saves", &trace.saves, field("saves"), true),
                ("hooks", &trace.hooks, field("hooks"), true),
            ] {
                let (a, b) = if ordered { (texts(port), texts(&js)) } else { (sorted(port), sorted(&js)) };
                if a != b {
                    failures.push(format!("{at_cycle} {kind} ({} port, {} js) {}", a.len(), b.len(), first_difference(&a, &b)));
                }
            }
            let mut diffs = Vec::new();
            value_difference(&state, &expected["state"], "state", &mut diffs);
            if !diffs.is_empty() {
                failures.push(format!("{at_cycle} state:\n  {}", diffs.into_iter().take(8).collect::<Vec<_>>().join("\n  ")));
            }
        }
    }
    assert!(failures.is_empty(), "{} differences over {cycles_checked} cycles:\n{}", failures.len(), failures.join("\n"));
    assert!(cycles_checked >= 60, "only {cycles_checked} cycles");
}
