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

/// The models a quota clear must leave blocked: those a spent (or unread) model-specific
/// window covers, matched by the window's scope against the models the proxy routes for the
/// account. A spent model limit blocks only its own models, never the account. `None` when a
/// spent window names no routed model, or the adapter flagged a model limit the windows do
/// not show; the clear then waits, as it did for every model limit before.
pub fn recovery_keep(a: &Value, snapshot: Option<&Value>) -> Option<Vec<String>> {
    // Codex flags a model limit by its status as well as its windows, and a Codex usage-limit
    // 429 blocks the whole credential, so a Codex model limit keeps the account-wide wait.
    if str_at(a, "provider") != Some("claude") {
        return if js::truthy(at(a, "modelQuotaRecoveryBlocked")) { None } else { Some(Vec::new()) };
    }
    let mut scopes: Vec<String> = Vec::new();
    for w in js::values(at(a, "quotaWindows")) {
        if js::get(Some(w), "common") == Some(&Value::Bool(true)) {
            continue;
        }
        let used = js::number(js::get(Some(w), "used"));
        if used.is_some_and(|u| u.is_finite() && u < 100.0) {
            continue;
        }
        let scope = js::to_string(js::get(Some(w), "scope")).trim().to_lowercase().replace(' ', "-");
        if scope.is_empty() || scope == "undefined" || scope == "null" {
            return None;
        }
        scopes.push(scope);
    }
    if scopes.is_empty() {
        return if js::truthy(at(a, "modelQuotaRecoveryBlocked")) { None } else { Some(Vec::new()) };
    }
    let routing = snapshot.and_then(|s| at(s, "routing"));
    let routed: Vec<String> = ["models", "unlisted"]
        .iter()
        .flat_map(|k| js::entries(js::get(routing, k)))
        .map(|(model, _)| model)
        .collect();
    let mut keep: Vec<String> = Vec::new();
    for scope in &scopes {
        let covered: Vec<&String> = routed.iter().filter(|m| model_in_scope(m, scope)).collect();
        if covered.is_empty() {
            return None;
        }
        keep.extend(covered.into_iter().cloned());
    }
    keep.sort();
    keep.dedup();
    Some(keep)
}

fn model_in_scope(model: &str, scope: &str) -> bool {
    let m = model.to_lowercase();
    m == scope || m.starts_with(&format!("{scope}-")) || m.split(['-', '_', '.']).any(|t| t == scope)
}

/// `canRecoverQuota(account, snapshot, now)`: a fresh, available account the proxy still
/// blocks for quota, with a block long enough to be worth clearing on a model no spent model
/// limit covers ([`recovery_keep`]).
pub fn can_recover_quota(a: &Value, snapshot: Option<&Value>, now: f64) -> bool {
    let Some(provider) = str_at(a, "provider").filter(|p| crate::providers::provider(p).is_some()) else {
        return false;
    };
    let Some(snap) = snapshot else { return false };
    let Some(keep) = recovery_keep(a, snapshot) else { return false };
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
        && eq("id")
        && eq("auth_index")
        && eq("provider")
        && !js::truthy(at(snap, "disabled"))
        && js::get(routing, "quota_blocked") == Some(&Value::Bool(true))
        && js::get(routing, "generation").is_some_and(Value::is_string)
        && long_block(routing, now, adapters::min_recoverable_block_ms(provider), &keep)
}

