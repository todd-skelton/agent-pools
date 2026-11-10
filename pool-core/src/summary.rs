//! `GET /api/summary` (pool-summary.mjs `buildSummary`, `agent-pools-summary/v1`): per provider,
//! what the pool holds, what it costs, how much is left and when it runs short; per account,
//! plan, price, windows and banked credits. Also freshness.mjs `currentAccount`, which the
//! service applies to every account before it is summarized.

use crate::js;
use serde_json::{json, Map, Value};

pub const SUMMARY_FORMAT: &str = "agent-pools-summary/v1";
pub const STATUS_MAX_AGE_MS: f64 = 5.0 * 60.0 * 1000.0;

const NOTES: [&str; 7] = [
    "Routing: the pool picks among its own accounts; forecast.state and the published balance are signals the model-routing skill and dispatch-lane.ps1 read, not limits the pool enforces.",
    "Scenarios: naturalOnly = window resets alone; bankedCredits = banked credits spent the way the pool spends them (the headline); weeklyGrant = bankedCredits plus an assumed new credit each week, which providers do not promise.",
    "pace: how fast to use the provider to get the most out of it. Units are weighted account-weeks per hour (x168 = account-weeks per week). Objective: total usage over the run (work done plus quota still available at the end); running short loses nothing, quota is lost only when a window resets unused or a credit lapses. maxUnits = usage when draining as fast as possible; atDemand = usage at the observed pace (lostUnits = what it leaves unused); fullUse = lowest steady pace within 1% of maxUnits (null: only as fast as possible gets there), then = the lower floor after its last credit is spent; noGap = highest pace that never runs out and what that costs; steps[k-1] = pace that uses k credits; curve = usage at a ladder of paces. Reused for up to 10 minutes (computedAt); a signal for dispatchers, not enforced.",
    "creditPlan (refill providers): per banked credit, what automation does and whether it gets used. action: redeem-now (the account is out and redeeming now beats waiting for every account to run out), drain-first (routing sends new sessions to that account before its weekly reset order so it reaches its limit before drainBy, the credit expiry; a refill only applies at the limit), wait (acting early would not add usage). Each action is taken only when it is never worse at the low, measured and planning paces and better at one. outcome = the credit's fate at the measured pace under the plan (used, lapses, banked); planned false = too little demand evidence or automation off, nothing is acted on; needUnitsPerHour = lowest steady pace that uses it (x168 = account-weeks per week), null when none does. AgentPools applies these itself; a lapsing outcome means more work on that provider is the only remedy.",
    "Model limits: a cap sits inside the all-models quota; a reserve adds capacity outside it. Kind and share are measured or configured, and stay unknown without evidence.",
    "priceSource: provider = the provider's own billing data; list = public list price for the plan tier the provider reports; config = install.json usageLedger.planPrices.",
    "value: apiEquivalentUsd = the window's requests at API list prices; actualUsd = what the subscriptions cost over the same windowDays, each account's plan price spread over the requests it served by fitted quota use (idleUsd = plan cost of accounts that served none); per-request figures divide by the requests they cover.",
];

// --- freshness -------------------------------------------------------------------------

/// `isFresh(timestamp, now)`: read within the last five minutes, and not in the future.
pub fn is_fresh(timestamp: Option<&Value>, now: f64) -> bool {
    fresh_at(js::date_parse_value(timestamp), now)
}

/// The same, for a reading already parsed to milliseconds.
pub fn fresh_at(observed: f64, now: f64) -> bool {
    observed.is_finite() && observed <= now && now - observed <= STATUS_MAX_AGE_MS
}

/// The readings [`current_account`] judges by `now`, in the order it reads them. Two moments
/// at which each is equally fresh give the same account.
pub const READINGS: [&str; 3] = ["usageUpdatedAt", "routingCheckedAt", "creditsUpdatedAt"];

