//! `GET /api/status` (pool-service.mjs): the pool state with each account as it reads now,
//! credit ids, auth indexes and account ids left out, and the settings and forecast beside it.
//!
//! `processes`, `holds` and `usageLedger` live only in the service's memory (its children,
//! the proxy's hold reading, the ledger writer's counters); a caller that has them passes them
//! in [`Live`], and a caller that does not gets `null` for each.

use crate::{js, providers, resets::ResetPolicy, summary};
use serde_json::{json, Map, Value};

/// What only the running service knows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Live {
    pub processes: Option<Value>,
    pub holds: Option<Value>,
    pub usage_ledger: Option<Value>,
}

/// The keys of `/api/status` that come from [`Live`].
pub const LIVE_KEYS: [&str; 3] = ["processes", "holds", "usageLedger"];

/// One account as `/api/status` shows it:
/// `({ credits, auth_index, accountId, ...a }) => ({ ...a, nextCreditExpiry, creditExpiries })`
/// over `currentAccount(a)`.
pub fn status_account(account: &Value, now: f64) -> Value {
    let current = summary::current_account(account, now);
    let credits = current.get("credits").cloned();
    let mut m = match current {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    for k in ["credits", "auth_index", "accountId"] {
        m.shift_remove(k);
    }
    let list: &[Value] = match &credits {
        Some(Value::Array(a)) => a,
        _ => &[],
    };
    // credits?.[0]?.expires_at ?? null
    let first = list.first().and_then(|c| js::get(Some(c), "expires_at"));
    m.insert(
        "nextCreditExpiry".into(),
        first
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or(Value::Null),
    );
    // (credits ?? []).map((c) => c?.expires_at ?? c?.expiresAt ?? null).filter((x) => typeof x === "string")
    let expiries: Vec<Value> = list
        .iter()
        .filter_map(|c| {
            let c = Some(c);
            js::coalesce([js::get(c, "expires_at"), js::get(c, "expiresAt")])
                .filter(|v| v.is_string())
                .cloned()
        })
        .collect();
    m.insert("creditExpiries".into(), Value::Array(expiries));
    Value::Object(m)
}

/// `/api/status` from the saved pool state, the install's `expectedAccounts`, the reset policies
/// and the published forecast.
pub fn build_status(
    state: &Value,
    expected: Option<&Value>,
    policies: &[ResetPolicy],
    capacity: Option<&Value>,
    live: &Live,
    now: f64,
) -> Value {
    let accounts: Vec<Value> = match state.get("accounts") {
        Some(Value::Array(a)) => a.iter().map(|a| status_account(a, now)).collect(),
        _ => Vec::new(),
    };
    // `{ ...state, accounts }`: each saved field once, the accounts (never copied as saved) in
    // their place, or last when the state has none.
    let mut accounts = Some(Value::Array(accounts));
    let mut m: Map<String, Value> = match state {
        Value::Object(fields) => fields
            .iter()
            .map(|(k, v)| match k.as_str() {
                "accounts" => (k.clone(), accounts.take().unwrap_or(Value::Null)),
                _ => (k.clone(), v.clone()),
            })
            .collect(),
        _ => Map::new(),
    };
    if let Some(a) = accounts {
        m.insert("accounts".into(), a);
    }
    m.insert(
        "processes".into(),
        live.processes.clone().unwrap_or(Value::Null),
    );
    m.insert("holds".into(), live.holds.clone().unwrap_or(Value::Null));
    // JSON.stringify leaves out a key whose value is undefined.
    match expected {
        Some(v) => m.insert("expected".into(), v.clone()),
        None => m.shift_remove("expected"),
    };
    let mut policy = Map::new();
    for p in policies {
        policy.insert(p.provider.clone(), p.to_value());
    }
    m.insert("resetPolicy".into(), Value::Object(policy));
    let mut names = Map::new();
    for p in providers::PROVIDERS.iter() {
        names.insert(
            p.id.into(),
            json!({ "name": p.name, "loginId": p.login_id }),
        );
    }
    m.insert("providers".into(), Value::Object(names));
    m.insert("capacity".into(), capacity.cloned().unwrap_or(Value::Null));
    m.insert(
        "usageLedger".into(),
        live.usage_ledger.clone().unwrap_or(Value::Null),
    );
    Value::Object(m)
}
