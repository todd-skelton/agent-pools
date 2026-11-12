mod common;

use common::{input, golden};
use pool_core::forecast::{self, guidance::{Policy, simulate_policy}, Cap, Supply, SimOptions, SimResult, HOUR};
use pool_core::{js, resets};
use serde_json::{json, Value};

fn account(id: &str, refill: bool, used: f64, reset: f64, credits: Vec<f64>) -> Supply {
    Supply { id: id.into(), weight: 1.0, used, reset_at: Some(reset * HOUR), period_ms: 168.0 * HOUR, first_use: !refill, floating: false, refill, caps: Vec::new(), credits: credits.into_iter().map(|t| t * HOUR).collect(), unmodeled_credits: 0.0, stale: false, scope: json!("All models"), label: json!("Weekly") }
}

fn fields(r: &SimResult) -> Value {
    json!([r.unmet_units, r.remaining_units, r.credit_units_left, r.outage_hours, r.first_outage_at, r.expiring_units, r.credits_used, r.credits_expired, r.credit_units_expired, r.wasted_units, r.credits_granted,
        r.redemptions.iter().map(|x| json!([x.id, x.at])).collect::<Vec<_>>(), r.lapses.iter().map(|x| json!([x.id, x.at])).collect::<Vec<_>>()])
}

fn ledger(run: &forecast::guidance::Run) {
    let source = run.starting + run.renewed + run.redeemed;
    let destination = run.delivered() + run.terminal_nominal + run.window_waste;
    assert!((source - destination).abs() < 1e-6, "ledger: {source} != {destination}");
}

#[test]
fn legacy_policy_matches_every_simulation_field_on_seeded_supplies() {
    let mut seed = 73829u64;
    let mut next = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 32) as f64 / u32::MAX as f64 };
    for n in 0..100 {
        let mut supply = Vec::new();
        for i in 0..(n % 4 + 1) {
            let mut a = account(&format!("a{i}"), n % 2 == 0, (next() * 100.0).floor(), next() * 168.0, vec![next() * 300.0, 400.0]);
            a.weight = if i % 2 == 0 { 0.5 } else { 1.0 };
            a.floating = n % 5 == 0;
            if n % 3 == 0 { a.caps.push(Cap { used: next() * 100.0, reset_at: Some(5.0 * HOUR), period_ms: 5.0 * HOUR, floating: false, scope: json!("All models"), label: json!("5-hour") }); }
            supply.push(a);
        }
        let rate = next() * 0.4;
        let grants = [50.0 * HOUR, 200.0 * HOUR];
        let early = supply.iter().map(|a| a.id.clone()).collect();
        let drain = forecast::DrainFirst(supply.iter().map(|a| (a.id.clone(), a.credits[0])).collect());
        let mut opts = SimOptions::new(0.0, 450.0, n % 7 != 0, 50.0);
        opts.grants = &grants;
        if n % 4 == 0 { opts.early_refill = Some(&early); opts.drain_first = Some(&drain); }
        let old = forecast::simulate(&supply, &|_| rate, &opts);
        let new = simulate_policy(&supply, &|_| rate, &opts, &Policy::legacy());
        assert_eq!(fields(&old), fields(&new.result), "seeded case {n}");
        ledger(&new);
    }
}

#[test]
fn legacy_policy_matches_golden_supply_variants() {
    for case in golden("forecast/variants.json")["cases"].as_array().unwrap() {
        let raw = &case["input"];
        let now = raw["now"].as_f64().unwrap();
        let (config, _) = forecast::forecast_config(raw.get("capacityForecast"));
        for provider in ["codex", "claude"] {
            let window = if provider == "codex" { "codex:primary_window" } else { "limit:1" };
            let supply = forecast::supply_for(raw["accounts"].as_array().unwrap(), provider, window, &config.weights, true);
            let opts = SimOptions::new(now, 336.0, true, 50.0);
            for rate in [0.0, 0.01, 0.1] {
                let old = forecast::simulate(&supply, &|_| rate, &opts);
                let new = simulate_policy(&supply, &|_| rate, &opts, &Policy::legacy());
                assert_eq!(fields(&old), fields(&new.result), "{} {provider}", case["name"]);
                ledger(&new);
            }
        }
    }
}

