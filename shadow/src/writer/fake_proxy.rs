//! A stand-in for the pool proxy's management API, on loopback, for the writer's copy. It answers
//! from the copy's own accounts: their saved quota windows (use drifting up slowly, so the
//! dashboard moves), their banked credits and plans. Every claim is refused (`no_credit`,
//! `ineligible`), so the reset lifecycle runs end to end and nothing is ever spent. No request
//! leaves the machine; each one is logged to `logs/fake-proxy.jsonl` in the copy.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use corten::axum::{
    self, Json as AxumJson, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, patch, post},
};
use pool_core::js;
use serde_json::{Map, Value, json};

const CODEX_USAGE: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_CREDITS: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const CLAUDE_USAGE: &str = "https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1";
const CLAUDE_PROFILE: &str = "https://api.anthropic.com/api/oauth/profile";
/// Use rises this many points an hour on every window, up to 100.
const DRIFT_PER_HOUR: f64 = 2.0;

struct Fake {
    root: PathBuf,
    key: String,
    started: f64,
    accounts: Vec<Value>,
    priorities: Mutex<Map<String, Value>>,
    /// Handlers run concurrently; one whole line per write, one writer at a time.
    log_lock: Mutex<()>,
}

type Shared = Arc<Fake>;

/// Serves the stand-in on 127.0.0.1 (a free port) for the copy at `root`, accepting `key`, until
/// the process ends. Returns its base URL.
pub async fn start(root: PathBuf, key: String) -> std::io::Result<String> {
    let state = std::fs::read_to_string(root.join("state/pool-state.json")).unwrap_or_default();
    let state: Value = serde_json::from_str(state.trim_start_matches('\u{feff}')).unwrap_or(Value::Null);
    let accounts = js::values(state.get("accounts")).into_iter().cloned().collect();
    let fake = Arc::new(Fake { root, key, started: crate::watch::now_ms(), accounts, priorities: Mutex::new(Map::new()), log_lock: Mutex::new(()) });
    let app = Router::new()
        .route("/v0/management/auth-files", get(auth_files))
        .route("/v0/management/auth-files/fields", patch(fields))
        .route("/v0/management/api-call", post(api_call))
        .route("/v0/management/recover-quota", post(recover_quota))
        .route("/v0/management/client-versions", get(client_versions))
        .route("/v0/management/oauth-callback", post(oauth_callback))
        .route("/v0/management/{endpoint}", get(other_get).post(other_post))
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(url)
}

impl Fake {
    fn allowed(&self, headers: &HeaderMap) -> bool {
        headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(format!("Bearer {}", self.key).as_str())
    }

