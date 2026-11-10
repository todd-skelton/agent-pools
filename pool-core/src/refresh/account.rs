//! One account through a cycle: usage (usage.mjs `refreshUsage`), invalidation
//! (freshness.mjs `invalidateAccount`), the proxy's routing view and quota recovery
//! (quota-recovery.mjs `applyRouting`, `canRecoverQuota`, `reconcileQuota`).

use super::adapters;
use super::obj::{at, del, is_integer, jsify, put, str_at};
use super::{Cycle, Failure, Io};
use crate::js;
use crate::quota::normalize_usage;
use crate::summary::is_fresh;
use serde_json::{json, Map, Value};

/// A failed transport read is retried once after this pause.
pub const READ_RETRY_DELAY_MS: f64 = 2000.0;

/// `isTransientReadError(error)`: worth re-reading soon (a proxy 5xx, a timeout, a socket
/// error); provider verdicts (401, 403, 429) and unreadable bodies are not.
pub fn is_transient(e: &Failure) -> bool {
    if e.syntax {
        return false;
    }
    if let Some(status) = e.status {
        return (500..=599).contains(&status);
    }
    if matches!(e.name.as_deref(), Some("TimeoutError" | "AbortError")) {
        return true;
    }
    let m = e.message.to_ascii_lowercase();
    let http5 = m.match_indices("returned http 5").any(|(i, _)| {
        let rest = &m.as_bytes()[i + "returned http 5".len()..];
        rest.len() >= 2 && rest[0].is_ascii_digit() && rest[1].is_ascii_digit()
    });
    http5 || ["fetch failed", "econn", "etimedout", "eai_again", "socket"].iter().any(|s| m.contains(s))
}

/// `invalidateAccount(account)`: usage, routing and credits marked unknown, display rows kept.
pub fn invalidate(a: &mut Value) {
    let disabled = js::truthy(at(a, "disabled"));
    if !disabled {
        put(a, "status", json!("unknown"));
    }
    let stale = js::truthy(at(a, "usageUpdatedAt"));
    put(a, "usageStale", json!(stale));
    put(a, "routingStatus", json!(if disabled { "disabled" } else { "unknown" }));
    put(a, "routingModels", json!({}));
    put(a, "creditsKnown", json!(false));
    for k in ["remaining", "nextReset", "rankReset", "rankRemaining"] {
        del(a, k);
    }
}