#[test]
fn ledger_counts_weekly_stock_overwritten_when_a_cap_blocks_redemption() {
    let mut a = account("a", true, 25.0, 100.0, vec![300.0]);
    a.caps.push(Cap { used: 100.0, reset_at: Some(100.0 * HOUR), period_ms: 168.0 * HOUR, floating: false, scope: json!("Fable"), label: json!("Weekly") });
    let run = simulate_policy(&[a], &|_| 0.1, &SimOptions::new(0.0, 2.0, true, 50.0), &Policy::legacy());
    assert_eq!(run.overwritten, 0.75);
    ledger(&run);
}

fn inputs() -> (Vec<Value>, forecast::ForecastConfig, f64) {
    let pool = input("pool-state.json");
    let install = input("install.json");
    let now = js::date_parse_value(input("manifest.json").get("now"));
    (pool["accounts"].as_array().unwrap().clone(), forecast::forecast_config(install.get("capacityForecast")).0, now)
}

#[test]
fn guided_forecast_preserves_every_policy_field_and_is_deterministic() {
    let (accounts, mut config, now) = inputs();
    config.min_observed_hours = 0.0;
    let old = forecast::build_forecast(&accounts, &[], &config, now, None, &[], true);
    let mut new = forecast::build_guided_forecast(&accounts, &[], &config, now, None, &[], true);
    let again = forecast::build_guided_forecast(&accounts, &[], &config, now, None, &[], true);
    assert_eq!(new, again);
    let mut count = 0;
    for f in new["providers"].as_object_mut().unwrap().values_mut() {
        let g = f.as_object_mut().unwrap().remove("guidance").unwrap();
        assert_eq!(g["optimal"], false);
        assert_eq!(g["feasibility"], "unverified");
        assert!(g["recommended"]["requiredPeakUnitsPerHour"].as_f64().unwrap() <= g["bestFound"]["requiredPeakUnitsPerHour"].as_f64().unwrap());
        assert!(g["recommended"]["requiredPeakUnitsPerHour"].as_f64().unwrap() <= g["rateCapUnitsPerHour"].as_f64().unwrap());
        assert!(g["evaluations"].as_u64().unwrap() <= g["evaluationBudget"].as_u64().unwrap());
        let best = g["benchmark"]["bestFoundUnits"].as_f64().unwrap();
        for key in ["atDemand", "fullUse", "noGap"] {
            if let Some(x) = f["pace"][key]["usedUnits"].as_f64() { assert!(best + 0.02 >= x, "{key}: {best} < {x}"); }
        }
        for row in f["pace"]["curve"].as_array().unwrap() { assert!(best + 0.02 >= row["usedUnits"].as_f64().unwrap()); }
        for row in g["bestFound"]["segments"].as_array().unwrap() {
            let rate = row["unitsPerHour"].as_f64().unwrap();
            assert!(rate.is_finite() && rate <= g["rateCapUnitsPerHour"].as_f64().unwrap());
        }
        assert_eq!(g["observedDemandDeliveredGainUnits"], 0.0);
        count += 1;
    }
    assert_eq!(count, 2);
    assert_eq!(old, new, "guidance changes no existing field, including pace and decisionDigest");
}

