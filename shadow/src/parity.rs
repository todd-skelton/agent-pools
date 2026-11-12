//! The parity checks: what the live service published, set beside what `pool-core` builds from
//! the same inputs. A check whose inputs the shadow cannot pin down (the live service read its
//! state at another moment than the files show) is recorded as unaligned, with the reason, and
//! never counted as a match or a difference.

use pool_core::{forecast, js, providers, resets, status, summary, value};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::sources::{DAY, Install};

pub const HOUR: f64 = 3_600_000.0;

/// What one check compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    /// `GET /api/summary` on the live service against `build_summary` over its files.
    Summary,
    /// `state/capacity-forecast.json` against `build_forecast` with the document before it.
    Forecast,
    /// `state/usage-value.json` against `build_usage_reports` over the ledger.
    Value,
    /// `GET /api/status` on the live service against `build_status` over its files.
    Status,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Summary, Kind::Forecast, Kind::Value, Kind::Status];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Summary => "summary",
            Kind::Forecast => "forecast",
            Kind::Value => "value",
            Kind::Status => "status",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    Match,
    Differs,
    /// The inputs could not be matched to what the live service used.
    Unaligned,
}

/// One check, as appended to `parity.jsonl`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub kind: Kind,
    /// When the shadow ran the check.
    pub checked_at: String,
    /// The live document's `generatedAt`.
    pub subject: Option<String>,
    pub outcome: Outcome,
    /// Why the check is unaligned, or the first differing path and both values.
    pub detail: Option<String>,
    /// Milliseconds to build the shadow's document.
    pub build_ms: Option<f64>,
}

impl Check {
    pub fn unaligned(
        kind: Kind,
        subject: Option<String>,
        why: impl Into<String>,
        now: f64,
    ) -> Check {
        Check {
            kind,
            checked_at: js::iso(now),
            subject,
            outcome: Outcome::Unaligned,
            detail: Some(why.into()),
            build_ms: None,
        }
    }

    fn compared(
        kind: Kind,
        subject: Option<String>,
        live: &Value,
        built: &Value,
        build_ms: f64,
        now: f64,
    ) -> Check {
        let d = diff(kind.name(), live, built);
        Check {
            kind,
            checked_at: js::iso(now),
            subject,
            outcome: if d.is_some() {
                Outcome::Differs
            } else {
                Outcome::Match
            },
            detail: d,
            build_ms: Some((build_ms * 10.0).round() / 10.0),
        }
    }
}

/// Exact equality of parsed JSON, numbers as f64, object keys in any order (`JSON.parse` keeps
/// none of the order a reader could rely on). Returns the first difference's path and values.
pub fn diff(path: &str, expected: &Value, actual: &Value) -> Option<String> {
    match (expected, actual) {
        (Value::Number(e), Value::Number(a)) => {
            (e.as_f64() != a.as_f64()).then(|| format!("{path}: live {e}, shadow {a}"))
        }
        (Value::Object(e), Value::Object(a)) => {
            for k in e.keys().chain(a.keys()) {
                match (e.get(k), a.get(k)) {
                    (Some(x), Some(y)) => {
                        if let Some(d) = diff(&format!("{path}.{k}"), x, y) {
                            return Some(d);
                        }
                    }
                    (x, y) => {
                        return Some(format!(
                            "{path}.{k}: live {}, shadow {}",
                            shown(x),
                            shown(y)
                        ));
                    }
                }
            }
            None
        }
        (Value::Array(e), Value::Array(a)) => {
            if e.len() != a.len() {
                return Some(format!(
                    "{path}: live {} items, shadow {}",
                    e.len(),
                    a.len()
                ));
            }
            e.iter()
                .zip(a)
                .enumerate()
                .find_map(|(i, (x, y))| diff(&format!("{path}[{i}]"), x, y))
        }
        _ => (expected != actual).then(|| {
            format!(
                "{path}: live {}, shadow {}",
                shown(Some(expected)),
                shown(Some(actual))
            )
        }),
    }
}

fn shown(v: Option<&Value>) -> String {
    match v {
        None => "(absent)".into(),
        Some(v) => {
            let s = v.to_string();
            if s.len() > 160 {
                format!("{}...", s.chars().take(160).collect::<String>())
            } else {
                s
            }
        }
    }
}

/// Provider id and display name, in `PROVIDER_IDS` order, as `/api/summary` is given them.
pub fn provider_names() -> Vec<(&'static str, &'static str)> {
    providers::PROVIDERS
        .iter()
        .map(|p| (p.id, p.name))
        .collect()
}

