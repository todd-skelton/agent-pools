//! The banked-reset lifecycle (reset-lifecycle.mjs): refusal backoff, redemptions made outside
//! Agent Pools, whether automation is about to act, and `processReset`, which journals a claim
//! before it is sent and never replaces an uncertain claim's identity.

use super::adapters::{request_id, TERMINAL};
use super::obj::{at, del, is_integer, put, spread, str_at};
use super::{CyclePolicy, Failure, Io};
use crate::js;
use crate::routing::{account_out, choose_reset, credit_key, Account};
use crate::summary::current_account;
use serde_json::{json, Map, Value};
use std::collections::HashSet;

/// `RESET_BACKOFF_MS`: outcomes that used nothing, and how long that credit is passed over.
pub fn backoff_ms(outcome: &str) -> Option<f64> {
    match outcome {
        "no_credit" | "nothing_to_reset" | "rejected" => Some(30.0 * 60_000.0),
        "deferred" => Some(5.0 * 60_000.0),
        _ => None,
    }
}

/// `activeRefusals(slot, now)`: refusals still inside their backoff.
pub fn active_refusals(slot: Option<&Value>, now: f64) -> Map<String, Value> {
    js::entries(slot.and_then(|s| at(s, "refused")))
        .into_iter()
        .filter(|(_, r)| {
            let wait = js::get(Some(r), "outcome").and_then(Value::as_str).and_then(backoff_ms).unwrap_or(0.0);
            now - js::date_parse_value(js::get(Some(r), "at")) < wait
        })
        .map(|(k, r)| (k, r.clone()))
        .collect()
}

const WEEK_SECONDS: f64 = 604_800.0;
const OWN_CLAIM_MS: f64 = 6.0 * 3_600_000.0;

/// `externalRedemptions(...)`: credits redeemed outside Agent Pools between two verified reads
/// of an account (a smaller known bank and a weekly window that fell 20 points before its reset).
pub fn external_redemptions(previous: &[Value], accounts: &[Value], resets: Option<&Value>, since: f64, now: f64) -> Vec<Value> {
    fn weekly(a: &Value) -> Option<&Value> {
        js::values(at(a, "windows")).into_iter().find(|w| {
            js::truthy(at(w, "common")) && js::number(at(w, "durationSeconds")) == Some(WEEK_SECONDS)
        })
    }
    let verified = |a: &Value| {
        !js::truthy(at(a, "disabled"))
            && at(a, "creditsKnown") == Some(&Value::Bool(true))
            && is_integer(at(a, "creditCount"))
            && str_at(a, "status") != Some("unknown")
            && !js::truthy(at(a, "usageStale"))
    };
    let key = |a: &Value| format!("{}:{}", js::to_string(at(a, "provider")), js::to_string(at(a, "id")));
    let mut out = Vec::new();
    for a in accounts {
        // A Map keeps the last account with a key.
        let Some(p) = previous.iter().rev().find(|p| key(p) == key(a)) else { continue };
        let count = |x: &Value| js::to_number(at(x, "creditCount"));
        if !verified(p) || !verified(a) || count(a) >= count(p) {
            continue;
        }
        let (Some(w0), Some(w1)) = (weekly(p), weekly(a)) else { continue };
        let (Some(u0), Some(u1)) = (js::finite(at(w0, "used")), js::finite(at(w1, "used"))) else { continue };
        if u0 - u1 < 20.0 || !js::finite(at(w0, "resetAt")).is_some_and(|r| r > now) {
            continue;
        }
        let slot = resets.and_then(|r| js::get(Some(r), &js::to_string(at(a, "provider"))));
        let pending = slot.and_then(|s| at(s, "pending"));
        if super::obj::strict_eq(pending.and_then(|p| at(p, "accountId")), at(a, "id")) {
            continue;
        }
        let last = slot.and_then(|s| at(s, "last"));
        let last_at = js::date_parse_value(last.and_then(|l| at(l, "at")));
        if super::obj::strict_eq(last.and_then(|l| at(l, "accountId")), at(a, "id"))
            && last_at.is_finite()
            && (!since.is_finite() || last_at >= since.min(now - OWN_CLAIM_MS))
        {
            continue;
        }
        out.push(json!({
            "provider": at(a, "provider"), "accountId": at(a, "id"),
            "creditsBefore": at(p, "creditCount"), "creditsAfter": at(a, "creditCount"),
            "weeklyUsedBefore": at(w0, "used"), "weeklyUsedAfter": at(w1, "used"),
        }));
    }
    out
}