/// A block worth clearing on a model the clear would free (not one in `keep`). A model the
/// proxy says is blocked for another reason than quota (`quota: false`: an unsupported model,
/// an auth error) is one the clear would not free; a proxy that does not say counts as quota.
fn long_block(routing: Option<&Value>, now: f64, min_ms: f64, keep: &[String]) -> bool {
    let freed: Vec<&Value> = ["models", "unlisted"]
        .iter()
        .flat_map(|k| js::entries(js::get(routing, k)))
        .filter(|(model, m)| !keep.contains(model) && at(m, "quota") != Some(&Value::Bool(false)))
        .map(|(_, m)| m)
        .collect();
    if !keep.is_empty() && !freed.iter().any(|m| str_at(m, "status") == Some("blocked")) {
        return false;
    }
    if min_ms <= 0.0 {
        return true;
    }
    freed.iter().any(|m| {
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
    let keep = recovery_keep(a, Some(&snapshot)).unwrap_or_default();
    if !keep.is_empty() {
        request.insert("keep_models".into(), json!(keep));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn account(fable_used: Value, flagged: bool) -> Value {
        json!({
            "provider": "claude",
            "modelQuotaRecoveryBlocked": flagged,
            "quotaWindows": [
                { "id": "limit:0", "scope": "All models", "used": 59, "common": true },
                { "id": "limit:1", "scope": "Fable", "used": fable_used, "common": false },
            ],
        })
    }

    fn snapshot(opus: &str) -> Value {
        let block = |status: &str| json!({ "status": status, "next_retry_after": "2026-11-25T13:00:04Z" });
        json!({ "routing": { "quota_blocked": true, "models": {
            "claude-fable-5-2": block("blocked"),
            "claude-fable-5-1": block("blocked"),
            "claude-opus-5-5": block(opus),
            "claude-haiku-5-5": block(opus),
        } } })
    }

    #[test]
    fn a_spent_model_limit_keeps_only_its_models() {
        let keep = recovery_keep(&account(json!(100), true), Some(&snapshot("blocked")));
        assert_eq!(keep, Some(vec!["claude-fable-5-1".to_string(), "claude-fable-5-2".to_string()]));
    }

    #[test]
    fn an_unread_model_limit_is_treated_as_spent() {
        let keep = recovery_keep(&account(Value::Null, true), Some(&snapshot("blocked")));
        assert_eq!(keep.map(|k| k.len()), Some(2));
    }

    #[test]
    fn no_spent_model_limit_keeps_nothing() {
        assert_eq!(recovery_keep(&account(json!(40), false), Some(&snapshot("blocked"))), Some(Vec::new()));
    }

    #[test]
    fn a_spent_limit_that_names_no_routed_model_waits() {
        let mut a = account(json!(100), true);
        a["quotaWindows"][1]["scope"] = json!("Mystery");
        assert_eq!(recovery_keep(&a, Some(&snapshot("blocked"))), None);
    }

    #[test]
    fn a_codex_model_limit_keeps_the_account_wide_wait() {
        let mut a = account(json!(100), true);
        a["provider"] = json!("codex");
        assert_eq!(recovery_keep(&a, Some(&snapshot("blocked"))), None);
        a["modelQuotaRecoveryBlocked"] = json!(false);
        assert_eq!(recovery_keep(&a, Some(&snapshot("blocked"))), Some(Vec::new()));
    }

    #[test]
    fn a_flag_the_windows_do_not_explain_waits() {
        assert_eq!(recovery_keep(&account(json!(40), true), Some(&snapshot("blocked"))), None);
    }

    #[test]
    fn only_blocks_on_freed_models_are_worth_clearing() {
        let now = js::date_parse("2026-11-23T04:00:00Z").unwrap();
        let keep = vec!["claude-fable-5-1".to_string(), "claude-fable-5-2".to_string()];
        let blocked = snapshot("blocked");
        assert!(long_block(at(&blocked, "routing"), now, 600_000.0, &keep));
        let fable_only = snapshot("ready");
        assert!(!long_block(at(&fable_only, "routing"), now, 600_000.0, &keep));
        assert!(!long_block(at(&fable_only, "routing"), now, 0.0, &keep));
    }

    #[test]
    fn a_freed_model_blocked_for_another_reason_is_not_worth_clearing() {
        let now = js::date_parse("2026-11-23T04:00:00Z").unwrap();
        let keep = vec!["claude-fable-5-1".to_string(), "claude-fable-5-2".to_string()];
        let mut other = snapshot("ready");
        other["routing"]["models"]["claude-opus-5-5"] =
            json!({ "status": "blocked", "next_retry_after": "2026-11-23T16:00:00Z", "quota": false });
        assert!(!long_block(at(&other, "routing"), now, 600_000.0, &keep));
        assert!(!long_block(at(&other, "routing"), now, 0.0, &keep));
        other["routing"]["models"]["claude-opus-5-5"]["quota"] = json!(true);
        assert!(long_block(at(&other, "routing"), now, 600_000.0, &keep));
    }
}
