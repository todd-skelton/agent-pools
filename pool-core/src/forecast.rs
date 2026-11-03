//! Capacity forecast (capacity-forecast.mjs `buildForecast`): per-provider quota supply
//! against observed demand, reduced to an ordinal state (conserve, normal, spend, unknown),
//! a balance between the providers, a credit plan, what-if scenarios and a pace plan.
//!
//! Every function follows the JavaScript expression by expression, in the same order of
//! floating-point operations and the same iteration order, so the published document is the
//! same number for number. Accounts, samples and the previous document stay
//! `serde_json::Value`, read loosely the way the JavaScript reads them.

use crate::js;
use crate::providers::{self, Redemption};
use crate::quota::WEEK_SECONDS;
use crate::resets::ResetPolicy;
use serde_json::{json, Map, Value};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

pub const FORECAST_FORMAT: &str = "capacity-forecast/v1";
pub const HOUR: f64 = 3_600_000.0;
/// Reset estimates jitter (Claude by milliseconds, Codex by minutes); only a larger move is a
/// new window.
pub const RESET_JITTER_MS: f64 = HOUR;
pub const FORECAST_MODES: [&str; 2] = ["shadow", "active"];
const INF: f64 = f64::INFINITY;

// Claude's usage response does not give windows stable ids. `limit:0` is the
// weekly all-models bucket when no session limit is present, while it becomes
// the session bucket when one is present and the weekly bucket moves to
// `limit:1`. Keep the published selector for compatibility, but resolve it by
// semantic properties below.
const CLAUDE_WEEKLY_SELECTOR: &str = "limit:1";
const CLAUDE_WEEKLY_KEY: &str = "__claude_weekly";

fn claude_weekly_window(w: &Value) -> bool {
    js::truthy(js::get(Some(w), "common"))
        && js::finite(js::get(Some(w), "durationSeconds")) == Some(WEEK_SECONDS)
}

fn replay_window(provider: Option<&str>, raw: &str, source: Option<&Value>) -> String {
    if provider != Some("claude") {
        return raw.to_string();
    }
    if source.is_some_and(claude_weekly_window) {
        CLAUDE_WEEKLY_KEY.to_string()
    } else {
        raw.to_string()
    }
}

fn forecast_replay_window(provider: &str, window: &str, semantic_weekly: bool) -> String {
    if provider != "claude" {
        return window.to_string();
    }
    if semantic_weekly {
        CLAUDE_WEEKLY_KEY.to_string()
    } else {
        window.to_string()
    }
}

/// A quota cap attached to an account's all-models weekly budget. Claude's
/// five-hour and model-specific windows are not extra account-weeks: they are
/// gates on the same weekly usage.
#[derive(Clone, Debug)]
pub struct Cap {
    pub used: f64,
    pub reset_at: Option<f64>,
    pub period_ms: f64,
    pub floating: bool,
    pub scope: Value,
    pub label: Value,
}

macro_rules! forecast_config {
    ($($field:ident $key:literal = $default:literal, max $max:literal;)*) => {
        /// `forecastConfig(input).config`.
        #[derive(Clone, Debug)]
        pub struct ForecastConfig {
            $(pub $field: f64,)*
            /// Capacity weight overrides: account id or `"<provider>:<plan>"` to a weight.
            pub weights: Map<String, Value>,
            pub mode: String,
        }

        /// Numeric settings in `DEFAULTS` order: key, default, upper bound.
        const NUMERIC: &[(&str, f64, f64)] = &[$(($key, $default, $max),)*];

        impl ForecastConfig {
            fn numeric_mut(&mut self, key: &str) -> &mut f64 {
                match key {
                    $($key => &mut self.$field,)*
                    _ => unreachable!("{key} is not a numeric setting"),
                }
            }

            fn numeric(&self, key: &str) -> f64 {
                match key {
                    $($key => self.$field,)*
                    _ => unreachable!("{key} is not a numeric setting"),
                }
            }
        }

        impl Default for ForecastConfig {
            fn default() -> Self {
                ForecastConfig { $($field: $default,)* weights: Map::new(), mode: "shadow".into() }
            }
        }
    };
}

forecast_config! {
    half_life_hours "halfLifeHours" = 12.0, max 168.0;
    lookback_hours "lookbackHours" = 168.0, max 720.0;
    horizon_days "horizonDays" = 14.0, max 35.0;
    max_horizon_days "maxHorizonDays" = 35.0, max 60.0;
    min_observed_hours "minObservedHours" = 6.0, max 168.0;
    margin "margin" = 0.25, max 4.0;
    spend_enter_units "spendEnterUnits" = 0.5, max 100.0;
    spend_exit_units "spendExitUnits" = 0.25, max 100.0;
    min_dwell_hours "minDwellHours" = 2.0, max 48.0;
    step_hours "stepHours" = 6.0, max 168.0;
    max_steps "maxSteps" = 4.0, max 20.0;
    stale_after_minutes "staleAfterMinutes" = 15.0, max 1440.0;
    stale_hold_minutes "staleHoldMinutes" = 30.0, max 1440.0;
    plan_margin "planMargin" = 0.5, max 4.0;
    plan_min_observed_hours "planMinObservedHours" = 72.0, max 168.0;
    plan_window_days "planWindowDays" = 14.0, max 35.0;
}

impl ForecastConfig {
    /// `{ ...config }` as the forecast publishes it.
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        for (key, ..) in NUMERIC {
            m.insert((*key).into(), js::number_value(self.numeric(key)));
        }
        m.insert("weights".into(), Value::Object(js_ordered(&self.weights)));
        m.insert("mode".into(), json!(self.mode));
        Value::Object(m)
    }
}

/// A copy of an object with its keys in JavaScript's own-key order.
fn js_ordered(m: &Map<String, Value>) -> Map<String, Value> {
    let v = Value::Object(m.clone());
    js::entries(Some(&v))
        .into_iter()
        .map(|(k, x)| (k, x.clone()))
        .collect()
}

/// `forecastConfig(input)`: the settings over their defaults, and what was rejected.
pub fn forecast_config(input: Option<&Value>) -> (ForecastConfig, Vec<String>) {
    let raw = match input {
        Some(Value::Object(m)) => Some(m),
        _ => None,
    };
    let mut problems = Vec::new();
    if !js::nullish(input) && raw.is_none() {
        problems.push("capacityForecast must be an object; using defaults".to_string());
    }
    let mut out = ForecastConfig::default();
    let invalid = |key: &str, def: Value| {
        format!(
            "capacityForecast.{key} is invalid; using {}",
            js::stringify(&def)
        )
    };
    for (key, def, max) in NUMERIC {
        let Some(v) = raw.and_then(|m| m.get(*key)) else {
            continue;
        };
        let ok = js::finite(Some(v))
            .is_some_and(|f| f >= 0.0 && f <= *max && (*key != "maxSteps" || f.fract() == 0.0));
        if ok {
            *out.numeric_mut(key) = v.as_f64().unwrap();
        } else {
            problems.push(invalid(key, js::number_value(*def)));
        }
    }
    if let Some(v) = raw.and_then(|m| m.get("weights")) {
        match v {
            Value::Object(w)
                if js::values(Some(v))
                    .iter()
                    .all(|n| js::finite(Some(n)).is_some_and(|f| f > 0.0)) =>
            {
                out.weights = w.clone();
            }
            _ => problems.push(invalid("weights", json!({}))),
        }
    }
    if let Some(v) = raw.and_then(|m| m.get("mode")) {
        match v.as_str().filter(|s| FORECAST_MODES.contains(s)) {
            Some(s) => out.mode = s.into(),
            None => problems.push(invalid("mode", json!("shadow"))),
        }
    }
    (out, problems)
}

/// `planOf(accountId)`: the plan named by an auth file (`...-prolite.json` is `prolite`).
pub fn plan_of(account_id: &str) -> Option<String> {
    let lower = account_id.to_ascii_lowercase();
    let stem = lower.strip_suffix(".json")?;
    let letters = stem
        .bytes()
        .rev()
        .take_while(u8::is_ascii_alphabetic)
        .count();
    let (head, plan) = stem.split_at(stem.len() - letters);
    (letters > 0 && head.ends_with('-')).then(|| plan.to_string())
}

/// `capacityWeight(provider, accountId, weights)`: exact account id, `"<provider>:<plan>"`,
/// the adapter's plan default, then 1.
pub fn capacity_weight(provider: &str, account_id: &str, weights: &Map<String, Value>) -> f64 {
    let plan = plan_of(account_id);
    let adapter = plan.as_ref().and_then(|p| {
        providers::provider(provider)?
            .capacity
            .plan_weights
            .iter()
            .find(|(k, _)| k == p)
            .map(|(_, w)| json!(w))
    });
    let candidates = [
        weights.get(account_id).cloned(),
        plan.as_ref()
            .and_then(|p| weights.get(&format!("{provider}:{p}")).cloned()),
        adapter,
    ];
    candidates
        .iter()
        .find_map(|v| js::finite(v.as_ref()).filter(|f| *f > 0.0))
        .unwrap_or(1.0)
}

fn weekly_window_id(provider: &str) -> Option<&'static str> {
    providers::provider(provider).map(|p| p.capacity.weekly_window_id)
}

fn starts_on_first_use(provider: &str) -> bool {
    providers::provider(provider).is_some_and(|p| p.capacity.window_starts_on_first_use)
}

fn refills(provider: &str) -> bool {
    providers::provider(provider).is_some_and(|p| p.redemption == Redemption::Refill)
}

/// `defaultMaxRunwayPercent(provider)`.
pub fn default_max_runway_percent(provider: &str) -> f64 {
    providers::provider(provider)
        .and_then(|p| p.reset_defaults.max_runway_percent)
        .unwrap_or(50.0)
}

// --- replay ---------------------------------------------------------------------------

/// A consumption increment in account-units.
#[derive(Clone, Debug)]
pub struct Event {
    pub at: f64,
    pub provider: Option<String>,
    pub window: String,
    pub units: f64,
}

