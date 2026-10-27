//! Transitions that raise an alert once and clear on positive evidence (notifications.mjs
//! `exhaustionTransition`, `reauthTransitions`, `creditRiskTransitions`), and the usage
//! samples the capacity forecast learns from (projection.mjs `usageSamples`).

use super::obj::{at, obj, str_at};
use crate::js;
use serde_json::{json, Map, Value};

/// `state[key]` as an object, replaced by `{}` when it is not a plain object.
fn table<'a>(state: &'a mut Value, key: &str) -> &'a mut Map<String, Value> {
    let t = obj(state).entry(key.to_string()).or_insert_with(|| json!({}));
    if !t.is_object() {
        *t = json!({});
    }
    obj(t)
}

/// `exhaustionTransition(state, accounts, { provider, resetExpected, now })`:
/// `"pool-exhausted"`, `"pool-recovered"` or nothing. Unknown accounts are never evidence.
pub fn exhaustion_transition(state: &mut Value, accounts: &[Value], provider: &str, reset_expected: bool, now: f64) -> Option<&'static str> {
    let pool: Vec<&Value> = accounts
        .iter()
        .filter(|a| str_at(a, "provider") == Some(provider) && !js::truthy(at(a, "disabled")))
        .collect();
    let exhausted = table(state, "exhausted");
    let all = !pool.is_empty() && pool.iter().all(|a| str_at(a, "status") == Some("exhausted"));
    if all && !reset_expected && !js::truthy(exhausted.get(provider)) {
        exhausted.insert(provider.into(), json!(js::iso(now)));
        return Some("pool-exhausted");
    }
    if pool.iter().any(|a| str_at(a, "status") == Some("available")) && js::truthy(exhausted.get(provider)) {
        exhausted.shift_remove(provider);
        return Some("pool-recovered");
    }
    None
}

