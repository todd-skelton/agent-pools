//! `buildLadders` against every call Agent Pools' own routing-ladder.test.mjs makes, recorded by
//! `node --import ./golden/ladder-hook.mjs reference/agent-pools/routing-ladder.test.mjs`: the
//! document as published (`JSON.stringify`, digest included) byte for byte, or the same error.

mod common;

use common::{diff, golden};
use pool_core::{js, ladder};

#[test]
fn every_ladder_call_in_agent_pools_tests_matches() {
    let doc = golden("ladders/cases.json");
    let cases = doc["cases"].as_array().expect("cases");
    assert!(cases.len() >= 30, "recorded {} calls", cases.len());
    replay(cases);
}

/// The live install's ladder inputs, when `AP_LADDER_LIVE` names a file from
/// `node golden/ladder-live.mjs` (not committed: its inputs are not scrubbed).
#[test]
fn the_live_installs_ladders_match_when_recorded() {
    let Ok(path) = std::env::var("AP_LADDER_LIVE") else {
        eprintln!("AP_LADDER_LIVE not set; skipped");
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
        let now = case["now"].as_f64().unwrap_or(f64::NAN);
        let got = ladder::build_ladders(&case["inputs"], now);
        match (case.get("output"), case.get("error"), got) {
            (Some(want), _, Ok(got)) => {
                if js::stringify(want) != js::stringify(&got) {
                    let why = diff("$", want, &got).unwrap_or_else(|| "same values, different text (key order or number form)".into());
                    failures.push(format!("case {i} ({at}): {why}"));
                }
            }
            (_, Some(want), Err(got)) if want.as_str() == Some(got.as_str()) => {}
            (_, want, got) => failures.push(format!("case {i} ({at}): expected error {want:?}, got {:?}", got.map(|_| "a document"))),
        }
    }
    assert!(failures.is_empty(), "{} of {} differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}
