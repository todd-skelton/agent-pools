//! pool-usage.mjs: the pool-wide overlay on the Codex desktop's \`/wham/usage\` poll, so its banner
//! and usage panel show the pool rather than the one signed-in account. The upstream body stays
//! the base (account id, credits, plan), and the rate-limit blocks are the pool's.
//!
//! Accounts are read as the service holds them (JSON, after \`currentAccount\`), so the port reads
//! them as loosely as the JavaScript does. Held to every call in Agent Pools' pool-usage tests
//! (\`golden/usage-hook.mjs\`, \`tests/pool_usage.rs\`).
//!
//! Where the JavaScript would throw a \`TypeError\` (a \`null\` in \`additional_rate_limits\`, a window
//! with no \`id\`), the port skips the entry or reads the id as empty; the relay fails open on a
//! throw either way and passes the account's own numbers on.

use crate::{js, quota};
use serde_json::{json, Map, Value};

/// How the pool's remaining capacity maps onto the desktop's 0-100 meter.
pub const POOL_MODES: [&str; 3] = ["capped", "sum", "average"];

/// How long a failed poll may lean on the account's last successful read.
pub const UNVERIFIED_GRACE_MS: f64 = 30.0 * 60.0 * 1000.0;

fn finite(v: Option<&Value>) -> Option<f64> {
    js::finite(v)
}

fn used_percent(mode: &str, used: &[f64], total_remaining: f64) -> f64 {
    match mode {
        "capped" => js::max([0.0, 100.0 - total_remaining]),
        "sum" => 100.0 - total_remaining,
        _ => used.iter().fold(0.0, |s, v| s + v) / used.len() as f64,
    }
}

fn windows_of(a: &Value) -> &[Value] {
    match a.get("quotaWindows") {
        Some(Value::Array(w)) => w,
        _ => &[],
    }
}

fn has_rows(a: &Value) -> bool {
    !windows_of(a).is_empty()
}

fn status(a: &Value) -> Option<&str> {
    a.get("status").and_then(Value::as_str)
}

struct Pooled<'a> {
    with_rows: Vec<&'a Value>,
    verified: Vec<&'a Value>,
    unverified: usize,
}

fn pooled_accounts<'a>(accounts: &'a [Value], provider: &str, now: f64) -> Pooled<'a> {
    let pool: Vec<&Value> = accounts
        .iter()
        .filter(|a| a.get("provider").and_then(Value::as_str) == Some(provider) && !js::truthy(a.get("disabled")))
        .collect();
    let is_verified = |a: &Value| !js::truthy(a.get("usageStale")) && status(a) != Some("unknown") && has_rows(a);
    let verified: Vec<&Value> = pool.iter().copied().filter(|a| is_verified(a)).collect();
    let unverified: Vec<&Value> = pool.iter().copied().filter(|a| !is_verified(a) && within_grace(a, now)).collect();
    let with_rows = verified.iter().chain(unverified.iter()).copied().filter(|a| has_rows(a)).collect();
    Pooled { with_rows, verified, unverified: unverified.len() }
}

fn within_grace(account: &Value, now: f64) -> bool {
    let last = js::date_parse_value(account.get("usageUpdatedAt"));
    last.is_finite() && last <= now && now - last <= UNVERIFIED_GRACE_MS
}

/// One window across accounts, as aggregateWindow publishes it (with the fields only the
/// overlay itself reads: accounts, available, unverified, total_remaining_percent, exhausted).
#[derive(Clone)]
struct Agg {
    used_percent: f64,
    limit_window_seconds: Value,
    reset_after_seconds: Option<f64>,
    reset_at: Option<f64>,
    accounts: usize,
    available: usize,
    unverified: usize,
    total_remaining_percent: f64,
    exhausted: bool,
}

struct Row<'a> {
    w: &'a Value,
    verdict: Option<&'a str>,
}