/// The routing view of accounts for `chooseReset` and `accountOut`.
fn routing_accounts(accounts: &[Value]) -> Vec<Account> {
    accounts.iter().map(Account::from_value).collect()
}

/// `resetExpected(state, policy, accounts, now)`: whether automation is about to act.
pub fn reset_expected(state: &Value, policy: Option<&CyclePolicy>, accounts: &[Value], now: f64) -> bool {
    let Some(policy) = policy.filter(|p| p.reset.enabled) else { return false };
    if !js::truthy(at(state, "resetAutomation")) {
        return false;
    }
    let slot = js::get(at(state, "resets"), &policy.reset.provider);
    if let Some(pending) = slot.and_then(|s| at(s, "pending")).filter(|p| js::truthy(Some(p))) {
        return str_at(pending, "phase") != Some("attention");
    }
    let skip: HashSet<String> = active_refusals(slot, now).keys().cloned().collect();
    choose_reset(&routing_accounts(accounts), &policy.routing(), now, &skip, None).is_some()
}

/// What `processReset` needs from the cycle: saving, logging, the credit planner and the
/// redemption call.
pub trait Lifecycle {
    type Io: Io;
    fn io(&self) -> &Self::Io;
    /// `plan({ accounts, policy, now })`: accounts allowed to redeem before a lapse; `None` when
    /// no planner is wired.
    fn plan(&self, accounts: &[Value], policy: &CyclePolicy, now: f64) -> Option<Result<Option<HashSet<String>>, Failure>>;
    /// The claim itself (`consume`).
    fn consume(&self, account: &Value, pending: &Value) -> impl std::future::Future<Output = Result<Value, Failure>> + Send;
}

