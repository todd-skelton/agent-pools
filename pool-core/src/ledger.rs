//! usage-ledger.mjs's rows: one compact line per proxied request, from a record of the proxy's
//! usage queue, keeping only what the value report needs (time, account, model, client, session,
//! effort, tier, outcome, tokens by type, and the quota the provider reported in that response's
//! headers). The record's API key, token hash, addresses, error body and every other header are
//! never copied. Held to `golden/outputs/ledger/cases.json` (`node golden/ledger-cases.mjs`,
//! Agent Pools' own functions over 230 records).

use crate::js;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

pub const LEDGER_DIR: &str = "logs/usage-ledger";

/// `ledgerConfig(input)`'s settings.
#[derive(Clone, Debug, PartialEq)]
pub struct LedgerConfig {
    pub enabled: bool,
    pub drain_interval_seconds: f64,
    pub raw_retention_days: f64,
    /// Rows kept in memory while the ledger cannot be written (already popped from the queue).
    pub max_pending_rows: usize,
}

impl Default for LedgerConfig {
    fn default() -> LedgerConfig {
        LedgerConfig { enabled: true, drain_interval_seconds: 10.0, raw_retention_days: 35.0, max_pending_rows: 100_000 }
    }
}

impl LedgerConfig {
    pub fn to_value(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "drainIntervalSeconds": js::number_value(self.drain_interval_seconds),
            "rawRetentionDays": js::number_value(self.raw_retention_days),
            "maxPendingRows": self.max_pending_rows,
        })
    }
}

/// `ledgerConfig(install.usageLedger)`: the settings, defaults where a value is out of range,
/// and what was wrong. `None` is the key missing.
pub fn ledger_config(input: Option<&Value>) -> (LedgerConfig, Vec<String>) {
    let mut config = LedgerConfig::default();
    let mut problems = Vec::new();
    let input = match input {
        None | Some(Value::Null) => return (config, problems),
        Some(Value::Object(m)) => m,
        Some(Value::Array(_)) => return (config, problems),
        Some(_) => return (config, vec!["usageLedger must be an object".into()]),
    };
    if let Some(v) = input.get("enabled") {
        match v {
            Value::Bool(b) => config.enabled = *b,
            _ => problems.push("usageLedger.enabled must be true or false".into()),
        }
    }
    for (key, min, max) in [("drainIntervalSeconds", 2.0, 50.0), ("rawRetentionDays", 1.0, 400.0)] {
        let Some(v) = input.get(key) else { continue };
        match v.as_f64().filter(|n| n.is_finite() && n.fract() == 0.0 && *n >= min && *n <= max) {
            Some(n) if key == "drainIntervalSeconds" => config.drain_interval_seconds = n,
            Some(n) => config.raw_retention_days = n,
            None => problems.push(format!("usageLedger.{key} must be an integer from {min} to {max}")),
        }
    }
    (config, problems)
}

/// JavaScript's `String.prototype.trim` (Unicode white space and the byte-order mark).
fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// `header(headers, name)`: the first header named so (any case); its first value when a list;
/// trimmed, and only when it is a string.
fn header<'a>(headers: Option<&'a Value>, name: &str) -> Option<&'a str> {
    let Some(Value::Object(m)) = headers else { return None };
    let (_, v) = m.iter().find(|(k, _)| k.to_lowercase() == name)?;
    let value = match v {
        Value::Array(a) => a.first()?,
        other => other,
    };
    value.as_str().map(trim)
}

/// `num(v)`: a finite number from header text, else `None` (also for missing or empty).
fn num(v: Option<&str>) -> Option<f64> {
    let v = v.filter(|s| !s.is_empty())?;
    Some(js::to_number(Some(&Value::String(v.to_string())))).filter(|n| n.is_finite())
}

/// `epochMs(v)`: epoch seconds to milliseconds, when positive.
fn epoch_ms(v: Option<&str>) -> Value {
    match num(v) {
        Some(n) if n > 0.0 => js::number_value(js::round(n * 1000.0)),
        _ => Value::Null,
    }
}