fn aggregate_window(rows: &[Row], mode: &str, now: f64, unverified: usize) -> Option<Agg> {
    let metered: Vec<&Row> = rows.iter().filter(|r| finite(r.w.get("used")).is_some()).collect();
    if metered.is_empty() {
        return None;
    }
    let remaining: Vec<f64> = metered
        .iter()
        .map(|r| {
            let used = finite(r.w.get("used")).unwrap_or(f64::NAN);
            match r.verdict {
                Some("exhausted") => 0.0,
                Some("available") => js::max([1.0, 100.0 - used]),
                _ => js::max([0.0, 100.0 - used]),
            }
        })
        .collect();
    let used: Vec<f64> = remaining.iter().map(|v| 100.0 - v).collect();
    let total_remaining = remaining.iter().fold(0.0, |s, v| s + v);
    let pct = used_percent(mode, &used, total_remaining);
    let resets: Vec<f64> = metered.iter().filter_map(|r| finite(r.w.get("resetAt"))).filter(|t| *t > now).collect();
    let reset_at = if resets.is_empty() { None } else { Some(js::min(resets)) };
    // \`resetAt ? ... : null\`: a reset at 0 counts as none.
    let reset_at = reset_at.filter(|t| *t != 0.0);
    Some(Agg {
        used_percent: js::round(pct),
        limit_window_seconds: metered
            .iter()
            .find(|r| finite(r.w.get("durationSeconds")).is_some())
            .and_then(|r| r.w.get("durationSeconds").cloned())
            .unwrap_or(Value::Null),
        reset_after_seconds: reset_at.map(|t| js::max([0.0, js::round((t - now) / 1000.0)])),
        reset_at: reset_at.map(|t| js::round(t / 1000.0)),
        accounts: used.len(),
        available: remaining.iter().filter(|v| **v > 0.0).count(),
        unverified,
        total_remaining_percent: js::round(total_remaining),
        exhausted: total_remaining <= 0.0 && unverified == 0,
    })
}

struct Scope {
    scope: Value,
    common: bool,
    /// (slot key, window), in the order slots were first seen.
    windows: Vec<(Value, Agg)>,
}

/// A scope's slots and their rows, in the order first seen.
type Slots<'a> = Vec<(Value, Vec<Row<'a>>)>;

/// scopedWindows: grouped by scope, then by slot (primary/secondary by id, weekly/session by
/// duration, else the label or id), each in the order first seen, as a \`Map\` keeps them.
fn scoped_windows(accounts: &[&Value], mode: &str, now: f64, unverified: usize) -> Vec<Scope> {
    // (scope, common of its first window, [(slot, rows)])
    let mut by_scope: Vec<(Value, bool, Slots)> = Vec::new();
    for a in accounts {
        for w in windows_of(a) {
            let key = match w.get("scope") {
                None | Some(Value::Null) => json!(""),
                Some(v) => v.clone(),
            };
            let id = w.get("id").and_then(Value::as_str).unwrap_or("");
            let duration = w.get("durationSeconds").and_then(Value::as_f64);
            let slot = if id.ends_with(":secondary_window") {
                json!("secondary")
            } else if id.ends_with(":primary_window") {
                json!("primary")
            } else if duration == Some(604_800.0) {
                json!("weekly")
            } else if duration == Some(18_000.0) {
                json!("session")
            } else {
                match w.get("label") {
                    None | Some(Value::Null) => w.get("id").cloned().unwrap_or(Value::Null),
                    Some(label) => label.clone(),
                }
            };
            let i = match by_scope.iter().position(|(k, _, _)| *k == key) {
                Some(i) => i,
                None => {
                    by_scope.push((key, js::truthy(w.get("common")), Vec::new()));
                    by_scope.len() - 1
                }
            };
            let slots = &mut by_scope[i].2;
            let j = match slots.iter().position(|(k, _)| *k == slot) {
                Some(j) => j,
                None => {
                    slots.push((slot, Vec::new()));
                    slots.len() - 1
                }
            };
            let verdict = match status(a) {
                Some(s @ ("exhausted" | "available")) if js::truthy(w.get("common")) => Some(s),
                _ => None,
            };
            slots[j].1.push(Row { w, verdict });
        }
    }
    let mut out = Vec::new();
    for (scope, common, slots) in by_scope {
        let windows: Vec<(Value, Agg)> = slots
            .into_iter()
            .filter_map(|(slot, rows)| aggregate_window(&rows, mode, now, unverified).map(|w| (slot, w)))
            .collect();
        if !windows.is_empty() {
            out.push(Scope { scope, common, windows });
        }
    }
    out
}

