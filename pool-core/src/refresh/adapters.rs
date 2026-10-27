//! What each provider adapter does on a refresh (provider-codex.mjs, provider-claude.mjs):
//! request identity, the reset bank, redemption, the plan read, and whether a model limit
//! blocks quota recovery. The calls go through [`Cycle::read`](super::Cycle), so the proxy
//! injects the account's token and nothing here sees one.

use super::obj::{at, is_integer, jsify, num, put, str_at};
use super::{Cycle, Failure, Io};
use crate::banks::{available_credits, claude_bank};
use crate::js;
use crate::quota::{normalize_usage, Usage};
use serde_json::{json, Map, Value};

pub const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
pub const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1";
const BACKEND: &str = "https://chatgpt.com/backend-api";
pub const CODEX_CREDITS_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
pub const CLAUDE_PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const PRICING_COUNTRY: &str = "US";
const CLAUDE_LIST_PRICE_SOURCE: &str = "claude.com/pricing, read 2026-10-15";

/// `adapter.usageUrl`.
pub fn usage_url(provider: &str) -> &'static str {
    if provider == "codex" { CODEX_USAGE_URL } else { CLAUDE_USAGE_URL }
}

/// `adapter.minRecoverableBlockMs`: proxy quota blocks shorter than this are left to lapse.
pub fn min_recoverable_block_ms(provider: &str) -> f64 {
    if provider == "claude" { 600_000.0 } else { 0.0 }
}

/// `adapter.accountId(file)`.
pub fn account_id(file: &Value) -> Value {
    if str_at(file, "provider") == Some("codex") {
        return js::coalesce([js::get(at(file, "id_token"), "chatgpt_account_id"), at(file, "id")])
            .cloned()
            .unwrap_or(Value::Null);
    }
    at(file, "id").cloned().unwrap_or(Value::Null)
}

/// `adapter.headers(file, io)`.
pub async fn headers<I: Io>(io: &I, file: &Value) -> Map<String, Value> {
    let mut h = Map::new();
    if str_at(file, "provider") == Some("codex") {
        h.insert("OpenAI-Beta".into(), json!("codex-1"));
        h.insert("Originator".into(), json!("Codex Desktop"));
        let id = js::get(at(file, "id_token"), "chatgpt_account_id");
        if js::truthy(id) {
            h.insert("Chatgpt-Account-Id".into(), id.cloned().unwrap_or(Value::Null));
        }
    } else {
        h.insert("anthropic-beta".into(), json!("oauth-2025-04-20"));
        if let Some(v) = io.client_version("claude").await.filter(|v| !v.is_empty()) {
            h.insert("User-Agent".into(), json!(format!("claude-cli/{v} (external, cli)")));
            h.insert("x-app".into(), json!("cli"));
        }
    }
    h
}

/// `adapter.modelRecoveryBlocked(usage, normalized)`: a model-specific limit is out or
/// unknown, so a credential-wide quota clear must wait.
pub fn model_recovery_blocked(provider: &str, usage: &Value, normalized: &Usage, now: f64) -> bool {
    if provider == "claude" {
        return normalized
            .quota_windows
            .iter()
            .any(|w| !w.common && !w.used.is_some_and(|u| u.is_finite() && u < 100.0));
    }
    let mut limits: Vec<&Value> = Vec::new();
    if let Some(Value::Array(rs)) = at(usage, "additional_rate_limits") {
        limits.extend(rs.iter().filter_map(|r| js::get(Some(r), "rate_limit")));
    }
    for (k, v) in js::entries(Some(usage)) {
        if k != "rate_limit" && k.ends_with("_rate_limit") {
            limits.push(v);
        }
    }
    limits
        .into_iter()
        .filter(|l| !l.is_null())
        .any(|l| normalize_usage("codex", &json!({ "rate_limit": l }), now).status != "available")
}

/// One reset bank (`Bank` in providers.mjs): `known: false` means not reported, never zero.
pub struct Bank {
    pub known: bool,
    pub credits: Value,
    pub count: Value,
    pub note: Option<String>,
}