    fn log(&self, endpoint: &str, body: &Value) {
        use std::io::Write;
        let line = json!({ "at": js::iso(crate::watch::now_ms()), "endpoint": endpoint, "body": body });
        let file = self.root.join("logs/fake-proxy.jsonl");
        let _ = std::fs::create_dir_all(self.root.join("logs"));
        let text = format!("{line}\n");
        let _guard = self.log_lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
            let _ = f.write_all(text.as_bytes());
        }
    }

    fn file(&self, a: &Value) -> Value {
        let id = js::to_string(a.get("id"));
        let provider = js::to_string(a.get("provider"));
        let priority = self.priorities.lock().unwrap().get(&id).cloned().or_else(|| a.get("priority").cloned()).unwrap_or(json!(0));
        let mut f = json!({
            "id": id, "name": a.get("name").cloned().unwrap_or(json!(id)), "auth_index": format!("{id}-idx"), "provider": provider,
            "email": a.get("email").cloned().unwrap_or(json!("")), "path": self.root.join("auth").join(&id).to_string_lossy(),
            "disabled": js::truthy(a.get("disabled")), "priority": priority,
            "routing": { "status": "ready", "quota_blocked": false, "generation": "1", "checked_at": js::iso(crate::watch::now_ms()),
                "models": a.get("routingModels").filter(|m| m.as_object().is_some_and(|m| !m.is_empty())).cloned().unwrap_or(json!({ "default": { "status": "ready" } })) },
        });
        if provider == "codex" {
            f["id_token"] = json!({ "chatgpt_account_id": a.get("accountId").cloned().unwrap_or(json!(format!("acct-{id}"))) });
        }
        f
    }

    fn account(&self, auth_index: &str) -> Option<&Value> {
        self.accounts.iter().find(|a| format!("{}-idx", js::to_string(a.get("id"))) == auth_index)
    }

    /// A saved window's use, drifted up since the stand-in started.
    fn used(&self, w: &Value, integer: bool) -> f64 {
        let hours = (crate::watch::now_ms() - self.started) / 3_600_000.0;
        let used = (js::finite(w.get("used")).unwrap_or(0.0) + hours * DRIFT_PER_HOUR).min(100.0);
        if integer { used.floor() } else { (used * 10.0).round() / 10.0 }
    }

    fn windows<'a>(&self, a: &'a Value) -> Vec<&'a Value> {
        js::values(a.get("quotaWindows"))
    }

    fn codex_usage(&self, a: &Value) -> Value {
        let mut limit = Map::new();
        let mut out_of_quota = false;
        for w in self.windows(a) {
            let Some(key) = w.get("id").and_then(Value::as_str).and_then(|id| id.strip_prefix("codex:")) else { continue };
            let used = self.used(w, true);
            out_of_quota |= used >= 100.0;
            limit.insert(key.into(), json!({
                "used_percent": used, "limit_window_seconds": w.get("durationSeconds"),
                "reset_at": js::finite(w.get("resetAt")).map(|r| (r / 1000.0).floor()),
            }));
        }
        if limit.is_empty() {
            limit.insert("primary_window".into(), json!({ "used_percent": 10, "limit_window_seconds": 604800, "reset_at": ((crate::watch::now_ms() + 3.0 * 86_400_000.0) / 1000.0).floor() }));
        }
        limit.insert("allowed".into(), json!(!out_of_quota));
        limit.insert("limit_reached".into(), json!(out_of_quota));
        json!({ "rate_limit": limit, "rate_limit_reset_credits": { "available_count": a.get("creditCount").cloned().unwrap_or(json!(0)) } })
    }

    fn codex_credits(&self, a: &Value) -> Value {
        let credits: Vec<Value> = js::values(a.get("credits"))
            .into_iter()
            .map(|c| json!({ "id": c.get("id"), "reset_type": "codex_rate_limits", "status": "available", "expires_at": c.get("expires_at").or(c.get("expiresAt")) }))
            .collect();
        json!({ "credits": credits, "available_count": a.get("creditCount").cloned().unwrap_or(json!(credits.len())) })
    }

    fn claude_usage(&self, a: &Value) -> Value {
        let mut limits: Vec<Value> = Vec::new();
        for w in self.windows(a) {
            let Some(i) = w.get("id").and_then(Value::as_str).and_then(|id| id.strip_prefix("limit:")).and_then(|i| i.parse::<usize>().ok()) else { continue };
            if limits.len() <= i {
                limits.resize(i + 1, Value::Null);
            }
            let common = js::truthy(w.get("common"));
            let session = js::number(w.get("durationSeconds")) == Some(18_000.0);
            let kind = if !common { "weekly_scoped" } else if session { "session" } else { "weekly_all" };
            limits[i] = json!({
                "kind": kind, "group": if session { "session" } else { "weekly" }, "percent": self.used(w, false),
                "resets_at": js::finite(w.get("resetAt")).map(js::iso),
                "scope": if common { Value::Null } else { json!({ "model": { "id": null, "display_name": w.get("scope") } }) },
            });
        }
        if limits.is_empty() {
            limits.push(json!({ "kind": "session", "group": "session", "percent": 0, "resets_at": null, "scope": null }));
            limits.push(json!({ "kind": "weekly_all", "group": "weekly", "percent": 10, "resets_at": js::iso(crate::watch::now_ms() + 4.0 * 86_400_000.0), "scope": null }));
        }
        let credits = js::values(a.get("credits"));
        let count = js::finite(a.get("creditCount")).unwrap_or(credits.len() as f64);
        let grants: Vec<Value> = credits
            .iter()
            .enumerate()
            .map(|(i, c)| json!({
                "id": c.get("id"), "label": c.get("label").cloned().unwrap_or(json!("")),
                "resets_left": if i == 0 { (count - credits.len() as f64 + 1.0).max(1.0) } else { 1.0 },
                "starts_at": js::iso(self.started - 86_400_000.0), "ends_at": c.get("expires_at"),
                "paused": false, "usable_now": true, "use_requires_limit": false,
            }))
            .collect();
        json!({
            "limits": limits,
            "cedar_ember": {
                "eligible": true, "ineligible_reason": null, "at_limit": false, "grants": grants,
                "next_grant_id": credits.first().and_then(|c| c.get("id")), "cooldown_until": null,
            },
        })
    }

    fn plan(&self, a: &Value) -> Option<String> {
        a.get("plan").and_then(|p| p.get("plan")).and_then(Value::as_str).map(str::to_string)
    }
}

fn reply(status: u16, body: Value) -> Value {
    json!({ "status_code": status, "body": body.to_string() })
}

