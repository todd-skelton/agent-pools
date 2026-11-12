//! Advisory reset/refill scheduling. The historical policy simulation and all
//! actuators remain unchanged. Candidate runs use the same hourly quota algebra,
//! but vary account priorities, redemption gates and up to three rate segments.

use super::*;

pub const FORMAT: &str = "benefit-guidance/v1";
const BUDGET: usize = 1536;
const EPS: f64 = 1e-9;

#[derive(Clone, Debug)]
pub struct Policy {
    pub legacy: bool,
    pub gates: Vec<f64>,
    pub drain_first: Vec<bool>,
}

impl Policy {
    pub fn legacy() -> Self {
        Self { legacy: true, gates: Vec::new(), drain_first: Vec::new() }
    }

    fn candidate(n: usize) -> Self {
        Self { legacy: false, gates: vec![0.0; n], drain_first: vec![false; n] }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Run {
    pub result: SimResult,
    pub asked: f64,
    pub starting: f64,
    pub renewed: f64,
    pub redeemed: f64,
    pub window_waste: f64,
    pub overwritten: f64,
    pub terminal_nominal: f64,
    pub per_account: Vec<f64>,
}

impl Run {
    pub fn delivered(&self) -> f64 { self.asked - self.result.unmet_units }
    pub fn value(&self) -> f64 { self.delivered() + self.result.remaining_units }

    fn view(&self) -> Value {
        json!({
            "usableUnits": rounded(self.value()),
            "deliveredUnits": rounded(self.delivered()),
            "terminalWindowUnits": rounded(self.result.remaining_units),
            "terminalCreditUnits": rounded(self.result.credit_units_left),
            "startingWindowUnits": rounded(self.starting),
            "naturalRenewalUnits": rounded(self.renewed),
            "redeemedCreditUnits": rounded(self.redeemed),
            "windowWasteUnits": rounded(self.window_waste),
            "overwrittenWindowUnits": rounded(self.overwritten),
            "capStrandedUnits": rounded((self.terminal_nominal - self.result.remaining_units).max(0.0)),
            "lapsedCreditUnits": rounded(self.result.credit_units_expired),
            "creditsUsed": self.result.credits_used,
            "creditsLapsing": self.result.credits_expired,
            "unmetUnits": rounded(self.result.unmet_units),
            "shortHours": self.result.outage_hours,
            "firstShortAt": self.result.first_outage_at.map(js::iso),
        })
    }
}

fn rounded(x: f64) -> f64 { (x * 1e8).round() / 1e8 }

fn eligible(a: &Supply, i: usize, t: f64, rate: f64, o: &SimOptions, p: &Policy) -> bool {
    if a.credits.is_empty() { return false; }
    if !p.legacy && (a.stale || a.used < 100.0 - EPS) { return false; }
    if a.refill {
        let Some(reset) = a.reset_at.filter(|r| *r > t) else { return false; };
        if a.credits[0] <= reset { return true; }
        let need = if rate > 0.0 { a.effective_units_left() / rate * HOUR } else { INF };
        return reset - t >= need.min(o.max_runway_percent / 100.0 * a.period_ms);
    }
    p.legacy || a.credits[0] <= t + HOUR
        || reset_or_inf(a) - t >= p.gates.get(i).copied().unwrap_or(0.0) * a.period_ms
}

fn redeem(a: &mut Supply, t: f64, r: &mut Run) {
    r.window_waste += a.units_left();
    r.overwritten += a.units_left();
    r.redeemed += a.weight;
    a.credits.remove(0);
    r.result.credits_used += 1.0;
    r.result.redemptions.push(Redeemed { id: a.id.clone(), at: t });
    a.used = 0.0;
    for cap in &mut a.caps { cap.used = 0.0; }
    if !a.refill { a.reset_at = None; }
}

fn consume(accts: &mut [Supply], need: &mut f64, t: f64, o: &SimOptions, p: &Policy, rate: f64, r: &mut Run) {
    let priority = |i: usize| !p.legacy && p.drain_first.get(i) == Some(&true)
        && !accts[i].stale && !accts[i].credits.is_empty()
        && (accts[i].refill || reset_or_inf(&accts[i]) - t >= p.gates[i] * accts[i].period_ms);
    let mut order: Vec<usize> = (0..accts.len()).collect();
    js::sort_by(&mut order, |&x, &y| {
        if priority(x) != priority(y) { return if priority(x) { -1.0 } else { 1.0 }; }
        drain_key(&accts[x], o.drain_first) - drain_key(&accts[y], o.drain_first)
    });
    for i in order {
        loop {
            if *need <= 1e-12 { break; }
            let a = &mut accts[i];
            let room = a.effective_units_left();
            if room <= 1e-12 {
                if !p.legacy && p.drain_first[i] && o.use_credits && eligible(a, i, t, rate, o, p) {
                    redeem(a, t, r);
                    continue;
                }
                break;
            }
            let take = js::min([room, *need]);
            if a.reset_at.is_none() { a.reset_at = Some(t + a.period_ms); a.floating = false; }
            a.used += take / a.weight * 100.0;
            for cap in &mut a.caps {
                if cap.reset_at.is_none() { cap.reset_at = Some(t + cap.period_ms); cap.floating = false; }
                cap.used += take / a.weight * 100.0;
            }
            r.per_account[i] += take;
            *need -= take;
            if p.legacy { break; }
        }
    }
}

/// Independent policy-pluggable simulator. Legacy mode retains operation order
/// so differential tests can compare every historical simulation field exactly.
pub fn simulate_policy(supply: &[Supply], rate: &dyn Fn(f64) -> f64, o: &SimOptions, p: &Policy) -> Run {
    let mut accts: Vec<Supply> = supply.iter().map(|a| {
        let mut b = a.clone();
        b.reset_at = if a.floating { None } else { Some(a.reset_at.unwrap_or(o.now + a.period_ms)) };
        b
    }).collect();
    let mut r = Run { starting: accts.iter().map(Supply::units_left).sum(), per_account: vec![0.0; accts.len()], ..Run::default() };
    let mut pending = o.grants.to_vec();
    js::sort_by(&mut pending, |x, y| x - y);
    let mut h = 0.0;
    while h < o.horizon_hours {
        let t = o.now + h * HOUR;
        for a in &mut accts {
            for cap in &mut a.caps {
                while let Some(reset) = cap.reset_at.filter(|r| *r <= t) {
                    cap.used = 0.0;
                    cap.reset_at = if cap.floating { None } else { Some(reset + cap.period_ms) };
                }
            }
            while let Some(reset) = a.reset_at.filter(|r| *r <= t) {
                r.result.expiring_units += a.effective_units_left();
                r.window_waste += a.units_left();
                r.renewed += a.weight;
                a.used = 0.0;
                if a.first_use { a.reset_at = None; a.floating = true; }
                else { a.reset_at = Some(reset + a.period_ms); }
            }
            while a.credits.first().is_some_and(|c| *c <= t) {
                a.credits.remove(0);
                r.result.credits_expired += 1.0;
                r.result.credit_units_expired += a.weight;
                r.result.lapses.push(Redeemed { id: a.id.clone(), at: t });
            }
        }
        while !pending.is_empty() && pending[0] <= t && !accts.is_empty() {
            pending.remove(0);
            let mut order: Vec<usize> = (0..accts.len()).collect();
            js::sort_by(&mut order, |&x, &y| reset_or_inf(&accts[y]) - reset_or_inf(&accts[x]));
            let to = &mut accts[order[0]];
            to.credits.push(t + o.grant_lifetime_hours * HOUR);
            js::sort_by(&mut to.credits, |x, y| x - y);
            r.result.credits_granted += 1.0;
        }
        let rate_now = rate(t);
        r.asked += rate_now;
        let mut need = rate_now;
        if o.use_credits {
            if let Some(early) = o.early_refill.filter(|s| !s.is_empty()) {
                for i in 0..accts.len() {
                    let a = &accts[i];
                    if !early.contains(&a.id) || !a.refill || a.credits.is_empty() || a.effective_units_left() > 1e-12 { continue; }
                    if !refill_runway(a, &accts, t, rate_now, o.max_runway_percent, &|a| drain_key(a, o.drain_first)) { continue; }
                    redeem(&mut accts[i], t, &mut r);
                }
            }
        }
        consume(&mut accts, &mut need, t, o, p, rate_now, &mut r);
        while need > 1e-12 && o.use_credits {
            let holders: Vec<usize> = (0..accts.len()).filter(|&i| eligible(&accts[i], i, t, rate_now, o, p)).collect();
            let mut urgent: Vec<usize> = holders.iter().copied().filter(|&i| accts[i].credits[0] <= t + 24.0 * HOUR).collect();
            js::sort_by(&mut urgent, |&x, &y| accts[x].credits[0] - accts[y].credits[0]);
            let holder = match urgent.first() {
                Some(&u) => Some(u),
                None => {
                    let mut hs = holders;
                    js::sort_by(&mut hs, |&x, &y| reset_or_inf(&accts[y]) - reset_or_inf(&accts[x]));
                    hs.first().copied()
                }
            };
            let Some(i) = holder else { break; };
            redeem(&mut accts[i], t, &mut r);
            consume(&mut accts, &mut need, t, o, p, rate_now, &mut r);
        }
        if need > 1e-12 {
            r.result.unmet_units += need;
            r.result.outage_hours += 1.0;
            r.result.first_outage_at.get_or_insert(t);
        }
        h += 1.0;
    }
    r.result.remaining_units = accts.iter().fold(0.0, |s, a| s + a.effective_units_left());
    r.result.credit_units_left = accts.iter().fold(0.0, |s, a| s + a.weight * a.credits.len() as f64);
    r.result.wasted_units = r.result.expiring_units + r.result.credit_units_expired;
    r.terminal_nominal = accts.iter().map(Supply::units_left).sum();
    r
}

#[derive(Clone)]
struct Candidate {
    policy: Policy,
    // End-exclusive absolute instants. One value per segment.
    segments: Vec<(f64, f64)>,
    run: Run,
}

impl Candidate {
    fn rate(&self, t: f64) -> f64 { self.segments.iter().find(|(until, _)| t < *until).or(self.segments.last()).map_or(0.0, |x| x.1) }
    fn peak(&self) -> f64 { self.segments.iter().map(|s| s.1).fold(0.0, f64::max) }
    fn better(&self, other: &Self) -> bool {
        let gain = self.run.value() - other.run.value();
        if gain.abs() > EPS { return gain > 0.0; }
        let peak = self.peak() - other.peak();
        if peak.abs() > EPS { return peak < 0.0; }
        self.run.result.outage_hours < other.run.result.outage_hours
    }
}

struct Search<'a> {
    supply: &'a [Supply],
    opts: SimOptions<'a>,
    evaluations: usize,
    exhausted: bool,
    evaluated: Vec<Candidate>,
    memo: HashMap<String, usize>,
}

impl Search<'_> {
    fn evaluate(&mut self, policy: &Policy, segments: &[(f64, f64)]) -> Option<Candidate> {
        let key = js::stringify(&json!([policy.legacy, policy.gates, policy.drain_first, segments]));
        if let Some(&i) = self.memo.get(&key) { return Some(self.evaluated[i].clone()); }
        if self.evaluations >= BUDGET { self.exhausted = true; return None; }
        self.evaluations += 1;
        let mut c = Candidate { policy: policy.clone(), segments: segments.to_vec(), run: Run::default() };
        c.run = simulate_policy(self.supply, &|t| c.rate(t), &self.opts, policy);
        self.memo.insert(key, self.evaluated.len());
        self.evaluated.push(c.clone());
        Some(c)
    }

