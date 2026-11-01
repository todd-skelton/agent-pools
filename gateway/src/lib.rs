//! Agent Pools' public gateway (public-gateway.mjs at 5c696f6) on Corten: the entry for remote
//! machines, `https://<hostname>` through a Cloudflare tunnel to this app on loopback, then to the
//! pool proxy (inference) or the pool service (dashboard). Nothing reaches the pool without a
//! registered machine key.
//!
//! | Route | Who | Answer |
//! |---|---|---|
//! | `POST /v1/messages`, `/v1/messages/count_tokens`, `/v1/responses`, `/v1/responses/compact`, `/v1/chat/completions`; `GET /v1/models`, `/v1/models/{id}` | a machine key (`x-api-key` or Bearer) | the pool's answer, relayed as it streams, sent with the pool's own key |
//! | `GET /login`, `POST /login` | anyone | the sign-in form; a machine key gives a signed session cookie (30 days) |
//! | `/logout` | anyone | clears the cookie |
//! | `GET /`, `/api/status`, `/api/capacity`, `/api/ladders`, `/api/value`, `/api/summary`, `/api/health` | a session | the service's answer |
//! | `GET /accounts`, `/usage`, `/policy`, `/signin`, `/live` | a session (else to `/login`) | the dashboard's other pages and its live updates, from the service |
//! | `GET /assets/..`, `/corten/..` the gateway does not have | a session | the dashboard's stylesheets and scripts, from the service |
//! | `POST /refresh`, `/signin`, `/signin/finish`, `/signin/cancel` | a session, from the public origin, a form | the dashboard's own forms, to the service as a write from its own origin |
//! | `POST /api/refresh` | a session, from the public origin, JSON | the service's answer |
//!
//! Machine keys never reach the pool, and cookies never spend quota. Repeated failures from one
//! address are refused for a while (429).
//!
//! # Where it differs from public-gateway.mjs
//!
//! 1. Who is asking is Corten's `ClientIp`: behind the tunnel (a proxy on loopback, trusted), the
//!    nearest `X-Forwarded-For` address, where Agent Pools reads `CF-Connecting-IP`. cloudflared
//!    sets both.
//! 2. A request for a name that is not the gateway's gets Corten's 421 before routing, where
//!    Agent Pools answers 404 (`localhost` and IP addresses, which Corten answers, get the 404).
//! 3. Failures Corten answers itself under `/v1` (an unknown URL, a wrong method, a cross-site
//!    write) use the gateway's Anthropic-style shape (`Api::errors`, corten#163), with Corten's
//!    statuses: a wrong method is 405 where Agent Pools answers 404. Outside `/v1` they are the
//!    pages' (the HTML error page), and an unlisted host is Corten's 421 in plain text.
//! 4. `Referrer-Policy` is Corten's `strict-origin-when-cross-origin`, which, like Agent Pools'
//!    `same-origin`, keeps the real `Origin` on the sign-in form's POST.
//! 5. The machine key is Corten's `Caller<Machine>` (corten#177), looked up before the handler:
//!    a `/v1/models/{id}` with a malformed id is a 404 only once the key is good (Agent Pools
//!    answers it before the key), and the 429 for too many failures carries `Retry-After` (the
//!    seconds until the address's window ends), which Agent Pools does not send.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use corten::axum::body::Body;
use corten::axum::extract::Request;
use corten::axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use corten::axum::response::{IntoResponse, Response};
use corten::prelude::*;
use corten::{ClientIp, Relay};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub mod backend;
pub mod inference;

// The sign-in page's stylesheet, compiled into the program.
corten::assets::embed!(pub struct Static = "static");

/// A registered machine (state/remote-clients.json): its name and the SHA-256 of its key.
#[derive(Clone, Debug, Hash)]
pub struct Client {
    pub name: String,
    pub key_sha256: String,
}