/// One sample's reading, for censoring and the first-seen hour.
#[derive(Clone, Debug)]
pub struct Mark {
    pub at: f64,
    pub provider: Option<String>,
    pub window: String,
    pub account_id: Option<String>,
    pub used: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Replay {
    pub events: Vec<Event>,
    pub marks: Vec<Mark>,
}

/// `replaySamples(samples, weights)`: consumption increments from an ordered sample stream. A
/// window that reset (used fell, or resetAt moved by more than the jitter) contributes its
/// new used value. Claude's weekly all-models bucket is canonicalized here because the upstream
/// array index (`limit:N`) changes when a session bucket is present.
pub fn replay_samples(samples: &[Value], weights: &Map<String, Value>, accounts: &[Value]) -> Replay {
    let mut last: HashMap<String, (f64, Option<f64>)> = HashMap::new();
    let mut out = Replay::default();
    for s in samples {
        let s = Some(s);
        let Some(used) = js::finite(js::get(s, "used")) else {
            continue;
        };
        let Some(Value::String(raw_window)) = js::get(s, "window") else {
            continue;
        };
        let provider = js::get(s, "provider");
        let account = js::get(s, "accountId");
        let source = accounts
            .iter()
            .find(|a| {
                js::get(Some(a), "id").and_then(Value::as_str)
                    == account.and_then(Value::as_str)
                    || js::get(Some(a), "accountId").and_then(Value::as_str)
                        == account.and_then(Value::as_str)
            })
            .and_then(|a| js::get(Some(a), "quotaWindows"))
            .and_then(|ws| ws.as_array())
            .and_then(|ws| ws.iter().find(|w| js::get(Some(w), "id").and_then(Value::as_str) == Some(raw_window)));
        let window = replay_window(provider.and_then(Value::as_str), raw_window, source);
        let at = js::date_parse_value(js::get(s, "at"));
        if !at.is_finite() {
            continue;
        }
        let key = format!(
            "{}|{window}|{}",
            js::to_string(provider),
            js::to_string(account)
        );
        let reset_at = js::finite(js::get(s, "resetAt"));
        let prev = last.insert(key, (used, reset_at));
        let provider_s = provider.and_then(Value::as_str).map(str::to_string);
        out.marks.push(Mark {
            at,
            provider: provider_s.clone(),
            window: window.clone(),
            account_id: account.and_then(Value::as_str).map(str::to_string),
            used,
        });
        let Some((prev_used, prev_reset)) = prev else {
            continue;
        };
        let moved =
            matches!((reset_at, prev_reset), (Some(r), Some(p)) if (r - p).abs() > RESET_JITTER_MS);
        let delta = if moved || used < prev_used {
            used
        } else {
            used - prev_used
        };
        if delta > 0.0 {
            let weight = capacity_weight(
                &js::to_string(provider),
                &if js::nullish(account) {
                    String::new()
                } else {
                    js::to_string(account)
                },
                weights,
            );
            out.events.push(Event {
                at,
                provider: provider_s,
                window: window.clone(),
                units: (weight * delta) / 100.0,
            });
        }
    }
    out
}

/// `censoredHours(...)`: hours at whose end every roster account of the class (or of the
/// provider's weekly class) was at 100%. Demand cannot be observed then.
pub fn censored_hours(
    marks: &[Mark],
    provider: &str,
    window: &str,
    roster: &[String],
    start_hour: f64,
    end_hour: f64,
) -> HashSet<i64> {
    let weekly = if provider == "claude" {
        Some(CLAUDE_WEEKLY_KEY)
    } else {
        weekly_window_id(provider)
    };
    let ids: HashSet<&str> = roster.iter().map(String::as_str).collect();
    let mut rel: Vec<&Mark> = marks
        .iter()
        .filter(|m| {
            m.provider.as_deref() == Some(provider)
                && m.account_id.as_deref().is_some_and(|id| ids.contains(id))
                && (m.window == window || Some(m.window.as_str()) == weekly)
        })
        .collect();
    js::sort_by(&mut rel, |a, b| a.at - b.at);
    let mut cls: HashMap<&str, f64> = HashMap::new();
    let mut wk: HashMap<&str, f64> = HashMap::new();
    let all_out = |map: &HashMap<&str, f64>| {
        !ids.is_empty()
            && ids
                .iter()
                .all(|id| map.get(id).copied().unwrap_or(0.0) >= 100.0)
    };
    let mut out = HashSet::new();
    let mut i = 0;
    let mut h = start_hour;
    while h <= end_hour {
        let end = (h + 1.0) * HOUR;
        while i < rel.len() && rel[i].at < end {
            let m = rel[i];
            i += 1;
            let id = m.account_id.as_deref().unwrap();
            if m.window == window {
                cls.insert(id, m.used);
            }
            if Some(m.window.as_str()) == weekly {
                wk.insert(id, m.used);
            }
        }
        if all_out(&cls) || all_out(&wk) {
            out.insert(h as i64);
        }
        h += 1.0;
    }
    out
}

/// `demandRate(...)`: account-units per hour.
#[derive(Clone, Debug, Default)]
pub struct Demand {
    pub ewma: f64,
    pub last24h: f64,
    pub last72h: f64,
    pub last168h: f64,
    pub observed_hours: f64,
    pub censored_hours: f64,
    pub current_hour_units: f64,
}

/// `demandRate(events, censored, { now, halfLifeHours, lookbackHours, firstHour })`: EWMA of
/// hourly consumption over completed, uncensored hours, plus trailing means.
pub fn demand_rate(
    events: &[&Event],
    censored: &HashSet<i64>,
    now: f64,
    half_life_hours: f64,
    lookback_hours: f64,
    first_hour: Option<f64>,
) -> Demand {
    let current = (now / HOUR).floor();
    let end = current - 1.0;
    let start = end - lookback_hours + 1.0;
    let mut current_hour_units = 0.0;
    for e in events {
        if (e.at / HOUR).floor() == current {
            current_hour_units += e.units;
        }
    }
    let mut buckets: HashMap<i64, f64> = HashMap::new();
    let mut bucket_hours: Vec<f64> = Vec::new();
    for e in events {
        let h = (e.at / HOUR).floor();
        if h < start || h > end {
            continue;
        }
        let entry = buckets.entry(h as i64).or_insert_with(|| {
            bucket_hours.push(h);
            0.0
        });
        *entry += e.units;
    }
    let censored_n = censored.len() as f64;
    let first_seen = js::min(
        std::iter::once(first_hour.filter(|f| f.is_finite()).unwrap_or(INF))
            .chain(bucket_hours.iter().copied()),
    );
    if !first_seen.is_finite() || first_seen > end {
        return Demand {
            censored_hours: censored_n,
            current_hour_units,
            ..Demand::default()
        };
    }
    let first = js::max([start, first_seen]);
    let decay = 0.5f64.powf(1.0 / half_life_hours);
    let bucket = |h: f64| buckets.get(&(h as i64)).copied().unwrap_or(0.0);
    let (mut ewma, mut observed): (Option<f64>, f64) = (None, 0.0);
    let mut h = first;
    while h <= end {
        if !censored.contains(&(h as i64)) {
            let v = bucket(h);
            ewma = Some(match ewma {
                None => v,
                Some(e) => decay * e + (1.0 - decay) * v,
            });
            observed += 1.0;
        }
        h += 1.0;
    }
    let trailing = |hours: f64| {
        let (mut sum, mut n) = (0.0, 0.0);
        let mut h = end - hours + 1.0;
        while h <= end {
            if !censored.contains(&(h as i64)) {
                n += 1.0;
                sum += bucket(h);
            }
            h += 1.0;
        }
        if n != 0.0 {
            sum / n
        } else {
            0.0
        }
    };
    Demand {
        ewma: ewma.unwrap_or(0.0),
        last24h: trailing(24.0),
        last72h: trailing(72.0),
        last168h: trailing(js::min([168.0, lookback_hours])),
        observed_hours: observed,
        censored_hours: censored_n,
        current_hour_units,
    }
}
// --- supply and simulation --------------------------------------------------------------

/// One account's supply for a window class (`supplyFor`).
#[derive(Clone, Debug)]
pub struct Supply {
    pub id: String,
    pub weight: f64,
    pub used: f64,
    pub reset_at: Option<f64>,
    pub period_ms: f64,
    pub first_use: bool,
    pub floating: bool,
    pub refill: bool,
    pub caps: Vec<Cap>,
    /// Credit expiries, soonest first (`Infinity` for none listed).
    pub credits: Vec<f64>,
    pub unmodeled_credits: f64,
    pub stale: bool,
    pub scope: Value,
    pub label: Value,
}

impl Supply {
    fn units_left(&self) -> f64 {
        (self.weight * (100.0 - self.used)) / 100.0
    }

    fn cap_units_left(&self) -> f64 {
        self.caps.iter().fold(self.weight, |left, cap| {
            left.min((self.weight * (100.0 - cap.used)) / 100.0)
        })
    }

