//! usage-ledger.mjs's pure functions against Agent Pools' own, over the records and settings in
//! `golden/outputs/ledger/cases.json` (`node golden/ledger-cases.mjs`): each row, config and
//! client name as `JSON.stringify` writes it.

mod common;

use std::collections::HashMap;

use common::{diff, golden};
use pool_core::{js, ledger};
use serde_json::Value;

#[test]
fn rows_configs_and_clients_match_agent_pools() {
    let doc = golden("ledger/cases.json");
    let mut failures = Vec::new();
    let rows = doc["rows"].as_array().unwrap();
    assert!(rows.len() >= 200);
    for (i, case) in rows.iter().enumerate() {
        let map: HashMap<String, Value> = case["map"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p[0].as_str().unwrap().to_string(), p[1].clone()))
            .collect();
        let got = ledger::ledger_row(&case["record"], &map).unwrap_or(Value::Null);
        if js::stringify(&got) != js::stringify(&case["output"]) {
            failures.push(format!("row {i}: {}", diff("$", &case["output"], &got).unwrap_or_else(|| format!("{} vs {}", case["output"], got))));
        }
    }
    for (i, case) in doc["configs"].as_array().unwrap().iter().enumerate() {
        let input = (case["input"] != "__undefined__").then_some(&case["input"]);
        let (config, problems) = ledger::ledger_config(input);
        let got = serde_json::json!({ "config": config.to_value(), "problems": problems });
        if js::stringify(&got) != js::stringify(&case["output"]) {
            failures.push(format!("config {i}: {} vs {}", case["output"], got));
        }
    }
    for case in doc["clients"].as_array().unwrap() {
        let got = ledger::client_of(Some(&case["ua"]));
        if got != case["output"].as_str().unwrap() {
            failures.push(format!("client {}: {} vs {got}", case["ua"], case["output"]));
        }
    }
    assert!(failures.is_empty(), "{} differ:\n{}", failures.len(), failures.join("\n"));
}
