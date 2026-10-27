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
}

/// `claudeBank(block, now)`: the grant the server will redeem next (if it may be offered), the
/// count of usable refills, and why none is offered. `None` when the block is unreadable.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ClaudeBank {
    pub credits: Vec<ClaudeCredit>,
    pub count: f64,
    pub note: Option<String>,
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
    Some(ClaudeBank {
        credits: next
            .map(|g| ClaudeCredit {
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
            })
            .into_iter()
            .collect(),
        count: if eligible {
            usable
                .iter()
                .map(|g| js::to_number(js::get(Some(g), "resets_left")))
                .sum()
        } else {
            0.0
        },
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
