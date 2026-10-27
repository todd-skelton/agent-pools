//! `GET /api/summary` against `golden/outputs/summary` (golden/capture.mjs, section 3).

mod common;

use common::{diff, golden, input};
use pool_core::{js, providers, summary};
use serde_json::{json, Value};

fn state_at(pool: &Value, now: f64) -> Value {
    let mut state = pool.clone();
    state["accounts"] = Value::Array(
        pool["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| summary::current_account(a, now))
            .collect(),
    );
    state
}

fn provider_names() -> Vec<(&'static str, &'static str)> {
    providers::PROVIDERS
        .iter()
        .map(|p| (p.id, p.name))
        .collect()
}

#[test]
fn summary_now() {
    let now = js::date_parse_value(input("manifest.json").get("now"));
    let pool = input("pool-state.json");
    let doc = summary::build_summary(
        &state_at(&pool, now),
        &provider_names(),
        Some(&golden("forecast/now.json")),
        Some(&golden("value/deployed.json")),
        now,
    );
    if let Some(d) = diff("summary", &golden("summary/now.json"), &doc) {
        panic!("summary/now.json differs: {d}");
    }
}

#[test]
fn summary_cases() {
    let pool = input("pool-state.json");
    let forecast = golden("forecast/now.json");
    let value = golden("value/deployed.json");
    let cases = golden("summary/cases.json");
    for case in cases["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let now = case["input"]["now"].as_f64().unwrap();
        let pick = |k: &str, doc: &Value| (case["input"][k] == json!(true)).then(|| doc.clone());
        let (f, v) = (pick("forecast", &forecast), pick("value", &value));
        let doc = summary::build_summary(
            &state_at(&pool, now),
            &provider_names(),
            f.as_ref(),
            v.as_ref(),
            now,
        );
        if let Some(d) = diff("summary", &case["result"], &doc) {
            panic!("summary/cases.json {name} differs: {d}");
        }
    }
}