/// accountHasCapacity: a verdict decides; without one, every metered common window below 100%.
fn account_has_capacity(a: &Value) -> bool {
    match status(a) {
        Some("exhausted") => return false,
        Some("available") => return true,
        _ => {}
    }
    let common: Vec<f64> = windows_of(a)
        .iter()
        .filter(|w| js::truthy(w.get("common")))
        .filter_map(|w| finite(w.get("used")))
        .collect();
    !common.is_empty() && common.iter().all(|u| *u < 100.0)
}

/// soonestCommonReset: each account recovers when its last blocking common window resets (or
/// at its soonest future common reset when none is at 100%); the pool with the earliest.
fn soonest_common_reset(accounts: &[&Value], now: f64) -> Option<f64> {
    let per_account: Vec<f64> = accounts
        .iter()
        .filter_map(|a| {
            let future: Vec<&Value> = windows_of(a)
                .iter()
                .filter(|w| js::truthy(w.get("common")) && finite(w.get("resetAt")).is_some_and(|r| r > now))
                .collect();
            let reset = |w: &Value| finite(w.get("resetAt")).unwrap_or(f64::NAN);
            let blocking: Vec<f64> = future
                .iter()
                .filter(|w| finite(w.get("used")).is_some_and(|u| u >= 100.0))
                .map(|w| reset(w))
                .collect();
            if !blocking.is_empty() {
                return Some(js::max(blocking));
            }
            if future.is_empty() {
                None
            } else {
                Some(js::min(future.iter().map(|w| reset(w))))
            }
        })
        .filter(|t| t.is_finite())
        .collect();
    if per_account.is_empty() {
        None
    } else {
        Some(js::min(per_account))
    }
}

fn opt_number(v: Option<f64>) -> Value {
    v.map_or(Value::Null, js::number_value)
}

fn strip(w: Option<&Agg>) -> Value {
    match w {
        None => Value::Null,
        Some(w) => json!({
            "used_percent": js::number_value(w.used_percent),
            "limit_window_seconds": w.limit_window_seconds,
            "reset_after_seconds": opt_number(w.reset_after_seconds),
            "reset_at": opt_number(w.reset_at),
        }),
    }
}

/// rateLimitBlock: the named primary/secondary slots, or the longest-lived windows when the
/// upstream naming is missing (Claude's buckets).
fn rate_limit_block(windows: &[(Value, Agg)], pool_exhausted: Option<bool>) -> (Map<String, Value>, Option<Agg>) {
    let mut ordered: Vec<&(Value, Agg)> = windows.iter().collect();
    let lws = |w: &Agg| w.limit_window_seconds.as_f64().unwrap_or(0.0);
    js::sort_by(&mut ordered, |a, b| lws(&b.1) - lws(&a.1));
    // Object.fromEntries: keys as strings, the last of equal keys wins.
    let named = |name: &str| windows.iter().rev().find(|(k, _)| js::to_string(Some(k)) == name).map(|(_, w)| w);
    let primary = named("primary").or_else(|| ordered.first().map(|(_, w)| w));
    let secondary = named("secondary").or_else(|| {
        if ordered.len() > 1 && named("primary").is_none() {
            Some(&ordered[1].1)
        } else {
            None
        }
    });
    let exhausted = pool_exhausted.unwrap_or_else(|| [primary, secondary].iter().any(|w| w.is_some_and(|w| w.exhausted)));
    let mut block = Map::new();
    block.insert("allowed".into(), json!(!exhausted));
    block.insert("limit_reached".into(), json!(exhausted));
    block.insert("primary_window".into(), strip(primary));
    block.insert("secondary_window".into(), strip(secondary));
    (block, primary.cloned())
}