    fn improve(&mut self, best: &mut Candidate, policy: &Policy, segments: &[(f64, f64)]) {
        if let Some(c) = self.evaluate(policy, segments) {
            if c.better(best) { *best = c; }
        }
    }
}

fn segments_view(c: &Candidate, now: f64) -> Value {
    let mut from = now;
    Value::Array(c.segments.iter().map(|&(until, rate)| {
        let s = json!({ "from": js::iso(from), "until": js::iso(until), "unitsPerHour": rate });
        from = until;
        s
    }).collect())
}

fn guidance(supply: &[Supply], f: &Value, replay: &Replay, now: f64, config: &ForecastConfig, enabled: bool, max_runway: f64) -> Value {
    let demand = f["demand"]["unitsPerHour"].as_f64().unwrap_or(0.0);
    let horizon = f["pace"]["horizonHours"].as_f64().unwrap_or_else(|| run_hours(supply, config.horizon_days * 24.0));
    let end = now + horizon * HOUR;
    let natural = natural_units_per_hour(supply);
    let cap = natural * PACE.burst;
    let mut search = Search { supply, opts: SimOptions::new(now, horizon, enabled, max_runway), evaluations: 0, exhausted: false, evaluated: Vec::new(), memo: HashMap::new() };
    let legacy = Policy::legacy();
    let current = search.evaluate(&legacy, &[(end, demand)]).unwrap();
    let burst = search.evaluate(&legacy, &[(end, cap)]).unwrap();
    let mut best = if demand <= cap && current.better(&burst) { current.clone() } else { burst.clone() };
    let mut rates: Vec<f64> = vec![0.0, demand.min(cap)];
    rates.extend(PACE.ladder.iter().map(|x| natural * x));
    for key in ["fullUse", "noGap"] {
        if let Some(r) = f["pace"][key]["unitsPerHour"].as_f64().filter(|r| r.is_finite() && *r >= 0.0 && *r <= cap) { rates.push(r); }
    }
    rates.push(cap);
    rates.sort_by(f64::total_cmp);
    rates.dedup_by(|a, b| (*a - *b).abs() < 1e-12);
    let mut constants = Vec::new();
    for &rate in &rates {
        let c = search.evaluate(&legacy, &[(end, rate)]).unwrap();
        if c.better(&best) { best = c.clone(); }
        constants.push(c);
    }
    let mut observed = current.clone();
    let mut candidate = Policy::candidate(supply.len());
    let mut coordinate_limited = false;
    // Fixed coordinate order and two passes; no wall-clock-dependent decisions.
    'coordinates: for _ in 0..2 {
        for i in 0..supply.len() {
            if !enabled || supply[i].credits.is_empty() || supply[i].stale { continue; }
            let gates: &[f64] = if supply[i].refill { &[0.0] } else { &[0.0, 0.25, 0.5, 0.75] };
            for &gate in gates {
                for drain in [false, true] {
                    candidate.gates[i] = gate;
                    candidate.drain_first[i] = drain;
                    for &rate in &rates {
                        if rate == 0.0 { continue; }
                        if search.evaluations >= BUDGET / 2 { coordinate_limited = true; break 'coordinates; }
                        if let Some(c) = search.evaluate(&candidate, &[(end, rate)]) {
                            if c.better(&best) { best = c.clone(); }
                            constants.push(c);
                        }
                    }
                    if let Some(c) = search.evaluate(&candidate, &[(end, demand)]) {
                        if c.run.delivered() + EPS >= current.run.delivered() && c.better(&observed) { observed = c; }
                    }
                }
            }
            candidate = if best.policy.legacy { Policy::candidate(supply.len()) } else { best.policy.clone() };
        }
    }
    // Search breakpoints at actual quota/credit event hours. A three-segment
    // candidate can front-load benefits and return to the recurring pace.
    let mut events: Vec<f64> = supply.iter().flat_map(|a| a.reset_at.into_iter().chain(a.credits.iter().copied())).filter(|t| t.is_finite() && *t > now && *t < end).map(|t| now + ((t - now) / HOUR).ceil() * HOUR).collect();
    events.extend(best.run.result.redemptions.iter().map(|r| r.at + HOUR).filter(|t| *t < end));
    events.sort_by(f64::total_cmp);
    events.dedup();
    let policy = best.policy.clone();
    for &at in events.iter().take(16) {
        for &rate in &rates {
            search.improve(&mut best, &policy, &[(at, rate), (end, natural)]);
        }
    }
    let policy = best.policy.clone();
    for pair in events.windows(2).take(8) {
        for &rate in &rates {
            search.improve(&mut best, &policy, &[(pair[0], cap), (pair[1], rate), (end, natural)]);
        }
    }
    let steady = constants.iter().filter(|c| c.run.value() + EPS >= best.run.value() * (1.0 - PACE.tie)).min_by(|a, b| a.peak().total_cmp(&b.peak())).map(Candidate::peak);
    // Keep the best benchmark, but do not prescribe a burst for a negligible
    // improvement. Publish the recommendation's exact ledger and objective gap.
    let tolerance = PACE.precision * natural * horizon;
    let mut recommended = best.clone();
    for c in &search.evaluated {
        if c.peak() > cap { continue; }
        if c.run.value() + EPS < best.run.value() - tolerance { continue; }
        if c.peak() < recommended.peak() - EPS
            || ((c.peak() - recommended.peak()).abs() <= EPS && c.better(&recommended)) { recommended = c.clone(); }
    }
    let mut best_view = best.run.view();
    best_view["segments"] = segments_view(&best, now);
    best_view["frontLoad"] = json!(steady.is_none() || best.segments.len() > 1);
    best_view["steadyUnitsPerHour"] = json!(steady);
    best_view["requiredPeakUnitsPerHour"] = json!(best.peak());
    let mut recommended_view = recommended.run.view();
    recommended_view["segments"] = segments_view(&recommended, now);
    recommended_view["requiredPeakUnitsPerHour"] = json!(recommended.peak());
    recommended_view["objectiveGapUnits"] = json!(rounded(best.run.value() - recommended.run.value()));
    recommended_view["materialityUnits"] = json!(tolerance);
    let provider = f["provider"].as_str().unwrap_or("");
    let window = forecast_replay_window(provider, f["window"].as_str().unwrap_or(""), true);
    let roster: Vec<String> = supply.iter().map(|a| a.id.clone()).collect();
    let hour_end = (now / HOUR).floor();
    let censored = censored_hours(&replay.marks, provider, &window, &roster, hour_end - config.lookback_hours + 1.0, hour_end);
    let mut hourly: HashMap<i64, f64> = HashMap::new();
    for event in &replay.events {
        if event.provider.as_deref() == Some(provider) && event.window == window {
            let h = (event.at / HOUR).floor();
            if h > hour_end - config.lookback_hours && h < hour_end && !censored.contains(&(h as i64)) {
                *hourly.entry(h as i64).or_default() += event.units;
            }
        }
    }
    let observed_max = hourly.values().copied().fold(0.0, f64::max);
    let mut without = supply.to_vec();
    for a in &mut without { a.credits.clear(); }
    search.evaluations += 1;
    let natural_run = simulate_policy(&without, &|t| recommended.rate(t), &search.opts, &recommended.policy);
    let marginal_total = recommended.run.value() - natural_run.value();
    let accounts: Vec<Value> = supply.iter().enumerate().map(|(i, a)| {
        let redemptions: Vec<f64> = recommended.run.result.redemptions.iter().filter(|r| r.id == a.id).map(|r| r.at).collect();
        let deadline = redemptions.first().copied();
        let gain = redemptions.len() as f64;
        let current_redemptions: Vec<f64> = current.run.result.redemptions.iter().filter(|r| r.id == a.id).map(|r| r.at).collect();
        let observed_redemptions: Vec<f64> = observed.run.result.redemptions.iter().filter(|r| r.id == a.id).map(|r| r.at).collect();
        let conflict = observed_redemptions != current_redemptions && observed.run.value() - current.run.value() > tolerance;
        let plan = (a.refill && f["creditPlan"]["planned"] == true).then(|| f["creditPlan"]["credits"].as_array().and_then(|cs| cs.iter().find(|c| c["accountId"] == a.id))).flatten();
        let plan_reason = if !a.refill { Some("not applicable to reset provider") } else if f["creditPlan"]["planned"] != true { Some("creditPlan is not planned") } else if plan.is_none() { Some("no published per-account creditPlan action") } else { None };
        let action = if !enabled { "automation-off" } else if a.stale { "unavailable" } else if gain == 0.0 { "not-redeemed-in-plan" }
            else if deadline.is_some_and(|t| t >= a.reset_at.unwrap_or(INF)) { "hold-until-reset" }
            else if a.effective_units_left() > EPS { "drain-now" } else { "redeem-when-out-by" };
        let credits: Vec<Value> = a.credits.iter().enumerate().map(|(j, expiry)| {
            let mut minus = supply.to_vec();
            minus[i].credits.remove(j);
            let marginal = if !enabled { Some(0.0) } else if search.evaluations < BUDGET {
                search.evaluations += 1;
                let x = simulate_policy(&minus, &|t| recommended.rate(t), &search.opts, &recommended.policy);
                Some(rounded(recommended.run.value() - x.value()))
            } else { search.exhausted = true; None };
            json!({ "expiresAt": if expiry.is_finite() { Some(js::iso(*expiry)) } else { None }, "nominalUnits": a.weight, "marginalUnits": marginal })
        }).collect();
        json!({
            "accountId": a.id, "action": action, "deadline": deadline.map(js::iso),
            "legacyBaselineAction": if current_redemptions.is_empty() { "wait" } else { "redeem-when-pool-out" },
            "currentDemandAction": if !enabled { "automation-off" } else if observed_redemptions.is_empty() { "wait" } else { "redeem-when-out-by" },
            "currentDemandDeadline": observed_redemptions.first().copied().map(js::iso),
            "differsFromLegacyBaseline": conflict, "credits": credits,
            "creditPlanAction": plan.and_then(|p| p.get("action")),
            "creditPlanDrainBy": plan.and_then(|p| p.get("drainBy")), "creditPlanReason": plan_reason,
            "scheduleDiffersFromCurrentDemand": redemptions != current_redemptions,
            "currentUnitsPerHour": rounded(current.run.per_account[i] / horizon),
            "recommendedUnitsPerHour": rounded(recommended.run.per_account[i] / horizon),
        })
    }).collect();
    json!({
        "format": FORMAT, "computedAt": js::iso(now), "key": "",
        "horizonHours": horizon, "until": js::iso(end),
        "method": "bounded-search", "optimal": false,
        "evaluations": search.evaluations, "evaluationBudget": BUDGET, "budgetExhausted": search.exhausted || coordinate_limited,
        "objective": "delivered work plus terminal window quota; terminal banked credits valued at zero",
        "demandUnitsPerHour": demand,
        "advisoryOnly": true, "feasibility": "unverified", "rateCapUnitsPerHour": cap,
        "creditsModeled": enabled,
        "assumptions": ["known general credits only; no future grants", "hourly quota algebra, not measured throughput", "session and model caps constrain the same weekly stock", "requires useful additional work and account scheduling; does not change automation"],
        "currentPolicy": current.run.view(), "observedDemandBest": observed.run.view(),
        "currentPolicyBasis": "legacy forecast baseline (bare simulate, same as pace); not a replay of creditPlan, before-lapse selection, real-time headroom/cooldown/refusal gates, holder tie-breaks, or future replanning",
        "observedDemandDeliveredGainUnits": rounded(observed.run.delivered() - current.run.delivered()),
        "bestFound": best_view,
        "recommended": recommended_view,
        "capacityGainUnits": rounded((best.run.value() - current.run.value()).max(0.0)),
        "recommendedGainUnits": rounded((recommended.run.value() - current.run.value()).max(0.0)),
        "sustainableUnitsPerHour": natural,
        "finiteHorizonUnitsPerHour": recommended.run.value() / horizon,
        "knownCreditsMarginalUnits": rounded(marginal_total),
        "extraWorkNeededUnits": rounded((recommended.run.delivered() - current.run.delivered()).max(0.0)),
        "highestObservedUnitsPerHour": if observed_max > 0.0 { Some(observed_max) } else { None },
        "requiredToObservedPeakRatio": if observed_max > 0.0 { Some(recommended.peak() / observed_max) } else { None },
        "benchmark": { "bestFoundUnits": rounded(best.run.value()), "legacyBurstUnits": rounded(burst.run.value()), "legacyBurstIsOptimum": false },
        "accounts": accounts,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn enrich(doc: &mut Value, accounts: &[Value], samples: &[Value], config: &ForecastConfig, now: f64, previous: Option<&Value>, policies: &[ResetPolicy], automation: bool) {
    let replay = replay_samples(samples, &config.weights, accounts);
    let Some(providers) = doc["providers"].as_object_mut() else { return; };
    for (provider, f) in providers {
        let supply = supply_for(accounts, provider, weekly_window_id(provider).unwrap(), &config.weights, true);
        if supply.is_empty() || f["state"] == "unknown" || f["stale"] == true || supply.iter().any(|a| a.stale)
            || !(natural_units_per_hour(&supply) * PACE.burst).is_finite() { continue; }
        let policy = policies.iter().find(|p| p.provider == *provider);
        let enabled = automation && policy.is_none_or(|p| p.enabled);
        let runway = policy.and_then(|p| p.max_runway_percent).unwrap_or_else(|| default_max_runway_percent(provider));
        let key = js::digest(&json!([FORMAT, supply.iter().map(|a| json!([a.id, a.weight, a.used, a.reset_at, a.period_ms, a.first_use, a.floating, a.refill, a.unmodeled_credits, a.credits.iter().map(|c| if c.is_finite() { Some(*c) } else { None }).collect::<Vec<_>>(), a.stale, a.caps.iter().map(|c| json!([c.used, c.reset_at, c.period_ms, c.floating])).collect::<Vec<_>>()])).collect::<Vec<_>>(), accounts.iter().filter(|a| a["provider"] == *provider).map(|a| json!([a["id"], a["modelCreditCount"]])).collect::<Vec<_>>(), enabled, runway, f["demand"]["unitsPerHour"], (now / HOUR).floor(), f["pace"], config.to_value()]));
        let key = js::digest(&json!([key, f["creditPlan"]]));
        let prev = previous.and_then(|d| d["providers"][provider]["guidance"].as_object());
        if let Some(prev) = prev {
            let at = js::date_parse_value(prev.get("computedAt"));
            if prev.get("key") == Some(&json!(key)) && at <= now && now - at < PACE.reuse_minutes * 60000.0 {
                f["guidance"] = Value::Object(prev.clone());
                continue;
            }
        }
        let mut g = guidance(&supply, f, &replay, now, config, enabled, runway);
        g["inventory"] = json!({
            "modeledGeneralCredits": supply.iter().map(|a| a.credits.len()).sum::<usize>(),
            "unmodeledGeneralCredits": supply.iter().map(|a| a.unmodeled_credits).sum::<f64>(),
            "modelScopedCreditsExcluded": accounts.iter().filter(|a| a["provider"] == *provider && !js::truthy(js::get(Some(*a), "disabled"))).map(|a| a["modelCreditCount"].as_u64().unwrap_or(0)).sum::<u64>(),
            "reason": "model-scoped grants are not additional all-model supply; no unverified scope conversion",
        });
        g["key"] = json!(key);
        f["guidance"] = g;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, refill: bool, used: f64, reset: f64, credits: Vec<f64>) -> Supply {
        Supply { id: id.into(), weight: 1.0, used, reset_at: Some(reset * HOUR), period_ms: 168.0 * HOUR, first_use: !refill, floating: false, refill, caps: Vec::new(), credits: credits.into_iter().map(|h| h * HOUR).collect(), unmodeled_credits: 0.0, stale: false, scope: json!("All models"), label: json!("Weekly") }
    }

    fn check_ledger(r: &Run) {
        assert!((r.starting + r.renewed + r.redeemed - r.delivered() - r.terminal_nominal - r.window_waste).abs() < 1e-8);
    }

    #[test]
    fn positive_demand_gains_are_real_work_not_inventory() {
        let config = ForecastConfig::default();
        let f = json!({"provider":"codex", "window":"codex:primary_window", "demand":{"unitsPerHour":0.001}, "pace":{"horizonHours":192}});
        let supply = vec![account("a", false, 0.0, 100.0, vec![72.0])];
        let covered = guidance(&supply, &f, &Replay::default(), 0.0, &config, true, 50.0);
        assert_eq!(covered["observedDemandDeliveredGainUnits"], 0.0);
        assert_eq!(covered["accounts"][0]["differsFromLegacyBaseline"], false);
        let f = json!({"provider":"codex", "window":"codex:primary_window", "demand":{"unitsPerHour":0.1}, "pace":{"horizonHours":192}});
        let short = guidance(&[account("a", false, 100.0, 1.0, vec![72.0])], &f, &Replay::default(), 0.0, &config, true, 50.0);
        assert!(short["observedDemandDeliveredGainUnits"].as_f64().unwrap() > 0.5, "{short}");
        assert_eq!(short["accounts"][0]["differsFromLegacyBaseline"], true);
        assert!(short["recommended"]["objectiveGapUnits"].as_f64().unwrap() <= short["recommended"]["materialityUnits"].as_f64().unwrap() + EPS);
    }

    #[test]
    fn claude_drain_first_refills_without_moving_its_natural_reset() {
        let supply = vec![account("credit", true, 99.0, 10.0, vec![5.0]), account("other", true, 0.0, 20.0, vec![])];
        let mut policy = Policy::candidate(2);
        policy.drain_first[0] = true;
        let opts = SimOptions::new(0.0, 12.0, true, 50.0);
        let run = simulate_policy(&supply, &|_| 0.01, &opts, &policy);
        let old = simulate_policy(&supply, &|_| 0.01, &opts, &Policy::legacy());
        assert_eq!(run.result.credits_used, 1.0);
        assert_eq!(run.result.redemptions[0].at, HOUR);
        assert_eq!(old.result.credits_used, 0.0);
        assert_eq!(run.renewed, 1.0, "refill keeps the 10h renewal");
        check_ledger(&run);
        check_ledger(&old);
    }

    #[test]
    fn memoization_does_not_spend_another_evaluation() {
        let supply = vec![account("a", false, 0.0, 100.0, vec![72.0])];
        let mut search = Search { supply: &supply, opts: SimOptions::new(0.0, 24.0, true, 50.0), evaluations: 0, exhausted: false, evaluated: Vec::new(), memo: HashMap::new() };
        let p = Policy::candidate(1);
        let a = search.evaluate(&p, &[(24.0 * HOUR, 0.1)]).unwrap();
        let b = search.evaluate(&p, &[(24.0 * HOUR, 0.1)]).unwrap();
        assert_eq!(a.run.view(), b.run.view());
        assert_eq!(search.evaluations, 1);
    }

    #[test]
    fn published_credit_plan_is_copied_not_mistaken_for_a_simulated_policy() {
        let supply = vec![account("a", true, 0.0, 100.0, vec![72.0])];
        let mut f = json!({"provider":"claude", "window":"limit:1", "demand":{"unitsPerHour":0.001}, "pace":{"horizonHours":24}, "creditPlan":{"planned":true,"credits":[{"accountId":"a","action":"drain-first","drainBy":"1970-01-04T00:00:00.000Z"}]}});
        let g = guidance(&supply, &f, &Replay::default(), 0.0, &ForecastConfig::default(), true, 50.0);
        assert_eq!(g["accounts"][0]["creditPlanAction"], "drain-first");
        assert_eq!(g["accounts"][0]["creditPlanDrainBy"], f["creditPlan"]["credits"][0]["drainBy"]);
        assert!(g["accounts"][0]["creditPlanReason"].is_null());
        assert!(g["currentPolicyBasis"].as_str().unwrap().contains("not a replay"));
        assert!(g["accounts"][0].get("conflictsWithAutomation").is_none());
        f["creditPlan"]["planned"] = json!(false);
        let g = guidance(&supply, &f, &Replay::default(), 0.0, &ForecastConfig::default(), true, 50.0);
        assert!(g["accounts"][0]["creditPlanAction"].is_null());
        assert_eq!(g["accounts"][0]["creditPlanReason"], "creditPlan is not planned");
    }

    #[test]
    fn exhausted_search_budget_is_explicit_and_marginals_become_unknown() {
        let supply: Vec<_> = (0..40).map(|i| account(&format!("a{i}"), false, 50.0, 100.0, vec![72.0; 30])).collect();
        let f = json!({"provider":"codex", "window":"codex:primary_window", "demand":{"unitsPerHour":0.1}, "pace":{"horizonHours":24}});
        let g = guidance(&supply, &f, &Replay::default(), 0.0, &ForecastConfig::default(), true, 50.0);
        assert_eq!(g["budgetExhausted"], true);
        assert_eq!(g["evaluations"], BUDGET);
        assert!(g["accounts"].as_array().unwrap().iter().flat_map(|a| a["credits"].as_array().unwrap()).any(|c| c["marginalUnits"].is_null()));
    }
}