/// One sign-in transition.
pub enum Reauth {
    Required(Value),
    Cleared { account_id: String, provider: Value, reason: &'static str },
}

/// `reauthTransitions(state, accounts)`: an account whose login the provider rejected alerts
/// once per episode; the episode ends on a verified read, removal or disable.
pub fn reauth_transitions(state: &mut Value, accounts: &[Value]) -> Vec<Reauth> {
    let reauth = table(state, "reauth");
    let mut out = Vec::new();
    for a in accounts {
        let code = js::get(at(a, "refreshError"), "code").and_then(Value::as_str);
        let id = js::to_string(at(a, "id"));
        if js::truthy(at(a, "disabled")) || code != Some("reauth_required") || js::truthy(reauth.get(&id)) {
            continue;
        }
        let since = js::coalesce([js::get(at(a, "refreshError"), "since")]).cloned().unwrap_or(Value::Null);
        let email = js::coalesce([at(a, "email")]).cloned().unwrap_or(json!(""));
        reauth.insert(id, json!({ "provider": at(a, "provider"), "email": email, "since": since }));
        out.push(Reauth::Required(a.clone()));
    }
    let ids: Vec<String> = reauth.keys().cloned().collect();
    for id in ids {
        // A Map keeps the last account with an id.
        let a = accounts.iter().rev().find(|a| js::to_string(at(a, "id")) == id);
        let reason = match a {
            None => Some("removed"),
            Some(a) if js::truthy(at(a, "disabled")) => Some("disabled"),
            Some(a)
                if js::get(at(a, "refreshError"), "code").and_then(Value::as_str) != Some("reauth_required")
                    && str_at(a, "status") != Some("unknown")
                    && !js::truthy(at(a, "usageStale")) =>
            {
                Some("recovered")
            }
            _ => None,
        };
        let Some(reason) = reason else { continue };
        let entry = reauth.shift_remove(&id).unwrap_or(Value::Null);
        out.push(Reauth::Cleared { account_id: id, provider: at(&entry, "provider").cloned().unwrap_or(Value::Null), reason });
    }
    out
}

/// One credit-risk transition.
pub enum CreditRisk {
    AtRisk { provider: String, credit: Value },
    Cleared { provider: Value, account_id: Value, expires_at: Value, reason: &'static str },
}

/// `creditRiskTransitions(state, forecast, { now, atRiskHours: 72 })`: a banked credit the
/// fresh forecast still loses under the credit plan, inside 72 hours, alerts once.
pub fn credit_risk_transitions(state: &mut Value, forecast: &Value, now: f64) -> Vec<CreditRisk> {
    const AT_RISK_MS: f64 = 72.0 * 3_600_000.0;
    let risk = table(state, "creditRisk");
    let mut out = Vec::new();
    let fresh = js::date_parse_value(at(forecast, "staleAfter")) > now;
    let mut judged: Vec<String> = Vec::new();
    let mut listed: Map<String, Value> = Map::new();
    let providers = if fresh { at(forecast, "providers") } else { None };
    for (provider, p) in js::entries(providers) {
        if !js::truthy(Some(p)) || str_at(p, "state") == Some("unknown") || js::truthy(at(p, "stale")) {
            continue;
        }
        let Some(Value::Array(credits)) = js::get(at(p, "creditPlan"), "credits") else { continue };
        judged.push(provider.clone());
        for c in credits {
            let expires = js::date_parse_value(at(c, "expiresAt"));
            if !at(c, "accountId").is_some_and(Value::is_string) || !expires.is_finite() {
                continue;
            }
            let key = format!("{}|{}", js::to_string(at(c, "accountId")), js::to_string(at(c, "expiresAt")));
            listed.insert(key.clone(), c.clone());
            if str_at(c, "outcome") != Some("lapses") || expires <= now || expires - now > AT_RISK_MS || js::truthy(risk.get(&key)) {
                continue;
            }
            risk.insert(key, json!({ "provider": provider, "accountId": at(c, "accountId"), "expiresAt": at(c, "expiresAt"), "since": js::iso(now) }));
            out.push(CreditRisk::AtRisk { provider: provider.clone(), credit: c.clone() });
        }
    }
    let keys: Vec<String> = risk.keys().cloned().collect();
    for key in keys {
        let entry = risk.get(&key).cloned().unwrap_or(Value::Null);
        let provider = js::get(Some(&entry), "provider");
        // `!(expiry > now)`: an unreadable expiry (NaN) has expired too.
        let expiry = js::date_parse_value(js::get(Some(&entry), "expiresAt"));
        let reason = if expiry.is_nan() || expiry <= now {
            Some("expired")
        } else if !provider.is_some_and(|p| judged.contains(&js::to_string(Some(p)))) {
            None
        } else if !listed.contains_key(&key) {
            Some("gone")
        } else if str_at(&listed[&key], "outcome") != Some("lapses") {
            Some("planned")
        } else {
            None
        };
        let Some(reason) = reason else { continue };
        risk.shift_remove(&key);
        let field = |k: &str| js::coalesce([js::get(Some(&entry), k)]).cloned().unwrap_or(Value::Null);
        out.push(CreditRisk::Cleared { provider: field("provider"), account_id: field("accountId"), expires_at: field("expiresAt"), reason });
    }
    out
}

/// Reset estimates jitter between reads; only a move past this counts as a reset.
pub const RESET_JITTER_MS: f64 = 3_600_000.0;

/// `usageSamples(previousAccounts, accounts, at)`: one sample per window whose use changed.
pub fn usage_samples(previous: &[Value], accounts: &[Value], at_ms: f64) -> Vec<Value> {
    let key = |a: &Value| format!("{}:{}", js::to_string(at(a, "provider")), js::to_string(at(a, "id")));
    let mut out = Vec::new();
    for a in accounts {
        if js::truthy(at(a, "disabled")) || str_at(a, "status") == Some("unknown") || js::truthy(at(a, "usageStale")) {
            continue;
        }
        let before = previous.iter().rev().find(|p| key(p) == key(a));
        for w in js::values(at(a, "quotaWindows")) {
            let Some(used) = js::finite(at(w, "used")) else { continue };
            let prior = before.and_then(|b| {
                js::values(at(b, "quotaWindows")).into_iter().find(|p| super::obj::strict_eq(at(p, "id"), at(w, "id")))
            });
            if let Some(prior) = prior {
                let (pr, wr) = (at(prior, "resetAt"), at(w, "resetAt"));
                let same_reset = super::obj::strict_eq(pr, wr)
                    || matches!((js::finite(pr), js::finite(wr)), (Some(x), Some(y)) if (x - y).abs() <= RESET_JITTER_MS);
                if super::obj::strict_eq(at(prior, "used"), at(w, "used")) && same_reset {
                    continue;
                }
            }
            let mut s = Map::new();
            s.insert("at".into(), json!(js::iso(at_ms)));
            for (k, v) in [("provider", at(a, "provider")), ("accountId", at(a, "id")), ("window", at(w, "id"))] {
                if let Some(v) = v {
                    s.insert(k.into(), v.clone());
                }
            }
            s.insert("used".into(), super::obj::num(used));
            if let Some(r) = at(w, "resetAt") {
                s.insert("resetAt".into(), r.clone());
            }
            s.insert("common".into(), json!(js::truthy(at(w, "common"))));
            out.push(Value::Object(s));
        }
    }
    out
}

/// `(x * 168).toFixed(1)` for a finite `x`, else `None` (`perWeek` in refresh.mjs).
pub fn per_week(x: Option<f64>) -> Option<String> {
    x.filter(|x| x.is_finite()).map(|x| to_fixed(x * 168.0, 1))
}

/// `Number.prototype.toFixed(digits)` for a finite number under 1e21: the exact binary value
/// rounded to `digits` places, an exact tie going to the larger magnitude.
pub fn to_fixed(x: f64, digits: usize) -> String {
    let exact = format!("{:.*}", digits + 40, x.abs());
    let (head, tail) = exact.split_at(exact.len() - 40);
    let mut digits_vec: Vec<u8> = head.bytes().collect();
    let first = tail.as_bytes()[0];
    if first >= b'5' {
        // `n / 10^f - x` is closest from above, or a tie (pick the larger n).
        let mut i = digits_vec.len();
        loop {
            if i == 0 {
                digits_vec.insert(0, b'1');
                break;
            }
            i -= 1;
            match digits_vec[i] {
                b'.' => continue,
                b'9' => digits_vec[i] = b'0',
                d => {
                    digits_vec[i] = d + 1;
                    break;
                }
            }
        }
    }
    let mut s = String::from_utf8(digits_vec).expect("ascii digits");
    if s.ends_with('.') {
        s.pop();
    }
    let zero = s.bytes().all(|b| b == b'0' || b == b'.');
    if x < 0.0 && !zero { format!("-{s}") } else { s }
}

/// `new Date(ms).toLocaleString("en-US", { weekday: "short", month: "short", day: "numeric",
/// hour: "numeric", minute: "2-digit" })` at a zone `offset_minutes` east of UTC.
pub fn local_when(ms: f64, offset_minutes: f64) -> String {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let l = ms + offset_minutes * 60_000.0;
    let days = (l / 86_400_000.0).floor();
    let (_, m, d) = js::civil_from_days(days as i64);
    let weekday = WEEKDAYS[((days as i64 + 4).rem_euclid(7)) as usize];
    let minute_of_day = ((l - days * 86_400_000.0) / 60_000.0).floor() as i64;
    let (h, min) = (minute_of_day / 60, minute_of_day % 60);
    let (h12, ampm) = match h {
        0 => (12, "AM"),
        1..=11 => (h, "AM"),
        12 => (12, "PM"),
        _ => (h - 12, "PM"),
    };
    format!("{weekday}, {} {d}, {h12}:{min:02} {ampm}", MONTHS[(m - 1) as usize])
}