/// `{ ...state, accounts: state.accounts.map(currentAccount) }`.
pub fn current_state(pool: &Value, now: f64) -> Value {
    let Value::Object(fields) = pool else { return pool.clone() };
    // Each field once: the accounts as currentAccount gives them, the rest as saved (the saved
    // accounts are never copied only to be replaced).
    Value::Object(
        fields
            .iter()
            .map(|(k, v)| match (k.as_str(), v) {
                ("accounts", Value::Array(accounts)) => (k.clone(), Value::Array(accounts.iter().map(|a| summary::current_account(a, now)).collect())),
                _ => (k.clone(), v.clone()),
            })
            .collect(),
    )
}

/// `/api/summary` over the install's files at `now`, built afresh: what the shadow's kept answers
/// are checked against.
#[cfg(test)]
pub fn summary_now(install: &Install, now: f64) -> Option<Value> {
    let pool = install.doc("state/pool-state.json")?;
    let forecast = install
        .doc("state/capacity-forecast.json")
        .filter(|d| d.get("format") == Some(&json!(forecast::FORECAST_FORMAT)));
    let value = install.doc("state/usage-value.json");
    Some(summary::build_summary(
        &current_state(&pool, now),
        &provider_names(),
        forecast.as_deref(),
        value.as_deref(),
        now,
    ))
}

fn generated_at(doc: Option<&Value>) -> Value {
    js::get(doc, "generatedAt").cloned().unwrap_or(Value::Null)
}

fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_string)
}

/// The live `/api/summary` against `build_summary` over the files read just after it. Aligned
/// when the live answer names the same `checkedAt`, forecast and value report the files hold;
/// the build uses the live answer's `generatedAt` as `now`, so freshness is judged at the same
/// moment.
pub fn check_summary(
    live: &Value,
    pool: Option<&Value>,
    forecast_doc: Option<&Value>,
    value_doc: Option<&Value>,
    now: f64,
) -> Check {
    let subject = text(live.get("generatedAt"));
    let Some(pool) = pool else {
        return Check::unaligned(
            Kind::Summary,
            subject,
            "state/pool-state.json did not read",
            now,
        );
    };
    let at = js::date_parse_value(live.get("generatedAt"));
    if !at.is_finite() {
        return Check::unaligned(
            Kind::Summary,
            subject,
            "the live summary has no generatedAt",
            now,
        );
    }
    let mut skew = Vec::new();
    if live.get("checkedAt") != Some(pool.get("checkedAt").unwrap_or(&Value::Null)) {
        skew.push("pool state was saved after the live answer (checkedAt)");
    }
    if live.get("forecastGeneratedAt") != Some(&generated_at(forecast_doc)) {
        skew.push("the forecast was republished after the live answer");
    }
    if live.get("valueGeneratedAt") != Some(&generated_at(value_doc)) {
        skew.push("the value report was rewritten after the live answer");
    }
    if !skew.is_empty() {
        return Check::unaligned(Kind::Summary, subject, skew.join("; "), now);
    }
    let started = std::time::Instant::now();
    let built = summary::build_summary(
        &current_state(pool, at),
        &provider_names(),
        forecast_doc,
        value_doc,
        at,
    );
    Check::compared(
        Kind::Summary,
        subject,
        live,
        &built,
        started.elapsed().as_secs_f64() * 1000.0,
        now,
    )
}

/// `/api/status` over the install's files at `now`, with what only the live service knows
/// (`live`) as given.
pub fn status_now(install: &Install, live: &status::Live, now: f64) -> Option<Value> {
    let pool = install.doc("state/pool-state.json")?;
    let settings = install.settings();
    let forecast = install
        .doc("state/capacity-forecast.json")
        .filter(|d| d.get("format") == Some(&json!(forecast::FORECAST_FORMAT)));
    Some(status::build_status(
        &pool,
        settings.get("expectedAccounts"),
        &resets::reset_policies(&settings),
        forecast.as_deref(),
        live,
        now,
    ))
}

