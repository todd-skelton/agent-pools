//! routing-ladder.mjs `buildLadders`: for each of the 15 routing rows, the current model of
//! every family at every effort, scored by quota price, time and failure risk, and ordered.
//! With `scores` and `arenaScore` from model-calibrate.mjs. Pure over the documents it is given;
//! availability is the controller's.
//!
//! Held to every call Agent Pools' own routing-ladder.test.mjs makes
//! (`golden/ladder-hook.mjs`, `tests/ladder.rs`).

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::js::{self, finite, get, number_value, truthy};

const HOUR: f64 = 3_600_000.0;
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const INDEX_METRICS: [&str; 3] = [
    "artificial_analysis_intelligence_index",
    "artificial_analysis_coding_index",
    "artificial_analysis_math_index",
];

/// `Number.isFinite(v) && v > 0`.
fn positive(v: Option<&Value>) -> Option<f64> {
    finite(v).filter(|x| *x > 0.0)
}

/// `Number.isFinite(v) && v >= 0`.
fn nonnegative(v: Option<&Value>) -> Option<f64> {
    finite(v).filter(|x| *x >= 0.0)
}

/// `createHash("sha256").update(JSON.stringify(v ?? null)).digest("hex")`.
fn hash(v: Option<&Value>) -> String {
    let text = js::stringify(v.unwrap_or(&Value::Null));
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn median(xs: &[f64]) -> Option<f64> {
    crate::value::median(xs)
}

/// `JSON.stringify([String(row), model, effort])`.
fn key(row: &str, model: &Value, effort: &str) -> String {
    js::stringify(&json!([row, model, effort]))
}

/// `a === b` for two values read from JSON (absent as `undefined`).
fn same(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
        (Some(Value::Object(_) | Value::Array(_)), _) | (_, Some(Value::Object(_) | Value::Array(_))) => false,
        (a, b) => a == b,
    }
}

/// `snapshot.rows.find((r) => r.model === model && r.effort === effort)`.
fn snapshot_row<'a>(snapshot: Option<&'a Value>, model: &Value, effort: &str) -> Option<&'a Value> {
    match get(snapshot, "rows") {
        Some(Value::Array(rows)) => rows
            .iter()
            .find(|r| same(get(Some(r), "model"), Some(model)) && get(Some(r), "effort").and_then(Value::as_str) == Some(effort)),
        _ => None,
    }
}

/// `taskCost(inputs, model, effort)`: the benchmark's USD per task, when positive.
fn task_cost(inputs: &Value, model: &Value, effort: &str) -> Option<f64> {
    positive(get(snapshot_row(inputs.get("snapshot"), model, effort), "usdPerTask"))
}

/// model-calibrate.mjs `scores(snapshot, id, effort, metrics)`: each finite evaluation, the
/// AA indexes as they are and the other benchmarks (fractions) times 100.
pub fn scores(snapshot: Option<&Value>, model: &Value, effort: &str, metrics: &[String]) -> Option<Map<String, Value>> {
    let row = snapshot_row(snapshot, model, effort)?;
    let mut values = Map::new();
    for k in metrics {
        if let Some(v) = finite(get(get(Some(row), "evaluations"), k)) {
            let v = if INDEX_METRICS.contains(&k.as_str()) { v } else { v * 100.0 };
            values.insert(k.clone(), number_value(v));
        }
    }
    Some(values)
}

const MANUAL_KEYS: [&str; 11] = ["board", "model", "effort", "rating", "lower", "upper", "votes", "source", "sourceUrl", "capturedAt", "enteredBy"];