/// `currentAccount(account, now)`: a copy with stale usage, routing and credits marked unknown.
pub fn current_account(account: &Value, now: f64) -> Value {
    let mut copy = match account {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    let get = |c: &Map<String, Value>, k: &str| c.get(k).cloned();
    let disabled = |c: &Map<String, Value>| js::truthy(c.get("disabled"));
    if !is_fresh(copy.get("usageUpdatedAt"), now) || js::truthy(copy.get("usageStale")) {
        // invalidateAccount
        if !disabled(&copy) {
            copy.insert("status".into(), json!("unknown"));
        }
        let had_update = js::truthy(get(&copy, "usageUpdatedAt").as_ref());
        copy.insert("usageStale".into(), json!(had_update));
        let routing = if disabled(&copy) {
            "disabled"
        } else {
            "unknown"
        };
        copy.insert("routingStatus".into(), json!(routing));
        copy.insert("routingModels".into(), json!({}));
        copy.insert("creditsKnown".into(), json!(false));
        for k in ["remaining", "nextReset", "rankReset", "rankRemaining"] {
            copy.shift_remove(k);
        }
    }
    if !is_fresh(copy.get("routingCheckedAt"), now) {
        let routing = if disabled(&copy) {
            "disabled"
        } else {
            "unknown"
        };
        copy.insert("routingStatus".into(), json!(routing));
        copy.insert("routingModels".into(), json!({}));
    }
    if !is_fresh(copy.get("creditsUpdatedAt"), now) {
        copy.insert("creditsKnown".into(), json!(false));
    }
    Value::Object(copy)
}

// --- summary ---------------------------------------------------------------------------

/// pool-summary.mjs `iso(t)`: a time in milliseconds as ISO text, a string as it is, else null.
fn iso(t: Option<&Value>) -> Value {
    match t {
        Some(Value::Number(n)) if n.as_f64().is_some_and(f64::is_finite) => {
            json!(js::iso(n.as_f64().unwrap()))
        }
        Some(Value::String(s)) => json!(s),
        _ => Value::Null,
    }
}

/// `x ?? null`.
fn or_null(v: Option<&Value>) -> Value {
    js::coalesce([v]).cloned().unwrap_or(Value::Null)
}

/// `a === b` for values read from JSON (`None` is undefined).
fn strict_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(Value::Null), Some(Value::Null)) => true,
        (Some(Value::Bool(x)), Some(Value::Bool(y))) => x == y,
        (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
        (Some(Value::String(x)), Some(Value::String(y))) => x == y,
        _ => false,
    }
}

/// `round(v, 2)`.
fn round2(v: f64) -> Value {
    if v.is_finite() {
        js::number_value(js::round(v * 100.0) / 100.0)
    } else {
        Value::Null
    }
}

/// Insert unless undefined (`JSON.stringify` drops undefined members).
fn put(m: &mut Map<String, Value>, k: &str, v: Option<&Value>) {
    if let Some(v) = v {
        m.insert(k.into(), v.clone());
    }
}

/// Array.prototype.sort() with no comparator over values read from JSON: by their string
/// form, undefined last.
fn default_sort(mut xs: Vec<Option<Value>>) -> Vec<Option<Value>> {
    xs.sort_by(|a, b| match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, _) => std::cmp::Ordering::Greater,
        (_, None) => std::cmp::Ordering::Less,
        (Some(x), Some(y)) => js::to_string(Some(x)).cmp(&js::to_string(Some(y))),
    });
    xs
}