type Hook<T> = Arc<dyn Fn() -> T + Send + Sync>;
type Observe = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// What the gateway is given (startPublicGateway's options).
#[derive(Clone)]
pub struct Config {
    pub hostname: String,
    /// The pool proxy, `http://127.0.0.1:<port>`.
    pub pool: String,
    /// The pool service, `http://127.0.0.1:<port>`.
    pub service: String,
    /// The registered machines, read again on each request.
    pub clients: Hook<Vec<Client>>,
    /// The pool's own key, sent in place of a machine's.
    pub pool_key: Hook<Option<String>>,
    /// The session cookie's signing secret.
    pub session_secret: Hook<Result<String, String>>,
    /// An event (`public-request`, `public-login`, `public-auth-rejected`, `public-gateway-problem`).
    pub observe: Observe,
    pub session_days: f64,
    pub max_failures: u32,
    pub failure_window_ms: f64,
    /// Milliseconds since the epoch.
    pub now: Hook<f64>,
}

impl Config {
    /// The defaults public-gateway.mjs has: 30-day sessions, 30 failures in 15 minutes.
    pub fn new(hostname: &str, pool: &str, service: &str) -> Config {
        Config {
            hostname: hostname.into(),
            pool: pool.into(),
            service: service.into(),
            clients: Arc::new(Vec::new),
            pool_key: Arc::new(|| None),
            session_secret: Arc::new(|| Err("no session secret".into())),
            observe: Arc::new(|_, _| {}),
            session_days: 30.0,
            max_failures: 30,
            failure_window_ms: 15.0 * 60_000.0,
            now: Arc::new(now_ms),
        }
    }
}

pub fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_millis() as f64)
}

/// The gateway's state, shared by its handlers.
#[derive(Clone)]
pub struct Gw(Arc<Inner>);

struct Inner {
    config: Config,
    public_origin: String,
    pool: Upstream,
    service: Upstream,
    http: reqwest::Client,
    failures: Mutex<HashMap<String, (f64, u32)>>,
    last_reject_log: Mutex<HashMap<String, f64>>,
}

/// A loopback upstream: its origin (`http://127.0.0.1:8317`) and `host:port`.
struct Upstream {
    origin: String,
    host: String,
}

impl Upstream {
    fn parse(url: &str) -> Result<Upstream, String> {
        let rest = url.trim_end_matches('/').strip_prefix("http://").ok_or("Only loopback upstreams are allowed")?;
        let (host, port) = rest.split_once(':').ok_or("Only loopback upstreams are allowed")?;
        if host != "127.0.0.1" || port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return Err("Only loopback upstreams are allowed".into());
        }
        Ok(Upstream { origin: format!("http://{rest}"), host: rest.to_string() })
    }
}

/// The app for `config`, or why it cannot start.
pub fn app(config: Config) -> Result<App<Headers>, String> {
    let ok_name = !config.hostname.is_empty() && config.hostname.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-');
    if !ok_name {
        return Err("Public hostname required".into());
    }
    let pool = Upstream::parse(&config.pool)?;
    let service = Upstream::parse(&config.service)?;
    // An upstream's redirect goes to the browser as it is (the dashboard's forms answer 303), as
    // public-gateway.mjs's http.request passes it on; following it here would leave the browser
    // at the form's address.
    let http = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().map_err(|e| e.to_string())?;
    let gw = Gw(Arc::new(Inner {
        public_origin: format!("https://{}", config.hostname),
        pool,
        service,
        http,
        failures: Mutex::default(),
        last_reject_log: Mutex::default(),
        config: config.clone(),
    }));
    let policy = corten::security::Policy::new().host(&config.hostname).proxy("127.0.0.0/8");
    Ok(App::new(url::router(gw.clone()))
        .assets(Static)
        .policy(policy)
        // Streams run for many minutes; a model may think long before its first byte.
        .deadline(None)
        .middleware(Headers { hostname: config.hostname.clone(), gw: gw.clone() })
        .api(move |routes| v1::api(routes, gw).deadline(None).errors(anthropic_error)))
}

/// The failures Corten answers itself under `/v1` (an unknown URL, a wrong method, a cross-site
/// write, a rejected input), in the Anthropic-style shape the gateway's own failures use, so a
/// CLI reads its `error.message` (corten#163).
pub struct AnthropicError(Value);

impl corten::json::ToJson for AnthropicError {
    fn write_json(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.0.to_string().as_bytes());
    }
}

fn anthropic_error(status: StatusCode, message: &str) -> AnthropicError {
    let kind = match status.as_u16() {
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 | 415 => "error",
        429 => "rate_limit_error",
        400..=499 => "invalid_request_error",
        _ => "api_error",
    };
    AnthropicError(json!({ "type": "error", "error": { "type": kind, "message": message } }))
}

