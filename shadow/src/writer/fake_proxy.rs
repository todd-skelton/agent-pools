//! A stand-in for the pool proxy's management API, on loopback, for the writer's copy. It answers
//! from the copy's own accounts: their saved quota windows (use drifting up slowly, so the
//! dashboard moves), their banked credits and plans. Every claim is refused (`no_credit`,
//! `ineligible`), so the reset lifecycle runs end to end and nothing is ever spent. No request
//! leaves the machine; each one is logged to `logs/fake-proxy.jsonl` in the copy.
//!
//! It also stands in for chatgpt.com's `/backend-api` as the copy's desktop relay sees it:
//! `GET /backend-api/wham/usage` answers the signed-in account's usage (the account named by
//! `chatgpt-account-id`, else the first Codex account), and anything else there is a 404.
//!
//! Its usage queue (`usage-queue?count=`, which the ledger drains) makes up requests on the
//! copy's accounts, `SHADOW_STANDIN_REQUESTS_PER_HOUR` of them (30; 0 for none), with the
//! shape of the proxy's records and quota headers from the drifting windows. As in the proxy, a
//! record not read within 60 s is gone. Each made-up request names its client `stand-in`.
//!
//! It answers inference too, for the copy's gateway and inference relay, with the pool's client
//! key (not the management key): `/v1/messages` (and `count_tokens`), `/v1/responses` (and
//! `compact`, and its WebSocket), `/v1/chat/completions` and `/v1/models`, each a short canned
//! answer in the provider's shape (streamed when asked). Each answered request is queued as a
//! usage record on the next account of its provider, so the ledger sees the traffic, and logged to
//! the copy's `logs/main.log` as the proxy logs a request (its selector line binding the model to
//! an account, then its status line), which the copy's admission probes read for routing evidence.
//! The admission prompt (`Reply with exactly: OK`) is answered `OK`; anything else gets the canned
//! answer.
//!
//! Its live model lists are re-read every hour and on `POST live-models/refresh`; each account's
//! `live_models` time in `auth-files` is the last re-read.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use corten::axum::{
    self, Json as AxumJson, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, patch, post},
};
use futures_util::{SinkExt, StreamExt};
use pool_core::js;
use serde_json::{Map, Value, json};

const CODEX_USAGE: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_CREDITS: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const CLAUDE_USAGE: &str = "https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1";
const CLAUDE_PROFILE: &str = "https://api.anthropic.com/api/oauth/profile";
/// Use rises this many points an hour on every window, up to 100.
const DRIFT_PER_HOUR: f64 = 2.0;
/// The proxy keeps a usage record this long for the ledger to read (`redis-usage-queue-retention-seconds`).
const QUEUE_RETENTION_MS: f64 = 60_000.0;

struct Fake {
    root: PathBuf,
    key: String,
    started: f64,
    accounts: Vec<Value>,
    priorities: Mutex<Map<String, Value>>,
    /// The made-up requests: when the next is due, and how many have been made.
    queue: Mutex<(f64, u64)>,
    requests_per_hour: f64,
    /// Records of inference the stand-in answered, waiting for the ledger.
    served: Mutex<std::collections::VecDeque<Value>>,
    /// The pool's client key, which inference must present (`None`: no inference).
    client_key: Option<String>,
    answered: std::sync::atomic::AtomicU64,
    /// Handlers run concurrently; one whole line per write, one writer at a time.
    log_lock: Mutex<()>,
    /// When `live-models/refresh` last re-read the lists (else the hourly re-read since the start).
    lists_at: Mutex<f64>,
    /// The zone `logs/main.log`'s times are written in (the proxy writes local time).
    zone: crate::dash::fmt::Zone,
}

type Shared = Arc<Fake>;