/// `summaryAccount(a, valueRow)`.
pub fn summary_account(a: &Value, value_row: Option<&Value>) -> Value {
    let a = Some(a);
    let g = |k: &str| js::get(a, k);
    let windows: Vec<Value> = match g("quotaWindows") {
        Some(Value::Array(ws)) => ws
            .iter()
            .map(|w| {
                let w = Some(w);
                let mut m = Map::new();
                put(&mut m, "id", js::get(w, "id"));
                m.insert("scope".into(), or_null(js::get(w, "scope")));
                m.insert("label".into(), or_null(js::get(w, "label")));
                m.insert(
                    "usedPercent".into(),
                    js::finite(js::get(w, "used")).map_or(Value::Null, js::number_value),
                );
                m.insert("resetAt".into(), iso(js::get(w, "resetAt")));
                m.insert(
                    "limitsAllModels".into(),
                    json!(js::get(w, "common") != Some(&Value::Bool(false))),
                );
                Value::Object(m)
            })
            .collect(),
        _ => Vec::new(),
    };
    let mut expiries: Vec<String> = match g("credits") {
        Some(Value::Array(cs)) => cs
            .iter()
            .filter_map(|c| {
                let c = Some(c);
                js::coalesce([js::get(c, "expires_at"), js::get(c, "expiresAt")])
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect(),
        _ => Vec::new(),
    };
    expiries.sort();
    let plan = g("plan");
    let price = js::coalesce([js::get(plan, "price")]);
    let status = if js::truthy(g("disabled")) {
        json!("disabled")
    } else if js::truthy(g("refreshError")) {
        json!("reauth-required")
    } else if js::truthy(g("usageStale")) || strict_eq(g("status"), Some(&json!("unknown"))) {
        json!("unverified")
    } else {
        or_null(g("status"))
    };
    let routable = !js::truthy(g("disabled"))
        && strict_eq(g("status"), Some(&json!("available")))
        && strict_eq(g("routingStatus"), Some(&json!("ready")));
    let plan_value = if js::truthy(plan) {
        let mut m = Map::new();
        put(&mut m, "plan", js::get(plan, "plan"));
        for k in ["label", "status", "renewsAt", "readAt"] {
            m.insert(k.into(), or_null(js::get(plan, k)));
        }
        Value::Object(m)
    } else {
        Value::Null
    };
    let value_price = js::coalesce([js::get(value_row, "planUsdPerMonth")]);
    let price_source: Option<Value> = if js::truthy(price) {
        js::get(price, "source").cloned()
    } else if value_price.is_some() {
        Some(or_null(js::get(value_row, "planPriceSource")).pipe_or(json!("config")))
    } else {
        Some(Value::Null)
    };
    let credit_count = js::finite(g("creditCount"))
        .filter(|n| n.fract() == 0.0)
        .map_or(json!(expiries.len()), |_| {
            g("creditCount").cloned().unwrap()
        });
    let mut m = Map::new();
    put(&mut m, "id", g("id"));
    put(&mut m, "provider", g("provider"));
    m.insert(
        "email".into(),
        if js::truthy(g("email")) {
            g("email").cloned().unwrap()
        } else {
            Value::Null
        },
    );
    m.insert("routable".into(), json!(routable));
    m.insert("status".into(), status);
    m.insert("usageUpdatedAt".into(), or_null(g("usageUpdatedAt")));
    m.insert("plan".into(), plan_value);
    m.insert(
        "priceUsdPerMonth".into(),
        js::coalesce([js::get(price, "usdPerMonth"), value_price])
            .cloned()
            .unwrap_or(Value::Null),
    );
    if let Some(s) = price_source {
        m.insert("priceSource".into(), s);
    }
    m.insert("windows".into(), Value::Array(windows));
    let mut credits = json!({
        "known": g("creditsKnown") == Some(&Value::Bool(true)),
        "count": credit_count,
        "expiries": expiries,
    });
    // Grants that reset only a model's cap (not counted above): listed only where held.
    let model_listed = js::values(g("modelCredits"));
    let model_count = js::finite(g("modelCreditCount")).filter(|n| n.fract() == 0.0);
    if model_count.is_some_and(|n| n > 0.0) || !model_listed.is_empty() {
        let mut model_expiries: Vec<String> = model_listed
            .iter()
            .filter_map(|c| js::get(Some(c), "expires_at").and_then(Value::as_str).map(str::to_string))
            .collect();
        model_expiries.sort();
        let clears: Vec<Value> = model_listed.iter().flat_map(|c| js::values(js::get(Some(c), "clears"))).cloned().collect();
        credits["modelCount"] = model_count.map_or(json!(model_listed.len()), js::number_value);
        credits["modelExpiries"] = json!(model_expiries);
        credits["modelClears"] = Value::Array(clears);
    }
    m.insert("credits".into(), credits);
    m.insert(
        "value".into(),
        if value_row.is_some() {
            json!({
                "apiEquivalentUsd": or_null(js::get(value_row, "apiEquivalentUsd")),
                "valueMultiple": or_null(js::get(value_row, "valueMultiple")),
                "requests": or_null(js::get(value_row, "requests")),
                "actualUsd": or_null(js::get(value_row, "actualUsd")),
                "actualUsdPerRequest": or_null(js::get(value_row, "actualUsdPerRequest")),
            })
        } else {
            Value::Null
        },
    );
    Value::Object(m)
}

trait PipeOr {
    fn pipe_or(self, d: Value) -> Value;
}

impl PipeOr for Value {
    /// `x ?? d` on an already-read value.
    fn pipe_or(self, d: Value) -> Value {
        if self.is_null() {
            d
        } else {
            self
        }
    }
}

fn scenario_view(s: Option<&Value>) -> Value {
    if !js::truthy(s) {
        return Value::Null;
    }
    let mut m = Map::new();
    m.insert("firstShortAt".into(), or_null(js::get(s, "firstOutageAt")));
    m.insert("shortHours".into(), or_null(js::get(s, "outageHours")));
    m.insert("creditsUsed".into(), or_null(js::get(s, "creditsUsed")));
    m.insert(
        "creditsLapsing".into(),
        or_null(js::get(s, "creditsLapsing")),
    );
    if js::truthy(js::get(s, "assumption")) {
        put(&mut m, "assumption", js::get(s, "assumption"));
    }
    Value::Object(m)
}

/// `buildSummary({ state, providers, forecast, value, now })`. `providers` is provider id and
/// display name, in `PROVIDER_IDS` order; `state.accounts` should already have been through
/// [`current_account`].
pub fn build_summary(
    state: &Value,
    providers: &[(&str, &str)],
    forecast: Option<&Value>,
    value: Option<&Value>,
    now: f64,
) -> Value {
    let state_accounts: &[Value] = match js::get(Some(state), "accounts") {
        Some(Value::Array(a)) => a,
        _ => &[],
    };
    let value_rows: &[Value] = match js::get(value, "accounts") {
        Some(Value::Array(a)) => a,
        _ => &[],
    };
    // new Map(rows.map((r) => [r.id, r])): the last row for an id wins.
    let value_by_id = |id: Option<&Value>| {
        value_rows
            .iter()
            .rev()
            .find(|r| strict_eq(js::get(Some(r), "id"), id))
    };
    let accounts: Vec<Value> = state_accounts
        .iter()
        .map(|a| summary_account(a, value_by_id(js::get(Some(a), "id"))))
        .collect();
    let mut out = Map::new();
    for (id, name) in providers {
        let mine: Vec<&Value> = accounts
            .iter()
            .filter(|a| strict_eq(js::get(Some(a), "provider"), Some(&json!(id))))
            .collect();
        if mine.is_empty() {
            continue;
        }
        let f = js::coalesce([js::get(js::get(forecast, "providers"), id)]);
        let price_of = |a: &Value| js::finite(js::get(Some(a), "priceUsdPerMonth"));
        let priced: Vec<&Value> = mine
            .iter()
            .copied()
            .filter(|a| price_of(a).is_some())
            .collect();
        let s = js::coalesce([js::get(f, "scenarios")]);
        let mut sources: Vec<Option<Value>> = Vec::new();
        for a in &priced {
            let src = js::get(Some(a), "priceSource").cloned();
            if !sources.iter().any(|x| match (x, &src) {
                (None, None) => true,
                (Some(x), Some(y)) => strict_eq(Some(x), Some(y)),
                _ => false,
            }) {
                sources.push(src);
            }
        }
        let sources: Vec<Value> = default_sort(sources)
            .into_iter()
            .map(|x| x.unwrap_or(Value::Null))
            .collect();
        let count_of = |a: &Value| js::to_number(js::get(js::get(Some(a), "credits"), "count"));
        let mut all_expiries: Vec<String> = mine
            .iter()
            .flat_map(|a| {
                js::get(js::get(Some(a), "credits"), "expiries")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect();
        all_expiries.sort();
        let forecast_block = if js::truthy(f) {
            let credit_plan = js::get(f, "creditPlan");
            let plan_value = if strict_eq(
                js::get(credit_plan, "method"),
                Some(&json!("counterfactual")),
            ) {
                let credits: Vec<Value> = js::get(credit_plan, "credits")
                    .and_then(Value::as_array)
                    .map(|cs| {
                        cs.iter()
                            .map(|c| {
                                let mut m = Map::new();
                                for k in [
                                    "accountId",
                                    "expiresAt",
                                    "resetAt",
                                    "out",
                                    "unitsLeft",
                                    "action",
                                    "drainBy",
                                    "outcome",
                                    "needUnitsPerHour",
                                ] {
                                    put(&mut m, k, js::get(Some(c), k));
                                }
                                Value::Object(m)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                json!({
                    "planned": js::get(credit_plan, "planned") == Some(&Value::Bool(true)),
                    "redeemNow": js::coalesce([js::get(credit_plan, "holders")]).cloned().unwrap_or(json!([])),
                    "drainFirst": js::coalesce([js::get(credit_plan, "drainFirst")]).cloned().unwrap_or(json!([])),
                    "credits": credits,
                })
            } else {
                Value::Null
            };
            let model_windows: &[Value] = match js::get(f, "modelWindows") {
                Some(Value::Array(ws)) => ws,
                _ => &[],
            };
            let value_limits: &[Value] = match js::get(value, "modelLimits") {
                Some(Value::Array(ls)) => ls,
                _ => &[],
            };
            let limits: Vec<Value> = model_windows
                .iter()
                .map(|w| {
                    let w = Some(w);
                    let measured = value_limits.iter().find(|m| {
                        let m = Some(*m);
                        strict_eq(js::get(m, "provider"), Some(&json!(id)))
                            && strict_eq(js::get(m, "window"), js::get(w, "window"))
                            && strict_eq(js::get(m, "scope"), js::get(w, "scope"))
                    });
                    let mv = |k: &str| js::get(measured, k);
                    let window_accounts: Vec<Value> = state_accounts
                        .iter()
                        .filter(|a| strict_eq(js::get(Some(a), "provider"), Some(&json!(id))))
                        .flat_map(|a| {
                            let qs: &[Value] = match js::get(Some(a), "quotaWindows") {
                                Some(Value::Array(q)) => q,
                                _ => &[],
                            };
                            qs.iter()
                                .filter(|q| {
                                    let q = Some(*q);
                                    js::get(q, "common") == Some(&Value::Bool(false))
                                        && strict_eq(js::get(q, "id"), js::get(w, "window"))
                                        && strict_eq(js::get(q, "scope"), js::get(w, "scope"))
                                })
                                .map(|q| {
                                    let mut m = Map::new();
                                    put(&mut m, "id", js::get(Some(a), "id"));
                                    m.insert(
                                        "used".into(),
                                        js::finite(js::get(Some(q), "used"))
                                            .map_or(Value::Null, js::number_value),
                                    );
                                    m.insert("resetAt".into(), iso(js::get(Some(q), "resetAt")));
                                    Value::Object(m)
                                })
                                .collect::<Vec<_>>()
                        })
                        .collect();
                    let mut m = Map::new();
                    m.insert(
                        "scope".into(),
                        js::coalesce([js::get(w, "scope"), js::get(w, "window")])
                            .cloned()
                            .unwrap_or(Value::Null),
                    );
                    put(&mut m, "state", js::get(w, "state"));
                    m.insert(
                        "firstShortAt".into(),
                        or_null(js::get(js::get(w, "projection"), "firstOutageAt")),
                    );
                    put(&mut m, "window", js::get(w, "window"));
                    m.insert(
                        "kind".into(),
                        js::coalesce([mv("kind")])
                            .cloned()
                            .unwrap_or(json!("unknown")),
                    );
                    m.insert("shareOfCommon".into(), or_null(mv("shareOfCommon")));
                    m.insert("shareBasis".into(), or_null(mv("shareBasis")));
                    m.insert(
                        "source".into(),
                        js::coalesce([mv("source")])
                            .cloned()
                            .unwrap_or(json!("unknown")),
                    );
                    m.insert(
                        "models".into(),
                        js::coalesce([mv("models")]).cloned().unwrap_or(json!([])),
                    );
                    m.insert("accounts".into(), Value::Array(window_accounts));
                    Value::Object(m)
                })
                .collect();
            let mut fm = Map::new();
            put(&mut fm, "state", js::get(f, "state"));
            fm.insert("stale".into(), json!(js::truthy(js::get(f, "stale"))));
            fm.insert(
                "demandUnitsPerHour".into(),
                or_null(js::get(js::get(f, "demand"), "unitsPerHour")),
            );
            fm.insert(
                "remainingUnits".into(),
                or_null(js::get(js::get(f, "supply"), "remainingUnits")),
            );
            fm.insert(
                "nextResetAt".into(),
                or_null(js::get(js::get(f, "supply"), "nextRefillAt")),
            );
            fm.insert(
                "horizonHours".into(),
                js::coalesce([
                    js::get(s, "horizonHours"),
                    js::get(js::get(f, "projection"), "horizonHours"),
                ])
                .cloned()
                .unwrap_or(Value::Null),
            );
            fm.insert(
                "naturalOnly".into(),
                scenario_view(js::get(s, "naturalOnly")),
            );
            fm.insert(
                "bankedCredits".into(),
                scenario_view(js::get(s, "bankedCredits")),
            );
            fm.insert(
                "weeklyGrant".into(),
                scenario_view(js::get(s, "weeklyGrant")),
            );
            fm.insert("pace".into(), or_null(js::get(f, "pace")));
            fm.insert("creditPlan".into(), plan_value);
            fm.insert("modelLimits".into(), Value::Array(limits));
            Value::Object(fm)
        } else {
            Value::Null
        };
        let value_block = if js::truthy(value) {
            let usd = mine.iter().fold(0.0, |acc, a| {
                acc + js::to_number(Some(
                    &js::coalesce([js::get(js::get(Some(a), "value"), "apiEquivalentUsd")])
                        .cloned()
                        .unwrap_or(json!(0)),
                ))
            });
            let cost = js::get(js::get(value, "actualCost"), id);
            json!({
                "apiEquivalentUsd": round2(usd),
                "apiEquivalentUsdPerRequest": or_null(js::get(cost, "apiEquivalentUsdPerRequest")),
                "actualUsd": or_null(js::get(cost, "actualUsd")),
                "actualUsdPerRequest": or_null(js::get(cost, "actualUsdPerRequest")),
                "idleUsd": or_null(js::get(cost, "idleUsd")),
                "windowDays": or_null(js::get(value, "valueWindowDays")),
            })
        } else {
            Value::Null
        };
        let mut pm = Map::new();
        pm.insert("name".into(), json!(name));
        pm.insert("accounts".into(), json!(mine.len()));
        pm.insert(
            "routable".into(),
            json!(mine
                .iter()
                .filter(|a| js::get(Some(a), "routable") == Some(&Value::Bool(true)))
                .count()),
        );
        pm.insert(
            "monthlyCostUsd".into(),
            if priced.is_empty() {
                Value::Null
            } else {
                round2(priced.iter().fold(0.0, |s, a| s + price_of(a).unwrap()))
            },
        );
        pm.insert("unpricedAccounts".into(), json!(mine.len() - priced.len()));
        pm.insert("priceSources".into(), Value::Array(sources));
        pm.insert(
            "creditsBanked".into(),
            js::number_value(mine.iter().fold(0.0, |s, a| s + count_of(a))),
        );
        pm.insert(
            "nextCreditExpiry".into(),
            all_expiries.first().map_or(Value::Null, |e| json!(e)),
        );
        let model_banked = mine.iter().fold(0.0, |s, a| {
            s + js::finite(js::get(js::get(Some(a), "credits"), "modelCount")).unwrap_or(0.0)
        });
        if model_banked > 0.0 {
            pm.insert("modelCreditsBanked".into(), js::number_value(model_banked));
        }
        pm.insert("forecast".into(), forecast_block);
        pm.insert("value".into(), value_block);
        out.insert((*id).into(), Value::Object(pm));
    }
    let costs: Vec<f64> = out
        .values()
        .filter_map(|p| js::finite(p.get("monthlyCostUsd")))
        .collect();
    let mut doc = Map::new();
    doc.insert("format".into(), json!(SUMMARY_FORMAT));
    doc.insert("generatedAt".into(), json!(js::iso(now)));
    doc.insert(
        "checkedAt".into(),
        or_null(js::get(Some(state), "checkedAt")),
    );
    doc.insert("error".into(), or_null(js::get(Some(state), "error")));
    doc.insert(
        "forecastGeneratedAt".into(),
        or_null(js::get(forecast, "generatedAt")),
    );
    doc.insert(
        "forecastStaleAfter".into(),
        or_null(js::get(forecast, "staleAfter")),
    );
    doc.insert(
        "valueGeneratedAt".into(),
        or_null(js::get(value, "generatedAt")),
    );
    doc.insert(
        "monthlyCostUsd".into(),
        if costs.is_empty() {
            Value::Null
        } else {
            round2(costs.iter().fold(0.0, |s, c| s + c))
        },
    );
    doc.insert("notes".into(), json!(NOTES));
    doc.insert("providers".into(), Value::Object(out));
    doc.insert("accounts".into(), Value::Array(accounts));
    Value::Object(doc)
}
