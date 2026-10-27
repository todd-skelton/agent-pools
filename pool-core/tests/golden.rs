//! The port against `golden/outputs`: every case the JavaScript reference produced, compared
//! as parsed JSON with numbers equal as `f64` (golden/README.md, "Matching them in Rust").

use pool_core::{banks, js, quota, routing};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

fn golden(rel: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../golden/outputs")
        .join(rel);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

/// Exact equality of parsed JSON, numbers as f64. Returns the first difference's path.
fn diff(path: &str, expected: &Value, actual: &Value) -> Option<String> {
    match (expected, actual) {
        (Value::Number(e), Value::Number(a)) => {
            (e.as_f64() != a.as_f64()).then(|| format!("{path}: expected {e}, got {a}"))
        }
        (Value::Object(e), Value::Object(a)) => {
            if e.contains_key("$number") || e.contains_key("$set") || e.contains_key("$map") {
                return Some(format!(
                    "{path}: tagged value {expected} needs explicit handling"
                ));
            }
            for k in e.keys().chain(a.keys()) {
                match (e.get(k), a.get(k)) {
                    (Some(x), Some(y)) => {
                        if let Some(d) = diff(&format!("{path}.{k}"), x, y) {
                            return Some(d);
                        }
                    }
                    (x, y) => return Some(format!("{path}.{k}: expected {x:?}, got {y:?}")),
                }
            }
            None
        }
        (Value::Array(e), Value::Array(a)) => {
            if e.len() != a.len() {
                return Some(format!(
                    "{path}: expected {} items, got {}",
                    e.len(),
                    a.len()
                ));
            }
            e.iter()
                .zip(a)
                .enumerate()
                .find_map(|(i, (x, y))| diff(&format!("{path}[{i}]"), x, y))
        }
        _ => (expected != actual).then(|| format!("{path}: expected {expected}, got {actual}")),
    }
}

fn check_cases(file: &str, run: impl Fn(&Value) -> Value) {
    let doc = golden(file);
    let cases = doc["cases"].as_array().unwrap();
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|c| {
            diff("result", &c["result"], &run(&c["input"]))
                .map(|d| format!("{}: {d}", c["name"].as_str().unwrap()))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{file}: {} of {} cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures
            .iter()
            .take(15)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    eprintln!("{file}: {} cases match", cases.len());
}

fn now_of(input: &Value) -> f64 {
    input["now"].as_f64().unwrap()
}

#[test]
fn quota_windows_and_normalized_usage() {
    check_cases("quota/usage.json", |input| {
        let provider = input["provider"].as_str().unwrap();
        serde_json::json!({
            "quotaWindows": quota::quota_windows(provider, &input["data"]),
            "normalizeUsage": quota::normalize_usage(provider, &input["data"], now_of(input)),
        })
    });
}

#[test]
fn credit_banks() {
    check_cases("quota/banks.json", |input| {
        if input.get("block").is_some() {
            let block = Some(&input["block"]).filter(|b| !b.is_null());
            serde_json::to_value(banks::claude_bank(block, now_of(input))).unwrap()
        } else {
            serde_json::to_value(banks::available_credits(&input["data"], now_of(input))).unwrap()
        }
    });
}

fn strings(v: &Value) -> HashSet<String> {
    v.as_array()
        .map(|a| a.iter().map(|s| s.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

#[test]
fn choose_reset() {
    check_cases("routing/choose-reset.json", |input| {
        let accounts: Vec<routing::Account> = input["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(routing::Account::from_value)
            .collect();
        let policy = routing::Policy::from_value(&input["policy"]);
        let before_lapse =
            (!input["beforeLapse"].is_null()).then(|| strings(&input["beforeLapse"]));
        let choice = routing::choose_reset(
            &accounts,
            &policy,
            now_of(input),
            &strings(&input["skip"]),
            before_lapse.as_ref(),
        );
        serde_json::to_value(choice).unwrap()
    });
}

#[test]
fn rank_accounts() {
    check_cases("routing/rank-accounts.json", |input| {
        let accounts: Vec<routing::Account> = input["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(routing::Account::from_value)
            .collect();
        let drain_first: Option<HashMap<String, f64>> =
            input["drainFirst"].as_array().map(|pairs| {
                pairs
                    .iter()
                    .map(|p| (p[0].as_str().unwrap().to_string(), p[1].as_f64().unwrap()))
                    .collect()
            });
        let ranked: Vec<&str> = routing::rank_accounts(&accounts, drain_first.as_ref())
            .into_iter()
            .map(|a| a.id.as_str())
            .collect();
        serde_json::to_value(ranked).unwrap()
    });
}

#[test]
fn javascript_semantics() {
    let doc = golden("js/semantics.json");
    let mut failures = Vec::new();
    for pair in doc["localeCompare"].as_array().unwrap() {
        let (a, b, want) = (
            pair[0].as_str().unwrap(),
            pair[1].as_str().unwrap(),
            pair[2].as_i64().unwrap(),
        );
        let got = js::locale_compare(a, b) as i64;
        if got != want {
            failures.push(format!("localeCompare({a:?}, {b:?}) = {want}, got {got}"));
        }
    }
    let (mut local, mut parsed) = (0, 0);
    for pair in doc["dateParse"].as_array().unwrap() {
        let s = pair[0].as_str().unwrap();
        let want = pair[1].as_f64();
        let got = js::date_parse(s);
        // A date-time without an offset is local time in JavaScript; the port leaves it unparsed.
        let has_time = s.len() > 10 && matches!(s.as_bytes()[10], b'T' | b't' | b' ');
        let has_offset = s.ends_with(['Z', 'z'])
            || s.get(s.len().saturating_sub(6)..)
                .is_some_and(|t| t.starts_with(['+', '-']) && t.as_bytes()[3] == b':');
        if has_time && !has_offset && got.is_none() {
            local += 1;
            continue;
        }
        parsed += 1;
        if got != want {
            failures.push(format!("Date.parse({s:?}) = {want:?}, got {got:?}"));
        }
    }
    for pair in doc["numberToString"].as_array().unwrap() {
        let (n, want) = (pair[0].as_f64().unwrap(), pair[1].as_str().unwrap());
        let got = js::number_to_string(n);
        if got != want {
            failures.push(format!("String({n}) = {want:?}, got {got:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} differ:\n{}",
        failures.len(),
        failures
            .iter()
            .take(25)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    eprintln!("semantics match; {parsed} dates compared, {local} local-time dates left unparsed by design");
}
