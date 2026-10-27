//! The model registry, the refresh's planning and the admission judges against every call Agent
//! Pools' own model-registry.test.mjs makes, recorded (under `TZ=UTC`) by
//! `node --import ./golden/registry-hook.mjs reference/agent-pools/model-registry.test.mjs`: each
//! result as `JSON.stringify` writes it, byte for byte, or the same error.

mod common;

use common::{diff, golden};
use pool_core::{js, registry};
use serde_json::Value;

fn efforts(es: Vec<&str>) -> Value {
    Value::Array(es.into_iter().map(|e| Value::String(e.into())).collect())
}

fn call(f: &str, a: &[Value]) -> Result<Value, String> {
    let arg = |i: usize| a.get(i).unwrap_or(&Value::Null);
    let num = |i: usize| js::to_number(a.get(i));
    let text = |i: usize| js::to_string(a.get(i));
    Ok(match f {
        "parseVersion" => registry::parse_version(arg(0), &text(1)).map_or(Value::Null, Value::String),
        "compareVersions" => js::number_value(registry::compare_versions(&text(0), &text(1))),
        "admittedEfforts" => efforts(registry::admitted_efforts(arg(0), &text(1), a.get(2), num(3))),
        "buildRegistry" => registry::build_registry(arg(0))?,
        "judgeClaude" => registry::judge_claude(&text(0), &text(1))?,
        "judgeCodex" => registry::judge_codex(&text(0)),
        // Recorded under TZ=UTC, so the log's local times are UTC.
        "poolRouting" => registry::pool_routing(&text(0), &text(1), num(2), num(3), |s| js::date_parse(&format!("{s}Z")).unwrap_or(f64::NAN)),
        "wantedEfforts" => registry::wanted_efforts(a.first(), a.get(1)),
        "probePlan" => Value::Array(registry::probe_plan(arg(0), arg(1), arg(2), arg(3), num(4), arg(5))),
        "fairBatch" => Value::Array(registry::fair_batch(arg(0), arg(1), num(2), num(3))),
        "degraded" => Value::Bool(registry::degraded(a.first())),
        other => return Err(format!("no port of {other}")),
    })
}

#[test]
fn every_registry_call_in_agent_pools_tests_matches() {
    let doc = golden("registry/cases.json");
    let cases = doc["cases"].as_array().expect("cases");
    assert!(cases.len() >= 50, "recorded {} calls", cases.len());
    let mut failures = Vec::new();
    for (i, case) in cases.iter().enumerate() {
        let f = case["fn"].as_str().unwrap_or("?");
        let at = case["at"].as_str().unwrap_or("?");
        let args = case["args"].as_array().cloned().unwrap_or_default();
        match (case.get("output"), case.get("error"), call(f, &args)) {
            (Some(want), _, Ok(got)) => {
                if js::stringify(want) != js::stringify(&got) {
                    let why = diff("$", want, &got).unwrap_or_else(|| "same values, different text (key order or number form)".into());
                    failures.push(format!("case {i} {f} ({at}): {why}"));
                }
            }
            (_, Some(want), Err(got)) if want.as_str() == Some(got.as_str()) => {}
            (_, want, got) => failures.push(format!("case {i} {f} ({at}): expected {want:?}, got {got:?}")),
        }
    }
    assert!(failures.is_empty(), "{} of {} differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}
