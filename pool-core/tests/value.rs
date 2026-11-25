//! The usage value report against `golden/outputs/value` (golden/capture.mjs, section 2): the
//! deployed settings, their session usage document, and short windows with 6-hour buckets.

mod common;

use common::{diff, golden, input, input_exists, input_lines};
use pool_core::js;
use pool_core::value::{self, LedgerRow, ReportInput};
use serde_json::{json, Map, Value};

struct Inputs {
    now: f64,
    pool: Value,
    install: Value,
    prices: Value,
    registry: Option<Value>,
    samples: Vec<Value>,
    ledger: Vec<LedgerRow>,
}

fn inputs() -> Inputs {
    let manifest = input("manifest.json");
    let mut samples: Vec<(f64, Value)> = input_lines("usage-samples.jsonl.gz")
        .into_iter()
        .map(|s| (js::date_parse_value(s.get("at")), s))
        .collect();
    samples.sort_by(|a, b| a.0.total_cmp(&b.0));
    Inputs {
        now: js::date_parse_value(manifest.get("now")),
        pool: input("pool-state.json"),
        install: input("install.json"),
        prices: input("model-prices.json"),
        registry: input_exists("model-registry.json").then(|| input("model-registry.json")),
        samples: samples.into_iter().map(|(_, s)| s).collect(),
        ledger: input_lines("usage-ledger.jsonl.gz")
            .iter()
            .map(LedgerRow::from_value)
            .collect(),
    }
}

/// The accounts as the report is given them (usage-value.mjs writeValueReport).
fn report_accounts(pool: &Value) -> Vec<Value> {
    pool["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            let or = |k: &str, d: Value| a.get(k).filter(|v| !v.is_null()).cloned().unwrap_or(d);
            let mut m = Map::new();
            for k in ["id", "provider", "email", "auth_index"] {
                if let Some(v) = a.get(k) {
                    m.insert(k.into(), v.clone());
                }
            }
            m.insert("plan".into(), or("plan", Value::Null));
            m.insert("quotaWindows".into(), or("quotaWindows", json!([])));
            Value::Object(m)
        })
        .collect()
}

fn run(inp: &Inputs, usage_ledger: Option<&Value>) -> (Value, Value) {
    let (config, problems) = value::value_config(usage_ledger);
    let forecast = golden("forecast/now.json");
    let weights = match inp.install.pointer("/capacityForecast/weights") {
        Some(Value::Object(w)) => w.clone(),
        _ => Map::new(),
    };
    let accounts = report_accounts(&inp.pool);
    let started = std::time::Instant::now();
    let reports = value::build_usage_reports(
        inp.ledger.iter().cloned(),
        &ReportInput {
            samples: &inp.samples,
            accounts: &accounts,
            prices: &inp.prices,
            config: &config,
            weights: &weights,
            forecast: Some(&forecast),
            registry: inp.registry.as_ref(),
            now: inp.now,
        },
    );
    eprintln!("value report built in {:?}", started.elapsed());
    let mut report = reports.value_report;
    report["problems"] = json!(problems);
    (report, reports.session_usage)
}

fn check(name: &str, expected: &Value, actual: &Value) {
    if let Some(d) = diff("report", expected, actual) {
        panic!("{name} differs: {d}");
    }
}

#[test]
fn value_report_deployed() {
    let inp = inputs();
    let (report, sessions) = run(&inp, inp.install.get("usageLedger"));
    check(
        "value/deployed.json",
        &golden("value/deployed.json"),
        &report,
    );
    check(
        "value/deployed-session-usage.json",
        &golden("value/deployed-session-usage.json"),
        &sessions,
    );
}

#[test]
fn value_report_short_windows() {
    let inp = inputs();
    let mut ledger = match inp.install.get("usageLedger") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    for (k, v) in [
        ("analysisDays", 3),
        ("valueDays", 2),
        ("wasteDays", 5),
        ("bucketHours", 6),
    ] {
        ledger.insert(k.into(), json!(v));
    }
    let (report, _) = run(&inp, Some(&Value::Object(ledger)));
    check(
        "value/short-windows.json",
        &golden("value/short-windows.json"),
        &report,
    );
}
/// A number as the case files write it: finite, or tagged.
fn num(f: f64) -> Value {
    if f.is_finite() {
        json!(f)
    } else {
        json!({ "$number": js::number_to_string(f) })
    }
}

/// A case-file number back to f64.
fn untag(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap(),
        Value::Object(m) => match m["$number"].as_str().unwrap() {
            "Infinity" => f64::INFINITY,
            "-Infinity" => f64::NEG_INFINITY,
            _ => f64::NAN,
        },
        _ => panic!("not a number: {v}"),
    }
}

