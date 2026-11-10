//! Banked credits as the providers report them: Codex resets (provider-codex.mjs
//! `availableCredits`) and the Claude refill bank (provider-claude.mjs `claudeBank`).

use crate::js;
use serde::Serialize;
use serde_json::Value;

/// `availableCredits(data, now)`: available Codex rate-limit resets that have not expired,
/// soonest expiry first. The credits are returned as the payload held them.
pub fn available_credits(data: &Value, now: f64) -> Vec<Value> {
    let credits: &[Value] = match js::get(Some(data), "credits") {
        Some(Value::Array(a)) => a,
        _ => &[],
    };
    let expiry = |c: &Value| {
        js::expiry_ms(js::coalesce([
            js::get(Some(c), "expires_at"),
            js::get(Some(c), "expiresAt"),
        ]))
    };
    let mut out: Vec<Value> = credits
        .iter()
        .filter(|c| {
            let c = Some(*c);
            js::get(c, "status").and_then(Value::as_str) == Some("available")
                && matches!(
                    js::coalesce([js::get(c, "reset_type"), js::get(c, "resetType")])
                        .and_then(Value::as_str),
                    Some("codex_rate_limits" | "codexRateLimits")
                )
        })
        .filter(|c| expiry(c) > now)
        .cloned()
        .collect();
    js::sort_by(&mut out, |a, b| expiry(a) - expiry(b));
    out
}

/// One Claude credit offered for redemption.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ClaudeCredit {
    pub id: String,
    pub expires_at: Value,
    pub label: String,
    /// A model-scoped grant only: the usage windows the claim clears and how much of each the
    /// server reports used (`percent_used` also carries `seven_day`, the all-model weekly, which
    /// such a grant leaves as it is).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clears: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent_used: Option<Value>,
}

/// `claudeBank(block, now)`: the grant the server will redeem next (if it may be offered), the
/// count of usable refills, and why none is offered. `None` when the block is unreadable.
///
/// A full refill clears the all-model weekly (`seven_day`). A grant that does not (a launch
/// grant that resets only the Fable weekly cap, for one) restores nothing on an account that
/// is out of all-model quota, so it is kept apart: `credits` and `count` are full refills only,
/// `model_credits` and `model_count` the model-scoped grants.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ClaudeBank {
    pub credits: Vec<ClaudeCredit>,
    pub count: f64,
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub model_credits: Vec<ClaudeCredit>,
    #[serde(skip_serializing_if = "is_zero")]
    pub model_count: f64,
}

fn is_zero(n: &f64) -> bool {
    *n == 0.0
}

/// The all-model weekly window's key in a grant's `clears`.
pub const CLAUDE_ALL_MODEL_WEEKLY: &str = "seven_day";

/// The windows a grant clears when it is model-scoped; `None` for a full refill. A grant without
/// a readable `clears` list is taken as a full refill, as every grant was before scoped ones.
fn scoped_clears(g: &Value) -> Option<Vec<String>> {
    let Some(Value::Array(list)) = js::get(Some(g), "clears") else {
        return None;
    };
    let keys: Option<Vec<String>> = list.iter().map(|k| k.as_str().map(str::to_string)).collect();
    keys.filter(|k| !k.iter().any(|k| k == CLAUDE_ALL_MODEL_WEEKLY))
}