/// model-calibrate.mjs `validateManualScores(m)`: hand-entered ratings (`manual-scores/v1`), each
/// row complete, its rating within `[lower, upper]` and its capture time a date. The error is
/// the message it throws.
pub fn validate_manual_scores(m: &Value) -> Result<(), String> {
    let rows = match (m.get("format").and_then(Value::as_str), m.get("rows")) {
        (Some("manual-scores/v1"), Some(Value::Array(rows))) => rows,
        _ => return Err("manual-scores.json must be manual-scores/v1 with rows[]".into()),
    };
    for (i, r) in rows.iter().enumerate() {
        let missing: Vec<&str> = MANUAL_KEYS
            .iter()
            .copied()
            .filter(|k| matches!(get(Some(r), k), None | Some(Value::Null)) || get(Some(r), k).and_then(Value::as_str) == Some(""))
            .collect();
        if !missing.is_empty() {
            return Err(format!("manual-scores row {i} is missing {}", missing.join(", ")));
        }
        let n = |k: &str| finite(get(Some(r), k));
        let ok = match (n("rating"), n("lower"), n("upper"), n("votes")) {
            (Some(rating), Some(lower), Some(upper), Some(_)) => lower <= rating && rating <= upper,
            _ => false,
        };
        if !ok {
            return Err(format!("manual-scores row {i} needs numeric rating within [lower, upper] and votes"));
        }
        if !js::date_parse_value(get(Some(r), "capturedAt")).is_finite() {
            return Err(format!("manual-scores row {i} capturedAt is not a date"));
        }
    }
    Ok(())
}

/// model-calibrate.mjs `arenaScore(arena, id, effort, board, minVotes)`, as the ladder reads it:
/// the rating on `board` for the exact model and effort, absent under `minVotes`.
pub fn arena_rating(arena: Option<&Value>, model: &Value, effort: &str, board: Option<&Value>, min_votes: f64) -> Option<f64> {
    let rows = match get(arena, "rows") {
        Some(Value::Array(rows)) => rows,
        _ => return None,
    };
    let row = rows.iter().find(|r| {
        same(get(Some(r), "model"), Some(model)) && get(Some(r), "effort").and_then(Value::as_str) == Some(effort) && same(get(Some(r), "board"), board)
    })?;
    // `!(row.votes >= minVotes)`: a missing or NaN count is too few.
    let votes = js::to_number(get(Some(row), "votes"));
    if !matches!(votes.partial_cmp(&min_votes), Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)) {
        return None;
    }
    finite(get(Some(row), "rating"))
}

/// One family's current model at one effort (`candidates`).
#[derive(Clone)]
struct Config {
    model: Value,
    family: String,
    provider: Value,
    effort: &'static str,
}

impl Config {
    fn provider_key(&self) -> String {
        js::to_string(Some(&self.provider))
    }
}

/// `candidates(inputs)`: every family with a current model, in `localeCompare` order, at every effort.
fn candidates(inputs: &Value) -> Result<Vec<Config>, String> {
    let registry = inputs.get("registry");
    let families = match get(registry, "families") {
        Some(f @ Value::Object(_)) => js::entries(Some(f)),
        _ => return Err("Cannot convert undefined or null to object".into()),
    };
    let mut families: Vec<(String, &Value)> = families;
    js::sort_by(&mut families, |a, b| match js::locale_compare(&a.0, &b.0) {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.0,
        std::cmp::Ordering::Greater => 1.0,
    });
    let mut out = Vec::new();
    for (family, f) in families {
        let current = get(Some(f), "current");
        if !truthy(current) {
            continue;
        }
        let current = current.cloned().unwrap_or(Value::Null);
        let provider = match get(Some(f), "provider") {
            Some(p) if !p.is_null() => Some(p.clone()),
            _ => get(get(get(registry, "models"), &js::to_string(Some(&current))), "provider").cloned(),
        };
        let provider = match provider {
            Some(p) if truthy(Some(&p)) => p,
            _ => return Err(format!("routing ladder family {family} needs a provider")),
        };
        out.extend(EFFORTS.iter().map(|e| Config { model: current.clone(), family: family.clone(), provider: provider.clone(), effort: e }));
    }
    Ok(out)
}