/// `refreshUsage(account, previous, read)`: the account's usage, or `None` when the read
/// failed, in which case the last known rows of the same account are kept for display and
/// nothing about routing or credits is reused.
pub async fn refresh_usage<I: Io>(cycle: &Cycle<'_, I>, a: &mut Value, previous: Option<&Value>, file: &Value) -> Option<Value> {
    let provider = str_at(a, "provider").unwrap_or_default().to_string();
    put(a, "usageCheckStartedAt", json!(cycle.iso()));
    let url = adapters::usage_url(&provider);
    let read = async {
        match cycle.read(file, url, None, Map::new()).await {
            Err(e) if is_transient(&e) => {
                cycle.io.sleep(READ_RETRY_DELAY_MS).await;
                cycle.read(file, url, None, Map::new()).await
            }
            r => r,
        }
    };
    let result = match read.await {
        Ok(usage) => {
            let now = cycle.now();
            let normalized = normalize_usage(&provider, &usage, now);
            let usable = normalized.status != "unknown"
                && normalized.quota_windows.iter().any(|w| w.common && w.used.is_some_and(f64::is_finite));
            if usable { Ok((usage, normalized)) } else { Err(Failure::new("Account provider returned an unusable usage payload")) }
        }
        Err(e) => Err(e),
    };
    match result {
        Ok((usage, normalized)) => {
            for k in ["remaining", "nextReset", "rankReset", "rankRemaining"] {
                del(a, k);
            }
            if let Value::Object(fields) = jsify(serde_json::to_value(&normalized).unwrap_or(Value::Null)) {
                for (k, v) in fields {
                    put(a, &k, v);
                }
            }
            let blocked = adapters::model_recovery_blocked(&provider, &usage, &normalized, cycle.now());
            put(a, "modelQuotaRecoveryBlocked", json!(blocked));
            put(a, "usageUpdatedAt", json!(cycle.iso()));
            put(a, "usageStale", json!(false));
            del(a, "error");
            del(a, "errorTransient");
            Some(usage)
        }
        Err(e) => {
            let same = previous.is_some_and(|p| {
                super::obj::strict_eq(at(p, "id"), at(a, "id")) && super::obj::strict_eq(at(p, "provider"), at(a, "provider"))
            });
            let rows = if same {
                js::coalesce([previous.and_then(|p| at(p, "quotaWindows"))]).cloned().unwrap_or(json!([]))
            } else {
                json!([])
            };
            let has_rows = rows.as_array().is_some_and(|r| !r.is_empty());
            put(a, "quotaWindows", rows);
            let updated = if same {
                js::coalesce([previous.and_then(|p| at(p, "usageUpdatedAt"))]).cloned().unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            put(a, "usageUpdatedAt", updated);
            put(a, "usageStale", json!(has_rows));
            put(a, "error", json!(e.message));
            put(a, "errorTransient", json!(is_transient(&e)));
            invalidate(a);
            put(a, "status", json!("unknown"));
            for k in ["windows", "remaining", "nextReset", "rankReset", "rankRemaining"] {
                del(a, k);
            }
            put(a, "creditsKnown", json!(false));
            put(a, "creditCount", json!(0));
            put(a, "credits", json!([]));
            super::adapters::put_model_bank(a, json!([]), json!(0));
            None
        }
    }
}

/// `applyRouting(account, snapshot)`: what the proxy says about routing to the account.
pub fn apply_routing(a: &mut Value, snapshot: Option<&Value>) {
    let routing = snapshot.and_then(|s| at(s, "routing"));
    let field = |k: &str| js::coalesce([js::get(routing, k)]).cloned();
    put(a, "routingStatus", field("status").unwrap_or(json!("unknown")));
    put(a, "routingCheckedAt", field("checked_at").unwrap_or(Value::Null));
    put(a, "routingModels", field("models").unwrap_or(json!({})));
    let list = snapshot.and_then(|s| at(s, "live_models"));
    if let Some(list) = list.filter(|l| js::truthy(Some(l)) && js::is_object(Some(l))) {
        let text = |k: &str| js::coalesce([at(list, k)]).cloned().unwrap_or(Value::Null);
        let models = at(list, "models").filter(|m| is_integer(Some(m))).cloned().unwrap_or(Value::Null);
        let error = at(list, "error").filter(|e| e.as_str().is_some_and(|s| !s.is_empty())).cloned().unwrap_or(Value::Null);
        put(
            a,
            "modelList",
            json!({
                "source": if str_at(list, "source") == Some("live") { "live" } else { "pending" },
                "fetchedAt": text("fetched_at"),
                "checkedAt": text("checked_at"),
                "models": models,
                "error": error,
            }),
        );
    }
}

/// `canRecoverQuota(account, snapshot, now)`: a fresh, available account the proxy still
/// blocks for quota, with no model limit out and a block long enough to be worth clearing.
pub fn can_recover_quota(a: &Value, snapshot: Option<&Value>, now: f64) -> bool {
    let Some(provider) = str_at(a, "provider").filter(|p| crate::providers::provider(p).is_some()) else {
        return false;
    };
    let Some(snap) = snapshot else { return false };
    let started = js::date_parse_value(at(a, "usageCheckStartedAt"));
    let routing = at(snap, "routing");
    let eq = |k: &str| super::obj::strict_eq(at(snap, k), at(a, k));
    str_at(a, "status") == Some("available")
        && !js::truthy(at(a, "disabled"))
        && !js::truthy(at(a, "error"))
        && !js::truthy(at(a, "usageStale"))
        && is_fresh(at(a, "usageUpdatedAt"), now)
        && started.is_finite()
        && started <= now
        && now - started <= 60_000.0
        && !js::truthy(at(a, "modelQuotaRecoveryBlocked"))
        && eq("id")
        && eq("auth_index")
        && eq("provider")
        && !js::truthy(at(snap, "disabled"))
        && js::get(routing, "quota_blocked") == Some(&Value::Bool(true))
        && js::get(routing, "generation").is_some_and(Value::is_string)
        && long_block(routing, now, adapters::min_recoverable_block_ms(provider))
}

fn long_block(routing: Option<&Value>, now: f64, min_ms: f64) -> bool {
    if min_ms <= 0.0 {
        return true;
    }
    ["models", "unlisted"].iter().flat_map(|k| js::values(js::get(routing, k))).any(|m| {
        str_at(m, "status") == Some("blocked") && js::date_parse_value(at(m, "next_retry_after")) - now > min_ms
    })
}

/// `reconcileQuota(account, ...)`: apply the proxy's routing view and, when the account
/// qualifies, clear its quota block. True when the proxy changed something.
pub async fn reconcile_quota<I: Io>(cycle: &Cycle<'_, I>, a: &mut Value) -> Result<bool, Failure> {
    let snapshot = cycle.proxy_snapshot(a).await?;
    apply_routing(a, snapshot.as_ref());
    if !can_recover_quota(a, snapshot.as_ref(), cycle.now()) {
        return Ok(false);
    }
    let snapshot = snapshot.unwrap_or(Value::Null);
    let mut request = Map::new();
    for (k, v) in [
        ("auth_index", at(a, "auth_index")),
        ("generation", js::get(at(&snapshot, "routing"), "generation")),
        ("observed_after", at(a, "usageCheckStartedAt")),
    ] {
        if let Some(v) = v {
            request.insert(k.into(), v.clone());
        }
    }
    let result = match cycle.io.management("recover-quota", Some(&Value::Object(request)), None).await {
        Ok(r) => r,
        Err(e) if e.status == Some(409) => {
            let again = cycle.proxy_snapshot(a).await?;
            apply_routing(a, again.as_ref());
            return Ok(false);
        }
        Err(e) => return Err(e),
    };
    apply_routing(a, Some(&json!({ "routing": at(&result, "routing").cloned() })));
    let changed = js::truthy(at(&result, "changed"));
    if changed {
        put(a, "quotaRecoveredAt", json!(cycle.iso()));
    }
    Ok(changed)
}