/// \`String(s).toLowerCase().replace(/[^a-z0-9]+/g, "_").replace(/^_|_$/g, "")\`.
fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in s.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            gap = false;
        } else if !gap {
            out.push('_');
            gap = true;
        }
    }
    let out = out.strip_prefix('_').unwrap_or(&out);
    out.strip_suffix('_').unwrap_or(out).to_string()
}

/// poolLabel: \`Codex pool: 1 of 3 available, 99% of 300% left\`.
pub fn pool_label(scope: &str, accounts: usize, available: usize, unverified: usize, total_remaining_percent: f64) -> String {
    let note = if unverified > 0 { format!(", {unverified} unverified") } else { String::new() };
    format!(
        "{scope} pool: {available} of {accounts} available{note}, {}% of {}% left",
        js::number_to_string(total_remaining_percent),
        accounts * 100
    )
}

fn label_of(scope: &str, w: &Agg) -> String {
    pool_label(scope, w.accounts, w.available, w.unverified, w.total_remaining_percent)
}

/// withUpstreamEvidence: the signed-in account's fresh reading (the body the desktop just
/// fetched) stands as verified evidence for that pooled account.
fn with_upstream_evidence(accounts: &[Value], upstream: &Value, now: f64) -> Vec<Value> {
    let Some(id) = upstream.get("account_id").and_then(Value::as_str).filter(|id| !id.is_empty()) else {
        return accounts.to_vec();
    };
    accounts
        .iter()
        .map(|a| {
            if a.get("provider").and_then(Value::as_str) != Some("codex")
                || js::truthy(a.get("disabled"))
                || a.get("accountId").and_then(Value::as_str) != Some(id)
            {
                return a.clone();
            }
            let fresh = quota::normalize_usage("codex", upstream, now);
            let usable = fresh.status != "unknown" && fresh.quota_windows.iter().any(|w| w.common && w.used.is_some_and(f64::is_finite));
            if !usable {
                return a.clone();
            }
            let mut merged = match a {
                Value::Object(m) => m.clone(),
                _ => Map::new(),
            };
            if let Ok(Value::Object(f)) = serde_json::to_value(&fresh) {
                for (k, v) in f {
                    merged.insert(k, v);
                }
            }
            merged.insert("status".into(), json!(fresh.status));
            merged.insert("usageStale".into(), json!(false));
            merged.insert("usageUpdatedAt".into(), json!(js::iso(now)));
            merged.shift_remove("error");
            Value::Object(merged)
        })
        .collect()
}

fn set_resets(block: &mut Map<String, Value>, at: f64, now: f64) {
    for key in ["primary_window", "secondary_window"] {
        if let Some(Value::Object(w)) = block.get_mut(key) {
            w.insert("reset_at".into(), js::number_value(js::round(at / 1000.0)));
            w.insert("reset_after_seconds".into(), js::number_value(js::max([0.0, js::round((at - now) / 1000.0)])));
        }
    }
}