/// The live `/api/status` against `build_status` over the files read just after it, at `at`
/// (when the answer arrived: the status carries no time of its own). Aligned when the answer
/// holds the pool state's `checkedAt` and the forecast on disk. `processes`, `holds` and
/// `usageLedger` are the live service's memory and are taken from its answer.
pub fn check_status(live: &Value, install: &Install, at: f64, now: f64) -> Check {
    let subject = text(live.get("checkedAt"));
    let Some(pool) = install.json("state/pool-state.json") else {
        return Check::unaligned(
            Kind::Status,
            subject,
            "state/pool-state.json did not read",
            now,
        );
    };
    let forecast = install.json("state/capacity-forecast.json");
    let mut skew = Vec::new();
    if live.get("checkedAt") != Some(pool.get("checkedAt").unwrap_or(&Value::Null)) {
        skew.push("pool state was saved after the live answer (checkedAt)");
    }
    if js::get(live.get("capacity"), "generatedAt") != js::get(forecast.as_ref(), "generatedAt") {
        skew.push("the forecast was republished after the live answer");
    }
    if !skew.is_empty() {
        return Check::unaligned(Kind::Status, subject, skew.join("; "), now);
    }
    let taken = status::Live {
        processes: live.get("processes").cloned(),
        holds: live.get("holds").cloned(),
        usage_ledger: live.get("usageLedger").cloned(),
    };
    let started = std::time::Instant::now();
    let Some(built) = status_now(install, &taken, at) else {
        return Check::unaligned(
            Kind::Status,
            subject,
            "state/pool-state.json did not read",
            now,
        );
    };
    Check::compared(
        Kind::Status,
        subject,
        live,
        &built,
        started.elapsed().as_secs_f64() * 1000.0,
        now,
    )
}

/// The inputs `capacity-publisher.mjs` gives `buildForecast` for a document published at `t`.
pub struct ForecastInputs<'a> {
    pub install: &'a Install,
    /// The pool state saved in the same refresh cycle, before the forecast.
    pub pool: &'a Value,
    /// The document published before this one (the publisher's hysteresis memory).
    pub previous: Option<&'a Value>,
}

/// `build_forecast` as the publisher calls it at `t`, from the files.
pub fn forecast_at(inp: &ForecastInputs, t: f64) -> Value {
    let settings = inp.install.settings();
    let (config, _problems) = forecast::forecast_config(settings.get("capacityForecast"));
    let samples = inp
        .install
        .samples(t - (config.lookback_hours + 48.0) * HOUR, t);
    let accounts: Vec<Value> = match inp.pool.get("accounts") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    let policies = resets::reset_policies(&settings);
    let automation = inp.pool.get("resetAutomation") != Some(&json!(false));
    forecast::build_guided_forecast(
        &accounts,
        &samples,
        &config,
        t,
        inp.previous,
        &policies,
        automation,
    )
}

/// A published forecast against the shadow's build of it.
pub fn check_forecast(live: &Value, inp: &ForecastInputs, now: f64) -> Check {
    let subject = text(live.get("generatedAt"));
    let t = js::date_parse_value(live.get("generatedAt"));
    if !t.is_finite() {
        return Check::unaligned(
            Kind::Forecast,
            subject,
            "the forecast has no generatedAt",
            now,
        );
    }
    let started = std::time::Instant::now();
    let mut built = forecast_at(inp, t);
    // Historical Node documents predate advisory guidance. Compare their full
    // original contract; new documents must match guidance as well.
    if let Some(providers) = built["providers"].as_object_mut() {
        for (provider, f) in providers {
            if live["providers"][provider].get("guidance").is_none()
                && inp.previous.and_then(|d| d["providers"][provider].get("guidance")).is_none() {
                f.as_object_mut().unwrap().remove("guidance");
            }
        }
    }
    Check::compared(
        Kind::Forecast,
        subject,
        live,
        &built,
        started.elapsed().as_secs_f64() * 1000.0,
        now,
    )
}

/// The inputs `writeValueReport` read for a report built at `t`.
#[derive(Clone, Copy)]
pub struct ValueInputs<'a> {
    pub install: &'a Install,
    pub pool: &'a Value,
    pub forecast: Option<&'a Value>,
    /// Ledger rows dated after this were written after the report read the ledger.
    pub ledger_until: f64,
    /// Samples dated after this were written after the report read the sample logs (it reads
    /// them at once, with no upper bound of its own).
    pub samples_until: f64,
    /// The live report's `coverage.ledgerRows`. The report streams the ledger while the proxy
    /// appends to it, so what it read is a prefix of the files in name order; the rows it counted
    /// say where that prefix ends. `None` reads every row up to `ledger_until`.
    pub ledger_rows: Option<f64>,
}

/// `writeValueReport` at `t`, from the files, without writing anything.
pub fn value_at(inp: &ValueInputs, t: f64) -> Value {
    reports_at(inp, t).value_report
}