/// Only the public hostname is served (Corten also answers `localhost` and IP addresses, which
/// reach only loopback here; Agent Pools refuses them with a 404, and so does the port). Then
/// Agent Pools' `securityHeaders` on every answer: never cached, never indexed.
pub struct Headers {
    hostname: String,
    gw: Gw,
}

impl corten::Middleware for Headers {
    async fn call(&self, req: Request, next: impl corten::Next) -> Response {
        let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
        let mut res = if !host.eq_ignore_ascii_case(&self.hostname) {
            error(StatusCode::NOT_FOUND, "Not found", "not_found_error").into_response()
        } else if shared_file(&req) {
            self.own_or_service_file(req, next).await
        } else {
            next.run(req).await
        };
        let h = res.headers_mut();
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        h.insert(HeaderName::from_static("x-robots-tag"), HeaderValue::from_static("noindex, nofollow"));
        res
    }
}

/// A `GET` under the paths where Corten serves an app's files: the gateway's own (the sign-in
/// page's stylesheet, Corten's runtime) or the service's (the dashboard's).
fn shared_file(req: &Request) -> bool {
    let path = req.uri().path();
    matches!(*req.method(), Method::GET | Method::HEAD) && (path.starts_with("/assets/") || path.starts_with("/corten/"))
}

impl Headers {
    /// The gateway's own file; else, for a session, the service's (the dashboard's stylesheets,
    /// and Corten's runtime should the two builds ever differ). Without a session a file the
    /// gateway does not have stays a 404.
    async fn own_or_service_file(&self, req: Request, next: impl corten::Next) -> Response {
        let (parts, body) = req.into_parts();
        let (method, headers) = (parts.method.clone(), parts.headers.clone());
        let path_q = parts.uri.path_and_query().map_or_else(|| parts.uri.path().to_string(), |p| p.as_str().to_string());
        let own = next.run(Request::from_parts(parts, body)).await;
        if own.status() != StatusCode::NOT_FOUND || !matches!(self.gw.session(&headers), Ok(Some(_))) {
            return own;
        }
        let gw = &self.gw;
        gw.forward(&gw.0.service, method, &path_q, gw.read_headers(&headers), reqwest::Body::from(""), |_| {}).await.into_response()
    }
}

#[corten::routes(state = Gw)]
pub mod url {
    #[route("/", get = dashboard_read)]
    pub struct Home;

    #[route("/accounts", get = dashboard_read)]
    pub struct Accounts;

    #[route("/usage", get = dashboard_read)]
    pub struct Usage;

    #[route("/policy", get = dashboard_read)]
    pub struct PolicyTab;

    #[route("/live", get = dashboard_read)]
    pub struct Live;

    #[route("/signin", get = dashboard_read, post = dashboard_write)]
    pub struct Signin;

    #[route("/signin/finish", post = dashboard_write)]
    pub struct SigninFinish;

    #[route("/signin/cancel", post = dashboard_write)]
    pub struct SigninCancel;

    #[route("/refresh", post = dashboard_write)]
    pub struct RefreshForm;

    #[route("/login", get = login_page, post = login)]
    pub struct Login;

    #[route("/logout", get = logout, post = logout)]
    pub struct Logout;

    #[route("/api/status", get = dashboard_read)]
    pub struct Status;

    #[route("/api/capacity", get = dashboard_read)]
    pub struct Capacity;

    #[route("/api/ladders", get = dashboard_read)]
    pub struct Ladders;

    #[route("/api/value", get = dashboard_read)]
    pub struct ValueReport;

    #[route("/api/summary", get = dashboard_read)]
    pub struct Summary;

    #[route("/api/health", get = dashboard_read)]
    pub struct Health;

    #[route("/api/refresh", post = refresh)]
    pub struct Refresh;
}

/// A registered machine, found by the key it presents (`x-api-key` or Bearer). Every `/v1` route
/// takes one (`caller = Machine`), so nothing reaches the pool without it.
#[derive(Clone, Hash)]
pub struct Machine(pub Client);