/// `quotaFromHeaders(provider, headers)`: the quota the provider reported with the response, as
/// used percentages and reset times, or `None`.
pub fn quota_from_headers(provider: &str, headers: Option<&Value>) -> Option<Value> {
    let mut out = Map::new();
    match provider {
        "claude" => {
            let weekly = num(header(headers, "anthropic-ratelimit-unified-7d-utilization"));
            let short = num(header(headers, "anthropic-ratelimit-unified-5h-utilization"));
            if let Some(w) = weekly {
                let reset = epoch_ms(header(headers, "anthropic-ratelimit-unified-7d-reset"));
                out.insert("weekly".into(), json!({ "used": js::number_value(js::round(w * 10000.0) / 100.0), "resetAt": reset }));
            }
            if let Some(s) = short {
                let reset = epoch_ms(header(headers, "anthropic-ratelimit-unified-5h-reset"));
                out.insert("short".into(), json!({ "used": js::number_value(js::round(s * 10000.0) / 100.0), "resetAt": reset }));
            }
        }
        "codex" => {
            for prefix in ["x-codex-primary", "x-codex-secondary"] {
                let used = num(header(headers, &format!("{prefix}-used-percent")));
                let minutes = num(header(headers, &format!("{prefix}-window-minutes")));
                let (Some(used), Some(minutes)) = (used, minutes) else { continue };
                if minutes == 0.0 {
                    continue;
                }
                let reading = json!({ "used": js::number_value(used), "resetAt": epoch_ms(header(headers, &format!("{prefix}-reset-at"))) });
                if minutes == 10080.0 {
                    out.insert("weekly".into(), reading);
                } else if minutes < 10080.0 {
                    out.insert("short".into(), reading);
                }
            }
        }
        _ => return None,
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

/// `int(v)`: a positive count, rounded; 0 for anything else.
fn int(v: Option<&Value>) -> f64 {
    let n = js::to_number(v);
    if n.is_finite() && n > 0.0 {
        js::round(n)
    } else {
        0.0
    }
}

/// `a || b` over values.
fn or<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    if js::truthy(a) {
        a
    } else {
        b
    }
}

/// `tokensOf(record)`: token counts by billing type. The proxy's v2 breakdown when it has
/// both halves; else the plain counts, where OpenAI's input includes the cached tokens.
pub fn tokens_of(record: &Value) -> Map<String, Value> {
    let r = Some(record);
    let b = js::get(r, "token_breakdown");
    let n = |f: f64| js::number_value(f);
    let mut out = Map::new();
    if js::truthy(b) && js::truthy(js::get(b, "input")) && js::truthy(js::get(b, "output")) {
        let (i, o) = (js::get(b, "input"), js::get(b, "output"));
        out.insert("in".into(), n(int(js::get(i, "uncached_tokens"))));
        out.insert("cr".into(), n(int(js::get(i, "cache_read_tokens"))));
        out.insert("cw".into(), n(int(js::get(i, "cache_write_tokens"))));
        out.insert("out".into(), n(int(js::get(o, "total_tokens"))));
        out.insert("rsn".into(), n(int(js::get(o, "reasoning_tokens"))));
        let q = match js::get(b, "quality") {
            None | Some(Value::Null) => "unknown".to_string(),
            q => js::to_string(q),
        };
        out.insert("q".into(), json!(q));
        return out;
    }
    let t = match js::get(r, "tokens") {
        None | Some(Value::Null) => None,
        t => t,
    };
    let cached = int(or(js::get(t, "cache_read_tokens"), js::get(t, "cached_tokens")));
    let openai = js::get(r, "provider").and_then(Value::as_str) == Some("codex");
    let input = int(js::get(t, "input_tokens")) - if openai { cached } else { 0.0 };
    out.insert("in".into(), n(js::max([0.0, input])));
    out.insert("cr".into(), n(cached));
    out.insert("cw".into(), n(int(js::get(t, "cache_creation_tokens"))));
    out.insert("out".into(), n(int(js::get(t, "output_tokens"))));
    out.insert("rsn".into(), n(int(js::get(t, "reasoning_tokens"))));
    out.insert("q".into(), json!("fallback"));
    out
}

/// `clientOf(userAgent)`: the product before the first `/`, at most 40 UTF-16 units.
pub fn client_of(user_agent: Option<&Value>) -> String {
    let text = match user_agent {
        None | Some(Value::Null) => String::new(),
        ua => js::to_string(ua),
    };
    let s = trim(&text);
    if s.is_empty() {
        return "unknown".into();
    }
    let name = trim(s.split('/').next().unwrap_or(""));
    let units: Vec<u16> = name.encode_utf16().take(40).collect();
    let short = String::from_utf16_lossy(&units);
    if short.is_empty() {
        "unknown".into()
    } else {
        short
    }
}

/// `v || null`: the value when truthy.
fn or_null(v: Option<&Value>) -> Value {
    if js::truthy(v) {
        v.cloned().unwrap_or(Value::Null)
    } else {
        Value::Null
    }
}

fn string_or(v: Option<&Value>, default: &str) -> String {
    match v {
        None | Some(Value::Null) => default.to_string(),
        v => js::to_string(v),
    }
}

/// `ledgerRow(raw, accountsByAuthIndex)`: a queue record (an object, or its JSON text) as a
/// ledger row, or `None` when it has no readable time or is not an object. `accounts` maps the
/// proxy's `auth_index` to the pool's account id.
pub fn ledger_row(raw: &Value, accounts: &HashMap<String, Value>) -> Option<Value> {
    let parsed;
    let r = match raw {
        Value::String(text) => {
            parsed = serde_json::from_str::<Value>(text).ok()?;
            &parsed
        }
        other => other,
    };
    if !matches!(r, Value::Object(_) | Value::Array(_)) {
        return None;
    }
    let rr = Some(r);
    let at = js::date_parse_value(js::get(rr, "timestamp"));
    if !at.is_finite() {
        return None;
    }
    let provider = string_or(js::get(rr, "provider"), "unknown");
    let auth_index = string_or(js::get(rr, "auth_index"), "");
    let tier = match or(or(js::get(rr, "response_service_tier"), js::get(rr, "service_tier")), None) {
        v if js::truthy(v) => js::to_string(v),
        _ => String::new(),
    };
    let failed = js::truthy(js::get(rr, "failed"));
    let status = js::to_number(js::get(js::get(rr, "fail"), "status_code"));
    let latency = js::to_number(js::get(rr, "latency_ms"));
    let mut row = Map::new();
    row.insert("at".into(), json!(js::iso(at)));
    row.insert("prov".into(), json!(provider));
    row.insert("model".into(), json!(string_or(js::get(rr, "model"), "unknown")));
    row.insert("acct".into(), accounts.get(&auth_index).cloned().unwrap_or(Value::Null));
    if !accounts.contains_key(&auth_index) {
        row.insert("ai".into(), if auth_index.is_empty() { Value::Null } else { json!(auth_index) });
    }
    row.insert("ep".into(), json!(string_or(js::get(rr, "endpoint"), "")));
    row.insert("client".into(), json!(client_of(js::get(rr, "user_agent"))));
    row.insert("sid".into(), or_null(js::get(rr, "session_id")));
    if js::truthy(js::get(rr, "parent_session_id")) {
        row.insert("psid".into(), or_null(js::get(rr, "parent_session_id")));
    }
    row.insert("eff".into(), or_null(js::get(rr, "reasoning_effort")));
    row.insert("tier".into(), if tier.is_empty() { Value::Null } else { json!(tier) });
    row.insert("ok".into(), json!(!failed));
    let st = if status.is_nan() || status == 0.0 { if failed { 500.0 } else { 200.0 } } else { status };
    row.insert("st".into(), js::number_value(st));
    row.insert("lat".into(), js::number_value(if latency.is_nan() || latency == 0.0 { 0.0 } else { latency }));
    row.extend(tokens_of(r));
    row.insert("rid".into(), or_null(js::get(rr, "request_id")));
    if let Some(quota) = quota_from_headers(&provider, js::get(rr, "response_headers")) {
        row.insert("quota".into(), quota);
    }
    Some(Value::Object(row))
}