/// \`poolUsage(upstream, accounts, { mode, now, includeClaude })\`: the overlay, or \`upstream\`
/// unchanged (\`None\`) when the pool has no fresh Codex data. An unknown mode is an error.
pub fn pool_usage(upstream: &Value, accounts: &[Value], mode: &str, now: f64, include_claude: bool) -> Result<Option<Value>, String> {
    if !POOL_MODES.contains(&mode) {
        return Err(format!("Unknown pool mode: {mode}"));
    }
    let accounts = with_upstream_evidence(accounts, upstream, now);
    let codex = pooled_accounts(&accounts, "codex", now);
    if codex.verified.is_empty() {
        return Ok(None);
    }
    let scopes = scoped_windows(&codex.with_rows, mode, now, codex.unverified);
    let Some(common) = scopes.iter().find(|s| s.common) else {
        return Ok(None);
    };
    let pool_exhausted = codex.unverified == 0 && !codex.verified.iter().any(|a| account_has_capacity(a));
    let (mut rate_limit, primary) = rate_limit_block(&common.windows, Some(pool_exhausted));
    let primary = primary.expect("a scope has a window");
    if pool_exhausted {
        if let Some(at) = soonest_common_reset(&codex.verified, now) {
            set_resets(&mut rate_limit, at, now);
        }
    }
    let mut additional = vec![json!({
        "limit_name": label_of("Codex", &primary),
        "metered_feature": "codex_pool",
        "rate_limit": rate_limit.clone(),
        "normal_model_slug": null,
    })];
    let upstream_limits: &[Value] = match upstream.get("additional_rate_limits") {
        Some(Value::Array(a)) => a,
        _ => &[],
    };
    for s in scopes.iter().filter(|s| !s.common) {
        let (block, p) = rate_limit_block(&s.windows, None);
        let p = p.expect("a scope has a window");
        let entry = upstream_limits.iter().find(|e| e.get("limit_name") == Some(&s.scope));
        let field = |k: &str| entry.and_then(|e| e.get(k)).filter(|v| !v.is_null()).cloned();
        let scope = js::to_string(Some(&s.scope));
        additional.push(json!({
            "limit_name": label_of(&scope, &p),
            "metered_feature": field("metered_feature").unwrap_or_else(|| json!(format!("pool_{}", slug(&scope)))),
            "rate_limit": block,
            "normal_model_slug": field("normal_model_slug").unwrap_or(Value::Null),
        }));
    }
    if include_claude {
        let claude = pooled_accounts(&accounts, "claude", now);
        let scopes = if claude.verified.is_empty() { Vec::new() } else { scoped_windows(&claude.with_rows, mode, now, claude.unverified) };
        for s in &scopes {
            let exhausted = s.common.then(|| claude.unverified == 0 && !claude.verified.iter().any(|a| account_has_capacity(a)));
            let (mut block, p) = rate_limit_block(&s.windows, exhausted);
            let p = p.expect("a scope has a window");
            if exhausted == Some(true) {
                if let Some(at) = soonest_common_reset(&claude.verified, now) {
                    set_resets(&mut block, at, now);
                }
            }
            let scope = js::to_string(Some(&s.scope));
            let name = if s.scope == json!("All models") { "Claude".to_string() } else { format!("Claude {scope}") };
            additional.push(json!({
                "limit_name": label_of(&name, &p),
                "metered_feature": format!("pool_{}", slug(&name)),
                "rate_limit": block,
                "normal_model_slug": null,
            }));
        }
    }
    let exhausted = rate_limit.get("limit_reached") == Some(&Value::Bool(true));
    let reset_at = rate_limit.get("primary_window").and_then(|w| w.get("reset_at")).cloned().unwrap_or(Value::Null);
    let mut out = match upstream {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    out.insert("rate_limit".into(), Value::Object(rate_limit));
    out.insert("additional_rate_limits".into(), Value::Array(additional));
    out.insert(
        "rate_limit_reached_type".into(),
        if exhausted { json!({ "type": "rate_limit_reached", "details": "pool" }) } else { Value::Null },
    );
    out.insert(
        "rate_limit_upsell".into(),
        if exhausted {
            json!({
                "banner_type": "pro_rate_limit_reached",
                "title": format!("Codex pool exhausted ({} accounts)", primary.accounts),
                "description": "Every pooled account is at its limit. The soonest window resets on {time}.",
                "ctas": [],
                "reset_at": reset_at,
                "referral": null,
                "request_url": null,
            })
        } else {
            Value::Null
        },
    );
    let model_usage = if exhausted {
        upstream.get("model_usage").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!({}))
    } else {
        json!({})
    };
    out.insert("model_usage".into(), model_usage);
    out.insert(
        "pool".into(),
        json!({
            "mode": mode,
            "accounts": primary.accounts,
            "available": primary.available,
            "unverified": primary.unverified,
            "total_remaining_percent": js::number_value(primary.total_remaining_percent),
        }),
    );
    Ok(Some(Value::Object(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_the_regexes() {
        assert_eq!(slug("Claude Fable"), "claude_fable");
        assert_eq!(slug("GPT-5.3-Codex-Spark"), "gpt_5_3_codex_spark");
        assert_eq!(slug("--a  b--"), "a_b");
        assert_eq!(slug("\u{212A}elvin"), "kelvin");
        assert_eq!(slug(""), "");
        assert_eq!(slug("_"), "");
    }
}