#[test]
fn disabled_automation_never_spends_banked_supply_in_guidance() {
    let (accounts, mut config, now) = inputs();
    config.min_observed_hours = 0.0;
    let policies = resets::reset_policies(&json!({}));
    let doc = forecast::build_guided_forecast(&accounts, &[], &config, now, None, &policies, false);
    for f in doc["providers"].as_object().unwrap().values() {
        assert_eq!(f["guidance"]["bestFound"]["creditsUsed"], 0.0);
        assert_eq!(f["guidance"]["knownCreditsMarginalUnits"], 0.0);
        assert_eq!(f["guidance"]["creditsModeled"], false);
        for a in f["guidance"]["accounts"].as_array().unwrap() { assert_eq!(a["action"], "automation-off"); }
    }
}

#[test]
fn reset_just_before_natural_renewal_has_no_marginal_capacity() {
    let a = account("a", false, 100.0, 1.0, vec![72.0]);
    let opts = SimOptions::new(0.0, 10.0, true, 50.0);
    let with = simulate_policy(std::slice::from_ref(&a), &|_| 0.1, &opts, &Policy::legacy());
    let mut without = a;
    without.credits.clear();
    let without = simulate_policy(&[without], &|_| 0.1, &opts, &Policy::legacy());
    assert!((with.value() - without.value()).abs() < 1e-9);
    assert_eq!(with.result.credits_used, 1.0);
    ledger(&with);
    ledger(&without);
}

#[test]
fn sanitized_live_openai_snapshot_beats_the_burst_reference() {
    let now = js::date_parse_value(Some(&json!("2026-11-12T18:12:03.552Z")));
    let expiries = ["2026-11-30T19:48:06.617Z", "2026-12-01T22:23:18.072Z"];
    let accounts: Vec<Value> = [
        ("snapshot-a-pro.json", 63.0, "2026-11-15T14:02:00Z"),
        ("snapshot-b-pro.json", 0.0, "2026-11-18T06:37:23Z"),
        ("snapshot-c-pro.json", 0.0, "2026-11-16T23:36:52Z"),
        ("snapshot-d-prolite.json", 14.0, "2026-11-16T22:29:57Z"),
    ].into_iter().map(|(id, used, reset)| json!({
        "id": id, "provider": "codex", "status": "available", "creditsKnown": true,
        "creditCount": 2, "credits": expiries.iter().map(|e| json!({"expiresAt":e})).collect::<Vec<_>>(),
        "quotaWindows": [{"id":"codex:primary_window", "common":true, "used":used,
            "durationSeconds":604800, "resetAt":js::date_parse_value(Some(&json!(reset)))}]
    })).collect();
    let config = forecast::ForecastConfig { min_observed_hours: 0.0, ..Default::default() };
    let doc = forecast::build_guided_forecast(&accounts, &[], &config, now, None, &[], true);
    let g = &doc["providers"]["codex"]["guidance"];
    let best = g["benchmark"]["bestFoundUnits"].as_f64().unwrap();
    let burst = g["benchmark"]["legacyBurstUnits"].as_f64().unwrap();
    assert!(best >= 21.29, "{g}");
    assert!(best > burst + 0.5, "best={best}, burst={burst}");
    assert_eq!(g["benchmark"]["legacyBurstIsOptimum"], false);
}

#[test]
fn unknown_and_stale_account_evidence_suppresses_guidance() {
    let (mut accounts, mut config, now) = inputs();
    config.min_observed_hours = 0.0;
    for a in &mut accounts { a["usageStale"] = json!(true); }
    let doc = forecast::build_guided_forecast(&accounts, &[], &config, now, None, &[], true);
    for f in doc["providers"].as_object().unwrap().values() { assert!(f.get("guidance").is_none()); }
}

#[test]
fn one_stale_account_suppresses_the_otherwise_known_provider() {
    let (mut accounts, mut config, now) = inputs();
    config.min_observed_hours = 0.0;
    let provider = accounts[0]["provider"].as_str().unwrap().to_owned();
    accounts[0]["usageStale"] = json!(true);
    let doc = forecast::build_guided_forecast(&accounts, &[], &config, now, None, &[], true);
    assert_ne!(doc["providers"][&provider]["state"], "unknown");
    assert!(doc["providers"][&provider].get("guidance").is_none());
}