impl corten::Identify<Gw> for Machine {
    /// public-gateway.mjs's `inference` checks: an address with too many failures is refused
    /// before its key is looked at (429); a missing or unknown key is a 401 and a failure counted
    /// against the address.
    async fn identify(parts: &corten::axum::http::request::Parts, gw: &Gw) -> Result<Machine, corten::Refused> {
        let mut own = parts.clone();
        let ip = ip_of(<ClientIp as corten::axum::extract::OptionalFromRequestParts<Gw>>::from_request_parts(&mut own, gw).await.unwrap_or(None));
        if let Some(wait) = gw.locked_for(&ip) {
            return Err(corten::Refused::too_many(wait).message("Too many failed attempts; try again later"));
        }
        let presented = presented_key(&parts.headers);
        match gw.find_client(presented.as_ref().map(|p| p.0.as_str())) {
            Some(client) => Ok(Machine(client)),
            None => {
                gw.fail(&ip, parts.uri.path(), if presented.is_some() { "unknown-key" } else { "no-key" });
                Err(corten::Refused::unauthorized().message("Invalid API key"))
            }
        }
    }
}

#[corten::routes(api = V1, state = Gw, caller = Machine)]
pub mod v1 {
    #[route("/v1")]
    pub struct V1;

    #[route("/v1/messages", post = inference)]
    pub struct Messages;

    #[route("/v1/messages/count_tokens", post = inference)]
    pub struct CountTokens;

    #[route("/v1/responses", post = inference)]
    pub struct Responses;

    #[route("/v1/responses/compact", post = inference)]
    pub struct Compact;

    #[route("/v1/chat/completions", post = inference)]
    pub struct ChatCompletions;

    #[route("/v1/models", get = inference)]
    pub struct Models;

    #[route("/v1/models/{id}", get = inference_model)]
    pub struct Model<'a> {
        pub id: &'a str,
    }
}

/// What a gateway handler answers: a relayed upstream answer, a page, or an answer of its own
/// (an Anthropic-style JSON error, a redirect). Corten's own failures under it are JSON.
pub enum Out {
    Relay(Relay),
    Page(corten::HtmlResponse),
    Own(Response),
    Switch(corten::upgrade::Switch),
}

impl IntoResponse for Out {
    fn into_response(self) -> Response {
        match self {
            Out::Relay(r) => r.into_response(),
            Out::Page(p) => p.into_response(),
            Out::Own(r) => r,
            Out::Switch(s) => s.into_response(),
        }
    }
}

impl corten::answer::Answer for Out {
    const FORMAT: corten::answer::Format = corten::answer::Format::Json;
    type Inputs = corten::answer::AnyInputs;
    type Rows = corten::answer::NotAFeed;
}

/// Agent Pools' `json(res, status, error, type)`: `{"type":"error","error":{"type","message"}}`.
fn error(status: StatusCode, message: &str, kind: &str) -> Out {
    let body = json!({ "type": "error", "error": { "type": kind, "message": message } }).to_string();
    let mut res = Response::new(Body::from(body));
    *res.status_mut() = status;
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Out::Own(res)
}

fn see_other(to: &str, cookie: Option<String>) -> Out {
    let mut res = Response::new(Body::empty());
    *res.status_mut() = StatusCode::SEE_OTHER;
    res.headers_mut().insert(header::LOCATION, HeaderValue::from_str(to).unwrap_or(HeaderValue::from_static("/")));
    if let Some(c) = cookie.and_then(|c| HeaderValue::from_str(&c).ok()) {
        res.headers_mut().insert(header::SET_COOKIE, c);
    }
    Out::Own(res)
}

