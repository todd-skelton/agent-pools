//! \`poolUsage\` against every call Agent Pools' own pool-usage.test.mjs makes, recorded by
//! \`node --import ./golden/usage-hook.mjs reference/agent-pools/pool-usage.test.mjs\`: the overlay
//! as \`JSON.stringify\` writes it, byte for byte, the upstream itself when unchanged, or the same
//! error.

mod common;

use common::{diff, golden};
use pool_core::{js, pool_usage};

#[test]
fn every_pool_usage_call_in_agent_pools_tests_matches() {
    let doc = golden("pool-usage/cases.json");
    let cases = doc["cases"].as_array().expect("cases");
    assert!(cases.len() >= 30, "recorded {} calls", cases.len());
    let mut failures = Vec::new();
    for (i, case) in cases.iter().enumerate() {
        let at = case["at"].as_str().unwrap_or("?");
        let accounts = case["accounts"].as_array().cloned().unwrap_or_default();
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
