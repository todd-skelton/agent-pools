//! Routing order (routing.mjs `rankAccounts`) and the banked-credit choice
//! (`chooseReset`, with `accountOut`, `refillNeedMs` and `refillRunwayOk`).
//!
//! These decide when a banked credit is spent, so they follow the JavaScript expression by
//! expression, including its floating-point order of operations.

use crate::js;
use crate::quota::WEEK_SECONDS;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

const HOUR_MS: f64 = 3_600_000.0;
const DAY_MS: f64 = 24.0 * HOUR_MS;
/// Codex resets only: an account whose own refill is under a day away keeps its credit.
pub const EARLY_MIN_REFILL_MS: f64 = DAY_MS;

/// One account as routing reads it, from a pool-state account (`/api/status` shape).
#[derive(Clone, Debug)]
pub struct Account {
    pub id: String,
    pub provider: Option<String>,
    pub disabled: bool,
    pub status: Option<String>,
    pub credits_known: bool,
    /// `creditCount`, as a number (`NaN` when missing, as JavaScript's subtraction sees it).
    pub credit_count: f64,
    /// `remaining` when a number.
    pub remaining: Option<f64>,
    /// `rankReset` when a finite number.
    pub rank_reset: Option<f64>,
    /// `rankRemaining` as stored (`?? Infinity` and `Number.isFinite` read it differently).
    pub rank_remaining: Option<Value>,
    pub windows: Vec<Window>,
    pub credits: Vec<Value>,
}

#[derive(Clone, Debug)]
pub struct Window {
    pub common: bool,
    pub duration_seconds: Option<f64>,
    pub used: Option<f64>,
}

impl Account {
    pub fn from_value(a: &Value) -> Account {
        let a = Some(a);
        let s = |k| js::get(a, k).and_then(Value::as_str).map(str::to_string);
        Account {
            id: js::to_string(js::get(a, "id")),
            provider: s("provider"),
            disabled: js::truthy(js::get(a, "disabled")),
            status: s("status"),
            credits_known: js::truthy(js::get(a, "creditsKnown")),
            credit_count: js::to_number(js::get(a, "creditCount")),
            remaining: js::number(js::get(a, "remaining")),
            rank_reset: js::finite(js::get(a, "rankReset")),
            rank_remaining: js::get(a, "rankRemaining").cloned(),
            windows: match js::get(a, "windows") {
                Some(Value::Array(ws)) => ws
                    .iter()
                    .map(|w| Window {
                        common: js::truthy(js::get(Some(w), "common")),
                        duration_seconds: js::number(js::get(Some(w), "durationSeconds")),
                        used: js::number(js::get(Some(w), "used")),
                    })
                    .collect(),
                _ => Vec::new(),
            },
            credits: match js::get(a, "credits") {
                Some(Value::Array(cs)) => cs.clone(),
                _ => Vec::new(),
            },
        }
    }

    fn status_is(&self, any: &[&str]) -> bool {
        self.status.as_deref().is_some_and(|s| any.contains(&s))
    }

    /// `Number.isFinite(a.rankRemaining)`.
    fn rank_remaining_finite(&self) -> Option<f64> {
        js::finite(self.rank_remaining.as_ref())
    }

    fn weekly(&self) -> impl Iterator<Item = &Window> {
        self.windows
            .iter()
            .filter(|w| w.common && w.duration_seconds == Some(WEEK_SECONDS))
    }
}

/// A refill provider's demand for `refillNeedMs`: weighted account-units per hour.
#[derive(Clone, Debug, Default)]
pub struct RefillDemand {
    pub units_per_hour: Option<f64>,
    pub weights: HashMap<String, Value>,
}

/// A provider's reset policy (resets.mjs `resetPolicy`), plus what refresh adds to it.
#[derive(Clone, Debug)]
pub struct Policy {
    pub provider: String,
    /// `"reset"` (Codex) or `"refill"` (Claude).
    pub redemption: String,
    pub trigger: String,
    pub headroom_percent: f64,
    pub max_runway_percent: Option<f64>,
    pub expected_count: f64,
    pub refill_demand: Option<RefillDemand>,
    pub drain_first: Option<HashMap<String, f64>>,
}

