//! `poolUsage` against every call Agent Pools' own pool-usage.test.mjs makes, recorded by
//! `node --import ./golden/usage-hook.mjs reference/agent-pools/pool-usage.test.mjs`: the overlay
//! as `JSON.stringify` writes it, byte for byte, the upstream itself when unchanged, or the same
//! error.

mod common;

use common::{diff, golden};
use pool_core::{js, pool_usage, summary};

#[test]
fn every_pool_usage_call_in_agent_pools_tests_matches() {
    let doc = golden("pool-usage/cases.json");
    let cases = doc["cases"].as_array().expect("cases");
    assert!(cases.len() >= 30, "recorded {} calls", cases.len());
    replay(cases);
}

/// Real accounts, when `AP_USAGE_LIVE` names a file from `node golden/usage-live.mjs` (not
/// committed: the accounts are not scrubbed). Each case keeps the raw accounts, and the port
/// applies its own `current_account` first, as the service does before the overlay.
#[test]
fn real_accounts_match_when_recorded() {
    let Ok(path) = std::env::var("AP_USAGE_LIVE") else {
        eprintln!("AP_USAGE_LIVE not set; skipped");
        return;
    };
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    replay(doc["cases"].as_array().expect("cases"));
}

fn replay(cases: &[serde_json::Value]) {
    let mut failures = Vec::new();
    for (i, case) in cases.iter().enumerate() {
        let at = case["at"].as_str().unwrap_or("?");
        let accounts = match case.get("rawAccounts") {
            Some(raw) => {
                let now = case["now"].as_f64().unwrap_or(f64::NAN);
                raw.as_array().unwrap().iter().map(|a| summary::current_account(a, now)).collect()
            }
            None => case["accounts"].as_array().cloned().unwrap_or_default(),
        };
        let got = pool_usage::pool_usage(
            &case["upstream"],
            &accounts,
            case["mode"].as_str().unwrap_or(""),
            case["now"].as_f64().unwrap_or(f64::NAN),
            case["includeClaude"].as_bool().unwrap_or(true),
        );
        match (case.get("error"), got) {
            (Some(want), Err(got)) if want.as_str() == Some(got.as_str()) => {}
            (None, Ok(None)) if case["same"] == true => {}
            (None, Ok(Some(got))) if case["same"] != true => {
                let want = &case["output"];
                if js::stringify(want) != js::stringify(&got) {
                    let why = diff("$", want, &got).unwrap_or_else(|| "same values, different text (key order or number form)".into());
                    failures.push(format!("case {i} ({at}): {why}"));
                }
            }
            (want, got) => failures.push(format!("case {i} ({at}): expected {:?} (same: {}), got {got:?}", want, case["same"])),
        }
    }
    assert!(failures.is_empty(), "{} of {} differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}
