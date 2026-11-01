//! Usage value report (usage-value.mjs `buildUsageReports`): each proxied request priced at API
//! list price, the quota a dollar of each model uses fitted per provider with non-negative
//! least squares, model-specific limits, per-account value against the plan price, waste, and
//! the session usage document.
//!
//! The fits are sums over cells in the order the JavaScript creates them, so cells, accounts,
//! models and sessions keep insertion order here (`Ordered`), and every sum runs in the same
//! order as the reference.

use crate::forecast::{capacity_weight, plan_of};
use crate::js;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

pub const VALUE_FORMAT: &str = "usage-value/v1";
pub const HOUR: f64 = 3_600_000.0;
const DAY: f64 = 24.0 * HOUR;
/// Reset times jitter between reads; a move larger than this is a new window.
const RESET_JITTER_MS: f64 = HOUR;
/// Smallest fall in used percent read as a refill rather than out-of-order reads.
const REFILL_MIN_DROP: f64 = 5.0;
const TOKEN_TYPES: [&str; 4] = ["in", "cr", "cw", "out"];

// --- small JavaScript collections ---------------------------------------------------------

/// A JavaScript `Map` with string keys: insertion order, lookup by key.
#[derive(Clone, Debug)]
pub struct Ordered<V> {
    index: HashMap<String, usize>,
    items: Vec<(String, V)>,
}

impl<V> Default for Ordered<V> {
    fn default() -> Self {
        Ordered {
            index: HashMap::new(),
            items: Vec::new(),
        }
    }
}

impl<V> Ordered<V> {
    pub fn get(&self, k: &str) -> Option<&V> {
        self.index.get(k).map(|&i| &self.items[i].1)
    }

    pub fn get_mut(&mut self, k: &str) -> Option<&mut V> {
        self.index.get(k).map(|&i| &mut self.items[i].1)
    }

    pub fn contains(&self, k: &str) -> bool {
        self.index.contains_key(k)
    }

    pub fn entry(&mut self, k: &str, make: impl FnOnce() -> V) -> &mut V {
        let i = match self.index.get(k) {
            Some(&i) => i,
            None => {
                self.items.push((k.to_string(), make()));
                self.index.insert(k.to_string(), self.items.len() - 1);
                self.items.len() - 1
            }
        };
        &mut self.items[i].1
    }