impl Policy {
    pub fn from_value(p: &Value) -> Policy {
        let p = Some(p);
        let s = |k| {
            js::get(p, k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        Policy {
            provider: s("provider"),
            redemption: s("redemption"),
            trigger: s("trigger"),
            headroom_percent: js::to_number(js::get(p, "headroomPercent")),
            max_runway_percent: js::number(js::get(p, "maxRunwayPercent")),
            expected_count: js::to_number(js::get(p, "expectedCount")),
            refill_demand: js::get(p, "refillDemand")
                .filter(|d| !d.is_null())
                .map(|d| RefillDemand {
                    units_per_hour: js::number(js::get(Some(d), "unitsPerHour")),
                    weights: match js::get(Some(d), "weights") {
                        Some(Value::Object(m)) => {
                            m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                        }
                        _ => HashMap::new(),
                    },
                }),
            drain_first: None,
        }
    }
}

/// `accountOut(a, { trigger, headroomPercent })`.
pub fn account_out(a: &Account, trigger: &str, headroom_percent: f64) -> bool {
    if trigger == "weekly" {
        if !a.status_is(&["available", "exhausted"]) {
            return false;
        }
        let weekly: Vec<&Window> = a.weekly().collect();
        return !weekly.is_empty()
            && weekly.iter().all(|w| w.used.is_some_and(f64::is_finite))
            && weekly
                .iter()
                .any(|w| w.used.is_some_and(|u| u >= 100.0 - headroom_percent));
    }
    a.status_is(&["exhausted"])
        || (a.status_is(&["available"])
            && a.remaining
                .is_some_and(|r| r.is_finite() && r <= headroom_percent))
}

/// `drainKey(a, drainFirst)`.
pub fn drain_key(a: &Account, drain_first: Option<&HashMap<String, f64>>) -> f64 {
    let reset = a.rank_reset.unwrap_or(f64::INFINITY);
    match drain_first.and_then(|m| m.get(&a.id)) {
        Some(until) if until.is_finite() && *until < reset => *until,
        _ => reset,
    }
}

/// `rankAccounts(accounts, drainFirst)`: the order new sessions are routed in.
pub fn rank_accounts<'a>(
    accounts: &'a [Account],
    drain_first: Option<&HashMap<String, f64>>,
) -> Vec<&'a Account> {
    let rank = |a: &Account| match a.status.as_deref() {
        Some("available") => 0.0,
        Some("unknown") => 1.0,
        Some("exhausted") => 2.0,
        Some("disabled") => 3.0,
        _ => f64::NAN,
    };
    let remaining = |a: &Account| match &a.rank_remaining {
        None | Some(Value::Null) => f64::INFINITY,
        v => js::to_number(v.as_ref()),
    };
    let mut out: Vec<&Account> = accounts.iter().collect();
    js::sort_by(&mut out, |a, b| {
        js::or_else(rank(a) - rank(b), || {
            js::or_else(
                drain_key(a, drain_first) - drain_key(b, drain_first),
                || {
                    js::or_else(remaining(a) - remaining(b), || {
                        js::locale_compare(&a.id, &b.id) as i8 as f64
                    })
                },
            )
        })
    });
    out
}

/// `refillNeedMs(a, pool, demand, now, drainFirst)`: how long account `a` needs to use up a
/// refill after the accounts that reset sooner. `None` when demand is unknown.
pub fn refill_need_ms(
    a: &Account,
    a_rank_reset: f64,
    pool: &[Account],
    demand: Option<&RefillDemand>,
    now: f64,
    drain_first: Option<&HashMap<String, f64>>,
) -> Option<f64> {
    let demand = demand?;
    let rate = demand
        .units_per_hour
        .filter(|r| r.is_finite() && *r > 0.0)?;
    let weight = |id: &str| {
        js::finite(demand.weights.get(id))
            .filter(|w| *w > 0.0)
            .unwrap_or(1.0)
    };
    let mut units = weight(&a.id);
    for b in pool {
        if b.id == a.id || b.disabled || b.provider != a.provider {
            continue;
        }
        let Some(b_reset) = b.rank_reset else {
            continue;
        };
        if b_reset <= now || drain_key(b, drain_first) >= a_rank_reset {
            continue;
        }
        let left = match b.rank_remaining_finite() {
            Some(r) if b.status_is(&["available", "exhausted"]) => r / 100.0,
            _ => 1.0,
        };
        units += (weight(&b.id) * left).min((rate * (b_reset - now)) / HOUR_MS);
    }
    Some((units / rate) * HOUR_MS)
}

fn credit_expiry(c: &Value) -> f64 {
    js::expiry_ms(js::coalesce([
        js::get(Some(c), "expires_at"),
        js::get(Some(c), "expiresAt"),
    ]))
}

/// `refillRunwayOk(a, credit, policy, now, pool)`.
pub fn refill_runway_ok(
    a: &Account,
    credit: Option<&Value>,
    policy: &Policy,
    now: f64,
    pool: &[Account],
) -> bool {
    if policy.redemption != "refill" {
        return true;
    }
    let (Some(credit), Some(reset)) = (credit.filter(|c| js::truthy(Some(c))), a.rank_reset) else {
        return false;
    };
    if reset <= now {
        return false;
    }
    if credit_expiry(credit) <= reset {
        return true;
    }
    let period_ms = a
        .weekly()
        .next()
        .and_then(|w| w.duration_seconds)
        .unwrap_or(WEEK_SECONDS)
        * 1000.0;
    let cap = policy
        .max_runway_percent
        .filter(|p| p.is_finite())
        .map_or(0.5, |p| p / 100.0)
        * period_ms;
    let need = refill_need_ms(
        a,
        reset,
        pool,
        policy.refill_demand.as_ref(),
        now,
        policy.drain_first.as_ref(),
    );
    reset - now >= need.map_or(cap, |n| n.min(cap))
}