fn check_cases(file: &str, run: impl Fn(&Value) -> Value) {
    let doc = golden(file);
    let cases = doc["cases"].as_array().unwrap();
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|c| {
            let got = run(&c["input"]);
            (common::diff_tagged("result", &c["result"], &got))
                .map(|d| format!("{}: {d}", c["name"].as_str().unwrap()))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{file}: {} of {} differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
    eprintln!("{file}: {} cases match", cases.len());
}

#[test]
fn quota_increments() {
    check_cases("value/quota-increments.json", |input| {
        let stream: Vec<value::Reading> = input
            .as_array()
            .unwrap()
            .iter()
            .map(|o| value::Reading {
                at: untag(&o["at"]),
                used: untag(&o["used"]),
                reset_at: untag(&o["resetAt"]),
            })
            .collect();
        let q = value::quota_increments(&stream);
        json!({
            "increments": q.increments.iter().map(|i| json!({ "at": num(i.at), "percent": num(i.percent) })).collect::<Vec<_>>(),
            "windowEnds": q.window_ends.iter().map(|e| json!({ "at": num(e.at), "lastUsed": num(e.last_used), "endedAt": num(e.ended_at), "early": e.early })).collect::<Vec<_>>(),
            "refills": q.refills.iter().map(|r| json!({ "at": num(r.at), "from": num(r.from), "to": num(r.to) })).collect::<Vec<_>>(),
        })
    });
}

#[test]
fn cost_of() {
    let deployed = input("model-prices.json");
    check_cases("value/cost.json", |input| {
        let prices = if input["prices"] == json!("deployed") {
            &deployed
        } else {
            &input["prices"]
        };
        match value::cost_of(&LedgerRow::from_value(&input["row"]), prices) {
            None => Value::Null,
            Some(c) => json!({
                "model": c.model,
                "usd": num(c.usd),
                "byType": { "in": num(c.by_type[0]), "cr": num(c.by_type[1]), "cw": num(c.by_type[2]), "out": num(c.by_type[3]) },
            }),
        }
    });
}

#[test]
fn verified_prices_fill_gaps_without_overriding_the_install() {
    let base = json!({
        "models": { "gpt-6.2-sol": { "input": 3, "output": 15 } }
    });
    assert_eq!(value::supplement_prices(base.clone(), 0.0), base);
    let prices = value::supplement_prices(
        base,
        js::date_parse("2026-11-25T18:00:00Z").unwrap(),
    );
    let lookup = |model| value::price_entry(&prices, Some(&json!(model)));
    assert_eq!(lookup("gpt-6.2-sol").unwrap().1["input"], 3);
    let astra = lookup("gpt-6.2-astra").unwrap().1;
    assert_eq!(astra["cacheRead"], 0.40);
    assert_eq!(astra["longContext"]["output"], 60);
    assert_eq!(lookup("claude-fable-5-2").unwrap().1["cacheRead"], 0.20);
    assert_eq!(lookup("claude-sonnet-5").unwrap().1["cacheRead"], 0.20);
    let (id, haiku) = lookup("claude-haiku-4-5-20251001").unwrap();
    assert_eq!(id, "claude-haiku-4-5");
    assert_eq!(haiku["output"], 5);
    assert!(lookup("codex-auto-review").is_none());
    assert!(prices["verifiedSupplement"]["sources"]["gpt-6.2-sol"].is_null());
    assert!(prices["verifiedSupplement"]["sources"]["claude-fable-5-2"].is_string());
}

const HOUR_MS: f64 = 3_600_000.0;

fn coverage_report(rows: &[Value], samples: &[Value], accounts: &[Value], now: f64, config: Value) -> Value {
    let (config, problems) = value::value_config(Some(&config));
    assert!(problems.is_empty());
    value::build_usage_reports(
        rows.iter().map(LedgerRow::from_value),
        &ReportInput {
            samples,
            accounts,
            prices: &json!({ "models": { "priced": { "input": 5, "output": 1 },
                "free": { "input": 0, "output": 0 } } }),
            config: &config,
            weights: &Map::new(),
            forecast: None,
            registry: None,
            now,
        },
    )
    .value_report
}

#[test]
fn missing_prices_leave_costs_unknown_and_partial_totals_keep_known_spend() {
    let now = js::date_parse("2026-11-25T18:00:00Z").unwrap();
    let from = js::iso(now - 7.0 * 24.0 * HOUR_MS);
    let rows = [
        json!({ "at": from, "acct": "codex-mixed", "model": "priced", "in": 400_000, "sid": "mixed" }),
        json!({ "at": from, "acct": "codex-mixed", "model": "missing", "in": 600_000, "sid": "mixed" }),
        json!({ "at": from, "acct": "codex-known", "model": "priced", "in": 1_000_000 }),
        json!({ "at": from, "acct": "codex-known", "model": "free", "in": 10_000 }),
        json!({ "at": from, "acct": "claude-unknown", "model": "missing", "in": 100_000 }),
    ];
    let accounts = [
        json!({ "id": "codex-mixed", "provider": "codex" }),
        json!({ "id": "codex-known", "provider": "codex" }),
        json!({ "id": "codex-idle", "provider": "codex" }),
        json!({ "id": "claude-unknown", "provider": "claude" }),
    ];
    let r = coverage_report(&rows, &[], &accounts, now, json!({
        "planPrices": { "codex": 365, "claude": 365 }
    }));
    let codex = &r["actualCost"]["codex"];
    assert_eq!(codex["unpricedRequests"], 1.0);
    assert_eq!(codex["apiEquivalentUsd"], 7.0);
    assert_eq!(codex["apiEquivalentUsdPerRequest"], 2.3333);
    assert_eq!(codex["actualUsd"], 168.0);
    assert_eq!(codex["unallocatedUsd"], 84.0);
    assert_eq!(codex["idleUsd"], 84.0);
    assert_eq!(codex["actualUsdPerRequest"], 42.0);
    let claude = &r["actualCost"]["claude"];
    assert!(claude["apiEquivalentUsd"].is_null());
    assert_eq!(claude["actualUsd"], 84.0);
    assert_eq!(claude["unallocatedUsd"], 84.0);
    let model = |id| r["models"].as_array().unwrap().iter()
        .find(|m| m["provider"] == "codex" && m["model"] == id).unwrap();
    assert!(model("missing")["apiEquivalentUsd"].is_null());
    assert!(model("missing")["actualUsd"].is_null());
    assert_eq!(model("free")["apiEquivalentUsd"], 0.0);
    assert_eq!(model("free")["actualUsd"], 0.0);
    assert_eq!(model("priced")["actualUsd"], 84.0);
    assert_eq!(model("priced")["uncostedRequests"], 1.0);
    assert!(r["accounts"][0]["valueMultiple"].is_null());
    assert_eq!(r["sessions"][0]["unpricedRequests"], 1.0);
    assert_eq!(r["sessions"][0]["apiEquivalentUsd"], 2.0);
    assert!(r["sessions"][0]["actualUsd"].is_null());
}

#[test]
fn estimated_quota_share_uses_unrounded_units() {
    let now = js::date_parse("2026-11-25T18:00:00Z").unwrap();
    let history = now - 5.0 * 24.0 * HOUR_MS;
    let at = |i: i32| js::iso(history + f64::from(i) * 6.0 * HOUR_MS);
    let mut samples = Vec::new();
    let mut rows = Vec::new();
    for i in 0..8 {
        samples.push(json!({
            "at": at(i), "provider": "codex", "accountId": "codex-test",
            "window": "codex:primary_window", "used": i,
        }));
        if i > 0 {
            rows.push(json!({ "at": at(i), "acct": "codex-test", "model": "priced", "in": 200_000 }));
        }
    }
    // That historical bucket's quota delta cannot be attributed to the priced model alone.
    rows.push(json!({ "at": at(3), "acct": "codex-test", "model": "missing" }));
    let recent = js::iso(now - HOUR_MS);
    rows.push(json!({ "at": recent, "acct": "codex-test", "model": "priced", "in": 1_600 }));
    rows.push(json!({ "at": recent, "acct": "codex-test", "model": "missing" }));
    let accounts = [json!({ "id": "codex-test", "provider": "codex" })];
    let r = coverage_report(&rows, &samples, &accounts, now, json!({ "valueDays": 2, "bucketHours": 6 }));
    assert_eq!(r["fits"]["codex"]["status"], "ok");
    assert_eq!(r["fits"]["codex"]["observations"], 6);
    assert_eq!(r["models"][0]["estimatedQuotaUnits"], 0.0001);
    assert_eq!(r["models"][0]["shareOfProviderQuota"], 1.0);
    assert!(r["models"][1]["shareOfProviderQuota"].is_null());
}

#[test]
fn value_config() {
    check_cases("value/config.json", |input| {
        let arg = (input != &json!("undefined")).then_some(input);
        let (config, problems) = value::value_config(arg);
        json!({ "config": config.to_value(), "problems": problems })
    });
}