    /// `map.set(k, v)`: a new key goes last, an existing one keeps its place.
    pub fn set(&mut self, k: &str, v: V) {
        match self.index.get(k) {
            Some(&i) => self.items[i].1 = v,
            None => {
                self.items.push((k.to_string(), v));
                self.index.insert(k.to_string(), self.items.len() - 1);
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.items.iter().map(|(k, v)| (k, v))
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.items.iter().map(|(_, v)| v)
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.items.iter().map(|(k, _)| k)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// A JavaScript object of numbers built by `o[k] = (o[k] ?? 0) + v`, iterated in JavaScript's
/// own-key order (integer-like keys first).
#[derive(Clone, Debug, Default)]
pub struct NumObj(Vec<(String, f64)>);

impl NumObj {
    pub fn add(&mut self, k: &str, v: f64) {
        match self.0.iter_mut().find(|(x, _)| x == k) {
            Some(e) => e.1 += v,
            None => self.0.push((k.to_string(), 0.0 + v)),
        }
    }

    pub fn get(&self, k: &str) -> Option<f64> {
        self.0.iter().find(|(x, _)| x == k).map(|(_, v)| *v)
    }

    /// `Object.entries(o)`.
    pub fn entries(&self) -> Vec<(&str, f64)> {
        let mut ints: Vec<(u32, &str, f64)> = self
            .0
            .iter()
            .filter_map(|(k, v)| array_index(k).map(|n| (n, k.as_str(), *v)))
            .collect();
        ints.sort_by_key(|(n, ..)| *n);
        ints.into_iter()
            .map(|(_, k, v)| (k, v))
            .chain(
                self.0
                    .iter()
                    .filter(|(k, _)| array_index(k).is_none())
                    .map(|(k, v)| (k.as_str(), *v)),
            )
            .collect()
    }

    /// `Object.values(o).reduce((s, v) => s + v, 0)`.
    pub fn sum(&self) -> f64 {
        self.entries().iter().fold(0.0, |s, (_, v)| s + v)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

fn array_index(k: &str) -> Option<u32> {
    let n: u32 = k.parse().ok()?;
    (n != u32::MAX && n.to_string() == k).then_some(n)
}

/// `round(v, d)` in usage-value.mjs: `null` when not finite.
fn round(v: f64, d: i32) -> Value {
    if v.is_finite() {
        let scale = 10f64.powi(d);
        js::number_value(js::round(v * scale) / scale)
    } else {
        Value::Null
    }
}

fn round_opt(v: Option<f64>, d: i32) -> Value {
    v.map_or(Value::Null, |v| round(v, d))
}

/// `perWeek(k)`: USD per full account-week from a units-per-USD rate.
fn per_week(k: f64) -> Value {
    if k > 0.0 {
        round(1.0 / k, 0)
    } else {
        Value::Null
    }
}

/// `median(xs)`: `None` (JavaScript's `null`) for none.
pub fn median(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut s = xs.to_vec();
    s.sort_by(|a, b| js::order(a - b));
    let m = s.len() >> 1;
    Some(if s.len() % 2 == 1 {
        s[m]
    } else {
        (s[m - 1] + s[m]) / 2.0
    })
}

/// `null` as JavaScript arithmetic reads it.
fn or0(x: Option<f64>) -> f64 {
    x.unwrap_or(0.0)
}

// --- configuration -----------------------------------------------------------------------

/// A `usageLedger.modelLimits` override.
#[derive(Clone, Debug, PartialEq)]
pub struct LimitOverride {
    pub kind: Option<String>,
    pub share_of_common: Option<f64>,
}

/// `valueConfig(input).config`.
#[derive(Clone, Debug)]
pub struct ValueConfig {
    pub analysis_days: f64,
    pub value_days: f64,
    pub waste_days: f64,
    pub bucket_hours: f64,
    pub report_interval_minutes: f64,
    pub min_fit_observations: f64,
    pub single_model_share: f64,
    pub plan_prices: Map<String, Value>,
    /// provider -> scope -> override, in insertion order.
    pub model_limits: Vec<(String, Vec<(String, LimitOverride)>)>,
}

impl Default for ValueConfig {
    fn default() -> Self {
        ValueConfig {
            analysis_days: 14.0,
            value_days: 7.0,
            waste_days: 30.0,
            bucket_hours: 12.0,
            report_interval_minutes: 60.0,
            min_fit_observations: 6.0,
            single_model_share: 0.95,
            plan_prices: Map::new(),
            model_limits: Vec::new(),
        }
    }
}

impl ValueConfig {
    /// The config object as `JSON.stringify` writes it.
    pub fn to_value(&self) -> Value {
        let limits: Map<String, Value> = self
            .model_limits
            .iter()
            .map(|(provider, scopes)| {
                let scopes: Map<String, Value> = scopes
                    .iter()
                    .map(|(scope, o)| {
                        let mut m = Map::new();
                        if let Some(k) = &o.kind {
                            m.insert("kind".into(), json!(k));
                        }
                        if let Some(s) = o.share_of_common {
                            m.insert("shareOfCommon".into(), js::number_value(s));
                        }
                        (scope.clone(), Value::Object(m))
                    })
                    .collect();
                (provider.clone(), Value::Object(scopes))
            })
            .collect();
        json!({
            "analysisDays": self.analysis_days,
            "valueDays": self.value_days,
            "wasteDays": self.waste_days,
            "bucketHours": self.bucket_hours,
            "reportIntervalMinutes": self.report_interval_minutes,
            "minFitObservations": self.min_fit_observations,
            "singleModelShare": self.single_model_share,
            "planPrices": self.plan_prices,
            "modelLimits": limits,
        })
    }
}

/// `valueConfig(input)`: install.json `usageLedger` over the defaults, and what was rejected.
pub fn value_config(input: Option<&Value>) -> (ValueConfig, Vec<String>) {
    let mut config = ValueConfig::default();
    let mut problems = Vec::new();
    let Some(Value::Object(m)) = input else {
        return (config, problems);
    };
    let integers: [(&str, f64, f64); 5] = [
        ("analysisDays", 1.0, 35.0),
        ("valueDays", 1.0, 35.0),
        ("wasteDays", 1.0, 90.0),
        ("bucketHours", 1.0, 48.0),
        ("reportIntervalMinutes", 5.0, 1440.0),
    ];
    for (key, min, max) in integers {
        let Some(v) = m.get(key) else {
            continue;
        };
        match js::finite(Some(v)).filter(|f| f.fract() == 0.0 && *f >= min && *f <= max) {
            Some(f) => {
                *match key {
                    "analysisDays" => &mut config.analysis_days,
                    "valueDays" => &mut config.value_days,
                    "wasteDays" => &mut config.waste_days,
                    "bucketHours" => &mut config.bucket_hours,
                    _ => &mut config.report_interval_minutes,
                } = f;
            }
            None => problems.push(format!(
                "usageLedger.{key} must be an integer from {} to {}",
                js::number_to_string(min),
                js::number_to_string(max)
            )),
        }
    }
    if let Some(p) = m.get("planPrices") {
        if let Value::Object(_) = p {
            for (k, v) in js::entries(Some(p)) {
                match js::finite(Some(v)).filter(|f| *f > 0.0) {
                    Some(_) => {
                        config.plan_prices.insert(k, v.clone());
                    }
                    None => problems.push(format!(
                        "usageLedger.planPrices.{k} must be a positive number (USD per month)"
                    )),
                }
            }
        } else {
            problems.push("usageLedger.planPrices must be an object of USD per month".into());
        }
    }
    if let Some(limits) = m.get("modelLimits") {
        if !matches!(limits, Value::Object(_)) {
            problems.push("usageLedger.modelLimits must be an object".into());
        } else {
            for (provider, scopes) in js::entries(Some(limits)) {
                if !matches!(scopes, Value::Object(_)) {
                    problems.push(format!(
                        "usageLedger.modelLimits.{provider} must be an object"
                    ));
                    continue;
                }
                for (scope, o) in js::entries(Some(scopes)) {
                    let Value::Object(om) = o else {
                        problems.push(limit_problem(&provider, &scope));
                        continue;
                    };
                    let kind = om.get("kind");
                    let share = om.get("shareOfCommon");
                    let ok = (kind.is_some() || share.is_some())
                        && kind.is_none_or(|k| {
                            k.as_str()
                                .is_some_and(|k| ["cap", "reserve", "unknown"].contains(&k))
                        })
                        && share.is_none_or(|s| js::finite(Some(s)).is_some_and(|f| f > 0.0));
                    if !ok {
                        problems.push(limit_problem(&provider, &scope));
                        continue;
                    }
                    let entry = LimitOverride {
                        kind: kind.and_then(Value::as_str).map(str::to_string),
                        share_of_common: share.and_then(Value::as_f64),
                    };
                    let slot = match config.model_limits.iter_mut().find(|(p, _)| *p == provider) {
                        Some(slot) => slot,
                        None => {
                            config.model_limits.push((provider.clone(), Vec::new()));
                            config.model_limits.last_mut().unwrap()
                        }
                    };
                    match slot.1.iter_mut().find(|(s, _)| *s == scope) {
                        Some(e) => e.1 = entry,
                        None => slot.1.push((scope, entry)),
                    }
                }
            }
        }
    }
    if config.value_days > config.analysis_days {
        problems
            .push("usageLedger.valueDays cannot exceed analysisDays; using analysisDays".into());
        config.value_days = config.analysis_days;
    }
    (config, problems)
}

fn limit_problem(provider: &str, scope: &str) -> String {
    format!("usageLedger.modelLimits.{provider}.{scope} needs kind (cap, reserve or unknown) and/or positive shareOfCommon")
}

// --- pricing -------------------------------------------------------------------------------

const PRICE_SUFFIXES: [&str; 9] = [
    "minimal", "none", "low", "medium", "high", "xhigh", "max", "ultra", "",
];

/// `name.replace(/-(\d{8}|\d{4}-\d{2}-\d{2}|minimal|none|low|medium|high|xhigh|max|ultra)$/, "")`.
fn strip_variant(name: &str) -> String {
    let all_digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    let suffix_ok = |s: &str| {
        (s.len() == 8 && all_digits(s))
            || (s.len() == 10
                && all_digits(&s[..4])
                && &s[4..5] == "-"
                && all_digits(&s[5..7])
                && &s[7..8] == "-"
                && all_digits(&s[8..]))
            || (!s.is_empty() && PRICE_SUFFIXES.contains(&s))
    };
    for (i, _) in name.match_indices('-') {
        let rest = &name[i + 1..];
        if rest.is_ascii() && suffix_ok(rest) {
            return name[..i].to_string();
        }
    }
    name.to_string()
}

/// `name.replace(/^[a-z0-9_.-]+\//, "")`.
fn strip_prefix(name: &str) -> String {
    let n = name
        .bytes()
        .take_while(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(b))
        .count();
    if n > 0 && name.as_bytes().get(n) == Some(&b'/') {
        name[n + 1..].to_string()
    } else {
        name.to_string()
    }
}

/// `priceEntry(prices, model)`: the price table's entry for a model name, exact after removing a
/// provider prefix and a dated or effort suffix.
pub fn price_entry<'a>(prices: &'a Value, model: Option<&Value>) -> Option<(String, &'a Value)> {
    let models = js::coalesce([js::get(Some(prices), "models")]);
    let raw = if js::nullish(model) {
        String::new()
    } else {
        js::to_string(model)
    }
    .to_lowercase();
    let mut names = vec![raw.clone(), strip_prefix(&raw)];
    for n in names.clone() {
        names.push(strip_variant(&n));
    }
    names.into_iter().find_map(|name| match models {
        Some(Value::Object(m)) => m.get(&name).map(|e| (name, e)),
        _ => None,
    })
}

/// API-equivalent USD for one ledger row: model, total, and by token type (in, cr, cw, out).
#[derive(Clone, Debug)]
pub struct Cost {
    pub model: String,
    pub usd: f64,
    pub by_type: [f64; 4],
}

/// `costOf(row, prices)`.
pub fn cost_of(row: &LedgerRow, prices: &Value) -> Option<Cost> {
    let (model, e) = price_entry(prices, row.model.as_ref())?;
    let [tin, tcr, tcw, tout, _] = row.tokens;
    let prompt = tin + tcr + tcw;
    let long = js::get(Some(e), "longContext");
    let use_long = js::truthy(long) && prompt > js::to_number(js::get(long, "threshold"));
    // { ...e, ...e.longContext }: a long-context key wins where present.
    let rate = |key: &str| -> Option<&Value> {
        if use_long {
            if let Some(Value::Object(l)) = long {
                if let Some(v) = l.get(key) {
                    return Some(v);
                }
            }
        }
        js::get(Some(e), key)
    };
    let m = match js::finite(js::get(Some(e), "priorityMultiplier")) {
        Some(p) if row.priority => p,
        _ => 1.0,
    };
    let per = |r: f64, n: f64| {
        let r = if r.is_nan() || r == 0.0 { 0.0 } else { r };
        (r * n * m) / 1e6
    };
    let input = js::to_number(rate("input"));
    let fallback = |key: &str, factor: f64| match rate(key) {
        Some(v) if !v.is_null() => js::to_number(Some(v)),
        _ => input * factor,
    };
    let by_type = [
        per(input, tin),
        per(fallback("cacheRead", 0.1), tcr),
        per(fallback("cacheWrite", 1.25), tcw),
        per(js::to_number(rate("output")), tout),
    ];
    Some(Cost {
        model,
        usd: by_type[0] + by_type[1] + by_type[2] + by_type[3],
        by_type,
    })
}

// --- ledger rows ---------------------------------------------------------------------------

/// One usage-ledger row, as the report reads it.
#[derive(Clone, Debug, Default)]
pub struct LedgerRow {
    /// `Date.parse(row.at)`: `NaN` when unreadable.
    pub at: f64,
    /// `String(row.at)`, for the duplicate key.
    pub at_text: String,
    /// `row.rid` when truthy.
    pub rid: Option<String>,
    /// `row.acct` unless nullish.
    pub acct: Option<String>,
    /// `String(row.ai ?? "")`.
    pub ai: String,
    /// `row.prov` (`None` for undefined).
    pub prov: Option<Value>,
    /// `row.quota.weekly`: used (finite) and resetAt (`NaN` unless a number).
    pub weekly: Option<(f64, f64)>,
    pub model: Option<Value>,
    pub priority: bool,
    /// `row[t] || 0` for in, cr, cw, out, rsn.
    pub tokens: [f64; 5],
    /// `row.sid` and `row.psid` when truthy.
    pub sid: Option<String>,
    pub psid: Option<String>,
    /// `row.client` (`None` for undefined).
    pub client: Option<Value>,
    /// `row.ok === false`.
    pub failed: bool,
    /// `row.eff` unless nullish.
    pub eff: Option<Value>,
}

impl LedgerRow {
    pub fn from_value(row: &Value) -> LedgerRow {
        let r = Some(row);
        let text = |k: &str| {
            let v = js::get(r, k);
            js::truthy(v).then(|| js::to_string(v))
        };
        let number = |k: &str| {
            let v = js::number(js::get(r, k)).unwrap_or(0.0);
            if v.is_nan() {
                0.0
            } else {
                v
            }
        };
        let weekly = js::get(js::get(r, "quota"), "weekly");
        LedgerRow {
            at: js::date_parse_value(js::get(r, "at")),
            at_text: js::to_string(js::get(r, "at")),
            rid: text("rid"),
            acct: js::get(r, "acct")
                .filter(|v| !v.is_null())
                .map(|v| js::to_string(Some(v))),
            ai: js::coalesce([js::get(r, "ai")]).map_or(String::new(), |v| js::to_string(Some(v))),
            prov: js::get(r, "prov").cloned(),
            weekly: if js::truthy(weekly) {
                js::finite(js::get(weekly, "used")).map(|used| {
                    (
                        used,
                        js::number(js::coalesce([js::get(weekly, "resetAt")])).unwrap_or(f64::NAN),
                    )
                })
            } else {
                None
            },
            model: js::get(r, "model").cloned(),
            priority: js::get(r, "tier").and_then(Value::as_str) == Some("priority"),
            tokens: [
                number("in"),
                number("cr"),
                number("cw"),
                number("out"),
                number("rsn"),
            ],
            sid: text("sid"),
            psid: text("psid"),
            client: js::get(r, "client").cloned(),
            failed: js::get(r, "ok") == Some(&Value::Bool(false)),
            eff: js::get(r, "eff").filter(|v| !v.is_null()).cloned(),
        }
    }
}

// --- quota observations --------------------------------------------------------------------

/// One reading of an account's weekly window.
#[derive(Clone, Copy, Debug)]
pub struct Reading {
    pub at: f64,
    pub used: f64,
    pub reset_at: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct Increment {
    pub at: f64,
    pub percent: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct WindowEnd {
    pub at: f64,
    pub last_used: f64,
    /// The previous reading's reset time (`NaN` when it had none).
    pub ended_at: f64,
    pub early: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Refill {
    pub at: f64,
    pub from: f64,
    pub to: f64,
}

#[derive(Clone, Debug, Default)]
pub struct QuotaIncrements {
    pub increments: Vec<Increment>,
    pub window_ends: Vec<WindowEnd>,
    pub refills: Vec<Refill>,
}

/// `quotaIncrements(stream)`: rises of an account's weekly used percentage against the
/// window's high-water mark; a moved reset date ends a window, a drop of five points or more
/// is a refill.
pub fn quota_increments(stream: &[Reading]) -> QuotaIncrements {
    let mut out = QuotaIncrements::default();
    let mut prev: Option<Reading> = None;
    let mut high = 0.0;
    for o in stream {
        if !o.at.is_finite() || !o.used.is_finite() {
            continue;
        }
        if let Some(p) = prev {
            let moved = o.reset_at.is_finite()
                && p.reset_at.is_finite()
                && (o.reset_at - p.reset_at).abs() > RESET_JITTER_MS;
            if moved {
                out.window_ends.push(WindowEnd {
                    at: o.at,
                    last_used: high,
                    ended_at: p.reset_at,
                    early: p.reset_at.is_finite() && o.at < p.reset_at - RESET_JITTER_MS,
                });
                high = o.used;
            } else if o.used <= high - REFILL_MIN_DROP {
                out.refills.push(Refill {
                    at: o.at,
                    from: high,
                    to: o.used,
                });
                high = o.used;
            } else if o.used > high {
                out.increments.push(Increment {
                    at: o.at,
                    percent: o.used - high,
                });
                high = o.used;
            }
        } else {
            high = o.used;
        }
        prev = Some(*o);
    }
    out
}
// --- fitting -------------------------------------------------------------------------------

/// A least-squares fit: coefficients and R² (`None` where the JavaScript has `null`).
#[derive(Clone, Debug, Default)]
pub struct Fit {
    pub k: Vec<f64>,
    pub r2: Option<f64>,
    pub outliers: usize,
    pub covariance: Option<Vec<Vec<f64>>>,
}

fn dot(row: &[f64], k: &[f64]) -> f64 {
    row.iter().zip(k).fold(0.0, |s, (v, kj)| s + v * kj)
}

/// `nnls(X, y, iterations)`: non-negative least squares by projected coordinate descent.
pub fn nnls(x: &[Vec<f64>], y: &[f64], iterations: usize) -> Fit {
    let n = x.len();
    let p = if n > 0 { x[0].len() } else { 0 };
    let mut xtx = vec![vec![0.0; p]; p];
    let mut xty = vec![0.0; p];
    for i in 0..n {
        for a in 0..p {
            xty[a] += x[i][a] * y[i];
            for b in 0..p {
                xtx[a][b] += x[i][a] * x[i][b];
            }
        }
    }
    let mut k = vec![0.0; p];
    for _ in 0..iterations {
        let mut change: f64 = 0.0;
        for j in 0..p {
            if xtx[j][j] <= 0.0 {
                continue;
            }
            let mut g = xty[j];
            for b in 0..p {
                g -= xtx[j][b] * k[b];
            }
            let next = js::max([0.0, k[j] + g / xtx[j][j]]);
            change = js::max([change, (next - k[j]).abs()]);
            k[j] = next;
        }
        if change < 1e-15 {
            break;
        }
    }
    let mean = if n > 0 {
        y.iter().fold(0.0, |s, v| s + v) / n as f64
    } else {
        0.0
    };
    let (mut ssr, mut sst) = (0.0, 0.0);
    for i in 0..n {
        let pred = dot(&x[i], &k);
        let e = y[i] - pred;
        ssr += e * e;
        let d = y[i] - mean;
        sst += d * d;
    }
    Fit {
        r2: (sst > 0.0).then(|| 1.0 - ssr / sst),
        k,
        outliers: 0,
        covariance: None,
    }
}

fn select<T: Clone>(xs: &[T], keep: &[bool]) -> Vec<T> {
    xs.iter()
        .zip(keep)
        .filter(|(_, k)| **k)
        .map(|(x, _)| x.clone())
        .collect()
}

/// `robustNnls(X, y)`: refit without observations more than 4 robust standard deviations out,
/// up to three times, with the robust covariance of the final fit.
pub fn robust_nnls(x: &[Vec<f64>], y: &[f64]) -> Fit {
    let mut keep = vec![true; x.len()];
    let mut fit = nnls(x, y, 5000);
    for _ in 0..3 {
        let residuals: Vec<f64> = x
            .iter()
            .zip(y)
            .map(|(row, yi)| yi - dot(row, &fit.k))
            .collect();
        let kept = select(&residuals, &keep);
        let center = or0(median(&kept));
        let deviations: Vec<f64> = kept.iter().map(|r| (r - center).abs()).collect();
        let scale = 1.4826 * or0(median(&deviations));
        let limit = js::max([4.0 * scale, 1e-9]);
        let next: Vec<bool> = residuals
            .iter()
            .map(|r| (r - center).abs() <= limit)
            .collect();
        let width = x.first().map_or(1, Vec::len);
        if next == keep || next.iter().filter(|v| **v).count() < 3.max(width) {
            break;
        }
        keep = next;
        fit = nnls(&select(x, &keep), &select(y, &keep), 5000);
    }
    let (xk, yk) = (select(x, &keep), select(y, &keep));
    fit.outliers = keep.iter().filter(|v| !**v).count();
    fit.covariance = robust_covariance(&xk, &yk, &fit.k);
    fit
}

/// `robustCovariance(X, y, k)`: robust residual scale times the inverse information matrix;
/// `None` for a singular fit.
fn robust_covariance(x: &[Vec<f64>], y: &[f64], k: &[f64]) -> Option<Vec<Vec<f64>>> {
    let p = k.len();
    if p == 0 || x.len() <= p {
        return None;
    }
    let mut m: Vec<Vec<f64>> = (0..p)
        .map(|a| {
            (0..p)
                .map(|b| x.iter().fold(0.0, |s, row| s + row[a] * row[b]))
                .chain((0..p).map(|b| if a == b { 1.0 } else { 0.0 }))
                .collect()
        })
        .collect();
    let max = js::max((0..p).map(|i| m[i][i]));
    for j in 0..p {
        let mut pivot = j;
        for i in j + 1..p {
            if m[i][j].abs() > m[pivot][j].abs() {
                pivot = i;
            }
        }
        if m[pivot][j].abs() <= max * 1e-12 {
            return None;
        }
        m.swap(j, pivot);
        let divisor = m[j][j];
        m[j] = m[j].iter().map(|v| v / divisor).collect();
        for i in 0..p {
            if i != j {
                let factor = m[i][j];
                let pivot_row = m[j].clone();
                m[i] = m[i]
                    .iter()
                    .zip(&pivot_row)
                    .map(|(v, pj)| v - factor * pj)
                    .collect();
            }
        }
    }
    let residuals: Vec<f64> = x.iter().zip(y).map(|(row, yi)| yi - dot(row, k)).collect();
    let center = or0(median(&residuals));
    let deviations: Vec<f64> = residuals.iter().map(|r| (r - center).abs()).collect();
    let scale = 1.4826 * or0(median(&deviations));
    let n = x.len() as f64;
    Some(
        m.iter()
            .map(|row| {
                row[p..]
                    .iter()
                    .map(|v| v * (scale * scale) * n / (n - p as f64))
                    .collect()
            })
            .collect(),
    )
}
// --- report --------------------------------------------------------------------------------

struct Cell {
    provider: String,
    acct: String,
    bucket: f64,
    usd: NumObj,
    ty: [f64; 4],
    units: f64,
}

struct AnalysisSession {
    sid: String,
    psid: Option<String>,
    client: Value,
    provider: String,
    first_at: f64,
    last_at: f64,
    requests: f64,
    failed: f64,
    usd: f64,
    usd_by_model: NumObj,
    efforts: Vec<Value>,
}

#[derive(Default)]
struct AccountAgg {
    requests: f64,
    failed: f64,
    usd: f64,
    usd_by_model: NumObj,
    requests_by_model: NumObj,
}

struct ModelAgg {
    provider: String,
    model: String,
    requests: f64,
    failed: f64,
    usd: f64,
    tokens: [f64; 5],
    sessions: HashSet<String>,
    parts: Parts,
}

struct Session {
    sid: String,
    client: Option<Value>,
    provider: String,
    first_at: f64,
    last_at: f64,
    requests: f64,
    usd: f64,
    usd_by_model: NumObj,
    parts: Parts,
}

struct AccountUsage {
    fit_increments: Vec<Increment>,
    increments: Vec<Increment>,
    window_ends: Vec<WindowEnd>,
    refills: Vec<Refill>,
    source: &'static str,
    fit_source: &'static str,
}

/// `{ [account]: { [model]: { usd, requests } } }` (usage-value.mjs `addPart`): the value window's
/// requests of one model or session by account, for spreading each account's plan cost.
#[derive(Default)]
struct Parts(Ordered<Ordered<(f64, f64)>>);

impl Parts {
    fn add(&mut self, acct: &str, model: &str, usd: f64) {
        let p = self.0.entry(acct, Ordered::default).entry(model, || (0.0, 0.0));
        p.0 += usd;
        p.1 += 1.0;
    }
}

/// `Object.entries(o)` over an `Ordered` built as an object: integer-like keys first.
fn own_entries<V>(o: &Ordered<V>) -> Vec<(&String, &V)> {
    let mut ints: Vec<(u32, &String, &V)> = o.iter().filter_map(|(k, v)| array_index(k).map(|n| (n, k, v))).collect();
    ints.sort_by_key(|(n, ..)| *n);
    ints.into_iter()
        .map(|(_, k, v)| (k, v))
        .chain(o.iter().filter(|(k, _)| array_index(k).is_none()))
        .collect()
}

/// What an account's plan cost is spread by: fitted quota units, API dollars, or requests.
#[derive(Clone, Copy)]
enum CostBasis {
    Quota,
    Api,
    Requests,
}

impl CostBasis {
    fn name(self) -> &'static str {
        match self {
            CostBasis::Quota => "quota",
            CostBasis::Api => "api",
            CostBasis::Requests => "requests",
        }
    }
}

/// usage-value.mjs `accountCost`: the plan price over the value window and the dollars per unit
/// of weight that make the account's requests add up to it.
struct AccountCost {
    provider: String,
    plan_usd: Option<f64>,
    basis: Option<CostBasis>,
    per_weight: Option<f64>,
}

/// What `buildUsageReports` reads besides the ledger rows.
pub struct ReportInput<'a> {
    pub samples: &'a [Value],
    /// Pool accounts: id, provider, email, auth_index, plan, quotaWindows.
    pub accounts: &'a [Value],
    pub prices: &'a Value,
    pub config: &'a ValueConfig,
    /// install.json `capacityForecast.weights`.
    pub weights: &'a Map<String, Value>,
    /// The published capacity forecast, for projected credit lapses.
    pub forecast: Option<&'a Value>,
    /// model-registry/v2, for which models a model-specific limit covers.
    pub registry: Option<&'a Value>,
    pub now: f64,
}

/// The value report (`usage-value/v1`) and the session usage document (`session-usage/v1`).
pub struct UsageReports {
    pub value_report: Value,
    pub session_usage: Value,
}

fn text(v: Option<&Value>) -> String {
    js::to_string(v)
}

fn obj(entries: Vec<(String, Value)>) -> Value {
    let m: Map<String, Value> = entries.into_iter().collect();
    let v = Value::Object(m);
    Value::Object(
        js::entries(Some(&v))
            .into_iter()
            .map(|(k, x)| (k, x.clone()))
            .collect(),
    )
}

fn sorted_strings(mut xs: Vec<String>) -> Vec<String> {
    // Array.prototype.sort() with no comparator: by UTF-16 code units (ASCII here).
    xs.sort();
    xs
}

fn min_max_hours(buckets: impl Iterator<Item = f64> + Clone, bucket_ms: f64) -> f64 {
    let max = buckets
        .clone()
        .fold(f64::NEG_INFINITY, |a, b| js::max([a, b]));
    let min = buckets.fold(f64::INFINITY, |a, b| js::min([a, b]));
    ((max - min + 1.0) * bucket_ms) / HOUR
}

/// `buildUsageReports({ rows, samples, accounts, prices, config, weights, forecast, registry,
/// now })`. `rows` are the ledger rows in file order (deduplicated here).
pub fn build_usage_reports(
    rows: impl IntoIterator<Item = LedgerRow>,
    input: &ReportInput,
) -> UsageReports {
    let ReportInput {
        samples,
        accounts,
        prices,
        config,
        weights,
        forecast,
        registry,
        now,
    } = *input;
    let bucket_ms = config.bucket_hours * HOUR;
    let analysis_from = now - config.analysis_days * DAY;
    let value_from = now - config.value_days * DAY;
    let waste_from = now - config.waste_days * DAY;
    let mut by_auth_index: HashMap<String, &Value> = HashMap::new();
    let mut by_id: HashMap<String, &Value> = HashMap::new();
    for a in accounts {
        let ai = js::get(Some(a), "auth_index");
        if js::truthy(ai) {
            by_auth_index.insert(text(ai), a);
        }
        by_id.insert(text(js::get(Some(a), "id")), a);
    }
    let provider_of = |id: Option<&str>, fallback: Option<&Value>| -> String {
        let own = id
            .and_then(|id| by_id.get(id))
            .and_then(|a| js::get(Some(a), "provider"))
            .filter(|p| !p.is_null());
        text(own.or(fallback))
    };
    let weight_of = |id: &str, provider: &str| capacity_weight(provider, id, weights);

    let mut header_streams: Ordered<Vec<Reading>> = Ordered::default();
    let mut cells: Ordered<Cell> = Ordered::default();
    fn cell_of<'c>(
        cells: &'c mut Ordered<Cell>,
        provider: &str,
        acct: &str,
        bucket: f64,
    ) -> &'c mut Cell {
        let key = format!("{provider}|{acct}|{}", js::number_to_string(bucket));
        cells.entry(&key, || Cell {
            provider: provider.into(),
            acct: acct.into(),
            bucket,
            usd: NumObj::default(),
            ty: [0.0; 4],
            units: 0.0,
        })
    }
    let mut accounts_agg: Ordered<AccountAgg> = Ordered::default();
    let mut models_agg: Ordered<ModelAgg> = Ordered::default();
    let mut sessions: Ordered<Session> = Ordered::default();
    let mut analysis_sessions: Ordered<AnalysisSession> = Ordered::default();
    let mut unattributed_requests: Vec<(String, f64, f64)> =
        vec![("codex".into(), 0.0, 0.0), ("claude".into(), 0.0, 0.0)];
    let mut unpriced: Ordered<f64> = Ordered::default();
    let mut seen: HashSet<String> = HashSet::new();
    let (mut ledger_since, mut ledger_rows, mut duplicates, mut unattributed): (
        Option<f64>,
        f64,
        f64,
        f64,
    ) = (None, 0.0, 0.0, 0.0);

    for row in rows {
        let at = row.at;
        if !at.is_finite() || at < analysis_from || at > now + HOUR {
            continue;
        }
        if let Some(rid) = &row.rid {
            if !seen.insert(format!("{rid}|{}", row.at_text)) {
                duplicates += 1.0;
                continue;
            }
        }
        ledger_rows += 1.0;
        if ledger_since.is_none_or(|s| at < s) {
            ledger_since = Some(at);
        }
        let acct: Option<String> = row.acct.clone().or_else(|| {
            by_auth_index
                .get(&row.ai)
                .and_then(|a| js::get(Some(a), "id"))
                .filter(|v| !v.is_null())
                .map(|v| text(Some(v)))
        });
        let has_acct = acct.as_deref().is_some_and(|a| !a.is_empty());
        let provider = provider_of(acct.as_deref(), row.prov.as_ref());
        if !has_acct {
            unattributed += 1.0;
        }
        if let (true, Some((used, reset_at))) = (has_acct, row.weekly) {
            header_streams
                .entry(acct.as_deref().unwrap(), Vec::new)
                .push(Reading { at, used, reset_at });
        }
        let cost = cost_of(&row, prices);
        if cost.is_none() {
            let key = text(row.model.as_ref());
            *unpriced.entry(&key, || 0.0) += 1.0;
        }
        let usd = cost.as_ref().map_or(0.0, |c| c.usd);
        let model = cost
            .as_ref()
            .map_or_else(|| text(row.model.as_ref()), |c| c.model.clone());
        if at <= now {
            if let Some(sid) = &row.sid {
                let s = analysis_sessions.entry(sid, || AnalysisSession {
                    sid: sid.clone(),
                    psid: None,
                    client: row
                        .client
                        .clone()
                        .filter(|c| !c.is_null())
                        .unwrap_or(Value::Null),
                    provider: provider.clone(),
                    first_at: at,
                    last_at: at,
                    requests: 0.0,
                    failed: 0.0,
                    usd: 0.0,
                    usd_by_model: NumObj::default(),
                    efforts: Vec::new(),
                });
                if row.psid.is_some() && s.psid.is_none() {
                    s.psid = row.psid.clone();
                }
                s.requests += 1.0;
                if row.failed {
                    s.failed += 1.0;
                }
                s.usd += usd;
                s.usd_by_model.add(&model, usd);
                if let Some(e) = &row.eff {
                    if !s.efforts.contains(e) {
                        s.efforts.push(e.clone());
                    }
                }
                s.first_at = js::min([s.first_at, at]);
                s.last_at = js::max([s.last_at, at]);
            } else {
                let a = match unattributed_requests
                    .iter_mut()
                    .find(|(p, ..)| *p == provider)
                {
                    Some(a) => a,
                    None => {
                        unattributed_requests.push((provider.clone(), 0.0, 0.0));
                        unattributed_requests.last_mut().unwrap()
                    }
                };
                a.1 += 1.0;
                a.2 += usd;
            }
        }
        if let (true, Some(cost)) = (has_acct, &cost) {
            let cell = cell_of(
                &mut cells,
                &provider,
                acct.as_deref().unwrap(),
                (at / bucket_ms).floor(),
            );
            cell.usd.add(&model, usd);
            for t in 0..4 {
                cell.ty[t] += cost.by_type[t];
            }
        }
        if at >= value_from {
            let key = acct.clone().unwrap_or_else(|| "(unattributed)".into());
            let a = accounts_agg.entry(&key, AccountAgg::default);
            a.requests += 1.0;
            if row.failed {
                a.failed += 1.0;
            }
            a.usd += usd;
            a.usd_by_model.add(&model, usd);
            a.requests_by_model.add(&model, 1.0);
            let mk = format!("{provider}|{model}");
            let m = models_agg.entry(&mk, || ModelAgg {
                provider: provider.clone(),
                model: model.clone(),
                requests: 0.0,
                failed: 0.0,
                usd: 0.0,
                tokens: [0.0; 5],
                sessions: HashSet::new(),
                parts: Parts::default(),
            });
            m.requests += 1.0;
            if row.failed {
                m.failed += 1.0;
            }
            m.usd += usd;
            m.parts.add(&key, &model, usd);
            for t in 0..5 {
                m.tokens[t] += row.tokens[t];
            }
            if let Some(sid) = &row.sid {
                m.sessions.insert(sid.clone());
                let s = sessions.entry(sid, || Session {
                    sid: sid.clone(),
                    client: row.client.clone(),
                    provider: provider.clone(),
                    first_at: at,
                    last_at: at,
                    requests: 0.0,
                    usd: 0.0,
                    usd_by_model: NumObj::default(),
                    parts: Parts::default(),
                });
                s.requests += 1.0;
                s.usd += usd;
                s.usd_by_model.add(&model, usd);
                s.parts.add(&key, &model, usd);
                s.first_at = js::min([s.first_at, at]);
                s.last_at = js::max([s.last_at, at]);
            }
        }
    }

    // Sample streams (weekly all-models window per provider) per account.
    let mut sample_streams: Ordered<Vec<Reading>> = Ordered::default();
    for s in samples {
        let s = Some(s);
        let weekly = match js::get(s, "provider").and_then(Value::as_str) {
            Some("claude") => "limit:1",
            Some("codex") => "codex:primary_window",
            _ => continue,
        };
        if js::get(s, "window").and_then(Value::as_str) != Some(weekly) {
            continue;
        }
        let at = js::date_parse_value(js::get(s, "at"));
        let Some(used) = js::finite(js::get(s, "used")) else {
            continue;
        };
        if !at.is_finite() {
            continue;
        }
        sample_streams
            .entry(&text(js::get(s, "accountId")), Vec::new)
            .push(Reading {
                at,
                used,
                reset_at: js::to_number(js::get(s, "resetAt")),
            });
    }
    for (_, list) in sample_streams
        .items
        .iter_mut()
        .chain(header_streams.items.iter_mut())
    {
        js::sort_by(list, |a, b| a.at - b.at);
    }

    let mut ids: Vec<String> = Vec::new();
    for k in sample_streams.keys().chain(header_streams.keys()) {
        if !ids.contains(k) {
            ids.push(k.clone());
        }
    }
    let empty: Vec<Reading> = Vec::new();
    let mut account_usage: Ordered<AccountUsage> = Ordered::default();
    for id in &ids {
        let headers = header_streams.get(id).unwrap_or(&empty);
        let sampled = sample_streams.get(id).unwrap_or(&empty);
        let start = headers.first().map_or(f64::INFINITY, |h| h.at);
        let from_samples = quota_increments(sampled);
        let from_headers = quota_increments(headers);
        let early: Vec<Reading> = sampled.iter().copied().filter(|o| o.at < start).collect();
        let mut fit_increments = quota_increments(&early).increments;
        fit_increments.extend(from_headers.increments.iter().copied());
        let has_samples = !sampled.is_empty();
        account_usage.entry(id, || AccountUsage {
            fit_increments,
            increments: if has_samples {
                from_samples.increments.clone()
            } else {
                from_headers.increments.clone()
            },
            window_ends: from_samples.window_ends.clone(),
            refills: if has_samples {
                from_samples.refills.clone()
            } else {
                from_headers.refills.clone()
            },
            source: if has_samples { "samples" } else { "headers" },
            fit_source: if headers.is_empty() {
                "samples"
            } else {
                "headers"
            },
        });
    }
    // Fit observations: only buckets the ledger covers (dollars known).
    let ledger_start = ledger_since.unwrap_or(f64::INFINITY);
    let fit_from = js::max([analysis_from, ledger_start]);
    for (id, u) in account_usage.iter() {
        let fallback = json!(if id.starts_with("claude") {
            "claude"
        } else {
            "codex"
        });
        let provider = provider_of(Some(id), Some(&fallback));
        for inc in &u.fit_increments {
            if inc.at < fit_from {
                continue;
            }
            let w = weight_of(id, &provider);
            cell_of(&mut cells, &provider, id, (inc.at / bucket_ms).floor()).units +=
                (w * inc.percent) / 100.0;
        }
    }
    // The newest bucket is still filling; quota reads lag requests within it.
    let current_bucket = (now / bucket_ms).floor();
    let fit_cells: Vec<&Cell> = cells
        .values()
        .filter(|c| c.bucket < current_bucket)
        .collect();

    let mut fits: Ordered<Value> = Ordered::default();
    let mut model_fits: HashMap<String, (Vec<String>, Fit)> = HashMap::new();
    let mut providers: Vec<String> = Vec::new();
    for p in fit_cells
        .iter()
        .map(|c| c.provider.clone())
        .chain(models_agg.values().map(|m| m.provider.clone()))
    {
        if !providers.contains(&p) {
            providers.push(p);
        }
    }
    for provider in &providers {
        let obs: Vec<&Cell> = fit_cells
            .iter()
            .copied()
            .filter(|c| c.provider == *provider)
            .collect();
        let mut total_usd = NumObj::default();
        for c in &obs {
            for (m, v) in c.usd.entries() {
                total_usd.add(m, v);
            }
        }
        let grand = total_usd.sum();
        let hours = if obs.is_empty() {
            0.0
        } else {
            min_max_hours(obs.iter().map(|c| c.bucket), bucket_ms)
        };
        let mut fit: Vec<(String, Value)> = vec![
            ("observations".into(), json!(obs.len())),
            ("hours".into(), js::number_value(hours)),
            ("usd".into(), round(grand, 2)),
            (
                "units".into(),
                round(obs.iter().fold(0.0, |s, c| s + c.units), 4),
            ),
        ];
        if (obs.len() as f64) < config.min_fit_observations || hours < 24.0 || grand <= 0.0 {
            fit.push(("status".into(), json!("insufficient-data")));
            fit.push((
                "note".into(),
                json!(format!(
                    "Needs at least {} account-buckets over 24 hours with priced requests; has {} over {} h.",
                    js::number_to_string(config.min_fit_observations),
                    obs.len(),
                    js::number_to_string(hours)
                )),
            ));
            fits.set(provider, Value::Object(fit.into_iter().collect()));
            continue;
        }
        let y: Vec<f64> = obs.iter().map(|c| c.units).collect();
        let pooled = robust_nnls(
            &obs.iter().map(|c| vec![c.usd.sum()]).collect::<Vec<_>>(),
            &y,
        );
        // Models under 1% of the provider's dollars share one column.
        let all_models: Vec<(&str, f64)> = total_usd.entries();
        let models = sorted_strings(
            all_models
                .iter()
                .filter(|(_, v)| *v >= 0.01 * grand)
                .map(|(m, _)| m.to_string())
                .collect(),
        );
        let mut cols = models.clone();
        if models.len() < all_models.len() {
            cols.push("(other)".into());
        }
        let x_model: Vec<Vec<f64>> = obs
            .iter()
            .map(|c| {
                cols.iter()
                    .map(|m| {
                        if m == "(other)" {
                            c.usd
                                .entries()
                                .iter()
                                .filter(|(k, _)| !models.iter().any(|x| x == k))
                                .fold(0.0, |s, (_, v)| s + v)
                        } else {
                            or0(c.usd.get(m))
                        }
                    })
                    .collect()
            })
            .collect();
        let per_model = robust_nnls(&x_model, &y);
        let per_type = robust_nnls(&obs.iter().map(|c| c.ty.to_vec()).collect::<Vec<_>>(), &y);
        // Single-model check: buckets where one model is nearly all of an account's dollars.
        let mut single: Vec<(String, Vec<f64>)> = Vec::new();
        for c in &obs {
            let total = c.usd.sum();
            if total <= 0.0 || c.units <= 0.0 {
                continue;
            }
            let mut entries = c.usd.entries();
            js::sort_by(&mut entries, |a, b| b.1 - a.1);
            let (m, v) = entries[0];
            if v / total >= config.single_model_share {
                match single.iter_mut().find(|(x, _)| x == m) {
                    Some(e) => e.1.push(total / c.units),
                    None => single.push((m.to_string(), vec![total / c.units])),
                }
            }
        }
        let other_usd = grand - models.iter().fold(0.0, |s, x| s + or0(total_usd.get(x)));
        fit.push(("status".into(), json!("ok")));
        fit.push((
            "pooled".into(),
            json!({
                "usdPerAccountWeek": per_week(pooled.k[0]),
                "unitsPerUsd": pooled.k[0],
                "r2": round_opt(pooled.r2, 3),
                "outliers": pooled.outliers,
            }),
        ));
        fit.push((
            "perModel".into(),
            json!({
                "r2": round_opt(per_model.r2, 3),
                "outliers": per_model.outliers,
                "models": obj(cols.iter().enumerate().map(|(i, m)| (m.clone(), json!({
                    "usdPerAccountWeek": per_week(per_model.k[i]),
                    "unitsPerUsd": per_model.k[i],
                    "usd": round(if m == "(other)" { other_usd } else { or0(total_usd.get(m)) }, 2),
                }))).collect()),
            }),
        ));
        fit.push((
            "perTokenType".into(),
            json!({
                "r2": round_opt(per_type.r2, 3),
                "outliers": per_type.outliers,
                "types": obj(TOKEN_TYPES.iter().enumerate().map(|(i, t)| (t.to_string(), json!({
                    "unitsPerUsd": per_type.k[i],
                    "usdPerAccountWeek": per_week(per_type.k[i]),
                }))).collect()),
            }),
        ));
        fit.push((
            "singleModel".into(),
            obj(single
                .into_iter()
                .map(|(m, xs)| {
                    let usd = median(&xs).map_or(Value::Null, |v| round(v, 0));
                    (m, json!({ "buckets": xs.len(), "usdPerAccountWeek": usd }))
                })
                .collect()),
        ));
        fits.set(provider, Value::Object(fit.into_iter().collect()));
        model_fits.insert(provider.clone(), (cols, per_model));
    }
    let fits_value = obj(fits.iter().map(|(k, v)| (k.clone(), v.clone())).collect());

    // Model-specific limits (a weekly window for some models only).
    let coverage_of = |stream: &[Reading]| -> HashSet<i64> {
        let first = stream.iter().fold(f64::INFINITY, |t, s| js::min([t, s.at]));
        stream
            .iter()
            .filter(|s| s.at > first && first <= (s.at / bucket_ms).floor() * bucket_ms)
            .map(|s| (s.at / bucket_ms).floor() as i64)
            .collect()
    };
    let common_coverage: HashMap<&str, HashSet<i64>> = ids
        .iter()
        .map(|id| {
            let mut stream: Vec<Reading> = header_streams.get(id).unwrap_or(&empty).clone();
            stream.extend(sample_streams.get(id).unwrap_or(&empty).iter().copied());
            stream.retain(|s| s.at <= now);
            (id.as_str(), coverage_of(&stream))
        })
        .collect();
    let stream_key = |p: Option<&Value>, a: Option<&Value>, w: Option<&Value>| {
        let v = |x: Option<&Value>| x.cloned().unwrap_or(Value::Null);
        js::stringify(&json!([v(p), v(a), v(w)]))
    };
    let mut window_streams: Ordered<Vec<Reading>> = Ordered::default();
    for s in samples {
        let s = Some(s);
        let at = js::date_parse_value(js::get(s, "at"));
        let Some(used) = js::finite(js::get(s, "used")) else {
            continue;
        };
        if !at.is_finite() || at > now {
            continue;
        }
        let key = stream_key(
            js::get(s, "provider"),
            js::get(s, "accountId"),
            js::get(s, "window"),
        );
        window_streams.entry(&key, Vec::new).push(Reading {
            at,
            used,
            reset_at: js::finite(js::get(s, "resetAt")).unwrap_or(f64::NAN),
        });
    }
    struct Limit<'v> {
        provider: Option<&'v Value>,
        scope: Option<&'v Value>,
        window: Option<&'v Value>,
        accounts: Vec<String>,
    }
    let mut limits: Ordered<Limit> = Ordered::default();
    for a in accounts {
        let Some(Value::Array(ws)) = js::get(Some(a), "quotaWindows") else {
            continue;
        };
        for w in ws {
            let w = Some(w);
            if js::get(w, "common") != Some(&Value::Bool(false))
                || !js::finite(js::get(w, "durationSeconds")).is_some_and(|d| d >= 86400.0)
            {
                continue;
            }
            let provider = js::get(Some(a), "provider");
            let key = stream_key(provider, js::get(w, "scope"), js::get(w, "id"));
            let limit = limits.entry(&key, || Limit {
                provider,
                scope: js::get(w, "scope"),
                window: js::get(w, "id"),
                accounts: Vec::new(),
            });
            let id = text(js::get(Some(a), "id"));
            if !limit.accounts.contains(&id) {
                limit.accounts.push(id);
            }
        }
    }
    let pooled_rate = |provider: &str| {
        js::number(js::get(
            js::get(fits.get(provider), "pooled"),
            "unitsPerUsd",
        ))
    };
    let mut model_limits: Vec<Value> = Vec::new();
    for limit in limits.values() {
        let provider = text(limit.provider);
        let scope_text = text(limit.scope).to_lowercase();
        let models = sorted_strings(
            js::entries(js::get(registry, "identities"))
                .into_iter()
                .filter(|(_, m)| {
                    js::get(Some(m), "provider") == limit.provider
                        && text(js::get(Some(m), "family")).to_lowercase() == scope_text
                })
                .map(|(id, _)| id)
                .collect(),
        );
        let model_set: HashSet<&str> = models.iter().map(String::as_str).collect();
        let mut units: HashMap<String, f64> = HashMap::new();
        let mut coverage: HashMap<&str, HashSet<i64>> = HashMap::new();
        for id in &limit.accounts {
            let mut stream = window_streams
                .get(&stream_key(limit.provider, Some(&json!(id)), limit.window))
                .cloned()
                .unwrap_or_default();
            js::sort_by(&mut stream, |a, b| a.at - b.at);
            coverage.insert(id.as_str(), coverage_of(&stream));
            for inc in quota_increments(&stream).increments {
                if inc.at < fit_from {
                    continue;
                }
                let key = format!(
                    "{id}|{}",
                    js::number_to_string((inc.at / bucket_ms).floor())
                );
                *units.entry(key).or_insert(0.0) += weight_of(id, &provider) * inc.percent / 100.0;
            }
        }
        let scope_usd = |c: &Cell| {
            c.usd.entries().iter().fold(0.0, |s, (m, usd)| {
                s + if model_set.contains(m) { *usd } else { 0.0 }
            })
        };
        let obs: Vec<&Cell> = fit_cells
            .iter()
            .copied()
            .filter(|c| {
                c.provider == provider
                    && limit.accounts.contains(&c.acct)
                    && scope_usd(c) > 0.0
                    && coverage
                        .get(c.acct.as_str())
                        .is_some_and(|s| s.contains(&(c.bucket as i64)))
                    && common_coverage
                        .get(c.acct.as_str())
                        .is_some_and(|s| s.contains(&(c.bucket as i64)))
            })
            .collect();
        let hours = if obs.is_empty() {
            0.0
        } else {
            min_max_hours(obs.iter().map(|c| c.bucket), bucket_ms)
        };
        let mut kind = "unknown";
        let mut share: Option<f64> = None;
        let mut share_basis: Option<&str> = None;
        let mut source = "unknown";
        let mut note: Option<String> = None;
        let own = model_fits.get(&provider);
        let (mut window_rate, mut model_rate): (Option<f64>, Option<f64>) = (None, None);
        if (obs.len() as f64) < config.min_fit_observations || hours < 24.0 {
            note = Some(format!(
                "Needs at least {} account-buckets over 24 hours with this scope's priced requests and quota readings; has {} over {} h.",
                js::number_to_string(config.min_fit_observations),
                obs.len(),
                js::number_to_string(hours)
            ));
        } else if own.is_none_or(|(_, f)| f.covariance.is_none()) {
            note = Some("The all-models fit cannot distinguish the scope's own rate.".into());
        } else {
            let (cols, own_fit) = own.unwrap();
            let cov = own_fit.covariance.as_ref().unwrap();
            let total = obs.iter().fold(0.0, |s, c| s + scope_usd(c));
            let shares: Vec<f64> = cols
                .iter()
                .map(|m| {
                    if model_set.contains(m.as_str()) {
                        obs.iter().fold(0.0, |s, c| s + or0(c.usd.get(m))) / total
                    } else {
                        0.0
                    }
                })
                .collect();
            let mapped = shares.iter().fold(0.0, |s, v| s + v);
            let k = shares
                .iter()
                .enumerate()
                .fold(0.0, |s, (j, v)| s + v * own_fit.k[j]);
            let variance = shares.iter().enumerate().fold(0.0, |s, (i, a)| {
                s + shares
                    .iter()
                    .enumerate()
                    .fold(0.0, |v, (j, b)| v + a * b * cov[i][j])
            });
            let se = js::max([0.0, variance]).sqrt();
            let w = robust_nnls(
                &obs.iter().map(|c| vec![scope_usd(c)]).collect::<Vec<_>>(),
                &obs.iter()
                    .map(|c| {
                        or0(units
                            .get(&format!("{}|{}", c.acct, js::number_to_string(c.bucket)))
                            .copied())
                    })
                    .collect::<Vec<_>>(),
            )
            .k[0];
            if mapped > 1.0 - 1e-9 && w > 1e-12 {
                window_rate = Some(w);
                model_rate = Some(k);
                kind = if k > se + 1e-12 { "cap" } else { "reserve" };
                share_basis = Some(if kind == "cap" { "model" } else { "pooled" });
                let k_ref = if kind == "cap" {
                    Some(k)
                } else {
                    pooled_rate(&provider)
                };
                match k_ref.filter(|r| *r > 1e-12) {
                    Some(r) => {
                        share = Some(r / w);
                        source = "measured";
                    }
                    None => {
                        kind = "unknown";
                        share_basis = None;
                    }
                }
            }
            if source == "unknown" {
                note = Some(
                    "No positive, identifiable model-window rate and common reference rate.".into(),
                );
            }
        }
        let mut share_value = share.map_or(Value::Null, |s| round(s, 4));
        let mut kind_value = json!(kind);
        let override_ = config
            .model_limits
            .iter()
            .find(|(p, _)| *p == provider)
            .and_then(|(_, scopes)| {
                scopes
                    .iter()
                    .find(|(name, _)| name.to_lowercase() == scope_text)
            })
            .map(|(_, o)| o);
        if let Some(o) = override_ {
            if let Some(k) = &o.kind {
                kind_value = json!(k);
            }
            if let Some(s) = o.share_of_common {
                share_value = js::number_value(s);
            }
            source = "config";
            note = Some("Configured in usageLedger.modelLimits.".into());
            let kind_now = kind_value.as_str().unwrap_or("");
            share_basis = match kind_now {
                "cap" => Some("model"),
                "reserve" => Some("pooled"),
                _ => None,
            };
            if o.kind.as_deref().is_some_and(|k| !k.is_empty()) && o.share_of_common.is_none() {
                let reference = if kind_now == "cap" {
                    model_rate
                } else {
                    pooled_rate(&provider)
                };
                share_value = match (reference, window_rate) {
                    (Some(r), Some(w)) if r > 1e-12 && w > 1e-12 => round(r / w, 4),
                    _ => Value::Null,
                };
            }
            if kind_now == "unknown" {
                share_value = Value::Null;
            }
        }
        let v = |x: Option<&Value>| x.cloned().unwrap_or(Value::Null);
        model_limits.push(json!({
            "provider": v(limit.provider), "scope": v(limit.scope), "window": v(limit.window), "models": models,
            "kind": kind_value, "shareOfCommon": share_value, "shareBasis": share_basis, "source": source,
            "observations": obs.len(), "note": note,
        }));
    }

    // Units per USD for a model: its own fitted rate when it has one (and the per-model fit
    // explains at least as much as the pooled one), else pooled.
    let rate_for = |provider: &str, model: &str| -> Option<f64> {
        let f = fits.get(provider);
        if js::get(f, "status").and_then(Value::as_str) != Some("ok") {
            return None;
        }
        let per_model = js::get(f, "perModel");
        let models = js::get(per_model, "models");
        let own = js::coalesce([js::get(models, model)]).or_else(|| js::get(models, "(other)"));
        let r2 = |x: Option<&Value>| js::number(js::coalesce([js::get(x, "r2")])).unwrap_or(-1.0);
        let own_rate = js::number(js::get(own, "unitsPerUsd"));
        if own.is_some()
            && own_rate.is_some_and(|k| k > 0.0)
            && r2(per_model) >= r2(js::get(f, "pooled"))
        {
            return own_rate;
        }
        pooled_rate(provider).filter(|k| *k > 0.0)
    };
    let est_units = |provider: &str, usd_by_model: &NumObj| -> Option<f64> {
        let mut total = 0.0;
        for (m, v) in usd_by_model.entries() {
            total += rate_for(provider, m)? * v;
        }
        Some(total)
    };

    // Accounts over the value window, or since the ledger's first row when it is younger.
    let window_start = match ledger_since {
        None => value_from,
        Some(s) => js::max([value_from, s]),
    };
    let window_days = (now - window_start) / DAY;
    let enough = window_days >= 1.0;
    let weeks = window_days / 7.0;
    let plan_price = |id: &str, provider: &str| -> (Option<f64>, Value) {
        let account = by_id.get(id).copied();
        let plan_field = js::get(account, "plan");
        let reported = js::get(plan_field, "price");
        let usd = js::finite(js::get(reported, "usdPerMonth")).filter(|v| *v > 0.0);
        let reported_source = js::get(reported, "source");
        if usd.is_some() && reported_source.and_then(Value::as_str) == Some("provider") {
            return (usd, json!("provider"));
        }
        let plan: Option<String> = match js::get(plan_field, "plan").filter(|v| !v.is_null()) {
            Some(p) => js::truthy(Some(p)).then(|| text(Some(p))),
            None => plan_of(id),
        };
        let prices = &config.plan_prices;
        let v = prices
            .get(id)
            .or_else(|| {
                plan.as_ref()
                    .and_then(|p| prices.get(&format!("{provider}:{p}")))
            })
            .or_else(|| prices.get(provider));
        if let Some(v) = js::finite(v) {
            return (Some(v), json!("config"));
        }
        match usd {
            Some(u) => (
                Some(u),
                js::coalesce([reported_source])
                    .cloned()
                    .unwrap_or(json!("list")),
            ),
            None => (None, Value::Null),
        }
    };
    // Actual cost per account (usage-value.mjs `accountCost`, `actualOf`).
    let cost_weight = |c: &AccountCost, m: &str, usd: f64, requests: f64| -> f64 {
        match c.basis {
            Some(CostBasis::Quota) => rate_for(&c.provider, m).unwrap_or(0.0) * usd,
            Some(CostBasis::Api) => usd,
            Some(CostBasis::Requests) => requests,
            None => 0.0,
        }
    };
    let mut account_cost: HashMap<String, AccountCost> = HashMap::new();
    for a in accounts {
        let id = text(js::get(Some(a), "id"));
        let provider = text(js::get(Some(a), "provider"));
        let agg = accounts_agg.get(&id);
        let (monthly, _) = plan_price(&id, &provider);
        let plan_usd = monthly.map(|m| m * 12.0 / 365.0 * window_days);
        let entries = agg.map(|g| g.usd_by_model.entries()).unwrap_or_default();
        let mut c = AccountCost { provider, plan_usd, basis: None, per_weight: None };
        let mut total = 0.0;
        if let Some(g) = agg.filter(|g| g.requests != 0.0 && !g.requests.is_nan()) {
            let rates: Vec<Option<f64>> = entries.iter().map(|(m, _)| rate_for(&c.provider, m)).collect();
            c.basis = Some(
                if rates.iter().all(Option::is_some) && entries.iter().zip(&rates).any(|((_, v), k)| k.unwrap() * v > 0.0) {
                    CostBasis::Quota
                } else if g.usd > 0.0 {
                    CostBasis::Api
                } else {
                    CostBasis::Requests
                },
            );
            total = entries.iter().fold(0.0, |s, (m, v)| s + cost_weight(&c, m, *v, g.requests_by_model.get(m).unwrap_or(0.0)));
        }
        c.per_weight = match plan_usd {
            Some(p) if total > 0.0 => Some(p / total),
            _ => None,
        };
        account_cost.insert(id, c);
    }
    // (dollars, costed requests, uncosted requests) of a model's or session's parts.
    let actual_of = |parts: &Parts| -> (Option<f64>, f64, f64) {
        let (mut usd, mut costed, mut uncosted) = (0.0, 0.0, 0.0);
        for (acct, models) in own_entries(&parts.0) {
            for (m, &(p_usd, p_requests)) in own_entries(models) {
                match account_cost.get(acct.as_str()).and_then(|c| c.per_weight.map(|w| (c, w))) {
                    None => uncosted += p_requests,
                    Some((c, w)) => {
                        usd += cost_weight(c, m, p_usd, p_requests) * w;
                        costed += p_requests;
                    }
                }
            }
        }
        ((costed != 0.0).then_some(usd), costed, uncosted)
    };
    let empty_agg = AccountAgg::default();
    let account_rows: Vec<Value> = accounts
        .iter()
        .map(|a| {
            let id = text(js::get(Some(a), "id"));
            let provider = text(js::get(Some(a), "provider"));
            let agg = accounts_agg.get(&id).unwrap_or(&empty_agg);
            let usage = account_usage.get(&id);
            let weight = weight_of(&id, &provider);
            let used_percent = usage
                .map(|u| u.increments.as_slice())
                .unwrap_or(&[])
                .iter()
                .filter(|i| i.at >= window_start)
                .fold(0.0, |s, i| s + i.percent);
            let used_units = (weight * used_percent) / 100.0;
            let predicted = est_units(&provider, &agg.usd_by_model);
            let (monthly, plan_price_source) = plan_price(&id, &provider);
            let plan_per_window = monthly.map(|m| ((m * 12.0) / 52.0) * weeks);
            let mut row = Map::new();
            row.insert("id".into(), json!(id));
            if let Some(e) = js::get(Some(a), "email") {
                row.insert("email".into(), e.clone());
            }
            if let Some(p) = js::get(Some(a), "provider") {
                row.insert("provider".into(), p.clone());
            }
            row.insert("weight".into(), js::number_value(weight));
            row.insert("requests".into(), json!(agg.requests));
            row.insert("failedRequests".into(), json!(agg.failed));
            row.insert("apiEquivalentUsd".into(), round(agg.usd, 2));
            row.insert(
                "apiEquivalentUsdPerRequest".into(),
                if agg.requests != 0.0 && !agg.requests.is_nan() { round(agg.usd / agg.requests, 4) } else { Value::Null },
            );
            row.insert(
                "usdByModel".into(),
                obj(agg
                    .usd_by_model
                    .entries()
                    .into_iter()
                    .map(|(m, v)| (m.to_string(), round(v, 2)))
                    .collect()),
            );
            let cost = &account_cost[&id];
            row.insert("actualUsd".into(), round_opt(cost.plan_usd, 2));
            row.insert(
                "actualUsdPerRequest".into(),
                match (cost.per_weight, cost.plan_usd) {
                    (Some(_), Some(p)) => round(p / agg.requests, 4),
                    _ => Value::Null,
                },
            );
            row.insert(
                "actualUsdByModel".into(),
                match cost.per_weight {
                    None => json!({}),
                    Some(w) => obj(agg
                        .usd_by_model
                        .entries()
                        .into_iter()
                        .map(|(m, v)| (m.to_string(), round(cost_weight(cost, m, v, agg.requests_by_model.get(m).unwrap_or(0.0)) * w, 2)))
                        .collect()),
                },
            );
            row.insert("actualCostBasis".into(), cost.basis.map_or(Value::Null, |b| json!(b.name())));
            row.insert("weeklyPercentUsed".into(), round(used_percent, 1));
            row.insert("quotaUnitsUsed".into(), round(used_units, 4));
            row.insert(
                "quotaSource".into(),
                json!(usage.map_or("none", |u| u.source)),
            );
            row.insert(
                "fitSource".into(),
                json!(usage.map_or("none", |u| u.fit_source)),
            );
            row.insert(
                "usdPerAccountWeek".into(),
                if enough && used_units > 0.0 && agg.usd > 0.0 {
                    round(agg.usd / used_units, 0)
                } else {
                    Value::Null
                },
            );
            row.insert(
                "unexplainedUnits".into(),
                match predicted {
                    Some(p) if enough => round(used_units - p, 4),
                    _ => Value::Null,
                },
            );
            row.insert(
                "planUsdPerMonth".into(),
                monthly.map_or(Value::Null, js::number_value),
            );
            row.insert("planPriceSource".into(), plan_price_source);
            row.insert(
                "valueMultiple".into(),
                match plan_per_window {
                    Some(p) if p != 0.0 && !p.is_nan() && enough => round(agg.usd / p, 2),
                    _ => Value::Null,
                },
            );
            Value::Object(row)
        })
        .collect();
    let unattributed_agg = accounts_agg.get("(unattributed)");

    // Models over the value window, with their estimated share of quota.
    let mut provider_units = NumObj::default();
    let mut model_rows: Vec<(f64, Option<f64>, Map<String, Value>)> = models_agg
        .values()
        .map(|m| {
            let units = rate_for(&m.provider, &m.model).map(|k| k * m.usd);
            if let Some(u) = units {
                provider_units.add(&m.provider, u);
            }
            let estimated = units.filter(|u| u.is_finite()).map(|u| js::round(u * 1e4) / 1e4);
            let mut row = Map::new();
            row.insert("provider".into(), json!(m.provider));
            row.insert("model".into(), json!(m.model));
            row.insert("requests".into(), json!(m.requests));
            row.insert("failedRequests".into(), json!(m.failed));
            row.insert("sessions".into(), json!(m.sessions.len()));
            row.insert("apiEquivalentUsd".into(), round(m.usd, 2));
            row.insert(
                "usdPerRequest".into(),
                if m.requests != 0.0 { round(m.usd / m.requests, 3) } else { Value::Null },
            );
            let (actual_usd, costed, uncosted) = actual_of(&m.parts);
            row.insert("actualUsd".into(), round_opt(actual_usd, 2));
            row.insert(
                "actualUsdPerRequest".into(),
                match actual_usd {
                    Some(u) if costed != 0.0 => round(u / costed, 4),
                    _ => Value::Null,
                },
            );
            row.insert("uncostedRequests".into(), json!(uncosted));
            row.insert(
                "tokens".into(),
                json!({ "in": m.tokens[0], "cr": m.tokens[1], "cw": m.tokens[2], "out": m.tokens[3], "rsn": m.tokens[4] }),
            );
            row.insert("estimatedQuotaUnits".into(), estimated.map_or(Value::Null, js::number_value));
            (js::round(m.usd * 100.0) / 100.0, estimated, row)
        })
        .collect();
    for (_, estimated, row) in model_rows.iter_mut() {
        let provider = row["provider"].as_str().unwrap().to_string();
        let total = provider_units.get(&provider);
        let share = match (*estimated, total) {
            (Some(e), Some(t)) if t > 0.0 => round(e / t, 3),
            _ => Value::Null,
        };
        row.insert("shareOfProviderQuota".into(), share);
    }
    js::sort_by(&mut model_rows, |a, b| b.0 - a.0);
    let model_rows: Vec<Value> = model_rows
        .into_iter()
        .map(|(.., r)| Value::Object(r))
        .collect();

    let mut session_rows: Vec<(f64, Value)> = sessions
        .values()
        .map(|s| {
            let mut row = Map::new();
            row.insert("sid".into(), json!(s.sid));
            if let Some(c) = &s.client {
                row.insert("client".into(), c.clone());
            }
            row.insert("provider".into(), json!(s.provider));
            row.insert("firstAt".into(), json!(js::iso(s.first_at)));
            row.insert("lastAt".into(), json!(js::iso(s.last_at)));
            row.insert("requests".into(), json!(s.requests));
            row.insert("apiEquivalentUsd".into(), round(s.usd, 2));
            row.insert("actualUsd".into(), round_opt(actual_of(&s.parts).0, 2));
            row.insert(
                "models".into(),
                json!(s
                    .usd_by_model
                    .entries()
                    .iter()
                    .map(|(m, _)| *m)
                    .collect::<Vec<_>>()),
            );
            row.insert(
                "estimatedQuotaUnits".into(),
                round_opt(est_units(&s.provider, &s.usd_by_model), 4),
            );
            (js::round(s.usd * 100.0) / 100.0, Value::Object(row))
        })
        .collect();
    js::sort_by(&mut session_rows, |a, b| b.0 - a.0);
    let session_rows: Vec<Value> = session_rows.into_iter().take(15).map(|(_, r)| r).collect();

    // Per provider (usage-value.mjs `actualCost`): plan cost over the window, how much of it the
    // window's requests used and how much sat on accounts that served none.
    let row_provider = |r: &Value| r.get("provider").map(|p| text(Some(p)));
    let mut cost_providers: Vec<Option<String>> = Vec::new();
    for p in account_rows.iter().chain(model_rows.iter()).map(row_provider) {
        if !cost_providers.contains(&p) {
            cost_providers.push(p);
        }
    }
    cost_providers.sort_by(|a, b| match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, _) => std::cmp::Ordering::Greater,
        (_, None) => std::cmp::Ordering::Less,
        (Some(x), Some(y)) => x.cmp(y),
    });
    let num_of = |r: &Value, k: &str| js::to_number(r.get(k));
    let plan_of_row = |a: &Value| account_cost.get(&text(a.get("id"))).and_then(|c| c.plan_usd).unwrap_or(f64::NAN);
    let mut actual_cost = Map::new();
    for p in &cost_providers {
        let mine: Vec<&Value> = account_rows.iter().filter(|a| &row_provider(a) == p).collect();
        let priced: Vec<&Value> = mine.iter().copied().filter(|a| !a["actualUsd"].is_null()).collect();
        let used: Vec<&Value> = priced.iter().copied().filter(|a| !a["actualUsdPerRequest"].is_null()).collect();
        let models: Vec<&Value> = model_rows.iter().filter(|m| &row_provider(m) == p).collect();
        let requests = models.iter().fold(0.0, |s, m| s + num_of(m, "requests"));
        let uncosted = models.iter().fold(0.0, |s, m| s + num_of(m, "uncostedRequests"));
        let plan_usd = priced.iter().fold(0.0, |s, a| s + plan_of_row(a));
        let allocated = used.iter().fold(0.0, |s, a| s + plan_of_row(a));
        let api = models.iter().fold(0.0, |s, m| s + num_of(m, "apiEquivalentUsd"));
        let mut basis: Vec<String> = Vec::new();
        for a in &used {
            let b = text(a.get("actualCostBasis"));
            if !basis.contains(&b) {
                basis.push(b);
            }
        }
        let basis = sorted_strings(basis);
        actual_cost.insert(
            p.clone().unwrap_or_else(|| "undefined".into()),
            json!({
                "requests": requests,
                "apiEquivalentUsd": round(api, 2),
                "apiEquivalentUsdPerRequest": if requests != 0.0 && !requests.is_nan() { round(api / requests, 4) } else { Value::Null },
                "planUsd": if priced.is_empty() { Value::Null } else { round(plan_usd, 2) },
                "actualUsd": if used.is_empty() { Value::Null } else { round(allocated, 2) },
                "idleUsd": if priced.is_empty() { Value::Null } else { round(plan_usd - allocated, 2) },
                "actualUsdPerRequest": if !used.is_empty() && requests > uncosted { round(allocated / (requests - uncosted), 4) } else { Value::Null },
                "uncostedRequests": uncosted,
                "unpricedAccounts": mine.len() - priced.len(),
                "basis": basis,
            }),
        );
    }

    let mut basis_providers: Vec<String> = vec!["codex".into(), "claude".into()];
    for p in analysis_sessions
        .values()
        .map(|s| s.provider.clone())
        .chain(unattributed_requests.iter().map(|(p, ..)| p.clone()))
    {
        if !basis_providers.contains(&p) {
            basis_providers.push(p);
        }
    }
    let basis_providers = sorted_strings(basis_providers);
    let units_basis =
        |p: &str| js::get(fits.get(p), "status").and_then(Value::as_str) == Some("ok");
    let cost_basis = obj(basis_providers
        .iter()
        .map(|p| {
            (
                p.clone(),
                json!(if units_basis(p) { "units" } else { "usd" }),
            )
        })
        .collect());
    let root_of = |session: &AnalysisSession| -> String {
        let mut seen: HashSet<&str> = HashSet::from([session.sid.as_str()]);
        let mut current = session;
        while let Some(psid) = &current.psid {
            let Some(parent) = analysis_sessions.get(psid) else {
                break;
            };
            if parent.provider != session.provider || seen.contains(parent.sid.as_str()) {
                break;
            }
            seen.insert(parent.sid.as_str());
            current = parent;
        }
        current.sid.clone()
    };
    let mut ordered_sessions: Vec<&AnalysisSession> = analysis_sessions.values().collect();
    js::sort_by(&mut ordered_sessions, |a, b| {
        js::or_else(a.first_at - b.first_at, || {
            if a.sid < b.sid {
                -1.0
            } else if a.sid > b.sid {
                1.0
            } else {
                0.0
            }
        })
    });
    let session_usage_rows: Vec<Value> = ordered_sessions
        .iter()
        .map(|s| {
            let mut by_model = s.usd_by_model.entries();
            by_model.sort_by(|a, b| a.0.cmp(b.0));
            let mut efforts = s.efforts.clone();
            efforts.sort_by_key(|e| text(Some(e)));
            json!({
                "sid": s.sid, "psid": s.psid, "rootSid": root_of(s), "provider": s.provider, "client": s.client,
                "firstAt": js::iso(s.first_at), "lastAt": js::iso(s.last_at),
                "requests": s.requests, "failedRequests": s.failed,
                "apiEquivalentUsd": js::number_value(s.usd),
                "usdByModel": obj(by_model.into_iter().map(|(m, v)| (m.to_string(), js::number_value(v))).collect()),
                "efforts": efforts,
                "quotaUnits": if units_basis(&s.provider) {
                    est_units(&s.provider, &s.usd_by_model).map_or(Value::Null, js::number_value)
                } else {
                    Value::Null
                },
            })
        })
        .collect();
    let unattributed_value = obj(unattributed_requests
        .iter()
        .map(|(p, count, usd)| {
            (
                p.clone(),
                json!({ "count": count, "apiEquivalentUsd": js::number_value(*usd) }),
            )
        })
        .collect());
    let session_usage = json!({
        "format": "session-usage/v1",
        "generatedAt": js::iso(now),
        "from": js::iso(analysis_from),
        "costBasis": cost_basis,
        "sessions": session_usage_rows,
        "unattributedRequests": unattributed_value,
    });

    // Waste: weekly quota left when a window ended naturally (samples), over the waste window.
    let waste: Vec<Value> = accounts
        .iter()
        .map(|a| {
            let id = text(js::get(Some(a), "id"));
            let provider = text(js::get(Some(a), "provider"));
            let usage = account_usage.get(&id);
            let ends: Vec<&WindowEnd> = usage
                .map(|u| u.window_ends.as_slice())
                .unwrap_or(&[])
                .iter()
                .filter(|e| {
                    e.at >= waste_from && !e.early && !(provider == "codex" && e.last_used <= 0.0)
                })
                .collect();
            let weight = weight_of(&id, &provider);
            let refills = usage
                .map(|u| u.refills.as_slice())
                .unwrap_or(&[])
                .iter()
                .filter(|r| r.at >= waste_from && r.from - r.to >= 20.0)
                .count();
            let mut row = Map::new();
            row.insert("id".into(), json!(id));
            if let Some(p) = js::get(Some(a), "provider") {
                row.insert("provider".into(), p.clone());
            }
            row.insert("windowsEnded".into(), json!(ends.len()));
            row.insert(
                "unusedUnits".into(),
                round(
                    ends.iter().fold(0.0, |s, e| {
                        s + (weight * js::max([0.0, 100.0 - e.last_used])) / 100.0
                    }),
                    4,
                ),
            );
            row.insert(
                "ends".into(),
                Value::Array(
                    ends.iter()
                        .map(|e| json!({ "at": js::iso(e.at), "lastUsedPercent": e.last_used }))
                        .collect(),
                ),
            );
            row.insert("refillsSeen".into(), json!(refills));
            Value::Object(row)
        })
        .collect();
    let projected = obj(js::entries(js::coalesce([js::get(forecast, "providers")]))
        .into_iter()
        .map(|(p, f)| {
            let f = Some(f);
            let or_null = |v: Option<&Value>| js::coalesce([v]).cloned().unwrap_or(Value::Null);
            (p, json!({
                "state": or_null(js::get(f, "state")),
                "creditsExpiringUnused": or_null(js::get(js::get(f, "projection"), "creditsExpiringUnused")),
                "wastedUnits": or_null(js::get(js::get(f, "projection"), "wastedUnits")),
                "creditsLapsingOutlook": or_null(js::get(js::get(f, "creditOutlook"), "creditsLapsing")),
                "creditExpiries": js::coalesce([js::get(js::get(f, "supply"), "creditExpiries")]).cloned().unwrap_or(json!([])),
            }))
        })
        .collect());
    let samples_since = samples.iter().fold(f64::INFINITY, |min, s| {
        let t = js::date_parse_value(js::get(Some(s), "at"));
        if t.is_finite() && t < min {
            t
        } else {
            min
        }
    });
    let or_null = |v: Option<&Value>| js::coalesce([v]).cloned().unwrap_or(Value::Null);
    let value_report = json!({
        "format": VALUE_FORMAT,
        "generatedAt": js::iso(now),
        "config": {
            "analysisDays": config.analysis_days, "valueDays": config.value_days,
            "wasteDays": config.waste_days, "bucketHours": config.bucket_hours,
        },
        "valueFrom": js::iso(window_start),
        "valueWindowDays": round(window_days, 2),
        "coverage": {
            "ledgerSince": ledger_since.map(js::iso),
            "ledgerRows": ledger_rows,
            "duplicates": duplicates,
            "unattributedRows": unattributed,
            "unattributedUsd": round(unattributed_agg.map_or(0.0, |a| a.usd), 2),
            "samplesSince": samples_since.is_finite().then(|| js::iso(samples_since)),
            "unpricedModels": obj(unpriced.iter().map(|(k, v)| (k.clone(), json!(v))).collect()),
            "prices": {
                "source": or_null(js::get(Some(prices), "source")),
                "effectiveFrom": or_null(js::get(Some(prices), "effectiveFrom")),
            },
        },
        "fits": fits_value,
        "modelLimits": model_limits,
        "actualCost": Value::Object(actual_cost),
        "accounts": account_rows,
        "models": model_rows,
        "sessions": session_rows,
        "waste": { "measured": waste, "projected": projected },
    });
    UsageReports {
        value_report,
        session_usage,
    }
}