/// `adapter.readBank(io, file, usage, now)`. Never fails: an unreadable bank is unknown.
pub async fn read_bank<I: Io>(cycle: &Cycle<'_, I>, file: &Value, usage: &Value, now: f64) -> Bank {
    if str_at(file, "provider") == Some("codex") {
        let unknown = Bank { known: false, credits: json!([]), count: json!(0), note: None };
        let Ok(data) = cycle.read(file, CODEX_CREDITS_URL, None, Map::new()).await else {
            return unknown;
        };
        // `data.credits` throws on null or undefined, which the adapter catches.
        if data.is_null() {
            return unknown;
        }
        let credits = available_credits(&data, now);
        let count = js::coalesce([
            at(&data, "available_count"),
            js::get(at(usage, "rate_limit_reset_credits"), "available_count"),
        ])
        .map_or(credits.len() as f64, |v| js::to_number(Some(v)));
        return Bank {
            known: matches!(at(&data, "credits"), Some(Value::Array(_))),
            credits: Value::Array(credits),
            count: num(count),
            note: None,
        };
    }
    let Some(bank) = claude_bank(at(usage, "cedar_ember"), now) else {
        return Bank { known: false, credits: json!([]), count: json!(0), note: Some("Reset bank not reported".into()) };
    };
    let count = num(bank.count);
    if !bank.credits.is_empty() && claude_organization(cycle.io.auth_file(file).await.as_ref()).is_none() {
        return Bank {
            known: true,
            credits: json!([]),
            count,
            note: Some("Organization unknown; resets cannot be claimed automatically".into()),
        };
    }
    Bank { known: true, credits: jsify(serde_json::to_value(&bank.credits).unwrap_or(json!([]))), count, note: bank.note }
}

/// `claudeOrganization(authFile)`: the organization UUID a claim needs, when readable.
pub fn claude_organization(auth: Option<&Value>) -> Option<String> {
    let id = auth.and_then(|a| str_at(a, "organization_uuid"))?;
    let b = id.as_bytes();
    let ok = b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        });
    ok.then(|| id.to_string())
}

/// The outcome codes the reset lifecycle accepts as final; anything else is unknown.
pub const TERMINAL: [&str; 6] = ["reset", "already_redeemed", "no_credit", "nothing_to_reset", "rejected", "deferred"];

/// `adapter.redeem(io, file, pending)`: the response whose `code` the lifecycle reads, or a
/// failure (outcome unknown, identity kept).
pub async fn redeem<I: Io>(cycle: &Cycle<'_, I>, file: &Value, pending: &Value) -> Result<Value, Failure> {
    if str_at(file, "provider") == Some("codex") {
        let body = json!({
            "redeem_request_id": at(pending, "requestId").cloned(),
            "credit_id": at(pending, "creditId").cloned(),
        });
        return cycle.read(file, &format!("{CODEX_CREDITS_URL}/consume"), Some(&strip_undefined(body)), Map::new()).await;
    }
    let Some(organization) = claude_organization(cycle.io.auth_file(file).await.as_ref()) else {
        return Err(Failure::new("Claude organization unknown; original identity retained"));
    };
    let mut body = Map::new();
    body.insert("program".into(), json!("cedar_ember"));
    for (k, from) in [("grant_id", "creditId"), ("request_id", "requestId")] {
        if let Some(v) = at(pending, from) {
            body.insert(k.into(), v.clone());
        }
    }
    let url = format!("https://api.anthropic.com/api/organizations/{organization}/reset_rate_limits");
    match cycle.read(file, &url, Some(&Value::Object(body)), Map::new()).await {
        Ok(response) => Ok(json!({ "code": claude_claim_outcome(&response) })),
        Err(e) => match e.status {
            Some(429) => Ok(json!({ "code": "deferred" })),
            Some(400 | 404 | 405 | 422) => Ok(json!({ "code": "rejected" })),
            _ => Err(e),
        },
    }
}

/// A value built with `json!` from optional fields: `undefined` (null here) keys dropped, as
/// `JSON.stringify` drops them.
fn strip_undefined(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.into_iter().filter(|(_, v)| !v.is_null()).collect()),
        v => v,
    }
}

/// `claudeClaimOutcome(response).code`.
pub fn claude_claim_outcome(response: &Value) -> &'static str {
    if matches!(str_at(response, "reason"), Some("reset_unconfirmed" | "stamp_indeterminate")) {
        return "unknown";
    }
    match str_at(response, "result") {
        Some("reset") => "reset",
        Some("already_used") => "already_redeemed",
        Some("not_limited") => "nothing_to_reset",
        Some("ineligible" | "unavailable") => "no_credit",
        Some("cooldown") => "deferred",
        _ => "unknown",
    }
}

/// `adapter.requestId(account, creditId)`: Codex ids are deterministic per credit; each Claude
/// attempt gets a fresh one.
pub fn request_id<I: Io>(io: &I, account: &Value, credit_id: Option<&Value>) -> Value {
    if str_at(account, "provider") == Some("codex") {
        return json!(redemption_id(&js::to_string(at(account, "accountId")), &js::to_string(credit_id)));
    }
    json!(io.random_uuid())
}