pub fn sha256(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// `sameText`: equal strings, compared in time that does not depend on where they differ.
fn same_text(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn hmac(secret: &str, payload: &str) -> String {
    use hmac::Mac;
    let mut mac = hmac::Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key");
    mac.update(payload.as_bytes());
    b64(&mac.finalize().into_bytes())
}

/// `sessionToken(secret, client, expiresAt)`: the same bytes as Agent Pools', so a cookie it
/// signed stays valid here with the same secret.
pub fn session_token(secret: &str, client: &Client, expires_at: f64) -> String {
    let k: String = client.key_sha256.chars().take(16).collect();
    let payload = b64(pool_core::js::stringify(&json!({ "n": client.name, "k": k, "e": pool_core::js::number_value(expires_at) })).as_bytes());
    let sig = hmac(secret, &payload);
    format!("{payload}.{sig}")
}

fn read_session(secret: &str, token: &str, clients: &[Client], now: f64) -> Option<Client> {
    use base64::Engine;
    let mut parts = token.split('.');
    let (payload, sig) = (parts.next()?, parts.next()?);
    if payload.is_empty() || sig.is_empty() || parts.next().is_some() {
        return None;
    }
    if !same_text(sig, &hmac(secret, payload)) {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let data: Value = serde_json::from_slice(&bytes).ok()?;
    let e = data.get("e")?.as_f64()?;
    if e <= now {
        return None;
    }
    let (n, k) = (data.get("n")?.as_str()?, data.get("k")?.as_str()?);
    clients.iter().find(|c| c.name == n && c.key_sha256.starts_with(k)).cloned()
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|part| part.split_once('='))
        .find(|(k, _)| k.trim() == name)
        .map(|(_, v)| v.trim())
}

/// `presentedKey(req)`: the key and how it came (`x-api-key`, or Bearer).
fn presented_key(headers: &HeaderMap) -> Option<(String, bool)> {
    if let Some(k) = headers.get("x-api-key").and_then(|v| v.to_str().ok()).filter(|k| !k.is_empty()) {
        return Some((k.trim().to_string(), false));
    }
    let auth = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = auth.split_at_checked(6)?;
    let key = rest.trim_start();
    (scheme.eq_ignore_ascii_case("bearer") && rest.len() > key.len() && !key.is_empty() && !key.contains(char::is_whitespace))
        .then(|| (key.to_string(), true))
}

const COOKIE: &str = "ap_session";
const HOP_BY_HOP: [&str; 9] =
    ["connection", "keep-alive", "proxy-connection", "transfer-encoding", "upgrade", "te", "trailer", "proxy-authorization", "proxy-authenticate"];
/// Client identity and edge headers the pool has no use for.
const DROP_REQUEST: [&str; 10] =
    ["host", "cookie", "x-api-key", "authorization", "cdn-loop", "true-client-ip", "x-forwarded-for", "x-forwarded-proto", "x-forwarded-host", "x-real-ip"];

impl Gw {
    fn c(&self) -> &Config {
        &self.0.config
    }

    fn emit(&self, kind: &str, data: Value) {
        let observe = self.c().observe.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observe(kind, data)));
    }

    fn locked(&self, ip: &str) -> bool {
        self.locked_for(ip).is_some()
    }

    /// `locked(ip)`, and how long until its window ends and the address may try again.
    fn locked_for(&self, ip: &str) -> Option<std::time::Duration> {
        let now = (self.c().now)();
        let mut failures = self.0.failures.lock().unwrap();
        match failures.get(ip) {
            None => None,
            Some((first, _)) if now - first > self.c().failure_window_ms => {
                failures.remove(ip);
                None
            }
            Some((first, count)) if *count >= self.c().max_failures => {
                let left = (first + self.c().failure_window_ms - now).max(0.0);
                Some(std::time::Duration::from_millis(left as u64))
            }
            Some(_) => None,
        }
    }

    fn fail(&self, ip: &str, path: &str, reason: &str) {
        let now = (self.c().now)();
        {
            let mut failures = self.0.failures.lock().unwrap();
            match failures.get_mut(ip) {
                Some((first, count)) if now - *first <= self.c().failure_window_ms => *count += 1,
                _ => {
                    failures.insert(ip.to_string(), (now, 1));
                }
            }
            if failures.len() > 10_000 {
                failures.clear();
            }
        }
        // One log line per address per minute keeps scanners from filling the log.
        let log = {
            let mut last = self.0.last_reject_log.lock().unwrap();
            let due = last.get(ip).copied().unwrap_or(0.0) + 60_000.0 < now;
            if due {
                last.insert(ip.to_string(), now);
                if last.len() > 10_000 {
                    last.clear();
                }
            }
            due
        };
        if log {
            self.emit("public-auth-rejected", json!({ "ip": ip, "path": path, "reason": reason }));
        }
    }

    fn find_client(&self, key: Option<&str>) -> Option<Client> {
        let hash = sha256(key.filter(|k| !k.is_empty())?);
        (self.c().clients)().into_iter().find(|c| same_text(&c.key_sha256, &hash))
    }

    fn session(&self, headers: &HeaderMap) -> Result<Option<Client>, String> {
        let Some(token) = cookie(headers, COOKIE) else { return Ok(None) };
        let secret = (self.c().session_secret)()?;
        Ok(read_session(&secret, token, &(self.c().clients)(), (self.c().now)()))
    }

    /// The headers a read of the dashboard sends the service: its own `Host`, and what the page
    /// and Corten's runtime ask with (`corten: nav`/`action`, a live stream's `Last-Event-ID`, a
    /// cached file's validators). Never the cookie or the edge's headers.
    fn read_headers(&self, from: &HeaderMap) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_str(&self.0.service.host).expect("checked host"));
        headers.insert(header::ACCEPT, from.get(header::ACCEPT).cloned().unwrap_or(HeaderValue::from_static("*/*")));
        for name in ["corten", "last-event-id", "if-none-match", "if-modified-since", "accept-language"] {
            if let Some(v) = from.get(name) {
                headers.insert(HeaderName::from_static(name), v.clone());
            }
        }
        headers
    }

    /// `forward`: the request to `target`, its answer relayed as it arrives, less hop-by-hop
    /// headers and `Set-Cookie`. `done` gets the status once the answer has been read to its end
    /// (502 when the upstream cannot be reached).
    async fn forward(&self, target: &Upstream, method: Method, path: &str, headers: HeaderMap, body: reqwest::Body, done: impl FnOnce(u16) + Send + 'static) -> Out {
        let url = format!("{}{path}", target.origin);
        let sent = self.0.http.request(method, url).headers(headers).body(body).send().await;
        let res = match sent {
            Ok(res) => res,
            Err(_) => {
                done(502);
                return error(StatusCode::BAD_GATEWAY, "Pool unreachable", "api_error");
            }
        };
        let status = res.status();
        let mut out = res.headers().clone();
        for h in HOP_BY_HOP {
            out.remove(h);
        }
        out.remove(header::SET_COOKIE);
        let mut done = Some(done);
        let mut stream = res.bytes_stream();
        let body = async_stream(move |yield_| async move {
            while let Some(piece) = stream.next().await {
                let failed = piece.is_err();
                if yield_.send(piece).await.is_err() || failed {
                    return;
                }
            }
            if let Some(done) = done.take() {
                done(status.as_u16());
            }
        });
        Out::Relay(Relay::new(status, out, Body::from_stream(body)))
    }
}