/// What `writeValueReport` builds at `t`: the value report (with its config `problems`) and the
/// session usage document.
pub fn reports_at(inp: &ValueInputs, t: f64) -> value::UsageReports {
    let settings = inp.install.settings();
    let (config, problems) = value::value_config(settings.get("usageLedger"));
    let prices = inp.install.json("model-prices.json").unwrap_or(Value::Null);
    let registry = inp.install.json("state/model-registry.json");
    let first_day: String = js::iso(t - (config.analysis_days + 1.0) * DAY)
        .chars()
        .take(10)
        .collect();
    let keep_from = t - config.analysis_days.max(config.waste_days) * DAY - 8.0 * DAY;
    let samples = inp.install.samples(keep_from, inp.samples_until);
    let accounts: Vec<Value> = match inp.pool.get("accounts") {
        Some(Value::Array(a)) => a.iter().map(report_account).collect(),
        _ => Vec::new(),
    };
    let weights = match settings.pointer("/capacityForecast/weights") {
        Some(Value::Object(w)) => w.clone(),
        _ => Map::new(),
    };
    let analysis_from = t - config.analysis_days * DAY;
    let target = inp.ledger_rows.unwrap_or(f64::INFINITY);
    let mut counted = 0.0;
    let mut seen = std::collections::HashSet::new();
    // Stops at the row after the last one the report counted (rows outside the window it skips
    // are not counted, as in buildUsageReports).
    let rows = inp
        .install
        .ledger(&first_day, inp.ledger_until)
        .take_while(move |r| {
            if counted >= target {
                return false;
            }
            let duplicate = r
                .rid
                .as_ref()
                .is_some_and(|rid| !seen.insert(format!("{rid}|{}", r.at_text)));
            if r.at.is_finite() && r.at >= analysis_from && r.at <= t + HOUR && !duplicate {
                counted += 1.0;
            }
            true
        });
    let reports = value::build_usage_reports(
        rows,
        &value::ReportInput {
            samples: &samples,
            accounts: &accounts,
            prices: &prices,
            config: &config,
            weights: &weights,
            forecast: inp.forecast,
            registry: registry.as_ref(),
            now: t,
        },
    );
    let mut reports = reports;
    reports.value_report["problems"] = json!(problems);
    reports
}

/// `{ id, provider, email, auth_index, plan: a.plan ?? null, quotaWindows: a.quotaWindows ?? [] }`.
fn report_account(a: &Value) -> Value {
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
}

pub fn check_value(live: &Value, inp: &ValueInputs, now: f64) -> Check {
    let subject = text(live.get("generatedAt"));
    let ledger_rows = js::finite(live.pointer("/coverage/ledgerRows"));
    let inp = &ValueInputs {
        ledger_rows: inp.ledger_rows.or(ledger_rows),
        ..*inp
    };
    let t = js::date_parse_value(live.get("generatedAt"));
    if !t.is_finite() {
        return Check::unaligned(Kind::Value, subject, "the report has no generatedAt", now);
    }
    let started = std::time::Instant::now();
    let built = value_at(inp, t);
    Check::compared(
        Kind::Value,
        subject,
        live,
        &built,
        started.elapsed().as_secs_f64() * 1000.0,
        now,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_reports_the_first_path() {
        let live = json!({"a": [1, {"b": 2.5}], "c": "x"});
        assert_eq!(diff("s", &live, &live.clone()), None);
        assert_eq!(
            diff("s", &live, &json!({"c": "x", "a": [1, {"b": 2.5}]})),
            None
        );
        assert_eq!(
            diff("s", &live, &json!({"a": [1, {"b": 2.0}], "c": "x"})).unwrap(),
            "s.a[1].b: live 2.5, shadow 2.0"
        );
        assert_eq!(
            diff("s", &live, &json!({"a": [1], "c": "x"})).unwrap(),
            "s.a: live 2 items, shadow 1"
        );
        assert_eq!(
            diff("s", &json!({"a": 1}), &json!({})).unwrap(),
            "s.a: live 1, shadow (absent)"
        );
        // 1 and 1.0 are one number in JSON.
        assert_eq!(diff("s", &json!(1), &json!(1.0)), None);
    }

    #[test]
    fn summary_skew_is_unaligned() {
        let pool = json!({"checkedAt": "2026-10-27T04:00:00.000Z", "accounts": []});
        let live = json!({"generatedAt": "2026-10-27T04:00:05.000Z", "checkedAt": "2026-10-27T03:58:00.000Z",
            "forecastGeneratedAt": null, "valueGeneratedAt": null});
        let c = check_summary(&live, Some(&pool), None, None, 0.0);
        assert_eq!(c.outcome, Outcome::Unaligned);
        assert!(c.detail.unwrap().contains("checkedAt"));
    }
}