/// The credit to spend, from `chooseReset`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetChoice {
    pub account_id: String,
    pub credit_id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<&'static str>,
}

/// `creditKey(accountId, creditId)`.
pub fn credit_key(account_id: &str, credit_id: Option<&Value>) -> String {
    format!("{account_id}|{}", js::to_string(credit_id))
}

/// `chooseReset(accounts, policy, now, skip, beforeLapse)`: the banked credit to spend now,
/// if any. A reset fires once every enrolled account of the provider is out and every bank is
/// known; `before_lapse` lets a listed account that is itself out spend its own next credit
/// early. `skip` holds `credit_key`s recently refused.
pub fn choose_reset(
    accounts: &[Account],
    policy: &Policy,
    now: f64,
    skip: &HashSet<String>,
    before_lapse: Option<&HashSet<String>>,
) -> Option<ResetChoice> {
    let pool: Vec<Account> = accounts
        .iter()
        .filter(|a| a.provider.as_deref() == Some(policy.provider.as_str()) && !a.disabled)
        .cloned()
        .collect();
    if (pool.len() as f64) < policy.expected_count || pool.iter().any(|a| !a.credits_known) {
        return None;
    }
    let natural_reset = |a: &Account| a.rank_reset.unwrap_or(f64::NEG_INFINITY);
    if pool
        .iter()
        .any(|a| !account_out(a, &policy.trigger, policy.headroom_percent))
    {
        let before_lapse = before_lapse.filter(|s| !s.is_empty())?;
        let mut early: Vec<(&Account, &Value)> = pool
            .iter()
            .filter(|a| {
                before_lapse.contains(&a.id) && account_out(a, "weekly", policy.headroom_percent)
            })
            .filter_map(|a| {
                a.credits
                    .first()
                    .filter(|c| js::truthy(Some(c)))
                    .map(|c| (a, c))
            })
            .filter(|(a, c)| !skip.contains(&credit_key(&a.id, js::get(Some(c), "id"))))
            .filter(|(a, c)| {
                let Some(reset) = a.rank_reset else {
                    return false;
                };
                if policy.redemption == "refill" {
                    refill_runway_ok(a, Some(c), policy, now, &pool)
                } else {
                    reset - now >= EARLY_MIN_REFILL_MS || credit_expiry(c) < reset
                }
            })
            .collect();
        js::sort_by(&mut early, |(xa, xc), (ya, yc)| {
            let (ex, ey) = (credit_expiry(xc), credit_expiry(yc));
            let (ux, uy) = (ex <= now + DAY_MS, ey <= now + DAY_MS);
            if ux != uy {
                return if ux { -1.0 } else { 1.0 };
            }
            js::or_else(if ux { ex - ey } else { 0.0 }, || {
                js::or_else(natural_reset(ya) - natural_reset(xa), || {
                    js::or_else(ex - ey, || js::locale_compare(&xa.id, &ya.id) as i8 as f64)
                })
            })
        });
        return early.first().map(|(a, c)| ResetChoice {
            account_id: a.id.clone(),
            credit_id: js::get(Some(c), "id").cloned().unwrap_or(Value::Null),
            trigger: Some("before-lapse"),
        });
    }
    // Each account offers its soonest credit that was not refused (and, for a refill, that it
    // has the runway to use up).
    let offered: Vec<(&Account, &Value)> = pool
        .iter()
        .filter_map(|a| {
            a.credits
                .iter()
                .find(|c| {
                    !skip.contains(&credit_key(&a.id, js::get(Some(c), "id")))
                        && refill_runway_ok(a, Some(c), policy, now, &pool)
                })
                .filter(|c| js::truthy(Some(c)))
                .map(|c| (a, c))
        })
        .collect();
    let mut urgent: Vec<(&Account, &Value)> = offered
        .iter()
        .copied()
        .filter(|(_, c)| credit_expiry(c) <= now + DAY_MS)
        .collect();
    let chosen = if !urgent.is_empty() {
        js::sort_by(&mut urgent, |(a, ac), (b, bc)| {
            js::or_else(credit_expiry(ac) - credit_expiry(bc), || {
                js::or_else(natural_reset(b) - natural_reset(a), || {
                    js::locale_compare(&a.id, &b.id) as i8 as f64
                })
            })
        });
        urgent.first().copied()
    } else {
        let mut choices = offered.clone();
        js::sort_by(&mut choices, |(a, ac), (b, bc)| {
            js::or_else(natural_reset(b) - natural_reset(a), || {
                js::or_else(b.credit_count - a.credit_count, || {
                    js::or_else(credit_expiry(ac) - credit_expiry(bc), || {
                        js::locale_compare(&a.id, &b.id) as i8 as f64
                    })
                })
            })
        });
        choices.first().copied()
    };
    chosen.map(|(a, c)| ResetChoice {
        account_id: a.id.clone(),
        credit_id: js::get(Some(c), "id").cloned().unwrap_or(Value::Null),
        trigger: None,
    })
}