/// A stream fed by `produce` through a channel of one, so each piece goes on as it arrives and
/// `produce` stops (dropping the upstream) when the client's side is dropped.
pub(crate) fn async_stream<T: Send + 'static, F: std::future::Future<Output = ()> + Send + 'static>(
    produce: impl FnOnce(tokio::sync::mpsc::Sender<T>) -> F,
) -> impl futures_util::Stream<Item = T> + Send + 'static {
    let (send, receive) = tokio::sync::mpsc::channel(1);
    let task = tokio::spawn(produce(send));
    let guard = AbortOnDrop(task);
    futures_util::stream::unfold((receive, guard), |(mut receive, guard)| async move { receive.recv().await.map(|item| (item, (receive, guard))) })
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn ip_of(ip: Option<ClientIp>) -> String {
    ip.map_or_else(|| "unknown".into(), |ClientIp(ip): ClientIp| ip_text(ip))
}

fn ip_text(ip: IpAddr) -> String {
    ip.to_string()
}

/// Model ids `GET /v1/models/{id}` may name: `[\w.:-]{1,128}`.
fn model_id_ok(id: &str) -> bool {
    (1..=128).contains(&id.chars().count()) && id.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == ':' || c == '-')
}

/// `inference` for one model: the id is checked from the path itself (`model_id_ok`), as
/// public-gateway.mjs checks it, so `Params` is only taken.
async fn inference_model(_: Params<v1::Model<'_>>, gw: State<Gw>, caller: corten::Caller<Machine>, req: Request) -> Out {
    inference(gw, caller, req).await
}

