//! Banked-reset configuration (resets.mjs `resetPolicy` and `resetPolicies`): install.json
//! `resetPolicy.<provider>` over the adapter's defaults, with invalid keys reported.

use crate::js;
use crate::providers::{self, Provider};
use serde_json::{json, Map, Value};

pub const RESET_TRIGGERS: [&str; 2] = ["any", "weekly"];

/// One provider's reset policy.
#[derive(Clone, Debug)]
pub struct ResetPolicy {
    pub provider: String,
    pub redemption: &'static str,
    pub enabled: bool,
    pub trigger: String,
    pub headroom_percent: f64,
    pub redeem_before_lapse: bool,
    /// Refill providers only.
    pub max_runway_percent: Option<f64>,
    /// `expectedCount`: `Infinity` when the roster size is unknown or invalid.
    pub expected_count: f64,
    pub problems: Vec<String>,
}

impl ResetPolicy {
    /// The object resets.mjs returns, as `JSON.stringify` writes it (`Infinity` as `null`).
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("provider".into(), json!(self.provider));
        m.insert("redemption".into(), json!(self.redemption));
        m.insert("enabled".into(), json!(self.enabled));
        m.insert("trigger".into(), json!(self.trigger));
        m.insert("headroomPercent".into(), json!(self.headroom_percent));
        m.insert("redeemBeforeLapse".into(), json!(self.redeem_before_lapse));
        if let Some(p) = self.max_runway_percent {
            m.insert("maxRunwayPercent".into(), json!(p));
        }
        m.insert("expectedCount".into(), json!(self.expected_count));
        m.insert("problems".into(), json!(self.problems));
        Value::Object(m)
    }
}

/// `resetPolicy(provider, config, expectedCount)`.
pub fn reset_policy(
    adapter: &Provider,
    config: Option<&Value>,
    expected_count: Option<&Value>,
) -> ResetPolicy {
    let d = adapter.reset_defaults;
    let id = adapter.id;
    let input = match config {
        Some(Value::Object(_)) => config,
        _ => None,
    };
    let mut problems = Vec::new();
    if !js::nullish(config) && input.is_none() {
        problems.push(format!(
            "resetPolicy.{id} must be an object; using defaults"
        ));
    }
    let mut pick = |key: &str, default: Value, valid: &dyn Fn(&Value) -> bool| -> Value {
        match js::get(input, key) {
            None => default,
            Some(v) if valid(v) => v.clone(),
            Some(_) => {
                problems.push(format!(
                    "resetPolicy.{id}.{key} is invalid; using {}",
                    js::stringify(&default)
                ));
                default
            }
        }
    };
    let boolean = |v: &Value| v.is_boolean();
    let enabled = pick("enabled", json!(d.enabled), &boolean);
    let trigger = pick("trigger", json!(d.trigger), &|v| {
        v.as_str().is_some_and(|s| RESET_TRIGGERS.contains(&s))
    });
    let max_headroom = adapter.max_headroom_percent;
    let headroom = pick("headroomPercent", json!(d.headroom_percent), &|v| {
        v.as_f64()
            .is_some_and(|f| f.is_finite() && f >= 0.0 && f <= max_headroom)
    });
    let before_lapse = pick("redeemBeforeLapse", json!(d.redeem_before_lapse), &boolean);
    let runway = (adapter.redemption == providers::Redemption::Refill).then(|| {
        pick(
            "maxRunwayPercent",
            js::number_value(d.max_runway_percent.unwrap_or(f64::NAN)),
            &|v| {
                v.as_f64()
                    .is_some_and(|f| f.is_finite() && (0.0..=100.0).contains(&f))
            },
        )
    });
    let expected = js::number(expected_count)
        .filter(|n| n.is_finite() && n.fract() == 0.0 && *n > 0.0)
        .unwrap_or(f64::INFINITY);
    ResetPolicy {
        provider: id.into(),
        redemption: adapter.redemption.as_str(),
        enabled: enabled.as_bool().unwrap_or(d.enabled),
        trigger: trigger.as_str().unwrap_or(d.trigger).into(),
        headroom_percent: headroom.as_f64().unwrap_or(d.headroom_percent),
        redeem_before_lapse: before_lapse.as_bool().unwrap_or(d.redeem_before_lapse),
        max_runway_percent: runway.and_then(|v| v.as_f64()),
        expected_count: expected,
        problems,
    }
}

/// `resetPolicies(config)`: every provider, in `PROVIDER_IDS` order.
pub fn reset_policies(config: &Value) -> Vec<ResetPolicy> {
    let c = Some(config);
    providers::PROVIDERS
        .iter()
        .map(|p| {
            reset_policy(
                p,
                js::get(js::get(c, "resetPolicy"), p.id),
                js::get(js::get(c, "expectedAccounts"), p.id),
            )
        })
        .collect()
}