async fn auth_files(State(f): State<Shared>, headers: HeaderMap) -> Result<AxumJson<Value>, StatusCode> {
    if !f.allowed(&headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    f.log("auth-files", &Value::Null);
    Ok(AxumJson(json!({ "files": f.accounts.iter().map(|a| f.file(a)).collect::<Vec<_>>() })))
}

async fn fields(State(f): State<Shared>, headers: HeaderMap, AxumJson(body): AxumJson<Value>) -> Result<AxumJson<Value>, StatusCode> {
    if !f.allowed(&headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    f.log("auth-files/fields", &body);
    if let (Some(name), Some(p)) = (body.get("name").and_then(Value::as_str), body.get("priority"))
        && let Some(a) = f.accounts.iter().find(|a| a.get("name").and_then(Value::as_str) == Some(name))
    {
        f.priorities.lock().unwrap().insert(js::to_string(a.get("id")), p.clone());
    }
    Ok(AxumJson(json!({ "ok": true })))
}

async fn api_call(State(f): State<Shared>, headers: HeaderMap, AxumJson(body): AxumJson<Value>) -> Result<AxumJson<Value>, StatusCode> {
    if !f.allowed(&headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    f.log("api-call", &json!({ "auth_index": body.get("auth_index"), "method": body.get("method"), "url": body.get("url"), "data": body.get("data") }));
    let url = js::to_string(body.get("url"));
    let Some(a) = f.account(&js::to_string(body.get("auth_index"))) else {
        return Ok(AxumJson(reply(404, json!({ "error": "unknown auth_index" }))));
    };
    let answer = if url == CODEX_USAGE {
        reply(200, f.codex_usage(a))
    } else if url == CODEX_CREDITS {
        reply(200, f.codex_credits(a))
    } else if url == format!("{CODEX_CREDITS}/consume") {
        // Refused: the copy never spends a credit.
        reply(200, json!({ "code": "no_credit" }))
    } else if url == CLAUDE_USAGE {
        reply(200, f.claude_usage(a))
    } else if url.starts_with("https://api.anthropic.com/api/organizations/") && url.ends_with("/reset_rate_limits") {
        reply(200, json!({ "result": "ineligible" }))
    } else if url.starts_with("https://chatgpt.com/backend-api/subscriptions?account_id=") {
        match f.plan(a) {
            Some(plan) => reply(200, json!({ "plan_type": plan, "billing_period": "monthly", "billing_currency": "USD", "will_renew": true, "active_until": a.get("plan").and_then(|p| p.get("renewsAt")) })),
            None => reply(403, json!({})),
        }
    } else if url == "https://chatgpt.com/backend-api/checkout_pricing_config/configs/US" {
        reply(200, json!({ "currency_config": { "symbol_code": "USD", "plus": { "month": { "amount": 20 } }, "prolite": { "month": { "amount": 100 } }, "pro": { "month": { "amount": 200 } } } }))
    } else if url == CLAUDE_PROFILE {
        let tier = match f.plan(a).as_deref() {
            Some("max_20x") => "default_claude_max_20x",
            Some("max_5x") => "default_claude_max_5x",
            _ => "default_claude_pro",
        };
        reply(200, json!({ "organization": { "rate_limit_tier": tier, "subscription_status": "active" } }))
    } else {
        reply(404, json!({ "error": "not served by the stand-in" }))
    };
    Ok(AxumJson(answer))
}

async fn recover_quota(State(f): State<Shared>, headers: HeaderMap, AxumJson(body): AxumJson<Value>) -> Result<AxumJson<Value>, StatusCode> {
    if !f.allowed(&headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    f.log("recover-quota", &body);
    Ok(AxumJson(json!({ "changed": false, "routing": { "status": "ready", "quota_blocked": false } })))
}

async fn client_versions(State(f): State<Shared>, headers: HeaderMap) -> Result<AxumJson<Value>, StatusCode> {
    if !f.allowed(&headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(AxumJson(json!({ "clients": { "codex": { "effective": "0.162.1" }, "claude-code": { "effective": "2.1.284" } } })))
}

async fn oauth_callback(State(f): State<Shared>, headers: HeaderMap, AxumJson(body): AxumJson<Value>) -> Result<AxumJson<Value>, StatusCode> {
    if !f.allowed(&headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    f.log("oauth-callback", &json!({ "provider": body.get("provider") }));
    Ok(AxumJson(json!({ "status": "ok", "note": "stand-in: nothing was signed in" })))
}

/// `<loginId>-auth-url` and anything else: a stand-in sign-in address, or 404.
async fn other_get(State(f): State<Shared>, headers: HeaderMap, Path(endpoint): Path<String>) -> Result<AxumJson<Value>, StatusCode> {
    if !f.allowed(&headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    f.log(&endpoint, &Value::Null);
    if endpoint.ends_with("-auth-url") {
        return Ok(AxumJson(json!({ "status": "ok", "url": "https://example.invalid/stand-in-sign-in", "state": "stand-in" })));
    }
    Err(StatusCode::NOT_FOUND)
}

async fn other_post(State(f): State<Shared>, headers: HeaderMap, Path(endpoint): Path<String>) -> Result<AxumJson<Value>, StatusCode> {
    if !f.allowed(&headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    f.log(&endpoint, &Value::Null);
    Err(StatusCode::NOT_FOUND)
}