fn grant_id(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

pub fn claude_bank(block: Option<&Value>, now: f64) -> Option<ClaudeBank> {
    if !js::truthy(block) || !js::is_object(block) {
        return None;
    }
    let eligible = js::get(block, "eligible")?.as_bool()?;
    let grants: &[Value] = match js::get(block, "grants") {
        Some(Value::Array(a)) => a,
        _ => &[],
    };
    let usable: Vec<&Value> = grants
        .iter()
        .filter(|g| {
            let g = Some(*g);
            let resets_left = js::number(js::get(g, "resets_left"));
            js::is_object(g)
                && js::get(g, "id")
                    .and_then(Value::as_str)
                    .is_some_and(grant_id)
                && js::get(g, "usable_now") == Some(&Value::Bool(true))
                && js::get(g, "paused") != Some(&Value::Bool(true))
                && resets_left.is_some_and(|n| n.is_finite() && n.fract() == 0.0 && n > 0.0)
                && js::expiry_ms(js::get(g, "ends_at")) > now
                && !js::get(g, "starts_at")
                    .and_then(Value::as_str)
                    .and_then(js::date_parse)
                    .is_some_and(|s| s > now)
        })
        .collect();
    let cooling_until = js::expiry_ms(js::get(block, "cooldown_until"));
    let cooling = cooling_until != f64::INFINITY && cooling_until > now;
    let named = if eligible && !cooling {
        let next = js::get(block, "next_grant_id");
        usable
            .iter()
            .copied()
            .find(|g| js::get(Some(g), "id") == next)
    } else {
        None
    };
    let next = named.filter(|g| {
        !(js::get(Some(g), "use_requires_limit") == Some(&Value::Bool(true))
            && js::get(block, "at_limit") != Some(&Value::Bool(true)))
    });
    let credit = |g: &Value| {
        let clears = scoped_clears(g);
        ClaudeCredit {
            id: js::get(Some(g), "id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            expires_at: js::coalesce([js::get(Some(g), "ends_at")])
                .cloned()
                .unwrap_or(Value::Null),
            label: js::get(Some(g), "label")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            percent_used: clears.as_ref().map(|_| {
                js::get(Some(g), "percent_used")
                    .filter(|p| p.is_object())
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Default::default()))
            }),
            clears,
        }
    };
    let (model_next, full_next): (Vec<ClaudeCredit>, Vec<ClaudeCredit>) =
        next.map(credit).into_iter().partition(|c| c.clears.is_some());
    let left = |scoped: bool| -> f64 {
        if !eligible {
            return 0.0;
        }
        usable
            .iter()
            .filter(|g| scoped_clears(g).is_some() == scoped)
            .map(|g| js::to_number(js::get(Some(g), "resets_left")))
            .sum()
    };
    Some(ClaudeBank {
        credits: full_next,
        count: left(false),
        model_credits: model_next,
        model_count: left(true),
        note: if !eligible {
            let reason = js::coalesce([js::get(block, "ineligible_reason")]);
            Some(format!(
                "Resets not offered to this client ({})",
                reason.map_or("unknown".into(), |r| js::to_string(Some(r)))
            ))
        } else if cooling {
            Some(format!(
                "Resets cooling down until {}",
                js::to_string(js::get(block, "cooldown_until"))
            ))
        } else {
            None
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: f64 = 1_794_000_000_000.0; // 2026-11-07

    fn grant(id: &str, clears: &[&str], percent: Value) -> Value {
        json!({
            "id": id, "label": id, "resets_total": 1, "resets_left": 1,
            "starts_at": "2026-11-01T00:00:00+00:00", "ends_at": "2027-03-09T07:59:00+00:00",
            "clears": clears, "percent_used": percent,
            "paused": false, "usable_now": true, "use_requires_limit": false, "blocking": [],
        })
    }

    fn block(grants: Vec<Value>, next: &str) -> Value {
        json!({ "eligible": true, "at_limit": false, "grants": grants, "next_grant_id": next, "cooldown_until": null })
    }

    #[test]
    fn a_grant_that_leaves_the_all_model_weekly_is_kept_apart_from_full_refills() {
        // As read live on 2026-11-10: the Fable launch grant clears only the Fable cap.
        let fable = grant("launch-2610-fable", &["seven_day_overage_included"], json!({ "seven_day_overage_included": 13, "seven_day": 100 }));
        let bank = claude_bank(Some(&block(vec![fable], "launch-2610-fable")), NOW).unwrap();
        assert!(bank.credits.is_empty());
        assert_eq!(bank.count, 0.0);
        assert_eq!(bank.model_count, 1.0);
        assert_eq!(bank.model_credits.len(), 1);
        assert_eq!(bank.model_credits[0].clears.as_deref(), Some(&["seven_day_overage_included".to_string()][..]));
        assert_eq!(bank.model_credits[0].percent_used, Some(json!({ "seven_day_overage_included": 13, "seven_day": 100 })));
    }

    #[test]
    fn a_full_refill_reads_as_before() {
        let full = grant("opus-launch", &["five_hour", "seven_day", "seven_day_overage_included"], json!({}));
        let bank = claude_bank(Some(&block(vec![full.clone()], "opus-launch")), NOW).unwrap();
        assert_eq!(bank.count, 1.0);
        assert_eq!(bank.model_count, 0.0);
        assert!(bank.model_credits.is_empty());
        assert_eq!(bank.credits[0].clears, None);
        let shown = serde_json::to_value(&bank).unwrap();
        assert_eq!(shown, json!({ "credits": [{ "id": "opus-launch", "expires_at": "2027-03-09T07:59:00+00:00", "label": "opus-launch" }], "count": 1.0, "note": null }));

        // A grant without a readable `clears` is a full refill, as every grant was.
        let mut bare = full;
        bare.as_object_mut().unwrap().remove("clears");
        assert_eq!(claude_bank(Some(&block(vec![bare], "opus-launch")), NOW).unwrap().count, 1.0);
    }

    #[test]
    fn counts_split_by_scope_and_only_the_next_grant_is_offered() {
        let fable = grant("launch-2610-fable", &["five_hour", "seven_day_overage_included"], json!({}));
        let full = grant("opus-launch", &["five_hour", "seven_day"], json!({}));
        let bank = claude_bank(Some(&block(vec![fable, full], "opus-launch")), NOW).unwrap();
        assert_eq!((bank.count, bank.model_count), (1.0, 1.0));
        assert_eq!(bank.credits.len(), 1);
        assert!(bank.model_credits.is_empty(), "the server redeems only the grant it names next");
    }
}