/// routing.mjs `redemptionId(accountId, creditId)`: a stable name-based UUID, kept so a retry
/// is recognized as the same redemption.
pub fn redemption_id(account_id: &str, credit_id: &str) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update([0x6f, 0x1c, 0x2a, 0x9e, 0x2d, 0x4b, 0x4c, 0x1e, 0x9a, 0x7f, 0x3b, 0x8d, 0x5e, 0x0c, 0x1a, 0x42]);
    h.update(format!("{account_id}:{credit_id}").as_bytes());
    let mut b: [u8; 16] = h.finalize()[..16].try_into().expect("20-byte digest");
    b[6] = (b[6] & 15) | 80;
    b[8] = (b[8] & 63) | 128;
    let x: String = b.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &x[..8], &x[8..12], &x[12..16], &x[16..20], &x[20..])
}

/// `adapter.readPlan(io, file)`: the subscription plan and monthly price.
pub async fn read_plan<I: Io>(cycle: &Cycle<'_, I>, file: &Value) -> Result<Value, Failure> {
    if str_at(file, "provider") == Some("codex") {
        let account = js::get(at(file, "id_token"), "chatgpt_account_id");
        if !js::truthy(account) {
            return Err(Failure::new("ChatGPT account id unknown"));
        }
        let version = cycle.io.client_version("codex").await.unwrap_or_else(|| "0.0.0".into());
        let mut billing = Map::new();
        billing.insert("User-Agent".into(), json!(format!("codex_cli_rs/{version}")));
        let id = encode_uri_component(&js::to_string(account));
        let sub = cycle.read(file, &format!("{BACKEND}/subscriptions?account_id={id}"), None, billing.clone()).await?;
        let key = format!("codex-pricing:{PRICING_COUNTRY}");
        let url = format!("{BACKEND}/checkout_pricing_config/configs/{PRICING_COUNTRY}");
        let pricing = cycle.memo(&key, || cycle.read(file, &url, None, billing.clone())).await.ok();
        return codex_plan(&sub, pricing.as_ref()).ok_or_else(|| Failure::new("Subscription plan not reported"));
    }
    let profile = cycle.read(file, CLAUDE_PROFILE_URL, None, Map::new()).await?;
    claude_plan(&profile).ok_or_else(|| Failure::new("Subscription tier not reported"))
}

/// `codexPlan(sub, pricing)`.
pub fn codex_plan(sub: &Value, pricing: Option<&Value>) -> Option<Value> {
    let plan = str_at(sub, "plan_type").filter(|p| !p.is_empty())?;
    let ent = at(sub, "entitlement");
    let currency = js::coalesce([at(sub, "billing_currency"), js::get(ent, "billing_currency")]).cloned().unwrap_or(Value::Null);
    let period = js::coalesce([at(sub, "billing_period"), js::get(ent, "billing_period")]).cloned().unwrap_or(Value::Null);
    let cfg = pricing.and_then(|p| at(p, "currency_config"));
    let row = cfg.filter(|c| js::is_object(Some(c))).and_then(|c| at(c, plan));
    let amount = js::get(js::get(row, if period == "yearly" { "year" } else { "month" }), "amount");
    let priced = js::finite(amount).is_some_and(|a| a >= 0.0)
        && currency.is_string()
        && js::get(cfg, "symbol_code") == Some(&currency);
    let label = match plan {
        "free" => "Free",
        "go" => "Go",
        "plus" => "Plus",
        "prolite" => "Pro 100",
        "pro" => "Pro 200",
        "promax" => "Pro 500",
        other => other,
    };
    let mut m = Map::new();
    m.insert("plan".into(), json!(plan));
    m.insert("label".into(), json!(format!("ChatGPT {label}")));
    m.insert("status".into(), json!(if at(sub, "will_renew") == Some(&Value::Bool(false)) { "ends" } else { "active" }));
    m.insert("billingPeriod".into(), period);
    m.insert(
        "renewsAt".into(),
        js::coalesce([at(sub, "active_until"), js::get(ent, "renews_at")]).cloned().unwrap_or(Value::Null),
    );
    let price = if priced {
        let amount = amount.cloned().unwrap_or(Value::Null);
        json!({
            "usdPerMonth": if currency == "USD" { amount.clone() } else { Value::Null },
            "amount": amount,
            "currency": currency,
            "source": "provider",
            "detail": format!("ChatGPT checkout pricing ({PRICING_COUNTRY})"),
        })
    } else {
        Value::Null
    };
    m.insert("price".into(), price);
    Some(Value::Object(m))
}