/// `processReset(state, { policy, ... })`: one provider's reset slot, one step. Returns the
/// outcome code when a claim was answered.
pub async fn process_reset<L: Lifecycle + Sync>(state: &mut Value, policy: &CyclePolicy, cx: &L) -> Result<Option<String>, Failure> {
    let io = cx.io();
    let provider = policy.reset.provider.clone();
    let now = io.now();
    let iso = js::iso(now);
    let resets = state.as_object_mut().map(|s| s.entry("resets").or_insert_with(|| json!({}))).expect("state is an object");
    if resets.is_null() {
        *resets = json!({});
    }
    let slot_exists = js::truthy(at(resets, &provider));
    if !slot_exists {
        put(resets, &provider, json!({ "pending": null, "last": null }));
    }
    let refused = active_refusals(at(resets, &provider), now);
    put(slot_mut(state, &provider), "refused", Value::Object(refused.clone()));
    let automation = js::truthy(at(state, "resetAutomation")) && policy.reset.enabled;
    let accounts: Vec<Value> = js::values(at(state, "accounts")).into_iter().map(|a| current_account(a, now)).collect();
    let mut pending = slot(state, &provider).and_then(|s| at(s, "pending")).filter(|p| js::truthy(Some(p))).cloned();
    if let Some(p) = &pending {
        if !js::truthy(at(p, "phase")) {
            pending = Some(spread(p, vec![("phase", Some(json!("uncertain")))]));
        }
    }
    let mut before_lapse = None;
    if policy.reset.redeem_before_lapse {
        match cx.plan(&accounts, policy, now) {
            Some(Ok(set)) => before_lapse = set,
            Some(Err(e)) => io.event("credit-plan-error", json!({ "provider": provider, "message": e.message })).await?,
            None => {}
        }
    }
    let skip: HashSet<String> = refused.keys().cloned().collect();
    let routing = routing_accounts(&accounts);
    let p = policy.routing();
    let selected = choose_reset(&routing, &p, now, &skip, before_lapse.as_ref());
    let finish = |state: &mut Value, outcome: &str| {
        let s = slot_mut(state, &provider);
        let current = at(s, "pending").cloned().unwrap_or(Value::Null);
        let last = spread(&current, vec![("provider", Some(json!(provider))), ("at", Some(json!(iso))), ("outcome", Some(json!(outcome)))]);
        put(s, "last", last);
        if backoff_ms(outcome).is_some() {
            let k = credit_key(&js::to_string(at(&current, "accountId")), at(&current, "creditId"));
            let refused = s.as_object_mut().unwrap().entry("refused").or_insert_with(|| json!({}));
            put(refused, &k, json!({ "outcome": outcome, "at": iso }));
        }
        put(s, "pending", Value::Null);
    };
    if let Some(p) = pending.clone() {
        let mismatch = |sel: &crate::routing::ResetChoice| {
            !super::obj::strict_eq(Some(&json!(sel.account_id)), at(&p, "accountId"))
                || !super::obj::strict_eq(Some(&sel.credit_id), at(&p, "creditId"))
        };
        if str_at(&p, "phase") == Some("unsent") && (!automation || selected.as_ref().is_none_or(mismatch)) {
            put(slot_mut(state, &provider), "pending", p);
            finish(state, "cancelled-before-send");
            io.save(state).await?;
            pending = None;
        }
    }
    let mut pending = match pending {
        Some(p) => p,
        None => {
            let Some(sel) = selected.as_ref().filter(|_| automation) else { return Ok(None) };
            let a = accounts.iter().find(|a| at(a, "id") == Some(&json!(sel.account_id)));
            let mut p = super::obj::jsify(serde_json::to_value(sel).unwrap_or(Value::Null));
            put(&mut p, "phase", json!("unsent"));
            put(&mut p, "requestId", request_id(io, a.unwrap_or(&Value::Null), Some(&sel.credit_id)));
            if sel.trigger.is_some() {
                put(&mut p, "creditCountAtClaim", a.and_then(|a| at(a, "creditCount")).cloned().unwrap_or(Value::Null));
            }
            p
        }
    };
    let account = accounts.iter().find(|a| super::obj::strict_eq(at(a, "id"), at(&pending, "accountId"))).cloned();
    put(slot_mut(state, &provider), "pending", pending.clone());
    if !js::truthy(at(&pending, "requestId")) {
        let p = spread(&pending, vec![
            ("phase", Some(json!("attention"))),
            ("reason", Some(json!("Missing original redemption identity; automatic retry is unsafe"))),
        ]);
        put(slot_mut(state, &provider), "pending", p);
        io.save(state).await?;
        return Ok(None);
    }
    let Some(account) = account.filter(|a| !js::truthy(at(a, "disabled"))) else {
        if str_at(&pending, "phase") == Some("unsent") {
            finish(state, "cancelled-before-send");
        } else {
            let p = spread(&pending, vec![
                ("phase", Some(json!("attention"))),
                ("reason", Some(json!("Account missing or disabled; redemption outcome needs reconciliation"))),
            ]);
            put(slot_mut(state, &provider), "pending", p);
        }
        io.save(state).await?;
        return Ok(None);
    };
    let roster: Vec<&Account> = routing.iter().filter(|a| a.provider.as_deref() == Some(provider.as_str()) && !a.disabled).collect();
    let all_out = (roster.len() as f64) >= p.expected_count && roster.iter().all(|a| account_out(a, &p.trigger, p.headroom_percent));
    if str_at(&pending, "trigger") == Some("before-lapse")
        && str_at(&pending, "phase") != Some("unsent")
        && !all_out
        && !account_out(&Account::from_value(&account), "weekly", p.headroom_percent)
    {
        let at_claim = at(&pending, "creditCountAtClaim");
        if js::truthy(at(&account, "creditsKnown"))
            && is_integer(at_claim)
            && is_integer(at(&account, "creditCount"))
            && js::to_number(at(&account, "creditCount")) < js::to_number(at_claim)
        {
            put(slot_mut(state, &provider), "pending", pending.clone());
            finish(state, "already_redeemed");
            io.event("reset-result", json!({
                "provider": provider, "accountId": at(&pending, "accountId"), "creditId": at(&pending, "creditId"),
                "requestId": at(&pending, "requestId"), "outcome": "already_redeemed", "trigger": at(&pending, "trigger"),
                "reconciled": true,
            })).await?;
        } else if str_at(&pending, "phase") != Some("attention") {
            let p = spread(&pending, vec![
                ("phase", Some(json!("attention"))),
                ("reason", Some(json!("Early redemption outcome unknown and the account has refilled; resent with its original id only once the account is out again"))),
            ]);
            put(slot_mut(state, &provider), "pending", p);
        }
        io.save(state).await?;
        return Ok(None);
    }
    let first = js::coalesce([at(&pending, "firstAttemptAt")]).cloned().unwrap_or(json!(iso));
    pending = spread(&pending, vec![("phase", Some(json!("uncertain"))), ("reason", None), ("firstAttemptAt", Some(first))]);
    put(slot_mut(state, &provider), "pending", pending.clone());
    io.save(state).await?;
    let mut attempt = Map::new();
    attempt.insert("provider".into(), json!(provider));
    for k in ["accountId", "creditId", "requestId"] {
        if let Some(v) = at(&pending, k) {
            attempt.insert(k.into(), v.clone());
        }
    }
    let trigger = at(&pending, "trigger").filter(|t| js::truthy(Some(t))).cloned();
    if let Some(t) = &trigger {
        attempt.insert("trigger".into(), t.clone());
    }
    io.event("reset-attempt", Value::Object(attempt.clone())).await?;
    let response = cx.consume(&account, &pending).await?;
    let code = str_at(&response, "code").filter(|c| TERMINAL.contains(c)).map(str::to_string);
    let Some(code) = code else {
        return Err(Failure::new("Reset outcome unknown; original retry identity retained"));
    };
    let mut result = attempt;
    result.shift_remove("trigger");
    result.insert("outcome".into(), json!(code));
    if let Some(t) = trigger {
        result.insert("trigger".into(), t);
    }
    io.event("reset-result", Value::Object(result)).await?;
    finish(state, &code);
    io.save(state).await?;
    Ok(Some(code))
}