/// `quotaPrice(forecast, provider, floor, now)`: what a unit of the provider's quota costs now,
/// from how far demand falls short of the pace that uses it all; 1 (neutral) without a fresh pace.
fn quota_price(forecast: Option<&Value>, provider: &str, floor: f64, now: f64) -> Value {
    let p = get(get(forecast, "providers"), provider);
    let pace = get(p, "pace");
    let demand = nonnegative(get(get(pace, "atDemand"), "unitsPerHour"));
    let demand_v = demand.map_or(Value::Null, number_value);
    let neutral = |source: &str| json!({ "quotaPrice": 1, "priceSource": source, "demand": demand_v.clone(), "target": null });
    if !truthy(p)
        || get(p, "state").and_then(Value::as_str) == Some("unknown")
        || truthy(get(p, "stale"))
        // `!(Date.parse(staleAfter) > now)`: an unreadable time is stale.
        || js::date_parse_value(get(forecast, "staleAfter")).partial_cmp(&now) != Some(std::cmp::Ordering::Greater)
    {
        return neutral("forecast-unavailable");
    }
    let computed = js::date_parse_value(get(pace, "computedAt"));
    if !computed.is_finite() || computed > now || now - computed > 15.0 * 60000.0 {
        return neutral("pace-stale");
    }
    match nonnegative(get(get(pace, "atDemand"), "lostUnits")) {
        Some(lost) if lost > 0.05 => {}
        _ => return neutral("no-loss"),
    }
    let Some(demand) = demand else {
        return neutral("demand-unavailable");
    };
    let mut target = positive(get(get(pace, "fullUse"), "unitsPerHour"));
    let mut source = "full-use";
    let list = |k: &str| match get(pace, k) {
        Some(Value::Array(a)) => a.iter().collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    if target.is_none() {
        let steps: Vec<f64> = list("steps")
            .into_iter()
            .filter(|s| get(Some(s), "reachable") == Some(&Value::Bool(true)))
            .filter_map(|s| positive(get(Some(s), "unitsPerHour")))
            .collect();
        target = (!steps.is_empty()).then(|| js::max(steps.iter().copied()));
        source = "reachable-step";
    }
    if target.is_none() {
        let curve: Vec<f64> = list("curve")
            .into_iter()
            .filter(|s| nonnegative(get(Some(s), "lostUnits")).is_some_and(|l| l <= 0.05))
            .filter_map(|s| positive(get(Some(s), "unitsPerHour")))
            .collect();
        target = (!curve.is_empty()).then(|| js::min(curve.iter().copied()));
        source = "no-loss-curve";
    }
    let then = get(pace, "then");
    if js::date_parse_value(get(then, "from")) <= now {
        if let Some(t) = positive(get(then, "unitsPerHour")) {
            target = Some(t);
            source = "then";
        }
    }
    let Some(target) = target else {
        return neutral("target-unavailable");
    };
    json!({
        "quotaPrice": number_value(1f64.min(floor.max(demand / target))),
        "priceSource": source,
        "demand": number_value(demand),
        "target": number_value(target),
    })
}

/// A measured configuration (`measuredConfigs`): one row, model and effort's attempts in the
/// analysis window, with median cost, USD and hours.
struct Measured {
    row: String,
    model: Value,
    effort: String,
    n: usize,
    cost: f64,
    usd: f64,
    hours: f64,
}

fn measured_configs(inputs: &Value, now: f64, usd_basis: bool) -> Result<Vec<Measured>, String> {
    let days = match get(get(inputs.get("value"), "config"), "analysisDays") {
        None | Some(Value::Null) => Some(14.0),
        v => positive(v),
    };
    let Some(days) = days else {
        return Err("routing ladder needs positive analysisDays".into());
    };
    struct Group {
        row: String,
        model: Value,
        effort: String,
        costs: Vec<f64>,
        usd: Vec<f64>,
        hours: Vec<f64>,
    }
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Group> = Default::default();
    let attempts = match get(inputs.get("attempts"), "attempts") {
        Some(Value::Array(a)) => a.as_slice(),
        _ => &[],
    };
    for a in attempts {
        let a = Some(a);
        let labelled = get(a, "label").and_then(Value::as_str).is_some_and(|l| !l.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}').is_empty());
        let effort = get(a, "effort").and_then(Value::as_str).filter(|e| EFFORTS.contains(e));
        let row = js::to_string(get(a, "row"));
        // /^([1-9]|1[0-5])$/
        let row_ok = (1..=15).any(|n: u8| n.to_string() == row);
        let (true, Some(effort), true) = (labelled && truthy(get(a, "model")), effort, row_ok) else {
            continue;
        };
        let at = js::date_parse_value(get(a, "at"));
        let cost = nonnegative(get(a, if usd_basis { "apiEquivalentUsd" } else { "quotaUnits" }));
        let usd = nonnegative(get(a, "apiEquivalentUsd"));
        let hours = nonnegative(get(a, "hours"));
        let in_window = at >= now - days * 24.0 * HOUR && at <= now;
        let (true, Some(cost), Some(usd), Some(hours)) = (in_window, cost, usd, hours) else {
            continue;
        };
        let model = get(a, "model").cloned().unwrap_or(Value::Null);
        let k = key(&row, &model, effort);
        let g = groups.entry(k.clone()).or_insert_with(|| {
            order.push(k);
            Group { row: row.clone(), model, effort: effort.to_string(), costs: vec![], usd: vec![], hours: vec![] }
        });
        g.costs.push(cost);
        g.usd.push(usd);
        g.hours.push(hours);
    }
    Ok(order
        .into_iter()
        .map(|k| {
            let g = groups.remove(&k).expect("grouped");
            Measured {
                n: g.costs.len(),
                cost: median(&g.costs).unwrap_or(f64::NAN),
                usd: median(&g.usd).unwrap_or(f64::NAN),
                hours: median(&g.hours).unwrap_or(f64::NAN),
                row: g.row,
                model: g.model,
                effort: g.effort,
            }
        })
        .collect())
}

/// `probabilities(inputs, rule, configs)`: each configuration's chance of success on the row:
/// an Elo expectation against the best rated on an Arena or manual board, or the mean of the
/// row's benchmark metrics.
fn probabilities(inputs: &Value, rule: Option<&Value>, configs: &[Config]) -> Result<Vec<(Config, f64)>, String> {
    let source = get(rule, "source");
    let source_s = source.and_then(Value::as_str);
    if matches!(source_s, Some("arena" | "manual")) {
        let board_doc = inputs.get(if source_s == Some("arena") { "arena" } else { "manual" });
        let min_votes = match get(inputs.get("rules"), "arenaMinVotes") {
            None | Some(Value::Null) => 300.0,
            v => js::to_number(v),
        };
        let board = get(rule, "board");
        let rated: Vec<(Config, f64)> = configs
            .iter()
            .filter_map(|c| arena_rating(board_doc, &c.model, c.effort, board, min_votes).map(|r| (c.clone(), r)))
            .collect();
        let top = js::max(rated.iter().map(|(_, r)| *r));
        return Ok(rated.into_iter().map(|(c, rating)| (c, 1.0 / (1.0 + 10f64.powf((top - rating) / 400.0)))).collect());
    }
    if truthy(source) && source_s != Some("aa") {
        return Err(format!("unknown routing ladder source {}", js::to_string(source)));
    }
    let metrics: Vec<String> = match get(rule, "metrics") {
        Some(Value::Array(m)) if !m.is_empty() => m.iter().map(|m| js::to_string(Some(m))).collect(),
        _ => return Ok(Vec::new()),
    };
    Ok(configs
        .iter()
        .filter_map(|c| {
            let values = scores(inputs.get("snapshot"), &c.model, c.effort, &metrics)?;
            let ok = |m: &String| finite(values.get(m)).is_some_and(|v| (0.0..=100.0).contains(&v));
            if !metrics.iter().all(ok) {
                return None;
            }
            let sum = metrics.iter().fold(0.0, |s, m| s + finite(values.get(m)).unwrap_or(0.0));
            Some((c.clone(), sum / metrics.len() as f64 / 100.0))
        })
        .collect())
}

/// A rung of a row's ladder, in the published key order.
struct Rung {
    c: Config,
    p: Option<f64>,
    cost: Option<f64>,
    cost_source: Option<&'static str>,
    hours: Option<f64>,
    hours_source: Option<&'static str>,
    n: Option<usize>,
    score: Option<f64>,
}

impl Rung {
    fn to_json(&self) -> Value {
        let num = |x: Option<f64>| x.map_or(Value::Null, number_value);
        json!({
            "model": self.c.model, "family": self.c.family, "provider": self.c.provider, "effort": self.c.effort,
            "p": num(self.p), "cost": num(self.cost), "costSource": self.cost_source,
            "hours": num(self.hours), "hoursSource": self.hours_source,
            "n": self.n.map_or(Value::Null, |n| json!(n)), "score": num(self.score),
        })
    }
}

/// `buildLadders(inputs, now)`: the `routing-ladders/v1` document. `inputs` holds `rules`,
/// `registry`, `snapshot` (and `snapshotFile`), `arena` (`arenaFile`), `manual`, `attempts`,
/// `value`, `sessionUsage` and `forecast`. An error is the message Agent Pools would throw.
pub fn build_ladders(inputs: &Value, now: f64) -> Result<Value, String> {
    if !now.is_finite() {
        return Err("routing ladder needs a valid time".into());
    }
    let configs = candidates(inputs)?;
    let mut provider_ids: Vec<String> = Vec::new();
    for c in &configs {
        let k = c.provider_key();
        if !provider_ids.contains(&k) {
            provider_ids.push(k);
        }
    }
    provider_ids.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    let rules = inputs.get("rules");
    let floor = match get(rules, "quotaPriceFloor") {
        None | Some(Value::Null) => Some(0.1),
        v => positive(v),
    };
    let Some(floor) = floor.filter(|f| *f <= 1.0) else {
        return Err("quotaPriceFloor must be in (0, 1]".into());
    };
    let mut basis_providers: Vec<String> = vec!["codex".into(), "claude".into()];
    for p in &provider_ids {
        if !basis_providers.contains(p) {
            basis_providers.push(p.clone());
        }
    }
    let usd_basis = basis_providers.iter().any(|p| {
        let basis = match get(get(inputs.get("sessionUsage"), "costBasis"), p) {
            Some(v) if !v.is_null() => v.clone(),
            _ => json!(if get(get(get(inputs.get("value"), "fits"), p), "status").and_then(Value::as_str) == Some("ok") { "units" } else { "usd" }),
        };
        basis.as_str() != Some("units")
    });
    let mut providers = Map::new();
    for p in &provider_ids {
        providers.insert(p.clone(), quota_price(inputs.get("forecast"), p, floor, now));
    }
    let price = |p: &str| finite(get(providers.get(p), "quotaPrice")).unwrap_or(f64::NAN);
    let all = measured_configs(inputs, now, usd_basis)?;
    let measured: Vec<&Measured> = all.iter().filter(|c| c.n >= 5).collect();
    let groups: std::collections::HashMap<String, &Measured> = all.iter().map(|g| (key(&g.row, &g.model, &g.effort), g)).collect();
    let scales: Vec<(&str, f64)> = measured
        .iter()
        .filter_map(|c| task_cost(inputs, &c.model, &c.effort).map(|usd| (c.row.as_str(), c.usd / usd)))
        .collect();
    let rule_rows = match get(rules, "rows") {
        Some(r @ Value::Object(_)) => r,
        _ => return Err("Cannot read properties of undefined (reading '1')".into()),
    };
    let mut rows = Map::new();
    for i in 1..=15 {
        let id = i.to_string();
        let rule = match rule_rows.get(&id) {
            Some(Value::Null) | None => None,
            r => r,
        };
        if let Some(fixed) = get(rule, "fixed").filter(|f| truthy(Some(f))) {
            let specs = match fixed {
                Value::Array(a) => a.as_slice(),
                _ => return Err("rule.fixed.map is not a function".into()),
            };
            let mut rungs = Vec::new();
            for spec in specs {
                let spec = js::to_string(Some(spec));
                let mut parts = spec.split('/');
                let (family, effort) = (parts.next().unwrap_or(""), parts.next());
                let c = configs.iter().find(|r| r.family == family && Some(r.effort) == effort);
                let Some(c) = c else {
                    return Err(format!("fixed routing rung {spec} has no current model"));
                };
                rungs.push(Rung { c: c.clone(), p: None, cost: None, cost_source: None, hours: None, hours_source: None, n: None, score: None }.to_json());
            }
            rows.insert(id, json!({
                "basis": "fixed",
                "failureMultiple": match get(rule, "failureMultiple") { None => Value::Null, Some(v) => v.clone() },
                "typicalCost": null, "typicalHours": null, "hourValue": null, "failureCost": null,
                "rungs": rungs,
            }));
            continue;
        }
        let failure_multiple = match get(rule, "failureMultiple") {
            None | Some(Value::Null) => Some(0.0),
            v => nonnegative(v),
        };
        let Some(failure_multiple) = failure_multiple else {
            return Err(format!("row {id} needs a nonnegative failureMultiple"));
        };
        let row_measured: Vec<&&Measured> = measured.iter().filter(|m| m.row == id).collect();
        let row_scales: Vec<f64> = scales.iter().filter(|s| s.0 == id).map(|s| s.1).collect();
        let all_scales: Vec<f64> = scales.iter().map(|s| s.1).collect();
        let row_scale = median(if row_scales.is_empty() { &all_scales } else { &row_scales }).unwrap_or(1.0);
        let scale_source = if !row_scales.is_empty() { "row" } else if !scales.is_empty() { "all-rows" } else { "none" };
        let mut rungs: Vec<Rung> = Vec::new();
        for (c, p) in probabilities(inputs, rule, &configs)? {
            let group = groups.get(&key(&id, &c.model, c.effort)).copied();
            let n = group.map_or(0, |g| g.n);
            let (cost, cost_source) = if n >= 5 {
                (group.map(|g| g.cost), "measured")
            } else {
                let Some(usd) = task_cost(inputs, &c.model, c.effort) else {
                    continue;
                };
                let fit = get(get(inputs.get("value"), "fits"), &c.provider_key());
                let ok = get(fit, "status").and_then(Value::as_str) == Some("ok");
                let own = if ok { positive(get(get(get(get(fit, "perModel"), "models"), &js::to_string(Some(&c.model))), "unitsPerUsd")) } else { None };
                let pooled = if ok { positive(get(get(fit, "pooled"), "unitsPerUsd")) } else { None };
                let units = own.or(pooled);
                if !usd_basis && units.is_none() {
                    continue;
                }
                let factor = if usd_basis { 1.0 } else { units.unwrap_or(f64::NAN) };
                let source = if usd_basis { "estimated:usd" } else if own.is_some() { "estimated:model-fit" } else { "estimated:pooled-fit" };
                (Some(usd * row_scale * factor), source)
            };
            let effort_measured: Vec<f64> = row_measured.iter().filter(|m| m.effort == c.effort).map(|m| m.hours).collect();
            let (hours, hours_source) = if n >= 5 {
                (group.map(|g| g.hours), "measured")
            } else if !effort_measured.is_empty() {
                (median(&effort_measured), "row-effort")
            } else if !row_measured.is_empty() {
                (median(&row_measured.iter().map(|m| m.hours).collect::<Vec<_>>()), "row")
            } else if !measured.is_empty() {
                (median(&measured.iter().map(|m| m.hours).collect::<Vec<_>>()), "all-rows")
            } else {
                (None, "none")
            };
            rungs.push(Rung { c, p: Some(p), cost, cost_source: Some(cost_source), hours, hours_source: Some(hours_source), n: Some(n), score: None });
        }
        let costs: Vec<f64> = rungs.iter().filter_map(|r| r.cost).collect();
        let typical_cost = median(&costs);
        let hours: Vec<f64> = rungs.iter().filter_map(|r| r.hours).filter(|h| h.is_finite() && *h >= 0.0).collect();
        let typical_hours = median(&hours);
        let hour_value = match (typical_hours.filter(|h| h.is_finite() && *h > 0.0), typical_cost) {
            (Some(h), Some(c)) => Some(c / h),
            // `null / hours` is 0 in JavaScript.
            (Some(_), None) => Some(0.0),
            _ => None,
        };
        let failure_cost = typical_cost.map(|c| failure_multiple * c);
        for r in &mut rungs {
            let p = r.p.unwrap_or(f64::NAN);
            if let (true, Some(hv), Some(h)) = (p > 0.0, hour_value, r.hours) {
                let cost = r.cost.unwrap_or(f64::NAN);
                let fc = failure_cost.unwrap_or(0.0);
                r.score = Some((cost * price(&r.c.provider_key()) + hv * h + fc * (1.0 - p)) / p);
            }
        }
        js::sort_by(&mut rungs, |a, b| {
            let (sa, sb) = (a.score.unwrap_or(f64::INFINITY), b.score.unwrap_or(f64::INFINITY));
            js::or_else(sa - sb, || {
                js::or_else(a.cost.unwrap_or(f64::NAN) - b.cost.unwrap_or(f64::NAN), || {
                    js::or_else(
                        match js::locale_compare(&a.c.family, &b.c.family) {
                            std::cmp::Ordering::Less => -1.0,
                            std::cmp::Ordering::Equal => 0.0,
                            std::cmp::Ordering::Greater => 1.0,
                        },
                        || effort_index(a.c.effort) - effort_index(b.c.effort),
                    )
                })
            })
        });
        let source_s = get(rule, "source").and_then(Value::as_str);
        let basis = if matches!(source_s, Some("arena" | "manual")) {
            format!("{}:{}", source_s.unwrap_or_default(), js::to_string(get(rule, "board")))
        } else {
            "metrics".into()
        };
        let num = |x: Option<f64>| x.map_or(Value::Null, number_value);
        rows.insert(id, json!({
            "basis": basis,
            "failureMultiple": match get(rule, "failureMultiple") { None | Some(Value::Null) => json!(0), Some(v) => v.clone() },
            "rowScale": number_value(row_scale),
            "scaleSource": scale_source,
            "typicalCost": num(typical_cost),
            "typicalHours": num(typical_hours),
            "hourValue": num(hour_value),
            "failureCost": num(failure_cost),
            "rungs": rungs.iter().map(Rung::to_json).collect::<Vec<_>>(),
        }));
    }
    let registry = inputs.get("registry");
    let or_hash = |file: &str, doc: &str| match inputs.get(file) {
        Some(v) if !v.is_null() => v.clone(),
        _ => json!(hash(inputs.get(doc))),
    };
    let payload = json!({
        "format": "routing-ladders/v1",
        "inputs": {
            "registryAuthorityDigest": match get(registry, "authorityDigest") { None => Value::Null, Some(v) => v.clone() },
            "rules": hash(rules),
            "registry": hash(registry),
            "snapshot": or_hash("snapshotFile", "snapshot"),
            "arena": or_hash("arenaFile", "arena"),
            "manual": hash(inputs.get("manual")),
            "attempts": hash(inputs.get("attempts")),
            "value": hash(inputs.get("value")),
            "sessionUsage": hash(inputs.get("sessionUsage")),
            "forecast": "capacity-forecast.json",
        },
        "costBasis": if usd_basis { "usd" } else { "units" },
        "providers": providers,
        "rows": rows,
    });
    let digest = hash(Some(&payload));
    let Value::Object(mut doc) = payload else { unreachable!() };
    doc.insert("generatedAt".into(), json!(js::iso(now)));
    doc.insert("staleAfter".into(), json!(js::iso(now + 0.5 * HOUR)));
    doc.insert("digest".into(), json!(digest));
    Ok(Value::Object(doc))
}

fn effort_index(e: &str) -> f64 {
    EFFORTS.iter().position(|x| *x == e).map_or(-1.0, |i| i as f64)
}
