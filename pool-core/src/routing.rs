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
    /// Model caps (`quotaWindows` rows that are not `common`, a day or longer).
    pub model_windows: Vec<Window>,
    /// `modelCredits`: grants that reset a model's cap but not the all-model quota.
    pub model_credits: Vec<Value>,
}

#[derive(Clone, Debug)]
pub struct Window {
    pub common: bool,
    pub duration_seconds: Option<f64>,
    pub used: Option<f64>,
}

fn rows(ws: Option<&Value>) -> Vec<Window> {
    match ws {
        Some(Value::Array(ws)) => ws
            .iter()
            .map(|w| Window {
                common: js::truthy(js::get(Some(w), "common")),
                duration_seconds: js::number(js::get(Some(w), "durationSeconds")),
                used: js::number(js::get(Some(w), "used")),
            })
            .collect(),
        _ => Vec::new(),
    }
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
            windows: rows(js::get(a, "windows")),
            credits: match js::get(a, "credits") {
                Some(Value::Array(cs)) => cs.clone(),
                _ => Vec::new(),
            },
            model_windows: rows(js::get(a, "quotaWindows"))
                .into_iter()
                .filter(|w| !w.common && w.duration_seconds.is_some_and(|d| d >= 86_400.0))
                .collect(),
            model_credits: match js::get(a, "modelCredits") {
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

    /// The most used common weekly window, when every one reads.
    fn all_model_used(&self) -> Option<f64> {
        let weekly: Vec<f64> = self.weekly().map(|w| w.used.filter(|u| u.is_finite())).collect::<Option<_>>()?;
        weekly.into_iter().reduce(f64::max)
    }

    /// Out of some model's cap (and so unable to serve that model) though perhaps not out of
    /// all-model quota.
    fn model_out(&self, headroom_percent: f64) -> bool {
        self.model_windows
            .iter()
            .any(|w| w.used.is_some_and(|u| u.is_finite() && u >= 100.0 - headroom_percent))
    }
}

/// The session window's key in a grant's `clears`; any other key besides the all-model weekly
/// is a model's cap.
const CLAUDE_SESSION: &str = "five_hour";

/// What claiming model-scoped credit `c` on account `a` would add, in percent of `a`'s weekly:
/// the all-model quota left that the cleared model cap is keeping it from using. `None` when the
/// claim would add nothing worth spending it on: the account is out of all-model quota (the
/// claim cannot lift that), or not yet out of the cap the credit clears. Within a day of the
/// credit's expiry any real gain counts, as the credit is lost otherwise.
fn model_credit_gain(a: &Account, c: &Value, headroom_percent: f64, now: f64) -> Option<f64> {
    if !a.status_is(&["available", "exhausted"]) {
        return None;
    }
    let all_used = a.all_model_used()?;
    let headroom = 100.0 - all_used;
    if headroom <= headroom_percent.max(0.0) {
        return None;
    }
    let clears = js::values(js::get(Some(c), "clears"));
    let percent = js::get(Some(c), "percent_used");
    let cap_used = clears
        .iter()
        .filter_map(|k| k.as_str())
        .filter(|k| *k != CLAUDE_SESSION && *k != crate::banks::CLAUDE_ALL_MODEL_WEEKLY)
        .map(|k| js::finite(js::get(percent, k)).unwrap_or(0.0))
        .reduce(f64::max)?;
    let urgent = credit_expiry(c) <= now + DAY_MS;
    if urgent {
        let gain = cap_used.min(headroom);
        return (gain > 0.0).then_some(gain);
    }
    (cap_used >= 100.0 - headroom_percent).then_some(headroom)
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
/// early. `skip` holds `credit_key`s recently refused. When no full credit is chosen, a
/// model-scoped one may be ([`choose_model_reset`]).
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
    choose_full_reset(&pool, policy, now, skip, before_lapse)
        .or_else(|| choose_model_reset(&pool, policy, now, skip))
}

/// A model-scoped credit (Claude's launch grants that reset only the Fable weekly cap) is spent
/// once no enrolled account can serve that model, each being out of all-model quota or of a
/// model cap, on the account where it adds the most: one out of the cap it clears with the most
/// all-model quota left, which the cap is holding back. Never on an account out of all-model
/// quota, where it would restore nothing. A credit within a day of expiry is spent wherever it
/// adds anything. The refill runway rule applies as for a full refill.
fn choose_model_reset(pool: &[Account], policy: &Policy, now: f64, skip: &HashSet<String>) -> Option<ResetChoice> {
    let natural_reset = |a: &Account| a.rank_reset.unwrap_or(f64::NEG_INFINITY);
    let blocked = pool.iter().all(|a| {
        account_out(a, "weekly", policy.headroom_percent) || a.model_out(policy.headroom_percent)
    });
    let mut offered: Vec<(&Account, &Value, f64, bool)> = pool
        .iter()
        .flat_map(|a| a.model_credits.iter().map(move |c| (a, c)))
        .filter(|(a, c)| js::truthy(Some(c)) && !skip.contains(&credit_key(&a.id, js::get(Some(c), "id"))))
        .filter_map(|(a, c)| {
            let gain = model_credit_gain(a, c, policy.headroom_percent, now)?;
            let urgent = credit_expiry(c) <= now + DAY_MS;
            ((urgent || blocked) && refill_runway_ok(a, Some(c), policy, now, pool)).then_some((a, c, gain, urgent))
        })
        .collect();
    js::sort_by(&mut offered, |(a, ac, ag, au), (b, bc, bg, bu)| {
        if au != bu {
            return if *au { -1.0 } else { 1.0 };
        }
        js::or_else(if *au { credit_expiry(ac) - credit_expiry(bc) } else { 0.0 }, || {
            js::or_else(bg - ag, || {
                js::or_else(natural_reset(b) - natural_reset(a), || {
                    js::or_else(credit_expiry(ac) - credit_expiry(bc), || js::locale_compare(&a.id, &b.id) as i8 as f64)
                })
            })
        })
    });
    offered.first().map(|(a, c, _, _)| ResetChoice {
        account_id: a.id.clone(),
        credit_id: js::get(Some(c), "id").cloned().unwrap_or(Value::Null),
        trigger: Some("model-cap"),
    })
}

/// The full-refill choice (`chooseReset` as it was before model-scoped credits).
fn choose_full_reset(
    pool: &[Account],
    policy: &Policy,
    now: f64,
    skip: &HashSet<String>,
    before_lapse: Option<&HashSet<String>>,
) -> Option<ResetChoice> {
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

#[cfg(test)]
mod model_credit_tests {
    use super::*;
    use serde_json::json;

    const NOW: f64 = 1_794_000_000_000.0;
    const FAR: &str = "2027-03-09T07:59:00+00:00";

    /// A Claude account with an all-model weekly at `all`% and a Fable cap at `fable`%, holding
    /// the Fable launch grant when `grant` is set.
    fn account(id: &str, all: f64, fable: f64, grant: Option<&str>) -> Account {
        let weekly = json!({ "common": true, "durationSeconds": 604800, "used": all });
        let mut a = json!({
            "id": id, "provider": "claude", "disabled": false,
            "status": if all >= 100.0 { "exhausted" } else { "available" },
            "creditsKnown": true, "creditCount": 0, "credits": [],
            "rankReset": NOW + 5.0 * DAY_MS,
            "windows": [weekly],
            "quotaWindows": [weekly, { "common": false, "durationSeconds": 604800, "used": fable }],
        });
        if let Some(expires) = grant {
            a["modelCredits"] = json!([{
                "id": "launch-2610-fable", "expires_at": expires, "label": "Fable reset",
                "clears": ["seven_day_overage_included"],
                "percent_used": { "seven_day_overage_included": fable, "seven_day": all },
            }]);
            a["modelCreditCount"] = json!(1);
        }
        Account::from_value(&a)
    }

    fn policy(expected: usize) -> Policy {
        Policy::from_value(&json!({
            "provider": "claude", "redemption": "refill", "trigger": "weekly",
            "headroomPercent": 0, "maxRunwayPercent": 50, "expectedCount": expected,
        }))
    }

    fn choose(accounts: &[Account]) -> Option<ResetChoice> {
        choose_reset(accounts, &policy(accounts.len()), NOW, &HashSet::new(), None)
    }

    #[test]
    fn never_on_an_account_out_of_all_model_quota() {
        // The live case: all-model weekly spent, Fable barely used. Even with every account
        // blocked, clearing Fable there restores nothing.
        let pool = [account("a", 100.0, 13.0, Some(FAR)), account("b", 100.0, 2.0, Some(FAR)), account("c", 100.0, 0.0, None)];
        assert_eq!(choose(&pool), None);
    }

    #[test]
    fn waits_while_another_account_can_still_serve_the_model() {
        let pool = [account("a", 12.0, 100.0, Some(FAR)), account("b", 0.0, 0.0, None), account("c", 100.0, 13.0, Some(FAR))];
        assert_eq!(choose(&pool), None);
    }

    #[test]
    fn spent_where_the_cap_holds_back_the_most_all_model_quota() {
        let pool = [
            account("a", 40.0, 100.0, Some(FAR)),
            account("b", 12.0, 100.0, Some(FAR)),
            account("c", 100.0, 13.0, Some(FAR)),
            account("d", 100.0, 0.0, None),
        ];
        let chosen = choose(&pool).unwrap();
        assert_eq!((chosen.account_id.as_str(), chosen.trigger), ("b", Some("model-cap")));
        assert_eq!(chosen.credit_id, json!("launch-2610-fable"));
    }

    #[test]
    fn not_before_the_cap_it_clears_is_reached() {
        let pool = [account("a", 12.0, 99.0, Some(FAR)), account("b", 100.0, 0.0, None)];
        assert_eq!(choose(&pool), None);
    }

    #[test]
    fn within_a_day_of_expiry_any_real_gain_counts() {
        let soon = js::iso(NOW + 6.0 * HOUR_MS);
        let pool = [account("a", 100.0, 13.0, Some(&soon)), account("b", 30.0, 16.0, Some(&soon)), account("c", 0.0, 0.0, None)];
        assert_eq!(choose(&pool).map(|c| c.account_id), Some("b".into()));
        let spent = [account("a", 100.0, 13.0, Some(&soon)), account("c", 0.0, 0.0, None)];
        assert_eq!(choose(&spent), None, "nothing to gain on an account out of all-model quota");
    }

    #[test]
    fn a_full_refill_is_not_spent_by_the_model_rule_and_still_fires_as_before() {
        let mut out = [account("a", 100.0, 13.0, Some(FAR)), account("b", 100.0, 100.0, None)];
        out[1].credits = vec![json!({ "id": "opus-launch", "expires_at": FAR })];
        out[1].credit_count = 1.0;
        let chosen = choose(&out).unwrap();
        assert_eq!((chosen.account_id.as_str(), chosen.trigger), ("b", None));
    }

    #[test]
    fn a_refused_model_credit_is_passed_over() {
        let pool = [account("a", 12.0, 100.0, Some(FAR)), account("b", 100.0, 0.0, None)];
        let skip: HashSet<String> = [credit_key("a", Some(&json!("launch-2610-fable")))].into();
        assert_eq!(choose_reset(&pool, &policy(2), NOW, &skip, None), None);
    }
}
