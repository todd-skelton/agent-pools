//! The capacity forecast against `golden/outputs/forecast`: the document for the snapshot's
//! `now`, and the 72-hour replay from no memory (golden/capture.mjs, section 1).

mod common;

use common::{diff, golden, input, input_exists, input_lines};
use pool_core::forecast::{self, ForecastConfig, HOUR};
use pool_core::{js, resets};
use serde_json::{json, Value};
use std::collections::HashMap;

struct Snapshot {
    now: f64,
    pool: Value,
    install: Value,
    previous: Option<Value>,
    /// Samples in time order, with their parsed times.
    samples: Vec<(f64, Value)>,
}

fn snapshot() -> Snapshot {
    let manifest = input("manifest.json");
    let mut samples: Vec<(f64, Value)> = input_lines("usage-samples.jsonl.gz")
        .into_iter()
        .map(|s| (js::date_parse_value(s.get("at")), s))
        .collect();
    assert!(
        samples.iter().all(|(t, _)| t.is_finite()),
        "every sample time parses"
    );
    // Array.prototype.sort is stable, as is this.
    samples.sort_by(|a, b| a.0.total_cmp(&b.0));
    Snapshot {
        now: js::date_parse_value(manifest.get("now")),
        pool: input("pool-state.json"),
        install: input("install.json"),
        previous: input_exists("capacity-forecast.json").then(|| input("capacity-forecast.json")),
        samples,
    }
}

/// capture.mjs `samplesFor(t)`: what the publisher keeps for a forecast at `t`.
fn samples_for(s: &Snapshot, config: &ForecastConfig, t: f64) -> Vec<Value> {
    let from = t - (config.lookback_hours + 48.0) * HOUR;
    s.samples
        .iter()
        .filter(|(at, _)| *at >= from && *at <= t)
        .map(|(_, v)| v.clone())
        .collect()
}

fn config(s: &Snapshot) -> ForecastConfig {
    let (config, problems) = forecast::forecast_config(s.install.get("capacityForecast"));
    let index = golden("index.json");
    let expected: Vec<String> = serde_json::from_value(index["forecastProblems"].clone()).unwrap();
    assert_eq!(problems, expected, "forecastConfig problems");
    config
}

fn accounts(s: &Snapshot) -> Vec<Value> {
    s.pool["accounts"].as_array().unwrap().clone()
}

#[test]
fn forecast_now() {
    let s = snapshot();
    let config = config(&s);
    let policies = resets::reset_policies(&s.install);
    let reset_automation = s.pool.get("resetAutomation") != Some(&json!(false));
    let started = std::time::Instant::now();
    let doc = forecast::build_forecast(
        &accounts(&s),
        &samples_for(&s, &config, s.now),
        &config,
        s.now,
        s.previous.as_ref(),
        &policies,
        reset_automation,
    );
    eprintln!("forecast/now.json built in {:?}", started.elapsed());
    if let Some(d) = diff("forecast", &golden("forecast/now.json"), &doc) {
        panic!("forecast/now.json differs: {d}");
    }
}

#[test]
fn forecast_replay() {
    let s = snapshot();
    let config = config(&s);
    let reset_automation = s.pool.get("resetAutomation") != Some(&json!(false));
    let expected = golden("forecast/replay.json");
    let steps = expected["steps"].as_array().unwrap();
    let hours = expected["hours"].as_f64().unwrap();
    let end = (s.now / HOUR).floor() * HOUR;
    let mut previous: Option<Value> = None;
    let mut t = end - (hours - 1.0) * HOUR;
    let mut i = 0;
    let started = std::time::Instant::now();
    while t <= end {
        // The newest sample at or before t for each account and window.
        let mut latest: HashMap<String, &Value> = HashMap::new();
        for (at, sample) in &s.samples {
            if *at > t {
                break;
            }
            let key = format!(
                "{}|{}",
                js::to_string(sample.get("accountId")),
                js::to_string(sample.get("window"))
            );
            latest.insert(key, sample);
        }
        let accts: Vec<Value> = accounts(&s)
            .into_iter()
            .map(|mut a| {
                let id = js::to_string(a.get("id"));
                let windows: Vec<Value> = a["quotaWindows"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|mut w| {
                        let sample = latest.get(&format!("{id}|{}", js::to_string(w.get("id"))))?;
                        w["used"] = sample.get("used").cloned().unwrap_or(Value::Null);
                        w["resetAt"] = sample.get("resetAt").cloned().unwrap_or(Value::Null);
                        Some(w)
                    })
                    .collect();
                a["usageStale"] = json!(false);
                a["status"] = json!("available");
                a["quotaWindows"] = Value::Array(windows);
                a
            })
            .collect();
        let doc = forecast::build_forecast(
            &accts,
            &samples_for(&s, &config, t),
            &config,
            t,
            previous.as_ref(),
            &[],
            reset_automation,
        );
        let step = &steps[i];
        assert_eq!(step["now"], json!(js::iso(t)), "step {i} time");
        if let Some(d) = diff("forecast", &step["forecast"], &doc) {
            panic!(
                "forecast/replay.json step {i} ({}) differs: {d}",
                js::iso(t)
            );
        }
        previous = Some(doc);
        t += HOUR;
        i += 1;
    }
    assert_eq!(i, steps.len());
    eprintln!(
        "forecast/replay.json: {i} hourly steps match in {:?}",
        started.elapsed()
    );
}
#[test]
fn forecast_variants() {
    let s = snapshot();
    let base = config(&s);
    let expected = golden("forecast/variants.json");
    let mut docs: HashMap<String, Value> = HashMap::new();
    let mut checked = 0;
    for case in expected["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let input = &case["input"];
        let now = input["now"].as_f64().unwrap();
        let capacity = Some(&input["capacityForecast"]).filter(|v| !v.is_null());
        let (config, problems) = forecast::forecast_config(capacity);
        let want_problems: Vec<String> =
            serde_json::from_value(case["result"]["problems"].clone()).unwrap();
        assert_eq!(problems, want_problems, "{name}: forecastConfig problems");
        let policies = resets::reset_policies(&input["install"]);
        let policies_value = Value::Array(policies.iter().map(|p| p.to_value()).collect());
        let want_policies = Value::Array(
            js::values(Some(&case["result"]["resetPolicies"]))
                .into_iter()
                .cloned()
                .collect(),
        );
        if let Some(d) = diff("resetPolicies", &want_policies, &policies_value) {
            panic!("{name}: {d}");
        }
        let previous = match input["previous"].as_str() {
            None => None,
            Some("snapshot") => s.previous.clone(),
            Some(other) => Some(docs[other].clone()),
        };
        let samples: Vec<Value> = if input["samples"] == json!("none") {
            Vec::new()
        } else {
            let from = now - (config.lookback_hours + 48.0) * HOUR;
            samples_for(&s, &base, now)
                .into_iter()
                .filter(|x| js::date_parse_value(x.get("at")) >= from)
                .collect()
        };
        let doc = forecast::build_forecast(
            input["accounts"].as_array().unwrap(),
            &samples,
            &config,
            now,
            previous.as_ref(),
            &policies,
            input["resetAutomation"].as_bool().unwrap(),
        );
        if let Some(d) = diff("forecast", &case["result"]["forecast"], &doc) {
            panic!("forecast/variants.json {name} differs: {d}");
        }
        docs.insert(name.to_string(), doc);
        checked += 1;
    }
    eprintln!("forecast/variants.json: {checked} cases match");
}