/// `claudePlan(profile)`: the tier Anthropic reports, priced at the public list price.
pub fn claude_plan(profile: &Value) -> Option<Value> {
    let org = at(profile, "organization").filter(|o| js::is_object(Some(o)));
    let tier = js::get(org, "rate_limit_tier").and_then(Value::as_str).unwrap_or("");
    let kind = js::get(org, "organization_type").and_then(Value::as_str).unwrap_or("");
    let account = at(profile, "account");
    let plan = if tier.contains("max_20x") {
        "max_20x"
    } else if tier.contains("max_5x") {
        "max_5x"
    } else if kind == "claude_max" || js::get(account, "has_claude_max") == Some(&Value::Bool(true)) {
        "max"
    } else if kind == "claude_pro" || js::get(account, "has_claude_pro") == Some(&Value::Bool(true)) {
        "pro"
    } else {
        return None;
    };
    let (label, amount) = match plan {
        "pro" => ("Claude Pro", Some(20)),
        "max_5x" => ("Claude Max 5x", Some(100)),
        "max_20x" => ("Claude Max 20x", Some(200)),
        _ => ("Claude Max", None),
    };
    let mut m = Map::new();
    m.insert("plan".into(), json!(plan));
    m.insert("label".into(), json!(label));
    m.insert("status".into(), js::get(org, "subscription_status").filter(|s| s.is_string()).cloned().unwrap_or(Value::Null));
    m.insert("billingPeriod".into(), Value::Null);
    m.insert("renewsAt".into(), Value::Null);
    m.insert(
        "price".into(),
        amount.map_or(Value::Null, |a| {
            json!({
                "usdPerMonth": a, "amount": a, "currency": "USD", "source": "list",
                "detail": format!("{CLAUDE_LIST_PRICE_SOURCE}; Anthropic does not report the billed price to Claude Code sign-ins"),
            })
        }),
    );
    Some(Value::Object(m))
}

/// `encodeURIComponent(s)`.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// providers.mjs `refreshErrorOf(file)`: the proxy's current streak of failed credential
/// refreshes, or `None`.
pub fn refresh_error_of(file: &Value) -> Option<Value> {
    let e = at(file, "refresh_error").filter(|e| e.is_object())?;
    let code = str_at(e, "code").filter(|c| !c.is_empty())?;
    let int = |k: &str, positive: bool| {
        let v = at(e, k);
        if is_integer(v) && (!positive || js::to_number(v) > 0.0) { v.cloned().unwrap_or(Value::Null) } else { Value::Null }
    };
    let text = |k: &str| at(e, k).filter(|v| v.is_string()).cloned().unwrap_or(Value::Null);
    let mut m = Map::new();
    m.insert("code".into(), json!(code));
    m.insert("httpStatus".into(), int("http_status", true));
    m.insert("message".into(), at(e, "message").filter(|v| v.is_string()).cloned().unwrap_or(json!("")));
    m.insert("since".into(), text("since"));
    m.insert("lastFailedAt".into(), text("last_failed_at"));
    m.insert("attempts".into(), int("attempts", false));
    Some(Value::Object(m))
}

/// providers.mjs `createAccount(file)`: the normalized account a cycle starts from.
pub fn create_account(file: &Value) -> Value {
    let mut a = Value::Object(Map::new());
    let id = at(file, "id").cloned();
    put_opt(&mut a, "id", id.clone());
    put_opt(&mut a, "name", js::coalesce([at(file, "name"), id.as_ref()]).cloned());
    put_opt(&mut a, "auth_index", at(file, "auth_index").cloned());
    put(&mut a, "accountId", account_id(file));
    put(&mut a, "email", js::coalesce([at(file, "email")]).cloned().unwrap_or(json!("")));
    put_opt(&mut a, "provider", at(file, "provider").cloned());
    put(&mut a, "disabled", json!(js::truthy(at(file, "disabled"))));
    put(&mut a, "priority", js::coalesce([at(file, "priority")]).cloned().unwrap_or(json!(0)));
    put(&mut a, "status", json!("unknown"));
    put(&mut a, "creditsKnown", json!(false));
    put(&mut a, "creditCount", json!(0));
    put(&mut a, "credits", json!([]));
    if let Some(e) = refresh_error_of(file) {
        put(&mut a, "refreshError", e);
    }
    a
}

/// A key set to an optional value: `undefined` is left out, as `JSON.stringify` leaves it.
fn put_opt(a: &mut Value, k: &str, v: Option<Value>) {
    if let Some(v) = v {
        put(a, k, v);
    }
}