#[test]
fn unchanged_guidance_is_reused_and_material_state_changes_invalidate_it() {
    let (mut accounts, mut config, now) = inputs();
    let now = (now / HOUR).floor() * HOUR + 5.0 * 60000.0;
    config.min_observed_hours = 0.0;
    let first = forecast::build_guided_forecast(&accounts, &[], &config, now, None, &[], true);
    let reused = forecast::build_guided_forecast(&accounts, &[], &config, now + 60000.0, Some(&first), &[], true);
    for p in ["codex", "claude"] {
        assert_eq!(first["providers"][p]["guidance"], reused["providers"][p]["guidance"]);
    }
    let a = accounts.iter_mut().find(|a| a["creditCount"].as_f64().unwrap_or(0.0) > 0.0).unwrap();
    let provider = a["provider"].as_str().unwrap().to_owned();
    a["creditCount"] = json!(0);
    a["credits"] = json!([]);
    let changed = forecast::build_guided_forecast(&accounts, &[], &config, now + 60000.0, Some(&first), &[], true);
    assert_ne!(first["providers"][&provider]["guidance"]["key"], changed["providers"][&provider]["guidance"]["key"]);
}

#[test]
fn release_runtime_is_reported_at_the_maximum_horizon() {
    let (accounts, mut config, now) = inputs();
    config.min_observed_hours = 0.0;
    config.horizon_days = 35.0;
    config.max_horizon_days = 35.0;
    for p in ["codex", "claude"] {
        let mut roster: Vec<Value> = accounts.iter().filter(|a| a["provider"] == p).cloned().collect();
        if p == "claude" {
            let mut extra = roster.clone();
            for (i, a) in extra.iter_mut().enumerate() { a["id"] = json!(format!("extra-{i}-max.json")); }
            roster.extend(extra);
        }
        for (i, a) in roster.iter_mut().enumerate() {
            let count = if p == "codex" { 2 } else if i < 4 { 1 } else { 0 };
            a["creditsKnown"] = json!(true);
            a["creditCount"] = json!(count);
            a["credits"] = json!((0..count).map(|j| json!({"expiresAt":js::iso(now + (18.0 + j as f64) * 24.0 * HOUR)})).collect::<Vec<_>>());
        }
        let started = std::time::Instant::now();
        let doc = forecast::build_guided_forecast(&roster, &[], &config, now, None, &[], true);
        let elapsed = started.elapsed();
        let g = &doc["providers"][p]["guidance"];
        eprintln!("guidance {p}: {} accounts, {}h, {} simulations, {:.3}s", roster.len(), g["horizonHours"], g["evaluations"], elapsed.as_secs_f64());
        assert!(g.is_object());
        assert!(g["evaluations"].as_u64().unwrap() <= 1536);
    }
}

#[test]
fn account_gate_waits_for_imminent_natural_reset_before_spending_credit() {
    let now = js::date_parse_value(Some(&json!("2026-11-12T00:00:00Z")));
    let accounts = vec![json!({
        "id":"single-pro.json", "provider":"codex", "status":"available", "creditCount":1,
        "credits":[{"expiresAt":js::iso(now + 72.0 * HOUR)}],
        "quotaWindows":[{"id":"codex:primary_window", "common":true, "used":100,
            "durationSeconds":604800, "resetAt":now + HOUR}]
    })];
    let config = forecast::ForecastConfig { min_observed_hours:0.0, horizon_days:1.0, max_horizon_days:1.0, ..Default::default() };
    let doc = forecast::build_guided_forecast(&accounts, &[], &config, now, None, &[], true);
    let g = &doc["providers"]["codex"]["guidance"];
    assert_eq!(g["benchmark"]["legacyBurstUnits"], 2.0);
    assert_eq!(g["benchmark"]["bestFoundUnits"], 3.0, "{g}");
    assert_eq!(g["accounts"][0]["action"], "hold-until-reset");
}