fn slot<'a>(state: &'a Value, provider: &str) -> Option<&'a Value> {
    js::get(at(state, "resets"), provider)
}

fn slot_mut<'a>(state: &'a mut Value, provider: &str) -> &'a mut Value {
    let resets = super::obj::obj(state).entry("resets").or_insert_with(|| json!({}));
    super::obj::obj(resets).entry(provider.to_string()).or_insert_with(|| json!({ "pending": null, "last": null }))
}

/// `delete state.resets[provider].error`.
pub fn clear_error(state: &mut Value, provider: &str) {
    if let Some(s) = super::obj::obj(state).get_mut("resets").and_then(|r| r.get_mut(provider)) {
        del(s, "error");
    }
}

/// `(state.resets[provider] ??= { pending: null, last: null }).error = { at, message }`.
pub fn record_error(state: &mut Value, provider: &str, at_iso: &str, message: &str) {
    let resets = super::obj::obj(state).entry("resets").or_insert_with(|| json!({}));
    if resets.is_null() {
        *resets = json!({});
    }
    let s = super::obj::obj(resets).entry(provider.to_string()).or_insert(Value::Null);
    if s.is_null() {
        *s = json!({ "pending": null, "last": null });
    }
    put(s, "error", json!({ "at": at_iso, "message": message }));
}