async fn inference(State(gw): State<Gw>, corten::Caller(Machine(client)): corten::Caller<Machine>, req: Request) -> Out {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    if let Some(id) = path.strip_prefix("/v1/models/")
        && !model_id_ok(id)
    {
        return error(StatusCode::NOT_FOUND, "Not found", "not_found_error");
    }
    let presented = presented_key(&parts.headers);
    let Some(key) = (gw.c().pool_key)().filter(|k| !k.is_empty()) else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Pool key unavailable", "api_error");
    };
    let mut headers = HeaderMap::new();
    for (name, value) in &parts.headers {
        let n = name.as_str();
        if !HOP_BY_HOP.contains(&n) && !DROP_REQUEST.contains(&n) && !n.starts_with("cf-") {
            headers.append(name.clone(), value.clone());
        }
    }
    headers.insert(header::HOST, HeaderValue::from_str(&gw.0.pool.host).expect("checked host"));
    let bearer = presented.is_some_and(|p| p.1);
    if bearer {
        headers.insert(header::AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {key}")).unwrap_or(HeaderValue::from_static("")));
    } else {
        headers.insert("x-api-key", HeaderValue::from_str(&key).unwrap_or(HeaderValue::from_static("")));
    }
    let started = (gw.c().now)();
    let path_q = parts.uri.path_and_query().map_or(path.clone(), |p| p.as_str().to_string());
    let (g2, method) = (gw.clone(), parts.method.clone());
    let done = move |status: u16| {
        let ms = (g2.c().now)() - started;
        g2.emit("public-request", json!({ "client": client.name, "method": method.as_str(), "path": path, "status": status, "ms": ms }));
    };
    let body = reqwest::Body::wrap_stream(body.into_data_stream());
    gw.forward(&gw.0.pool, parts.method, &path_q, headers, body, done).await
}

#[component]
fn LoginPage<'a>(req: &'a Req, message: Option<&'a str>) -> impl Document {
    html! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8">
                <meta name="viewport" content="width=device-width,initial-scale=1">
                <link rel="stylesheet" href={req.asset(Static::LOGIN_CSS)}>
                <title>"Agent Pools"</title>
                <Corten />
            </head>
            <body>
                <main>
                <form method="post" action={url::Login}>
                    <h1>"Agent Pools"</h1>
                    <p>"Enter this machine's pool key."</p>
                    if let Some(m) = message { <p class="err">{m}</p> }
                    <input type="password" name="key" autocomplete="current-password" autofocus required>
                    <button type="submit">"Sign in"</button>
                </form>
                </main>
            </body>
        </html>
    }
}

async fn login_page(req: Req) -> Out {
    match req.page(html! { <LoginPage req={&req} message={None} /> }) {
        Ok(page) => Out::Page(page),
        Err(_) => error(StatusCode::SERVICE_UNAVAILABLE, "Gateway unavailable", "api_error"),
    }
}

async fn login(req: Req, State(gw): State<Gw>, ip: Option<ClientIp>, headers: HeaderMap, body: corten::axum::body::Bytes) -> Out {
    let ip = ip_of(ip);
    if headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) != Some(gw.0.public_origin.as_str()) {
        return error(StatusCode::FORBIDDEN, "Same-origin form required", "permission_error");
    }
    if gw.locked(&ip) {
        return error(StatusCode::TOO_MANY_REQUESTS, "Too many failed attempts; try again later", "rate_limit_error");
    }
    if body.len() > 4096 {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "Body too large", "error");
    }
    let key = form_urlencoded::parse(&body).find(|(k, _)| k == "key").map(|(_, v)| v.trim().to_string());
    let Some(client) = gw.find_client(key.as_deref()) else {
        gw.fail(&ip, "/login", "unknown-key");
        return match req.page(html! { <LoginPage req={&req} message={Some("That key is not registered.")} /> }) {
            Ok(mut page) => {
                *page.status_mut() = StatusCode::UNAUTHORIZED;
                Out::Page(page)
            }
            Err(_) => error(StatusCode::UNAUTHORIZED, "That key is not registered.", "authentication_error"),
        };
    };
    let secret = match (gw.c().session_secret)() {
        Ok(s) => s,
        Err(e) => return unavailable(&gw, &e),
    };
    let max_age = gw.c().session_days * 86400.0;
    let token = session_token(&secret, &client, (gw.c().now)() + max_age * 1000.0);
    gw.emit("public-login", json!({ "client": client.name, "ip": ip }));
    see_other("/", Some(format!("{COOKIE}={token}; Max-Age={}; Path=/; HttpOnly; Secure; SameSite=Strict", pool_core::js::number_to_string(max_age))))
}