/// Serves the stand-in on 127.0.0.1 (a free port) for the copy at `root`, accepting `key`, until
/// the process ends. Returns its base URL.
pub async fn start(root: PathBuf, key: String, client_key: Option<String>) -> std::io::Result<String> {
    let state = std::fs::read_to_string(root.join("state/pool-state.json")).unwrap_or_default();
    let state: Value = serde_json::from_str(state.trim_start_matches('\u{feff}')).unwrap_or(Value::Null);
    let accounts = js::values(state.get("accounts")).into_iter().cloned().collect();
    let started = crate::watch::now_ms();
    let requests_per_hour = std::env::var("SHADOW_STANDIN_REQUESTS_PER_HOUR").ok().and_then(|v| v.parse::<f64>().ok()).filter(|n| n.is_finite() && *n >= 0.0).unwrap_or(30.0);
    let fake = Arc::new(Fake {
        root,
        key,
        started,
        accounts,
        priorities: Mutex::new(Map::new()),
        queue: Mutex::new((started, 0)),
        requests_per_hour,
        served: Mutex::new(std::collections::VecDeque::new()),
        client_key,
        answered: std::sync::atomic::AtomicU64::new(0),
        log_lock: Mutex::new(()),
        lists_at: Mutex::new(started),
        zone: crate::dash::fmt::Zone::from_env(),
    });
    let app = Router::new()
        .route("/v0/management/auth-files", get(auth_files))
        .route("/v0/management/auth-files/fields", patch(fields))
        .route("/v0/management/api-call", post(api_call))
        .route("/v0/management/recover-quota", post(recover_quota))
        .route("/v0/management/client-versions", get(client_versions))
        .route("/v0/management/oauth-callback", post(oauth_callback))
        .route("/v0/management/outage-holds", get(outage_holds))
        .route("/v0/management/usage-queue", get(usage_queue))
        .route("/v0/management/live-models/refresh", post(live_models_refresh))
        .route("/v0/management/{endpoint}", get(other_get).post(other_post))
        .route("/v1/messages", post(inference))
        .route("/v1/messages/count_tokens", post(inference))
        .route("/v1/responses", post(inference).get(responses_socket))
        .route("/v1/responses/compact", post(inference))
        .route("/v1/chat/completions", post(inference))
        .route("/v1/models", get(models))
        .route("/v1/models/{id}", get(models))
        .route("/backend-api/wham/usage", get(desktop_usage))
        .route("/backend-api/{*rest}", axum::routing::any(desktop_other))
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
            "id": id, "name": a.get("name").cloned().unwrap_or(json!(id)), "auth_index": index_of(a), "provider": provider,
            "email": a.get("email").cloned().unwrap_or(json!("")), "path": self.root.join("auth").join(&id).to_string_lossy(),
            "disabled": js::truthy(a.get("disabled")), "priority": priority,
            "routing": { "status": "ready", "quota_blocked": false, "generation": "1", "checked_at": js::iso(crate::watch::now_ms()),
                "models": a.get("routingModels").filter(|m| m.as_object().is_some_and(|m| !m.is_empty())).cloned().unwrap_or(json!({ "default": { "status": "ready" } })) },
        });
        // The proxy's read of the account's own model list: live, as many models as it routes.
        let models = a.get("routingModels").and_then(Value::as_object).map_or(1, |m| m.len().max(1));
        let at = js::iso(self.lists_at());
        f["live_models"] = json!({ "source": "live", "fetched_at": at, "checked_at": at, "models": models });
        if provider == "codex" {
            f["id_token"] = json!({ "chatgpt_account_id": a.get("accountId").cloned().unwrap_or(json!(format!("acct-{id}"))) });
        }
        f
    }

    /// The lists' last re-read: hourly from the start, or a requested one since.
    fn lists_at(&self) -> f64 {
        let hourly = self.started + ((crate::watch::now_ms() - self.started) / 3_600_000.0).floor() * 3_600_000.0;
        hourly.max(*self.lists_at.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Two lines in the copy's `logs/main.log` as the proxy writes them for a request: the selector
    /// binding `model` to `account`, then the answer's status, under one 8-digit request id.
    fn log_request(&self, account: &str, provider: &str, model: &str, path: &str) {
        use std::io::Write;
        let now = crate::watch::now_ms();
        let local: String = js::iso(now + f64::from(self.zone.offset_hours(now)) * 3_600_000.0).chars().take(19).collect::<String>().replacen('T', " ", 1);
        let id: String = uuid::Uuid::new_v4().simple().to_string().chars().take(8).collect();
        let text = format!(
            "[{local}] [{id}] [info ] [selector.go:1107] session-affinity: cache miss, new binding | session=stand-in auth={account} provider={provider} model={model}\n[{local}] [{id}] [info ] [gin_logger.go:103] 200 |        0.010s |       127.0.0.1 | POST    \"{path}\"\n"
        );
        let _ = std::fs::create_dir_all(self.root.join("logs"));
        let _guard = self.log_lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.root.join("logs/main.log")) {
            let _ = f.write_all(text.as_bytes());
        }
    }

    fn account(&self, auth_index: &str) -> Option<&Value> {
        self.accounts.iter().find(|a| index_of(a) == auth_index)
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

    /// Up to `count` made-up requests that are due by `now`, oldest first; ones older than the
    /// proxy's 60 s queue retention are skipped.
    fn pop_queue(&self, count: usize, now: f64) -> Vec<Value> {
        let mut out = Vec::new();
        {
            let mut served = self.served.lock().unwrap_or_else(|e| e.into_inner());
            while let Some(r) = served.pop_front() {
                if out.len() == count {
                    served.push_front(r);
                    break;
                }
                if js::date_parse_value(r.get("timestamp")) >= now - QUEUE_RETENTION_MS {
                    out.push(r);
                }
            }
        }
        if self.requests_per_hour <= 0.0 || self.accounts.is_empty() {
            return out;
        }
        let every = 3_600_000.0 / self.requests_per_hour;
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        while q.0 < now - QUEUE_RETENTION_MS {
            q.0 += every;
            q.1 += 1;
        }
        while q.0 <= now && out.len() < count {
            out.push(self.request(q.1, q.0));
            q.0 += every;
            q.1 += 1;
        }
        out
    }

    /// The `n`th made-up request, at `at`: one of the copy's accounts in turn, a priced model of its
    /// provider, and the quota headers its windows give.
    fn request(&self, n: u64, at: f64) -> Value {
        let a = &self.accounts[(n % self.accounts.len() as u64) as usize];
        let codex = a.get("provider").and_then(Value::as_str) == Some("codex");
        let models: &[&str] = if codex { &["gpt-6.1-sol", "gpt-6-luna"] } else { &["claude-opus-5-5", "claude-sonnet-5-5"] };
        let model = models[(n / self.accounts.len() as u64 % models.len() as u64) as usize];
        // Varied but repeatable sizes.
        let k = (n % 7) + 1;
        let session = format!("stand-in-session-{}", n / 10);
        let mut r = self.record(a, at, model, (2_000 * k, 30_000 * k, 400 * k), "stand-in/1 (agent-pools-corten copy)", &session);
        r["latency_ms"] = json!(900 + 300 * k);
        r["request_id"] = json!(format!("stand-in-{n}"));
        r
    }

    /// A usage record as the proxy's queue holds one: account `a`, at `at`, `(input, cached,
    /// output)` tokens, the caller's user agent and session, and the quota headers `a`'s windows give.
    fn record(&self, a: &Value, at: f64, model: &str, (input, cached, output): (u64, u64, u64), user_agent: &str, session: &str) -> Value {
        let provider = js::to_string(a.get("provider"));
        let codex = provider == "codex";
        let window = |pick: &dyn Fn(&Value) -> bool| self.windows(a).into_iter().find(|w| pick(w)).cloned();
        let seconds = |w: &Value| js::finite(w.get("resetAt")).map(|ms| json!([format!("{}", (ms / 1000.0).floor())]));
        let mut headers = Map::new();
        if codex {
            if let Some(w) = window(&|w| w.get("id").and_then(Value::as_str) == Some("codex:primary_window")) {
                let minutes = js::finite(w.get("durationSeconds")).unwrap_or(604_800.0) / 60.0;
                headers.insert("X-Codex-Primary-Used-Percent".into(), json!([format!("{}", self.used(&w, true))]));
                headers.insert("X-Codex-Primary-Window-Minutes".into(), json!([format!("{minutes}")]));
                if let Some(s) = seconds(&w) {
                    headers.insert("X-Codex-Primary-Reset-At".into(), s);
                }
            }
        } else {
            for (id, name) in [("limit:1", "7d"), ("limit:0", "5h")] {
                if let Some(w) = window(&|w| w.get("id").and_then(Value::as_str) == Some(id)) {
                    headers.insert(format!("Anthropic-Ratelimit-Unified-{name}-Utilization"), json!([format!("{}", self.used(&w, false) / 100.0)]));
                    if let Some(s) = seconds(&w) {
                        headers.insert(format!("Anthropic-Ratelimit-Unified-{name}-Reset"), s);
                    }
                }
            }
        }
        json!({
            "timestamp": js::iso(at),
            "latency_ms": 40,
            "auth_index": index_of(a),
            "user_agent": user_agent,
            "tokens": if codex {
                json!({ "input_tokens": input + cached, "cached_tokens": cached, "output_tokens": output, "reasoning_tokens": output / 4 })
            } else {
                json!({ "input_tokens": input, "cache_read_tokens": cached, "cache_creation_tokens": input / 2, "output_tokens": output })
            },
            "failed": false,
            "fail": { "status_code": 200, "body": "" },
            "response_headers": headers,
            "provider": provider,
            "model": model,
            "endpoint": if codex { "POST /v1/responses" } else { "POST /v1/messages" },
            "request_id": format!("stand-in-answer-{}", uuid::Uuid::new_v4()),
            "session_id": session,
            "reasoning_effort": "medium",
        })
    }

    /// Queues the record of an answered request on the next account of `provider`, and logs it as
    /// the proxy does. Returns that account (none: not queued).
    fn serve(&self, provider: &str, model: &str, tokens: (u64, u64, u64), user_agent: &str, session: &str, path: &str) -> Option<String> {
        let n = self.answered.fetch_add(1, std::sync::atomic::Ordering::SeqCst) as usize;
        let of: Vec<&Value> = self
            .accounts
            .iter()
            .filter(|a| a.get("provider").and_then(Value::as_str) == Some(provider) && !js::truthy(a.get("disabled")))
            .collect();
        if of.is_empty() {
            return None;
        }
        let a = of[n % of.len()];
        let r = self.record(a, crate::watch::now_ms(), model, tokens, user_agent, session);
        self.served.lock().unwrap_or_else(|e| e.into_inner()).push_back(r);
        let id = js::to_string(a.get("id"));
        self.log_request(&id, provider, model, path);
        Some(id)
    }

    /// The pool's client key, as `x-api-key` or a Bearer.
    fn inference_allowed(&self, headers: &HeaderMap) -> bool {
        let Some(key) = self.client_key.as_deref() else { return false };
        let presented = headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .or_else(|| headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")));
        presented == Some(key)
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
            // No grant left: not offered, as Anthropic answers an account with none (`no_grant`).
            "cedar_ember": {
                "eligible": count > 0.0, "ineligible_reason": if count > 0.0 { Value::Null } else { json!("no_grant") }, "at_limit": false, "grants": grants,
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

/// `POST live-models/refresh`: every account's model list re-read now.
async fn live_models_refresh(State(f): State<Shared>, headers: HeaderMap) -> Result<AxumJson<Value>, StatusCode> {
    if !f.allowed(&headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    f.log("live-models/refresh", &Value::Null);
    *f.lists_at.lock().unwrap_or_else(|e| e.into_inner()) = crate::watch::now_ms();
    Ok(AxumJson(json!({ "ok": true })))
}

async fn other_post(State(f): State<Shared>, headers: HeaderMap, Path(endpoint): Path<String>) -> Result<AxumJson<Value>, StatusCode> {
    if !f.allowed(&headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    f.log(&endpoint, &Value::Null);
    Err(StatusCode::NOT_FOUND)
}

/// The desktop's own usage poll, as chatgpt.com answers it for the signed-in account. The
/// credential is the desktop's and is not checked or logged.
async fn desktop_usage(State(f): State<Shared>, headers: HeaderMap) -> Result<AxumJson<Value>, StatusCode> {
    let named = headers.get("chatgpt-account-id").and_then(|v| v.to_str().ok());
    f.log("backend-api/wham/usage", &json!({ "chatgpt-account-id": named }));
    let codex = |a: &&Value| a.get("provider").and_then(Value::as_str) == Some("codex") && !js::truthy(a.get("disabled"));
    let account = named
        .and_then(|id| f.accounts.iter().filter(codex).find(|a| a.get("accountId").and_then(Value::as_str) == Some(id)))
        .or_else(|| f.accounts.iter().find(codex));
    let Some(a) = account else { return Err(StatusCode::NOT_FOUND) };
    let mut body = f.codex_usage(a);
    body["account_id"] = a.get("accountId").cloned().unwrap_or(Value::Null);
    body["plan_type"] = json!(f.plan(a).unwrap_or_else(|| "pro".into()));
    Ok(AxumJson(body))
}

async fn desktop_other(State(f): State<Shared>, Path(rest): Path<String>) -> (StatusCode, AxumJson<Value>) {
    f.log("backend-api", &json!({ "path": rest }));
    (StatusCode::NOT_FOUND, AxumJson(json!({ "detail": "not served by the stand-in" })))
}

/// The proxy's index for an account: the one the copy saved (the live proxy's), else `{id}-idx`.
fn index_of(a: &Value) -> String {
    match a.get("auth_index").and_then(Value::as_str) {
        Some(i) if !i.is_empty() => i.to_string(),
        _ => format!("{}-idx", js::to_string(a.get("id"))),
    }
}

#[derive(serde::Deserialize)]
struct QueueQuery {
    count: Option<usize>,
}

/// `GET usage-queue?count=`: the made-up requests due now, removed as they are read.
async fn usage_queue(State(f): State<Shared>, headers: HeaderMap, axum::extract::Query(q): axum::extract::Query<QueueQuery>) -> Result<AxumJson<Value>, StatusCode> {
    if !f.allowed(&headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let records = f.pop_queue(q.count.unwrap_or(1).clamp(1, 10_000), crate::watch::now_ms());
    if !records.is_empty() {
        f.log("usage-queue", &json!({ "count": q.count, "returned": records.len() }));
    }
    Ok(AxumJson(Value::Array(records)))
}

/// The proxy's provider outage hold counters: nothing is ever held by the stand-in.
async fn outage_holds(State(f): State<Shared>, headers: HeaderMap) -> Result<AxumJson<Value>, StatusCode> {
    if !f.allowed(&headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(AxumJson(json!({ "enabled": true, "active": 0, "turns": 0 })))
}

// ---------------------------------------------------------------- Inference (the copy's gateway and relay)

const ANSWER: &str = "stand-in: no model was called";
/// What model-admission.mjs asks every model it probes.
pub const ADMISSION_PROMPT: &str = "Reply with exactly: OK";

/// The answer to a request: `OK` to the admission prompt (as a string `input`, or the last
/// message's string content), else the canned answer.
fn answer_to(body: &[u8]) -> &'static str {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let prompt = v.get("input").and_then(Value::as_str).or_else(|| js::values(v.get("messages")).last().and_then(|m| m.get("content")).and_then(Value::as_str));
    if prompt.map(str::trim) == Some(ADMISSION_PROMPT) { "OK" } else { ANSWER }
}

/// What a request's body says: its model (else the provider's default), whether it asks for a
/// stream, and a rough input token count (four bytes a token).
fn asked(body: &[u8], default_model: &str) -> (String, bool, u64) {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let model = v.get("model").and_then(Value::as_str).filter(|m| !m.is_empty()).unwrap_or(default_model).to_string();
    (model, v.get("stream").and_then(Value::as_bool).unwrap_or(false), (body.len() as u64 / 4).max(1))
}

fn sse(events: &[(&str, Value)]) -> axum::response::Response {
    let mut text = String::new();
    for (name, data) in events {
        text.push_str(&format!("event: {name}\ndata: {data}\n\n"));
    }
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(axum::body::Body::from(text))
        .expect("a static response")
}

fn json_answer(status: StatusCode, body: Value) -> axum::response::Response {
    use axum::response::IntoResponse;
    (status, AxumJson(body)).into_response()
}

/// A Responses API answer's events: created, one text delta, completed with usage.
fn responses_events(model: &str, input: u64, text: &str) -> Vec<(&'static str, Value)> {
    let id = format!("resp_standin_{}", uuid::Uuid::new_v4().simple());
    let item = json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": text }] });
    vec![
        ("response.created", json!({ "type": "response.created", "response": { "id": id, "model": model, "status": "in_progress" } })),
        ("response.output_text.delta", json!({ "type": "response.output_text.delta", "delta": text })),
        ("response.completed", json!({ "type": "response.completed", "response": {
            "id": id, "model": model, "status": "completed", "output": [item],
            "usage": { "input_tokens": input, "input_tokens_details": { "cached_tokens": 0 }, "output_tokens": 8, "output_tokens_details": { "reasoning_tokens": 0 }, "total_tokens": input + 8 },
        } })),
    ]
}

/// Inference over HTTP, with the pool's client key: a canned answer in the shape the path's
/// provider uses, queued for the ledger (not `count_tokens`, which is not inference).
async fn inference(State(f): State<Shared>, uri: axum::http::Uri, headers: HeaderMap, body: axum::body::Bytes) -> axum::response::Response {
    if !f.inference_allowed(&headers) {
        return json_answer(StatusCode::UNAUTHORIZED, json!({ "error": { "type": "authentication_error", "message": "stand-in: not the pool's client key" } }));
    }
    let path = uri.path();
    let claude = path.starts_with("/v1/messages");
    let (model, stream, input) = asked(&body, if claude { "claude-sonnet-5-5" } else { "gpt-6.1-sol" });
    let user_agent = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("");
    let session = headers.get("session_id").or(headers.get("x-claude-code-session-id")).and_then(|v| v.to_str().ok()).unwrap_or("");
    f.log("inference", &json!({ "path": path, "model": model, "stream": stream, "bytes": body.len() }));
    if path == "/v1/messages/count_tokens" {
        return json_answer(StatusCode::OK, json!({ "input_tokens": input }));
    }
    f.serve(if claude { "claude" } else { "codex" }, &model, (input, 0, 8), user_agent, session, path);
    let text = answer_to(&body);
    match path {
        "/v1/messages" if stream => {
            let id = format!("msg_standin_{}", uuid::Uuid::new_v4().simple());
            sse(&[
                ("message_start", json!({ "type": "message_start", "message": { "id": id, "type": "message", "role": "assistant", "model": model, "content": [], "usage": { "input_tokens": input, "output_tokens": 0 } } })),
                ("content_block_start", json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
                ("content_block_delta", json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } })),
                ("content_block_stop", json!({ "type": "content_block_stop", "index": 0 })),
                ("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 8 } })),
                ("message_stop", json!({ "type": "message_stop" })),
            ])
        }
        "/v1/messages" => json_answer(StatusCode::OK, json!({
            "id": format!("msg_standin_{}", uuid::Uuid::new_v4().simple()), "type": "message", "role": "assistant", "model": model,
            "content": [{ "type": "text", "text": text }], "stop_reason": "end_turn", "usage": { "input_tokens": input, "output_tokens": 8 },
        })),
        "/v1/responses" if stream => sse(&responses_events(&model, input, text)),
        "/v1/responses" => json_answer(StatusCode::OK, responses_events(&model, input, text).pop().expect("three events").1["response"].clone()),
        "/v1/responses/compact" => json_answer(StatusCode::OK, json!({ "object": "response.compaction", "output": [] })),
        _ => json_answer(StatusCode::OK, json!({
            "id": format!("chatcmpl-standin-{}", uuid::Uuid::new_v4().simple()), "object": "chat.completion", "model": model,
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": text }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": input, "completion_tokens": 8, "total_tokens": input + 8 },
        })),
    }
}

async fn models(State(f): State<Shared>, uri: axum::http::Uri, headers: HeaderMap) -> axum::response::Response {
    if !f.inference_allowed(&headers) {
        return json_answer(StatusCode::UNAUTHORIZED, json!({ "error": { "type": "authentication_error", "message": "stand-in: not the pool's client key" } }));
    }
    let model = |id: &str| json!({ "id": id, "object": "model", "owned_by": "stand-in" });
    match uri.path().strip_prefix("/v1/models/") {
        Some(id) => json_answer(StatusCode::OK, model(id)),
        None => {
            let data = ["gpt-6.1-sol", "gpt-6-luna", "claude-opus-5-5", "claude-sonnet-5-5"].map(model);
            json_answer(StatusCode::OK, json!({ "object": "list", "data": data }))
        }
    }
}

/// `GET /v1/responses` as a WebSocket (Codex's transport): each `response.create` text frame gets
/// the answer's events as text frames, and is queued for the ledger.
async fn responses_socket(State(f): State<Shared>, mut req: axum::extract::Request) -> axum::response::Response {
    use tokio_tungstenite::tungstenite::{Message, handshake::derive_accept_key, protocol::Role};
    let headers = req.headers().clone();
    if !f.inference_allowed(&headers) {
        return json_answer(StatusCode::UNAUTHORIZED, json!({ "error": "stand-in: not the pool's client key" }));
    }
    let Some(key) = headers.get("sec-websocket-key").map(|k| derive_accept_key(k.as_bytes())) else {
        return json_answer(StatusCode::UPGRADE_REQUIRED, json!({ "error": "stand-in: a WebSocket only" }));
    };
    let on = hyper::upgrade::on(&mut req);
    let user_agent = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let session = headers.get("session_id").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    tokio::spawn(async move {
        let Ok(upgraded) = on.await else { return };
        let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(hyper_util::rt::TokioIo::new(upgraded), Role::Server, None).await;
        while let Some(Ok(frame)) = ws.next().await {
            let Message::Text(text) = frame else { continue };
            let (model, _, input) = asked(text.as_bytes(), "gpt-6.1-sol");
            f.log("inference", &json!({ "path": "/v1/responses (websocket)", "model": model, "bytes": text.len() }));
            f.serve("codex", &model, (input, 0, 8), &user_agent, &session, "/v1/responses");
            for (_, event) in responses_events(&model, input, answer_to(text.as_bytes())) {
                if ws.send(Message::text(event.to_string())).await.is_err() {
                    return;
                }
            }
        }
    });
    axum::response::Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-accept", key)
        .body(axum::body::Body::empty())
        .expect("a static response")
}