    fn effective_units_left(&self) -> f64 {
        self.units_left().min(self.cap_units_left())
    }
}

/// `supplyFor(accounts, provider, window, weights)`.
pub fn supply_for(
    accounts: &[Value],
    provider: &str,
    window: &str,
    weights: &Map<String, Value>,
    semantic_weekly: bool,
) -> Vec<Supply> {
    accounts
        .iter()
        .filter(|a| {
            js::get(Some(a), "provider").and_then(Value::as_str) == Some(provider)
                && !js::truthy(js::get(Some(a), "disabled"))
        })
        .filter_map(|a| {
            let a = Some(a);
            let windows = match js::get(a, "quotaWindows") {
                Some(Value::Array(ws)) => ws.as_slice(),
                _ => &[],
            };
            let w = windows.iter().find(|x| {
                let x = Some(*x);
                let selected = if semantic_weekly && provider == "claude" && window == CLAUDE_WEEKLY_SELECTOR {
                    x.is_some_and(claude_weekly_window)
                } else if provider == "claude" && !semantic_weekly {
                    !js::truthy(js::get(x, "common"))
                        && js::get(x, "id").and_then(Value::as_str) == Some(window)
                } else {
                    js::get(x, "id").and_then(Value::as_str) == Some(window)
                };
                selected
                    && js::finite(js::get(x, "used")).is_some()
                    && js::finite(js::get(x, "durationSeconds")).is_some_and(|d| d > 0.0)
            })?;
            let w = Some(w);
            let mut listed: Vec<f64> = match js::get(a, "credits") {
                Some(Value::Array(cs)) => cs
                    .iter()
                    .map(|c| {
                        let c = Some(c);
                        let raw = js::coalesce([js::get(c, "expires_at"), js::get(c, "expiresAt")]);
                        let t = if raw.is_none() {
                            f64::NAN
                        } else {
                            js::date_parse_value(raw)
                        };
                        if t.is_finite() {
                            t
                        } else {
                            INF
                        }
                    })
                    .collect(),
                _ => Vec::new(),
            };
            js::sort_by(&mut listed, |x, y| x - y);
            let count_field = js::finite(js::get(a, "creditCount"))
                .filter(|n| n.fract() == 0.0)
                .unwrap_or(0.0);
            let count = js::max([listed.len() as f64, count_field]);
            let credits: Vec<f64> = if listed.is_empty() {
                Vec::new()
            } else {
                (0..count as usize)
                    .map(|i| listed[i.min(listed.len() - 1)])
                    .collect()
            };
            let used = js::min([
                100.0,
                js::max([0.0, js::finite(js::get(w, "used")).unwrap()]),
            ]);
            let id = js::to_string(js::get(a, "id"));
            let first_use = starts_on_first_use(provider);
            let caps = if semantic_weekly && provider == "claude" {
                windows
                    .iter()
                    .filter_map(|cap| {
                        if claude_weekly_window(cap) {
                            return None;
                        }
                        let duration = js::finite(js::get(Some(cap), "durationSeconds"))?;
                        if duration <= 0.0 {
                            return None;
                        }
                        let cap_used = js::finite(js::get(Some(cap), "used"))?;
                        Some(Cap {
                            used: js::min([100.0, js::max([0.0, cap_used])]),
                            reset_at: js::finite(js::get(Some(cap), "resetAt")),
                            period_ms: duration * 1000.0,
                            floating: js::finite(js::get(Some(cap), "resetAt")).is_none(),
                            scope: js::coalesce([js::get(Some(cap), "scope")])
                                .cloned()
                                .unwrap_or(Value::Null),
                            label: js::coalesce([js::get(Some(cap), "label")])
                                .cloned()
                                .unwrap_or(Value::Null),
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };
            Some(Supply {
                weight: capacity_weight(provider, &id, weights),
                id,
                used,
                reset_at: js::finite(js::get(w, "resetAt")),
                period_ms: js::finite(js::get(w, "durationSeconds")).unwrap() * 1000.0,
                first_use,
                floating: first_use && used == 0.0,
                refill: refills(provider),
                caps,
                unmodeled_credits: count - credits.len() as f64,
                credits,
                stale: js::truthy(js::get(a, "usageStale"))
                    || js::get(a, "status").and_then(Value::as_str) == Some("unknown"),
                scope: js::coalesce([js::get(w, "scope")])
                    .cloned()
                    .unwrap_or(Value::Null),
                label: js::coalesce([js::get(w, "label")])
                    .cloned()
                    .unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// A drain-first plan: account id to the credit expiry it drains by, in insertion order (a
/// JavaScript `Map`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DrainFirst(pub Vec<(String, f64)>);

impl DrainFirst {
    pub fn get(&self, id: &str) -> Option<f64> {
        self.0.iter().find(|(k, _)| k == id).map(|(_, v)| *v)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.0.iter().any(|(k, _)| k == id)
    }

    /// `new Map([...this, [id, until]])`.
    pub fn with(&self, id: &str, until: f64) -> DrainFirst {
        let mut out = self.clone();
        match out.0.iter_mut().find(|(k, _)| k == id) {
            Some(entry) => entry.1 = until,
            None => out.0.push((id.to_string(), until)),
        }
        out
    }
}

#[derive(Clone, Copy)]
pub struct SimOptions<'a> {
    pub now: f64,
    pub horizon_hours: f64,
    pub use_credits: bool,
    pub max_runway_percent: f64,
    pub grants: &'a [f64],
    pub grant_lifetime_hours: f64,
    pub drain_first: Option<&'a DrainFirst>,
    pub early_refill: Option<&'a HashSet<String>>,
}

impl SimOptions<'_> {
    pub fn new(now: f64, horizon_hours: f64, use_credits: bool, max_runway_percent: f64) -> Self {
        SimOptions {
            now,
            horizon_hours,
            use_credits,
            max_runway_percent,
            grants: &[],
            grant_lifetime_hours: 168.0,
            drain_first: None,
            early_refill: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Redeemed {
    pub id: String,
    pub at: f64,
}

#[derive(Clone, Debug, Default)]
pub struct SimResult {
    pub unmet_units: f64,
    pub remaining_units: f64,
    pub credit_units_left: f64,
    pub outage_hours: f64,
    pub first_outage_at: Option<f64>,
    pub expiring_units: f64,
    pub credits_used: f64,
    pub credits_expired: f64,
    pub credit_units_expired: f64,
    pub wasted_units: f64,
    pub redemptions: Vec<Redeemed>,
    pub lapses: Vec<Redeemed>,
    pub credits_granted: f64,
}

/// `drainKey(a, drainFirst)`.
fn drain_key(a: &Supply, drain_first: Option<&DrainFirst>) -> f64 {
    let reset = a.reset_at.unwrap_or(INF);
    match drain_first.and_then(|d| d.get(&a.id)) {
        Some(until) if until.is_finite() && a.credits.first() == Some(&until) && until < reset => {
            until
        }
        _ => reset,
    }
}

fn reset_or_inf(a: &Supply) -> f64 {
    a.reset_at.unwrap_or(INF)
}

/// `refillRunway(a, all, t, rate, maxRunwayPercent, key)`: whether account `a`, out at `t`,
/// can use a refill before its own reset at the given rate.
pub fn refill_runway(
    a: &Supply,
    all: &[Supply],
    t: f64,
    rate: f64,
    max_runway_percent: f64,
    key: &dyn Fn(&Supply) -> f64,
) -> bool {
    let Some(reset) = a.reset_at else {
        return false;
    };
    if reset <= t {
        return false;
    }
    if a.credits.first().is_some_and(|c| *c <= reset) {
        return true;
    }
    let cap = (max_runway_percent / 100.0) * a.period_ms;
    // `!(rate > 0)`: NaN included.
    if rate.partial_cmp(&0.0) != Some(Ordering::Greater) {
        return reset - t >= cap;
    }
    let mut units = a.effective_units_left();
    for b in all {
        if std::ptr::eq(b, a) || b.id == a.id {
            continue;
        }
        let Some(b_reset) = b.reset_at else {
            continue;
        };
        if b_reset <= t || key(b).partial_cmp(&reset) != Some(Ordering::Less) {
            continue;
        }
        units += js::min([b.effective_units_left(), (rate * (b_reset - t)) / HOUR]);
    }
    reset - t >= js::min([(units / rate) * HOUR, cap])
}

fn drain(accounts: &mut [Supply], need: &mut f64, t: f64, drain_first: Option<&DrainFirst>) {
    let mut order: Vec<usize> = (0..accounts.len()).collect();
    js::sort_by(&mut order, |&x, &y| {
        drain_key(&accounts[x], drain_first) - drain_key(&accounts[y], drain_first)
    });
    for i in order {
        if *need <= 1e-12 {
            break;
        }
        let a = &mut accounts[i];
        let room = a.effective_units_left();
        if room <= 1e-12 {
            continue;
        }
        let take = js::min([room, *need]);
        if a.reset_at.is_none() {
            a.reset_at = Some(t + a.period_ms);
            a.floating = false;
        }
        a.used += (take / a.weight) * 100.0;
        for cap in &mut a.caps {
            if cap.reset_at.is_none() {
                cap.reset_at = Some(t + cap.period_ms);
                cap.floating = false;
            }
            cap.used += (take / a.weight) * 100.0;
        }
        *need -= take;
    }
}

/// `simulate(supply, rate, opts)`: hour by hour at a demand rate (units per hour, or a
/// function of the hour's start time).
pub fn simulate(supply: &[Supply], rate: &dyn Fn(f64) -> f64, o: &SimOptions) -> SimResult {
    let now = o.now;
    let mut accts: Vec<Supply> = supply
        .iter()
        .map(|a| {
            let mut b = a.clone();
            b.reset_at = if a.floating {
                None
            } else {
                Some(a.reset_at.unwrap_or(now + a.period_ms))
            };
            b
        })
        .collect();
    let mut pending: Vec<f64> = o.grants.to_vec();
    js::sort_by(&mut pending, |x, y| x - y);
    let mut r = SimResult::default();
    let max_runway = o.max_runway_percent;
    let runway_ok = |a: &Supply, t: f64, rate: f64| -> bool {
        if !a.refill {
            return true;
        }
        let Some(reset) = a.reset_at else {
            return false;
        };
        if reset <= t {
            return false;
        }
        if a.credits[0] <= reset {
            return true;
        }
        let need = if rate > 0.0 {
            (a.effective_units_left() / rate) * HOUR
        } else {
            INF
        };
        reset - t >= js::min([need, (max_runway / 100.0) * a.period_ms])
    };
    let key = |a: &Supply| drain_key(a, o.drain_first);
    let mut h = 0.0;
    while h < o.horizon_hours {
        let t = now + h * HOUR;
        for a in accts.iter_mut() {
            for cap in &mut a.caps {
                while let Some(reset) = cap.reset_at.filter(|r| *r <= t) {
                    cap.used = 0.0;
                    if cap.floating {
                        cap.reset_at = None;
                    } else {
                        cap.reset_at = Some(reset + cap.period_ms);
                    }
                }
            }
            while let Some(reset) = a.reset_at.filter(|r| *r <= t) {
                r.expiring_units += a.effective_units_left();
                a.used = 0.0;
                if a.first_use {
                    a.reset_at = None;
                    a.floating = true;
                } else {
                    a.reset_at = Some(reset + a.period_ms);
                }
            }
            while a.credits.first().is_some_and(|c| *c <= t) {
                a.credits.remove(0);
                r.credits_expired += 1.0;
                r.credit_units_expired += a.weight;
                r.lapses.push(Redeemed {
                    id: a.id.clone(),
                    at: t,
                });
            }
        }
        while !pending.is_empty() && pending[0] <= t && !accts.is_empty() {
            pending.remove(0);
            let mut order: Vec<usize> = (0..accts.len()).collect();
            js::sort_by(&mut order, |&x, &y| {
                reset_or_inf(&accts[y]) - reset_or_inf(&accts[x])
            });
            let to = &mut accts[order[0]];
            to.credits.push(t + o.grant_lifetime_hours * HOUR);
            js::sort_by(&mut to.credits, |x, y| x - y);
            r.credits_granted += 1.0;
        }
        let rate_now = rate(t);
        let mut need = rate_now;
        if o.use_credits {
            if let Some(early) = o.early_refill.filter(|s| !s.is_empty()) {
                for i in 0..accts.len() {
                    let a = &accts[i];
                    if !early.contains(&a.id)
                        || !a.refill
                        || a.credits.is_empty()
                        || a.effective_units_left() > 1e-12
                    {
                        continue;
                    }
                    if !refill_runway(a, &accts, t, rate_now, max_runway, &key) {
                        continue;
                    }
                    let a = &mut accts[i];
                    a.credits.remove(0);
                    r.credits_used += 1.0;
                    r.redemptions.push(Redeemed {
                        id: a.id.clone(),
                        at: t,
                    });
                    a.used = 0.0;
                    for cap in &mut a.caps {
                        cap.used = 0.0;
                    }
                }
            }
        }
        drain(&mut accts, &mut need, t, o.drain_first);
        while need > 1e-12 && o.use_credits {
            let holders: Vec<usize> = (0..accts.len())
                .filter(|&i| !accts[i].credits.is_empty() && runway_ok(&accts[i], t, rate_now))
                .collect();
            let mut urgent: Vec<usize> = holders
                .iter()
                .copied()
                .filter(|&i| accts[i].credits[0] <= t + 24.0 * HOUR)
                .collect();
            js::sort_by(&mut urgent, |&x, &y| {
                accts[x].credits[0] - accts[y].credits[0]
            });
            let holder = match urgent.first() {
                Some(&u) => Some(u),
                None => {
                    let mut hs = holders.clone();
                    js::sort_by(&mut hs, |&x, &y| {
                        reset_or_inf(&accts[y]) - reset_or_inf(&accts[x])
                    });
                    hs.first().copied()
                }
            };
            let Some(i) = holder else {
                break;
            };
            let a = &mut accts[i];
            a.credits.remove(0);
            r.credits_used += 1.0;
            r.redemptions.push(Redeemed {
                id: a.id.clone(),
                at: t,
            });
            a.used = 0.0;
            if !a.refill {
                a.reset_at = None;
            }
            for cap in &mut a.caps {
                cap.used = 0.0;
            }
            drain(&mut accts, &mut need, t, o.drain_first);
        }
        if need > 1e-12 {
            r.unmet_units += need;
            r.outage_hours += 1.0;
            r.first_outage_at.get_or_insert(t);
        }
        h += 1.0;
    }
    r.remaining_units = accts
        .iter()
        .fold(0.0, |s, a| s + a.effective_units_left());
    r.credit_units_left = accts
        .iter()
        .fold(0.0, |s, a| s + a.weight * a.credits.len() as f64);
    r.wasted_units = r.expiring_units + r.credit_units_expired;
    r
}

fn natural_units_per_hour(supply: &[Supply]) -> f64 {
    supply
        .iter()
        .fold(0.0, |s, a| s + a.weight / (a.period_ms / HOUR))
}

fn run_hours(supply: &[Supply], horizon_hours: f64) -> f64 {
    horizon_hours + (js::max(supply.iter().map(|a| a.period_ms)) / HOUR).ceil()
}
// --- credit plan (refill providers) ------------------------------------------------------

pub const CREDIT_PLAN_TIE_UNITS: f64 = 0.02;

/// The paces a credit plan is judged at.
#[derive(Clone, Copy, Debug)]
pub struct Paces {
    pub low: f64,
    pub measured: f64,
    pub high: f64,
}

#[derive(Clone, Debug)]
pub struct PlannedCredit {
    pub account_id: String,
    pub expires_at: f64,
    pub reset_at: Option<f64>,
    pub out: bool,
    pub units_left: f64,
    pub stale: bool,
    pub action: &'static str,
    pub drain_by: Option<f64>,
    pub outcome: Option<&'static str>,
    pub need_units_per_hour: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct CreditPlan {
    pub holders: Vec<String>,
    pub drain_first: DrainFirst,
    pub credits: Vec<PlannedCredit>,
}

/// First event for an account's next credit in a run: used, lapsed, or still banked.
fn fate(x: &SimResult, id: &str) -> &'static str {
    let used = x.redemptions.iter().find(|e| e.id == id);
    let lapsed = x.lapses.iter().find(|e| e.id == id);
    match (used, lapsed) {
        (Some(u), l) if l.is_none_or(|l| u.at <= l.at) => "used",
        (_, Some(_)) => "lapses",
        _ => "banked",
    }
}

fn redeemed(sup: &[Supply], id: &str) -> Vec<Supply> {
    sup.iter()
        .map(|b| {
            let mut c = b.clone();
            if b.id == id {
                c.used = 0.0;
                for cap in &mut c.caps {
                    cap.used = 0.0;
                }
                c.credits = b.credits.iter().skip(1).copied().collect();
            }
            c
        })
        .collect()
}

/// `refillCreditPlan(supply, paces, { now, horizonHours, maxRunwayPercent, keep })`: for each
/// banked refill, whether acting now (redeem now, or drain the account first) gives more total
/// use than not acting, at every pace.
pub fn refill_credit_plan(
    supply: &[Supply],
    paces: Paces,
    now: f64,
    horizon_hours: f64,
    max_runway_percent: f64,
    keep: Option<&HashMap<String, f64>>,
) -> Option<CreditPlan> {
    let rates: Vec<f64> = [paces.low, paces.measured, paces.high]
        .into_iter()
        .filter(|r| r.is_finite() && *r > 0.0)
        .collect();
    if supply.is_empty() || rates.is_empty() {
        return None;
    }
    let run = run_hours(supply, horizon_hours);
    let opts = SimOptions::new(now, run, true, max_runway_percent);
    let total = |sup: &[Supply], rate: f64, df: Option<&DrainFirst>, early: &HashSet<String>| {
        let x = simulate(
            sup,
            &|_| rate,
            &SimOptions {
                drain_first: df,
                early_refill: Some(early),
                ..opts
            },
        );
        x.remaining_units + x.credit_units_left - x.unmet_units
    };
    let better = |a: &dyn Fn(f64) -> f64, b: &dyn Fn(f64) -> f64, not_worse: bool| {
        let mut gain = not_worse;
        for &r in &rates {
            let d = a(r) - b(r);
            if d < -CREDIT_PLAN_TIE_UNITS {
                return false;
            }
            if d > CREDIT_PLAN_TIE_UNITS {
                gain = true;
            }
        }
        gain
    };
    let banked: Vec<&Supply> = supply
        .iter()
        .filter(|a| a.credits.first().is_some_and(|c| c.is_finite() && *c > now))
        .collect();
    let usable: Vec<&Supply> = banked.iter().copied().filter(|a| !a.stale).collect();
    let deadline: Vec<&Supply> = usable
        .iter()
        .copied()
        .filter(|a| a.effective_units_left() > 1e-12 && a.credits[0] < reset_or_inf(a))
        .collect();
    let early: HashSet<String> = deadline.iter().map(|a| a.id.clone()).collect();
    let low = if paces.low > 0.0 { paces.low } else { 0.0 };
    let mut holders: Vec<String> = Vec::new();
    for a in usable.iter().filter(|x| x.effective_units_left() <= 1e-12) {
        if !refill_runway(a, supply, now, low, max_runway_percent, &reset_or_inf) {
            continue;
        }
        let after = redeemed(supply, &a.id);
        if better(
            &|r| total(&after, r, None, &early),
            &|r| total(supply, r, None, &early),
            false,
        ) {
            holders.push(a.id.clone());
        }
    }
    let mut planned: Vec<Supply> = supply.to_vec();
    for id in &holders {
        planned = redeemed(&planned, id);
    }
    let mut drain_first = DrainFirst::default();
    let mut ordered = deadline.clone();
    js::sort_by(&mut ordered, |x, y| {
        js::or_else(x.credits[0] - y.credits[0], || {
            if x.id < y.id {
                -1.0
            } else {
                1.0
            }
        })
    });
    for a in ordered {
        let with_a = drain_first.with(&a.id, a.credits[0]);
        let kept = keep.and_then(|k| k.get(&a.id)) == Some(&a.credits[0]);
        if better(
            &|r| total(&planned, r, Some(&with_a), &early),
            &|r| total(&planned, r, Some(&drain_first), &early),
            kept,
        ) {
            drain_first = with_a;
        }
    }
    let natural = natural_units_per_hour(supply);
    let credits = banked
        .iter()
        .map(|a| {
            let action = if holders.contains(&a.id) {
                "redeem-now"
            } else if drain_first.contains(&a.id) {
                "drain-first"
            } else {
                "wait"
            };
            let drain_by = (a.credits[0] < reset_or_inf(a)).then_some(a.credits[0]);
            let (mut outcome, mut need_units_per_hour) = (None, None);
            if action == "redeem-now" {
                outcome = Some("used");
            } else {
                if paces.measured > 0.0 {
                    let x = simulate(
                        &planned,
                        &|_| paces.measured,
                        &SimOptions {
                            drain_first: Some(&drain_first),
                            early_refill: Some(&early),
                            ..opts
                        },
                    );
                    outcome = Some(fate(&x, &a.id));
                }
                let forced_df = if drain_by.is_some() && a.effective_units_left() > 1e-12 {
                    drain_first.with(&a.id, a.credits[0])
                } else {
                    drain_first.clone()
                };
                let mut forced_early = early.clone();
                forced_early.insert(a.id.clone());
                let saved = |r: f64| {
                    let x = simulate(
                        &planned,
                        &|_| r,
                        &SimOptions {
                            drain_first: Some(&forced_df),
                            early_refill: Some(&forced_early),
                            ..opts
                        },
                    );
                    fate(&x, &a.id) == "used"
                };
                let top = PACE.max_factor * natural;
                let eps = PACE.precision * natural;
                if natural > 0.0 && saved(top) {
                    let (mut lo, mut hi) = (0.0, top);
                    while hi - lo > eps {
                        let mid = (lo + hi) / 2.0;
                        if saved(mid) {
                            hi = mid;
                        } else {
                            lo = mid;
                        }
                    }
                    need_units_per_hour = Some(hi);
                }
            }
            PlannedCredit {
                account_id: a.id.clone(),
                expires_at: a.credits[0],
                reset_at: a.reset_at,
                out: a.effective_units_left() <= 1e-12,
                units_left: a.effective_units_left(),
                stale: a.stale,
                action,
                drain_by,
                outcome,
                need_units_per_hour,
            }
        })
        .collect();
    holders.sort();
    Some(CreditPlan {
        holders,
        drain_first,
        credits,
    })
}

// --- scenarios -------------------------------------------------------------------------

pub const SCENARIO_GRANT_EVERY_HOURS: f64 = 168.0;
pub const SCENARIO_GRANT_LIFETIME_HOURS: f64 = 168.0;

fn first_events(list: &[Redeemed]) -> Value {
    Value::Array(
        list.iter()
            .take(20)
            .map(|e| json!({ "id": e.id, "at": js::iso(e.at) }))
            .collect(),
    )
}

fn truthy_time(t: Option<f64>) -> Value {
    match t.filter(|t| *t != 0.0 && !t.is_nan()) {
        Some(t) => json!(js::iso(t)),
        None => Value::Null,
    }
}

fn scenario_view(r: &SimResult) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("outageHours".into(), json!(r.outage_hours));
    m.insert("firstOutageAt".into(), truthy_time(r.first_outage_at));
    m.insert("creditsUsed".into(), json!(r.credits_used));
    m.insert("creditsLapsing".into(), json!(r.credits_expired));
    m.insert("redemptions".into(), first_events(&r.redemptions));
    m.insert("lapses".into(), first_events(&r.lapses));
    m
}

/// `scenarioRuns(supply, rate, { now, horizonHours, maxRunwayPercent })`: what-if runs for
/// people and agents reading the forecast. They never set the state.
pub fn scenario_runs(
    supply: &[Supply],
    rate: f64,
    now: f64,
    horizon_hours: f64,
    max_runway_percent: f64,
) -> Map<String, Value> {
    let mut grants = Vec::new();
    let mut t = now + SCENARIO_GRANT_EVERY_HOURS * HOUR;
    while t < now + horizon_hours * HOUR {
        grants.push(t);
        t += SCENARIO_GRANT_EVERY_HOURS * HOUR;
    }
    let constant = |_| rate;
    let opts = SimOptions::new(now, horizon_hours, true, max_runway_percent);
    let no_credits: Vec<Supply> = supply
        .iter()
        .map(|a| Supply {
            credits: Vec::new(),
            ..a.clone()
        })
        .collect();
    let natural = simulate(
        &no_credits,
        &constant,
        &SimOptions {
            use_credits: false,
            ..opts
        },
    );
    let banked = simulate(supply, &constant, &opts);
    let granted = simulate(
        supply,
        &constant,
        &SimOptions {
            grants: &grants,
            grant_lifetime_hours: SCENARIO_GRANT_LIFETIME_HOURS,
            ..opts
        },
    );
    let mut weekly = scenario_view(&granted);
    weekly.insert(
        "assumption".into(),
        json!(format!(
            "one new credit every {} days, each valid {} days",
            js::number_to_string(SCENARIO_GRANT_EVERY_HOURS / 24.0),
            js::number_to_string(SCENARIO_GRANT_LIFETIME_HOURS / 24.0)
        )),
    );
    weekly.insert("creditsGranted".into(), json!(granted.credits_granted));
    let mut m = Map::new();
    m.insert("horizonHours".into(), json!(horizon_hours));
    m.insert(
        "unitsPerHour".into(),
        js::number_value(js::round(rate * 1e5) / 1e5),
    );
    m.insert("naturalOnly".into(), Value::Object(scenario_view(&natural)));
    m.insert(
        "bankedCredits".into(),
        Value::Object(scenario_view(&banked)),
    );
    m.insert("weeklyGrant".into(), Value::Object(weekly));
    m
}

// --- pace plan ---------------------------------------------------------------------------

pub struct PaceConstants {
    pub max_factor: f64,
    pub burst: f64,
    pub grid_steps: f64,
    pub precision: f64,
    pub tie: f64,
    pub reuse_minutes: f64,
    pub ladder: [f64; 7],
}

pub const PACE: PaceConstants = PaceConstants {
    max_factor: 10.0,
    burst: 100.0,
    grid_steps: 40.0,
    precision: 0.002,
    tie: 0.01,
    reuse_minutes: 10.0,
    ladder: [0.5, 1.0, 1.5, 2.0, 3.0, 5.0, 10.0],
};
/// Bumped whenever the plan's shape or meaning changes.
const PACE_FORMAT: f64 = 2.0;

fn pace_constants_value() -> Value {
    json!({
        "maxFactor": PACE.max_factor, "burst": PACE.burst, "gridSteps": PACE.grid_steps,
        "precision": PACE.precision, "tie": PACE.tie, "reuseMinutes": PACE.reuse_minutes,
        "ladder": PACE.ladder,
    })
}

#[derive(Clone, Debug)]
struct PaceRun {
    units_per_hour: Option<f64>,
    used_units: f64,
    credits_used: f64,
    credits_lapsing: f64,
    short_hours: f64,
    first_short_at: Option<f64>,
    run: SimResult,
}

struct PaceStep {
    credits: f64,
    reachable: bool,
    units_per_hour: Option<f64>,
    spent_by: Option<f64>,
}

struct PacePlan {
    horizon_hours: f64,
    natural_units_per_hour: f64,
    demand_units_per_hour: f64,
    credits: f64,
    max_units: f64,
    max_credits_lapsing: f64,
    at_demand: PaceRun,
    full_use: Option<PaceRun>,
    then: Option<(f64, f64)>,
    steps: Vec<PaceStep>,
    curve: Vec<PaceRun>,
    no_gap: PaceRun,
}

/// `demand || 0`.
fn or_zero(x: f64) -> f64 {
    if x == 0.0 || x.is_nan() {
        0.0
    } else {
        x
    }
}

/// `pacePlan(supply, demand, { now, horizonHours, maxRunwayPercent })`: how fast to use a
/// provider to get the most out of it.
fn pace_plan(
    supply: &[Supply],
    demand: f64,
    now: f64,
    horizon_hours: f64,
    max_runway_percent: f64,
) -> Option<PacePlan> {
    let natural = natural_units_per_hour(supply);
    if supply.is_empty() || natural.partial_cmp(&0.0) != Some(Ordering::Greater) {
        return None;
    }
    let run = run_hours(supply, horizon_hours);
    let end = now + run * HOUR;
    let credits = supply
        .iter()
        .flat_map(|a| a.credits.iter().filter(|c| **c <= end))
        .count() as f64;
    let opts = SimOptions::new(now, run, true, max_runway_percent);
    let eps = PACE.precision * natural;
    let max_rate = PACE.max_factor * natural;
    let step = max_rate / PACE.grid_steps;
    let view_fn = |rate: &dyn Fn(f64) -> f64, constant: Option<f64>| -> PaceRun {
        let x = simulate(supply, rate, &opts);
        let mut asked = 0.0;
        let mut h = 0.0;
        while h < run {
            asked += rate(now + h * HOUR);
            h += 1.0;
        }
        let done = asked - x.unmet_units;
        PaceRun {
            units_per_hour: constant,
            used_units: done + x.remaining_units,
            credits_used: x.credits_used,
            credits_lapsing: x.credits_expired,
            short_hours: x.outage_hours,
            first_short_at: x.first_outage_at,
            run: x,
        }
    };
    let view = |rate: f64| view_fn(&|_| rate, Some(rate));
    let max = view(PACE.burst * natural);
    let full = |v: &PaceRun| v.used_units >= max.used_units * (1.0 - PACE.tie);
    // Lowest rate on a grid where ok holds, refined by bisection.
    let lowest = |ok: &dyn Fn(&PaceRun) -> bool, from: f64| -> Option<PaceRun> {
        let (mut lo, mut hi) = (from, None);
        let mut r = js::max([from, step]);
        while r <= max_rate + 1e-12 {
            if ok(&view(r)) {
                hi = Some(r);
                break;
            }
            lo = r;
            r += step;
        }
        let mut hi = hi?;
        while hi - lo > eps {
            let mid = (lo + hi) / 2.0;
            if ok(&view(mid)) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        Some(view(hi))
    };
    let full_use = lowest(&full, 0.0);
    let mut steps = Vec::new();
    let mut from = 0.0;
    let mut k = 1.0;
    while k <= credits {
        match lowest(&|x: &PaceRun| x.credits_lapsing <= credits - k, from) {
            None => {
                let mut j = k;
                while j <= credits {
                    steps.push(PaceStep {
                        credits: j,
                        reachable: false,
                        units_per_hour: None,
                        spent_by: None,
                    });
                    j += 1.0;
                }
                break;
            }
            Some(v) => {
                let uph = v.units_per_hour.unwrap();
                steps.push(PaceStep {
                    credits: k,
                    reachable: true,
                    units_per_hour: Some(uph),
                    spent_by: v.run.redemptions.last().map(|e| e.at),
                });
                from = js::max([0.0, uph - step]);
            }
        }
        k += 1.0;
    }
    // Highest steady pace that never runs out (0 never does).
    let (mut a, mut b) = (0.0, max_rate);
    if view(b).short_hours == 0.0 {
        a = b;
    } else {
        while b - a > eps {
            let mid = (a + b) / 2.0;
            if view(mid).short_hours == 0.0 {
                a = mid;
            } else {
                b = mid;
            }
        }
    }
    let then = full_use.as_ref().and_then(|fu| {
        let last = fu.run.redemptions.last()?;
        let from = last.at + HOUR;
        let fu_rate = fu.units_per_hour.unwrap();
        let plan = |r: f64| view_fn(&move |t| if t < from { fu_rate } else { r }, None);
        let (mut lo, mut hi) = (0.0, fu_rate);
        while hi - lo > eps {
            let mid = (lo + hi) / 2.0;
            if plan(mid).used_units >= fu.used_units - 1e-6 {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        Some((from, js::max([hi, natural])))
    });
    Some(PacePlan {
        horizon_hours: run,
        natural_units_per_hour: natural,
        demand_units_per_hour: or_zero(demand),
        credits,
        max_units: max.used_units,
        max_credits_lapsing: max.credits_lapsing,
        at_demand: view(or_zero(demand)),
        full_use,
        then,
        steps,
        curve: PACE.ladder.iter().map(|m| view(m * natural)).collect(),
        no_gap: view(a),
    })
}

/// `paceKey(supply, maxRunwayPercent)`: reuse key for a pace plan.
pub fn pace_key(supply: &[Supply], max_runway_percent: f64) -> String {
    let mut rows: Vec<Value> = supply
        .iter()
        .map(|a| {
            json!([
                a.id,
                js::number_value(a.weight),
                a.credits.len(),
                a.credits
                    .first()
                    .map_or(Value::Null, |c| js::number_value(*c)),
            ])
        })
        .collect();
    js::sort_by(&mut rows, |x, y| {
        if x[0].as_str() < y[0].as_str() {
            -1.0
        } else {
            1.0
        }
    });
    js::digest(&json!([
        PACE_FORMAT,
        pace_constants_value(),
        js::number_value(max_runway_percent),
        rows
    ]))
}

fn iso_or_null(t: Option<f64>) -> Value {
    match t.filter(|t| t.is_finite()) {
        Some(t) => json!(js::iso(t)),
        None => Value::Null,
    }
}

/// `Number.isFinite(x) ? Math.round(x * scale) / scale : null`.
fn round_to(x: Option<f64>, scale: f64) -> Value {
    match x.filter(|x| x.is_finite()) {
        Some(x) => js::number_value(js::round(x * scale) / scale),
        None => Value::Null,
    }
}

fn pace_view(p: Option<PacePlan>, key: String, now: f64) -> Value {
    let Some(p) = p else {
        return Value::Null;
    };
    let v = |x: Option<&PaceRun>| -> Value {
        let Some(x) = x else {
            return Value::Null;
        };
        json!({
            "unitsPerHour": round_to(x.units_per_hour, 1e5),
            "usedUnits": round_to(Some(x.used_units), 1e3),
            "lostUnits": round_to(Some(js::max([0.0, p.max_units - x.used_units])), 1e3),
            "creditsUsed": x.credits_used,
            "creditsLapsing": x.credits_lapsing,
            "shortHours": x.short_hours,
            "firstShortAt": iso_or_null(x.first_short_at),
        })
    };
    json!({
        "computedAt": iso_or_null(Some(now)),
        "key": key,
        "objective": "most total usage: work done plus quota still available at the end of the run; running short loses nothing",
        "horizonHours": p.horizon_hours,
        "demandUnitsPerHour": round_to(Some(p.demand_units_per_hour), 1e5),
        "naturalUnitsPerHour": round_to(Some(p.natural_units_per_hour), 1e5),
        "credits": p.credits,
        "maxUnits": round_to(Some(p.max_units), 1e3),
        "maxCreditsLapsing": p.max_credits_lapsing,
        "atDemand": v(Some(&p.at_demand)),
        "fullUse": v(p.full_use.as_ref()),
        "noGap": v(Some(&p.no_gap)),
        "then": match p.then {
            Some((from, uph)) => json!({ "from": iso_or_null(Some(from)), "unitsPerHour": round_to(Some(uph), 1e5) }),
            None => Value::Null,
        },
        "steps": p.steps.iter().map(|s| json!({
            "credits": s.credits,
            "reachable": s.reachable,
            "unitsPerHour": round_to(s.units_per_hour, 1e5),
            "spentBy": iso_or_null(s.spent_by),
        })).collect::<Vec<_>>(),
        "curve": p.curve.iter().map(|x| v(Some(x))).collect::<Vec<_>>(),
    })
}

/// `paceFor(prev, supply, demand, opts)`: the previous plan while its key matches and it is
/// under `PACE.reuseMinutes` old, otherwise a new one.
fn pace_for(
    prev: Option<&Value>,
    supply: &[Supply],
    demand: f64,
    now: f64,
    horizon_hours: f64,
    max_runway_percent: f64,
) -> Value {
    let key = pace_key(supply, max_runway_percent);
    let computed = js::coalesce([js::get(prev, "computedAt")])
        .cloned()
        .unwrap_or(json!(""));
    let age = now - js::date_parse_value(Some(&computed));
    if js::get(prev, "key").and_then(Value::as_str) == Some(key.as_str())
        && (0.0..PACE.reuse_minutes * 60000.0).contains(&age)
    {
        return prev.cloned().unwrap_or(Value::Null);
    }
    pace_view(
        pace_plan(supply, demand, now, horizon_hours, max_runway_percent),
        key,
        now,
    )
}

/// `creditHorizonHours(supply, now, config)`.
pub fn credit_horizon_hours(supply: &[Supply], now: f64, config: &ForecastConfig) -> f64 {
    let latest = js::max(
        std::iter::once(0.0).chain(
            supply
                .iter()
                .flat_map(|a| a.credits.iter().copied().filter(|c| c.is_finite())),
        ),
    );
    let credit_hours = if latest > now {
        ((latest - now) / HOUR).ceil() + 1.0
    } else {
        0.0
    };
    js::min([config.max_horizon_days * 24.0, credit_hours])
}
// --- state ------------------------------------------------------------------------------

const LADDER: [&str; 3] = ["conserve", "normal", "spend"];

/// What `nextState` judges.
#[derive(Clone, Copy, Debug)]
pub struct Measures {
    pub unknown: bool,
    pub exhausted_now: bool,
    pub outage: f64,
    pub outage_at_margin: f64,
    pub wasted: f64,
    pub wasted_at_margin: f64,
}

/// A state as published. `since` is `None` where the JavaScript leaves it undefined.
#[derive(Clone, Debug)]
pub struct StateOut {
    pub state: Value,
    pub since: Option<Value>,
    pub pending_state: Option<String>,
    pub last_known_state: Option<Value>,
}

fn is_str(v: Option<&Value>, s: &str) -> bool {
    v.and_then(Value::as_str) == Some(s)
}

/// `nextState(prev, m, config, now)`: hysteresis, one-step moves and a minimum dwell.
// The rules stay one branch each, as the JavaScript lists them.
#[allow(clippy::if_same_then_else)]
pub fn next_state(
    prev: Option<&Value>,
    m: &Measures,
    config: &ForecastConfig,
    now: f64,
) -> StateOut {
    let prev_state = js::get(prev, "state");
    let mut want = if m.unknown {
        "unknown"
    } else if m.exhausted_now || m.outage > 0.0 {
        "conserve"
    } else if is_str(prev_state, "conserve") && m.outage_at_margin > 0.0 {
        "conserve"
    } else if m.wasted_at_margin >= config.spend_enter_units {
        "spend"
    } else if is_str(prev_state, "spend") && m.wasted >= config.spend_exit_units {
        "spend"
    } else {
        "normal"
    };
    // An unknown spell does not reset the ladder: moves count from the last known state.
    let basis = if is_str(prev_state, "unknown") {
        js::get(prev, "lastKnownState")
    } else {
        prev_state
    };
    let from = basis
        .and_then(Value::as_str)
        .and_then(|b| LADDER.iter().position(|x| *x == b));
    let to = LADDER.iter().position(|x| *x == want);
    if let (Some(from), Some(to)) = (from, to) {
        if from.abs_diff(to) > 1 && !(want == "conserve" && m.exhausted_now) {
            want = "normal";
        }
    }
    let prev_since = js::get(prev, "since");
    let since = js::truthy(prev_since).then(|| js::date_parse_value(prev_since));
    let dwelling = js::truthy(prev_state)
        && !is_str(prev_state, "unknown")
        && since.is_some_and(|s| now - s < config.min_dwell_hours * HOUR);
    let same = is_str(prev_state, want);
    if js::truthy(prev_state)
        && !same
        && dwelling
        && !(want == "conserve" && m.exhausted_now)
        && want != "unknown"
    {
        return StateOut {
            state: prev_state.cloned().unwrap(),
            since: prev_since.cloned(),
            pending_state: Some(want.into()),
            last_known_state: None,
        };
    }
    let last_known = if want == "unknown" {
        if js::truthy(prev_state) && !is_str(prev_state, "unknown") {
            prev_state.cloned()
        } else {
            js::get(prev, "lastKnownState").cloned()
        }
    } else {
        None
    }
    .filter(|v| js::truthy(Some(v)));
    StateOut {
        state: json!(want),
        since: if same {
            prev_since.cloned()
        } else {
            Some(json!(js::iso(now)))
        },
        pending_state: None,
        last_known_state: last_known,
    }
}

fn insert_state(out: &mut Map<String, Value>, st: &StateOut) {
    out.insert("state".into(), st.state.clone());
    if let Some(since) = &st.since {
        out.insert("since".into(), since.clone());
    }
    if let Some(p) = &st.pending_state {
        out.insert("pendingState".into(), json!(p));
    }
    if let Some(l) = &st.last_known_state {
        out.insert("lastKnownState".into(), l.clone());
    }
}

/// forecastClass's `round(x, d)`: rounded when finite, as is otherwise.
fn round_d(x: f64, d: i32) -> Value {
    if x.is_finite() {
        let scale = 10f64.powi(d);
        js::number_value(js::round(x * scale) / scale)
    } else {
        js::number_value(x)
    }
}

/// One forecast class (`forecastClass`). `full` is false for model windows, whose scenarios
/// and pace plan the JavaScript computes and then drops; skipping them changes nothing
/// published.
#[allow(clippy::too_many_arguments)]
fn forecast_class(
    provider: &str,
    window: &str,
    accounts: &[Value],
    replay: &Replay,
    config: &ForecastConfig,
    now: f64,
    prev: Option<&Value>,
    credits_enabled: bool,
    max_runway_percent: f64,
    semantic_weekly: bool,
    full: bool,
) -> Map<String, Value> {
    let supply = supply_for(accounts, provider, window, &config.weights, semantic_weekly);
    let replay_window = forecast_replay_window(provider, window, semantic_weekly);
    let roster: Vec<String> = supply.iter().map(|a| a.id.clone()).collect();
    let end = (now / HOUR).floor();
    let censored = censored_hours(
        &replay.marks,
        provider,
        &replay_window,
        &roster,
        end - config.lookback_hours + 1.0,
        end,
    );
    let events: Vec<&Event> = replay
        .events
        .iter()
        .filter(|e| e.provider.as_deref() == Some(provider) && e.window == replay_window)
        .collect();
    let mut first_mark = INF;
    for m in &replay.marks {
        if m.provider.as_deref() == Some(provider) && m.window == replay_window && m.at < first_mark {
            first_mark = m.at;
        }
    }
    let demand = demand_rate(
        &events,
        &censored,
        now,
        config.half_life_hours,
        config.lookback_hours,
        first_mark.is_finite().then(|| (first_mark / HOUR).floor()),
    );
    let horizon_hours = js::round(config.horizon_days * 24.0);
    let opts = SimOptions::new(now, horizon_hours, credits_enabled, max_runway_percent);
    // Where credits cannot restore the window they are neither supply nor waste.
    let sim_supply: Vec<Supply> = if credits_enabled {
        supply.clone()
    } else {
        supply
            .iter()
            .map(|a| Supply {
                credits: Vec::new(),
                ..a.clone()
            })
            .collect()
    };
    let ewma = demand.ewma;
    let base = simulate(&sim_supply, &|_| ewma, &opts);
    let margin_rate = ewma * (1.0 + config.margin);
    let at_margin = simulate(&sim_supply, &|_| margin_rate, &opts);
    let long_hours = credit_horizon_hours(&sim_supply, now, config);
    let long = if credits_enabled && long_hours > horizon_hours {
        simulate(
            &sim_supply,
            &|_| ewma,
            &SimOptions {
                horizon_hours: long_hours,
                ..opts
            },
        )
    } else {
        base.clone()
    };
    let plan_rate = js::max([ewma, demand.last24h, demand.last72h, demand.last168h])
        * (1.0 + config.plan_margin);
    let plan_evidence = demand.observed_hours >= config.plan_min_observed_hours;
    let refill_rate = js::min([ewma, demand.last24h, demand.last72h, demand.last168h]);
    let refill_provider = refills(provider);
    let plan_horizon = js::max([horizon_hours, long_hours]);
    let refill_plan = if credits_enabled && refill_provider && plan_evidence {
        let keep: HashMap<String, f64> = match js::get(js::get(prev, "creditPlan"), "drainFirst") {
            Some(Value::Array(list)) => list
                .iter()
                .map(|d| {
                    (
                        js::to_string(js::get(Some(d), "accountId")),
                        js::date_parse_value(js::get(Some(d), "until")),
                    )
                })
                .collect(),
            _ => HashMap::new(),
        };
        refill_credit_plan(
            &sim_supply,
            Paces {
                low: refill_rate,
                measured: ewma,
                high: plan_rate,
            },
            now,
            plan_horizon,
            max_runway_percent,
            Some(&keep),
        )
    } else {
        None
    };
    let plan_run =
        (!refill_provider && credits_enabled && plan_evidence && long_hours > 0.0).then(|| {
            simulate(
                &sim_supply,
                &|_| plan_rate,
                &SimOptions {
                    horizon_hours: plan_horizon,
                    ..opts
                },
            )
        });
    let plan_until = now + config.plan_window_days * 24.0 * HOUR;
    let plan_holders: Vec<String> = if let Some(p) = &refill_plan {
        p.holders.clone()
    } else if let Some(run) = &plan_run {
        let mut ids: Vec<String> = Vec::new();
        for l in run.lapses.iter().filter(|l| l.at <= plan_until + HOUR) {
            if !ids.contains(&l.id) {
                ids.push(l.id.clone());
            }
        }
        ids.sort();
        ids
    } else {
        Vec::new()
    };
    let refill = if credits_enabled && refill_provider && plan_evidence && refill_rate > 0.0 {
        let weights: Map<String, Value> = supply
            .iter()
            .map(|a| (a.id.clone(), js::number_value(a.weight)))
            .collect();
        json!({ "unitsPerHour": round_to(Some(refill_rate), 1e5), "weights": js_ordered(&weights) })
    } else {
        Value::Null
    };
    let remaining_units = supply
        .iter()
        .fold(0.0, |s, a| s + a.effective_units_left());
    // A full session/model cap blocks the weekly budget just like a full
    // weekly window. Caps are constraints, not additional supply.
    let exhausted_now = !supply.is_empty()
        && supply
            .iter()
            .all(|a| a.effective_units_left() <= 1e-12);
    let all_stale = !supply.is_empty() && supply.iter().all(|a| a.stale);
    let fresh_at: Value = if all_stale {
        js::coalesce([js::get(prev, "freshAt")])
            .cloned()
            .unwrap_or(Value::Null)
    } else {
        json!(js::iso(now))
    };
    let prev_state = js::get(prev, "state");
    // A short read outage holds the last state instead of flipping to unknown.
    let hold_stale = all_stale
        && js::truthy(prev_state)
        && !is_str(prev_state, "unknown")
        && js::truthy(Some(&fresh_at))
        && now - js::date_parse_value(Some(&fresh_at)) < config.stale_hold_minutes * 60000.0;
    let unknown = supply.is_empty()
        || (all_stale && !hold_stale)
        || (demand.observed_hours < config.min_observed_hours && !exhausted_now);
    let mut natural_refills: Vec<f64> = supply
        .iter()
        .filter(|a| !a.floating && a.used > 0.0)
        .filter_map(|a| a.reset_at.filter(|r| r.is_finite()))
        .collect();
    js::sort_by(&mut natural_refills, |x, y| x - y);
    let natural_supply = natural_units_per_hour(&supply);
    let m = Measures {
        unknown,
        exhausted_now,
        outage: base.outage_hours,
        outage_at_margin: at_margin.outage_hours,
        wasted: base.wasted_units,
        wasted_at_margin: at_margin.wasted_units,
    };
    let st = if hold_stale {
        StateOut {
            state: prev_state.cloned().unwrap(),
            since: js::get(prev, "since").cloned(),
            pending_state: None,
            last_known_state: None,
        }
    } else {
        next_state(prev, &m, config, now)
    };
    let mut credit_expiries: Vec<String> = Vec::new();
    for t in supply
        .iter()
        .flat_map(|a| a.credits.iter().copied().filter(|c| c.is_finite()))
    {
        let s = js::iso(t);
        if !credit_expiries.contains(&s) {
            credit_expiries.push(s);
        }
    }
    credit_expiries.sort();

    let mut out = Map::new();
    out.insert("provider".into(), json!(provider));
    out.insert("window".into(), json!(window));
    out.insert(
        "scope".into(),
        supply.first().map_or(Value::Null, |a| a.scope.clone()),
    );
    insert_state(&mut out, &st);
    out.insert("freshAt".into(), fresh_at);
    if hold_stale {
        out.insert("stale".into(), json!(true));
    }
    out.insert(
        "demand".into(),
        json!({
            "unitsPerHour": round_d(ewma, 5),
            "last24h": round_d(demand.last24h, 5),
            "last72h": round_d(demand.last72h, 5),
            "last168h": round_d(demand.last168h, 5),
            "currentHourUnits": round_d(demand.current_hour_units, 5),
            "observedHours": demand.observed_hours,
            "censoredHours": demand.censored_hours,
        }),
    );
    out.insert(
        "supply".into(),
        json!({
            "accounts": supply.len(),
            "weightedAccounts": round_d(supply.iter().fold(0.0, |s, a| s + a.weight), 3),
            "remainingUnits": round_d(remaining_units, 3),
            "naturalUnitsPerHour": round_d(natural_supply, 5),
            "nextRefillAt": truthy_time(natural_refills.first().copied()),
            "credits": supply.iter().map(|a| a.credits.len()).sum::<usize>(),
            "unmodeledCredits": js::number_value(supply.iter().fold(0.0, |s, a| s + a.unmodeled_credits)),
            "creditExpiries": credit_expiries,
        }),
    );
    out.insert(
        "projection".into(),
        json!({
            "horizonHours": horizon_hours,
            "creditsModeled": credits_enabled,
            "outageHours": base.outage_hours,
            "firstOutageAt": truthy_time(base.first_outage_at),
            "expiringUnits": round_d(base.expiring_units, 3),
            "creditsUsed": base.credits_used,
            "creditsExpiringUnused": base.credits_expired,
            "wastedUnits": round_d(base.wasted_units, 3),
            "outageHoursAtMargin": at_margin.outage_hours,
            "wastedUnitsAtMargin": round_d(at_margin.wasted_units, 3),
            "demandToNaturalSupply": if natural_supply > 0.0 { round_d(ewma / natural_supply, 3) } else { Value::Null },
        }),
    );
    // Out to the last credit expiry at observed demand: a flag, it does not set the state.
    out.insert(
        "creditOutlook".into(),
        json!({
            "horizonHours": plan_horizon,
            "creditsUsed": long.credits_used,
            "creditsLapsing": long.credits_expired,
            "firstOutageAt": truthy_time(long.first_outage_at),
        }),
    );
    let mut credit_plan = Map::new();
    credit_plan.insert("unitsPerHour".into(), round_d(plan_rate, 5));
    credit_plan.insert("demandFactor".into(), round_d(1.0 + config.plan_margin, 3));
    credit_plan.insert("evidence".into(), json!(plan_evidence));
    credit_plan.insert(
        "windowDays".into(),
        js::number_value(config.plan_window_days),
    );
    credit_plan.insert("holders".into(), json!(plan_holders));
    credit_plan.insert("refill".into(), refill);
    if refill_provider {
        credit_plan.insert("method".into(), json!("counterfactual"));
        credit_plan.insert("planned".into(), json!(refill_plan.is_some()));
        let (drain_first, credits) = match &refill_plan {
            Some(p) => (p.drain_first.0.clone(), p.credits.clone()),
            None => (Vec::new(), Vec::new()),
        };
        credit_plan.insert(
            "drainFirst".into(),
            Value::Array(
                drain_first
                    .iter()
                    .map(|(id, until)| json!({ "accountId": id, "until": js::iso(*until) }))
                    .collect(),
            ),
        );
        credit_plan.insert(
            "credits".into(),
            Value::Array(
                credits
                    .iter()
                    .map(|c| {
                        json!({
                            "accountId": c.account_id,
                            "expiresAt": js::iso(c.expires_at),
                            "resetAt": iso_or_null(c.reset_at),
                            "out": c.out,
                            "unitsLeft": round_d(c.units_left, 3),
                            "stale": c.stale,
                            "action": c.action,
                            "drainBy": c.drain_by.map_or(Value::Null, |t| json!(js::iso(t))),
                            "outcome": c.outcome,
                            "needUnitsPerHour": c.need_units_per_hour.map_or(Value::Null, |x| round_d(x, 5)),
                        })
                    })
                    .collect(),
            ),
        );
    } else {
        credit_plan.insert("method".into(), json!("lapse-at-plan-rate"));
    }
    out.insert("creditPlan".into(), Value::Object(credit_plan));
    if full {
        // Banked credits are modeled here even when the state run leaves them out.
        let mut scenarios = scenario_runs(&supply, ewma, now, horizon_hours, max_runway_percent);
        scenarios.insert("creditsInStateRun".into(), json!(credits_enabled));
        out.insert("scenarios".into(), Value::Object(scenarios));
        out.insert(
            "pace".into(),
            pace_for(
                js::get(prev, "pace"),
                &sim_supply,
                ewma,
                now,
                plan_horizon,
                max_runway_percent,
            ),
        );
    }
    out
}

// --- balance and the document -----------------------------------------------------------

/// `nextBalance(prev, states, config, now)`: direction and step count between the providers.
pub fn next_balance(
    prev: Option<&Value>,
    states: &Map<String, Value>,
    config: &ForecastConfig,
    now: f64,
) -> Value {
    let (a, b) = ("codex", "claude");
    let sa = states.get(a);
    let sb = states.get(b);
    let mut target: Option<&str> = None;
    let reason: String =
        if is_str(sa, "unknown") || is_str(sb, "unknown") || !js::truthy(sa) || !js::truthy(sb) {
            "insufficient-data".into()
        } else if is_str(sa, "conserve") && !is_str(sb, "conserve") {
            target = Some(b);
            format!("{a}-conserve")
        } else if is_str(sb, "conserve") && !is_str(sa, "conserve") {
            target = Some(a);
            format!("{b}-conserve")
        } else if is_str(sa, "conserve") && is_str(sb, "conserve") {
            "both-conserve".into()
        } else if is_str(sa, "spend") && is_str(sb, "normal") {
            target = Some(a);
            format!("{a}-spend")
        } else if is_str(sb, "spend") && is_str(sa, "normal") {
            target = Some(b);
            format!("{b}-spend")
        } else {
            "balanced".into()
        };
    let conserve: Vec<&str> = [a, b]
        .into_iter()
        .filter(|x| is_str(states.get(*x), "conserve"))
        .collect();
    let or_null = |k: &str| {
        js::coalesce([js::get(prev, k)])
            .cloned()
            .unwrap_or(Value::Null)
    };
    let max_steps = js::number_value(config.max_steps);
    // Missing data freezes the balance; it neither ramps nor unwinds.
    if reason == "insufficient-data" {
        return json!({
            "toward": or_null("toward"),
            "steps": js::coalesce([js::get(prev, "steps")]).cloned().unwrap_or(json!(0)),
            "maxSteps": max_steps, "target": Value::Null, "reason": reason,
            "lastStepAt": or_null("lastStepAt"), "conserve": conserve,
        });
    }
    let last = if js::truthy(js::get(prev, "lastStepAt")) {
        js::date_parse_value(js::get(prev, "lastStepAt"))
    } else {
        f64::NEG_INFINITY
    };
    let due = now - last >= config.step_hours * HOUR;
    let mut toward = js::get(prev, "toward").cloned().unwrap_or(Value::Null);
    let mut steps = js::number(js::coalesce([js::get(prev, "steps")])).unwrap_or(0.0);
    let mut last_step_at = or_null("lastStepAt");
    if due {
        if let Some(t) = target.filter(|t| is_str(Some(&toward), t) || steps == 0.0) {
            if steps < config.max_steps {
                toward = json!(t);
                steps += 1.0;
                last_step_at = json!(js::iso(now));
            }
        } else if steps > 0.0 {
            steps -= 1.0;
            last_step_at = json!(js::iso(now));
            if steps == 0.0 {
                toward = Value::Null;
            }
        }
    }
    steps = js::min([steps, config.max_steps]);
    if steps == 0.0 {
        toward = Value::Null;
    }
    json!({
        "toward": toward, "steps": js::number_value(steps), "maxSteps": max_steps,
        "target": target, "reason": reason, "lastStepAt": last_step_at, "conserve": conserve,
    })
}

fn model_window_summary(window: &str, f: &Map<String, Value>) -> Value {
    let mut w = Map::new();
    w.insert("window".into(), json!(window));
    w.insert("scope".into(), f["scope"].clone());
    for k in [
        "state",
        "since",
        "pendingState",
        "lastKnownState",
        "freshAt",
        "stale",
    ] {
        if let Some(v) = f.get(k) {
            w.insert(k.into(), v.clone());
        }
    }
    w.insert("demand".into(), f["demand"].clone());
    w.insert(
        "supply".into(),
        json!({ "remainingUnits": f["supply"]["remainingUnits"], "nextRefillAt": f["supply"]["nextRefillAt"] }),
    );
    let p = &f["projection"];
    w.insert(
        "projection".into(),
        json!({
            "creditsModeled": p["creditsModeled"], "creditsUsed": p["creditsUsed"],
            "outageHours": p["outageHours"], "firstOutageAt": p["firstOutageAt"], "wastedUnits": p["wastedUnits"],
        }),
    );
    Value::Object(w)
}

/// `buildForecast({ accounts, samples, config, now, previous, resetPolicies, resetAutomation })`.
/// `samples` must already be in time order, as the publisher reads them. `previous` is the
/// last published document (hysteresis memory).
pub fn build_forecast(
    accounts: &[Value],
    samples: &[Value],
    config: &ForecastConfig,
    now: f64,
    previous: Option<&Value>,
    reset_policies: &[ResetPolicy],
    reset_automation: bool,
) -> Value {
    let replay = replay_samples(samples, &config.weights, accounts);
    let mut ids: Vec<&str> = Vec::new();
    for a in accounts {
        if let Some(p) = js::get(Some(a), "provider").and_then(Value::as_str) {
            if !ids.contains(&p) && providers::provider(p).is_some() {
                ids.push(p);
            }
        }
    }
    ids.sort();
    let mut out = Map::new();
    for provider in ids {
        let weekly = weekly_window_id(provider).unwrap();
        let policy = reset_policies.iter().find(|p| p.provider == provider);
        let credits_enabled = reset_automation && policy.is_none_or(|p| p.enabled);
        let max_runway = policy
            .and_then(|p| p.max_runway_percent)
            .unwrap_or_else(|| default_max_runway_percent(provider));
        let prev_p = js::get(js::get(previous, "providers"), provider);
        let mut main = forecast_class(
            provider,
            weekly,
            accounts,
            &replay,
            config,
            now,
            prev_p,
            credits_enabled,
            max_runway,
            true,
            true,
        );
        // Model-specific limits are forecast separately; credits are not assumed to restore
        // them.
        let mut model_ids: Vec<String> = Vec::new();
        for a in accounts
            .iter()
            .filter(|a| is_str(js::get(Some(a), "provider"), provider))
        {
            let Some(Value::Array(ws)) = js::get(Some(a), "quotaWindows") else {
                continue;
            };
            for w in ws {
                let w = Some(w);
                let id = js::get(w, "id");
                // The main class is semantic (common weekly for Claude), not a
                // positional id. A model cap may legitimately reuse that id
                // when the provider omitted a session window.
                if js::truthy(js::get(w, "common")) {
                    continue;
                }
                if !js::finite(js::get(w, "durationSeconds")).is_some_and(|d| d >= 86400.0) {
                    continue;
                }
                let id = js::to_string(id);
                if !model_ids.contains(&id) {
                    model_ids.push(id);
                }
            }
        }
        model_ids.sort();
        let model_windows: Vec<Value> = model_ids
            .iter()
            .map(|id| {
                let prev_w = match js::get(prev_p, "modelWindows") {
                    Some(Value::Array(ws)) => {
                        ws.iter().find(|w| is_str(js::get(Some(w), "window"), id))
                    }
                    _ => None,
                };
                let f = forecast_class(
                    provider,
                    id,
                    accounts,
                    &replay,
                    config,
                    now,
                    prev_w,
                    false,
                    default_max_runway_percent(provider),
                    false,
                    false,
                );
                model_window_summary(id, &f)
            })
            .collect();
        main.insert("modelWindows".into(), Value::Array(model_windows));
        out.insert(provider.into(), Value::Object(main));
    }
    let states: Map<String, Value> = out
        .iter()
        .map(|(p, f)| (p.clone(), f["state"].clone()))
        .collect();
    let balance = next_balance(js::get(previous, "balance"), &states, config, now);
    let model_window_states: Map<String, Value> = out
        .iter()
        .map(|(p, f)| {
            let mut m = Map::new();
            for w in f["modelWindows"].as_array().into_iter().flatten() {
                let key = js::coalesce([js::get(Some(w), "scope")]).unwrap_or(&w["window"]);
                m.insert(js::to_string(Some(key)), w["state"].clone());
            }
            (p.clone(), Value::Object(m))
        })
        .collect();
    let decision = json!({
        "mode": config.mode,
        "states": states,
        "modelWindowStates": model_window_states,
        "balance": {
            "toward": balance["toward"], "steps": balance["steps"],
            "maxSteps": balance["maxSteps"], "conserve": balance["conserve"],
        },
    });
    json!({
        "format": FORECAST_FORMAT,
        "generatedAt": js::iso(now),
        "staleAfter": js::iso(now + config.stale_after_minutes * 60000.0),
        "mode": config.mode,
        "config": config.to_value(),
        "decisionDigest": js::digest(&decision),
        "decision": decision,
        "balance": balance,
        "providers": out,
    })
}

/// The provider's block of a fresh, known forecast (`lapsingHolders`, `drainFirstOf` and
/// `refillDemandOf` read only those): `None` when unknown, stale or past `staleAfter`.
fn fresh_provider<'a>(forecast: Option<&'a Value>, provider: &str, now: f64) -> Option<&'a Value> {
    let p = js::get(js::get(forecast, "providers"), provider).filter(|p| js::truthy(Some(p)))?;
    let state_unknown = js::get(Some(p), "state").and_then(Value::as_str) == Some("unknown");
    let fresh = js::date_parse_value(js::get(forecast, "staleAfter")) > now;
    (!state_unknown && !js::truthy(js::get(Some(p), "stale")) && fresh).then_some(p)
}

/// `lapsingHolders(forecast, provider, now)`: the credit planner's holders, who may redeem
/// before a lapse (reset-lifecycle.mjs `processReset`).
pub fn lapsing_holders(forecast: Option<&Value>, provider: &str, now: f64) -> Option<std::collections::HashSet<String>> {
    let p = fresh_provider(forecast, provider, now)?;
    match js::get(js::get(Some(p), "creditPlan"), "holders") {
        Some(Value::Array(h)) if !h.is_empty() => Some(h.iter().map(|x| js::to_string(Some(x))).collect()),
        _ => None,
    }
}

/// `drainFirstOf(forecast, provider, now)`: account id to credit expiry (ms) for the plan's
/// drain-first holders whose expiry is still ahead.
pub fn drain_first_of(forecast: Option<&Value>, provider: &str, now: f64) -> Option<std::collections::HashMap<String, f64>> {
    let p = fresh_provider(forecast, provider, now)?;
    let list = js::values(js::get(js::get(Some(p), "creditPlan"), "drainFirst").filter(|v| v.is_array()));
    let mut out = std::collections::HashMap::new();
    for d in list {
        let id = js::get(Some(d), "accountId").and_then(Value::as_str);
        let t = js::date_parse_value(js::get(Some(d), "until"));
        if let Some(id) = id.filter(|_| t.is_finite() && t > now) {
            // A Map built from entries keeps the last value for a repeated key.
            out.insert(id.to_string(), t);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// `refillDemandOf(forecast, provider, now)`: `{ unitsPerHour, weights }` for refill timing.
pub fn refill_demand_of(forecast: Option<&Value>, provider: &str, now: f64) -> Option<Value> {
    let p = fresh_provider(forecast, provider, now)?;
    let r = js::get(js::get(Some(p), "creditPlan"), "refill").filter(|r| js::truthy(Some(r)))?;
    let rate = js::finite(js::get(Some(r), "unitsPerHour")).filter(|u| *u > 0.0)?;
    let weights = js::coalesce([js::get(Some(r), "weights")]).cloned().unwrap_or(json!({}));
    Some(json!({ "unitsPerHour": js::number_value(rate), "weights": weights }))
}

#[cfg(test)]
mod hook_tests {
    use super::*;

    #[test]
    fn hooks_read_only_a_fresh_known_provider() {
        let now = 1_000_000.0;
        let doc = json!({
            "staleAfter": js::iso(now + 60_000.0),
            "providers": {
                "claude": { "state": "conserve", "creditPlan": {
                    "holders": ["a"],
                    "drainFirst": [{ "accountId": "a", "until": js::iso(now + 3_600_000.0) }, { "accountId": "b", "until": js::iso(now - 1.0) }, { "accountId": 5, "until": js::iso(now + 1.0) }],
                    "refill": { "unitsPerHour": 0.02 },
                } },
                "codex": { "state": "unknown", "creditPlan": { "holders": ["c"] } },
            },
        });
        assert_eq!(lapsing_holders(Some(&doc), "claude", now), Some(["a".to_string()].into()));
        assert_eq!(lapsing_holders(Some(&doc), "codex", now), None);
        assert_eq!(drain_first_of(Some(&doc), "claude", now), Some([("a".to_string(), now + 3_600_000.0)].into()));
        assert_eq!(refill_demand_of(Some(&doc), "claude", now), Some(json!({ "unitsPerHour": 0.02, "weights": {} })));
        assert_eq!(refill_demand_of(Some(&doc), "claude", now + 60_000.0), None, "stale at staleAfter");
        assert_eq!(lapsing_holders(None, "claude", now), None);
    }

    #[test]
    fn claude_weekly_supply_is_semantic_and_caps_do_not_add_capacity() {
        let accounts = vec![
            json!({
                "id": "a", "provider": "claude", "quotaWindows": [
                    {"id":"limit:0", "scope":"All models", "label":"Weekly", "used":20, "resetAt":2_000_000.0, "durationSeconds":604800, "common":true},
                    {"id":"limit:1", "scope":"Fable", "label":"Weekly", "used":100, "resetAt":2_000_000.0, "durationSeconds":604800, "common":false}
                ]
            }),
            json!({
                "id": "b", "provider": "claude", "quotaWindows": [
                    {"id":"limit:0", "scope":"5-hour", "label":"5-hour", "used":100, "resetAt":1_500_000.0, "durationSeconds":18000, "common":true},
                    {"id":"limit:1", "scope":"All models", "label":"Weekly", "used":20, "resetAt":2_000_000.0, "durationSeconds":604800, "common":true},
                    {"id":"limit:2", "scope":"Fable", "label":"Weekly", "used":0, "resetAt":2_000_000.0, "durationSeconds":604800, "common":false}
                ]
            }),
        ];
        let weights = Map::new();
        let weekly = supply_for(&accounts, "claude", "limit:1", &weights, true);
        assert_eq!(weekly.len(), 2);
        assert_eq!(weekly.iter().map(|a| a.used).collect::<Vec<_>>(), vec![20.0, 20.0]);
        assert_eq!(weekly.iter().map(|a| a.caps.len()).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(weekly.iter().map(Supply::effective_units_left).sum::<f64>(), 0.0);

        let fable = supply_for(&accounts, "claude", "limit:1", &weights, false);
        assert_eq!(fable.len(), 1, "the common weekly row must not be treated as Fable");
        assert_eq!(fable[0].id, "a");
        assert_eq!(fable[0].used, 100.0);
    }
}