async fn logout() -> Out {
    see_other("/login", Some(format!("{COOKIE}=; Max-Age=0; Path=/; HttpOnly; Secure; SameSite=Strict")))
}

fn unavailable(gw: &Gw, problem: &str) -> Out {
    let message: String = problem.chars().take(200).collect();
    gw.emit("public-gateway-problem", json!({ "message": message }));
    error(StatusCode::SERVICE_UNAVAILABLE, "Gateway unavailable", "api_error")
}

async fn dashboard_read(State(gw): State<Gw>, req: Request) -> Out {
    let (parts, _) = req.into_parts();
    let client = match gw.session(&parts.headers) {
        Ok(c) => c,
        Err(e) => return unavailable(&gw, &e),
    };
    let path = parts.uri.path();
    if client.is_none() {
        // A page goes to the sign-in form; the JSON reads answer 401.
        if !path.starts_with("/api/") {
            return see_other("/login", None);
        }
        return error(StatusCode::UNAUTHORIZED, "Sign in at /login", "authentication_error");
    }
    let headers = gw.read_headers(&parts.headers);
    let path_q = parts.uri.path_and_query().map_or(path.to_string(), |p| p.as_str().to_string());
    gw.forward(&gw.0.service, parts.method, &path_q, headers, reqwest::Body::from(""), |_| {}).await
}

/// The dashboard's own forms (Refresh, and the sign-in steps: open, finish with a pasted link,
/// cancel), for a session and only from the public origin. The service gets them as a write from
/// its own origin, so they act as they do on this PC; the session is what admits them here.
async fn dashboard_write(State(gw): State<Gw>, req: Request) -> Out {
    let (parts, body) = req.into_parts();
    match gw.session(&parts.headers) {
        Ok(Some(_)) => {}
        Ok(None) => return see_other("/login", None),
        Err(e) => return unavailable(&gw, &e),
    }
    if parts.headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) != Some(gw.0.public_origin.as_str()) {
        return error(StatusCode::FORBIDDEN, "Same-origin request required", "permission_error");
    }
    let kind = parts.headers.get(header::CONTENT_TYPE).cloned();
    let form = kind.as_ref().and_then(|v| v.to_str().ok()).is_some_and(|t| {
        let t = t.to_ascii_lowercase();
        t.starts_with("application/x-www-form-urlencoded") || t.starts_with("multipart/form-data")
    });
    let Some(kind) = kind.filter(|_| form) else {
        return error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Form required", "error");
    };
    let Ok(body) = corten::axum::body::to_bytes(body, 64 * 1024).await else {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "Body too large", "error");
    };
    let s = &gw.0.service;
    let mut headers = gw.read_headers(&parts.headers);
    headers.insert(header::ORIGIN, HeaderValue::from_str(&s.origin).expect("checked origin"));
    headers.insert(header::CONTENT_TYPE, kind);
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    let path_q = parts.uri.path_and_query().map_or_else(|| parts.uri.path().to_string(), |p| p.as_str().to_string());
    gw.forward(s, Method::POST, &path_q, headers, reqwest::Body::from(body), |_| {}).await
}

async fn refresh(State(gw): State<Gw>, req: Request) -> Out {
    let (parts, body) = req.into_parts();
    match gw.session(&parts.headers) {
        Ok(Some(_)) => {}
        Ok(None) => return error(StatusCode::UNAUTHORIZED, "Sign in at /login", "authentication_error"),
        Err(e) => return unavailable(&gw, &e),
    }
    if parts.headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) != Some(gw.0.public_origin.as_str()) {
        return error(StatusCode::FORBIDDEN, "Same-origin request required", "permission_error");
    }
    if !parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").starts_with("application/json") {
        return error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "JSON required", "error");
    }
    let Ok(body) = corten::axum::body::to_bytes(body, 64 * 1024).await else {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "Body too large", "error");
    };
    let mut headers = HeaderMap::new();
    let s = &gw.0.service;
    headers.insert(header::HOST, HeaderValue::from_str(&s.host).expect("checked host"));
    headers.insert(header::ORIGIN, HeaderValue::from_str(&s.origin).expect("checked origin"));
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    gw.forward(s, Method::POST, "/api/refresh", headers, reqwest::Body::from(body), |_| {}).await
}
