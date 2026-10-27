//! Agent Pools' ChatGPT backend relay (backend-relay.mjs at 5c696f6) on Corten: the desktop
//! relay listener. Everything under \`/backend-api/\` goes to the upstream with the caller's own
//! credentials and streams back; only \`GET /backend-api/wham/usage\` is handed to the usage
//! rewrite, so the desktop's banner and usage panel show the pool. A WebSocket handshake (Remote
//! Control's host and client links, dictation) is relayed to the upstream, and once the upstream
//! has switched, bytes go both ways untouched.
//!
//! | Route | Answer |
//! |---|---|
//! | any method on \`/backend-api\`, \`/backend-api/{*rest}\` | the upstream's answer, streamed (\`/wham/usage\` rewritten) |
//! | \`GET\` asking to switch protocols on the same paths | the upstream's 101, then a tunnel |
//!
//! # Where it differs from backend-relay.mjs
//!
//! 1. A switch is Corten's \`Upgrade\`: the handshake is checked as a route (host, cross-site),
//!    the upstream's 101 is relayed with \`Upgrade::switch\`, and the tunnel starts after it.
//!    Agent Pools writes the handshake to a raw socket and pipes whatever comes back. An upstream
//!    that does not switch is relayed as an ordinary answer either way.
//! 2. A switch the relay cannot carry (not a backend path, the upstream unreachable or slower than
//!    the connect timeout) gets an answer: Corten's 404, or 502 with this relay's JSON. Agent Pools
//!    drops the connection.
//! 3. The host check covers switches too: Agent Pools' \`upgrade\` handler is not behind it.
//! 4. A browser's write from another site (an \`Origin\` not this listener's, or
//!    \`Sec-Fetch-Site: cross-site\`) is Corten's 403 unless \`Config::allow_origins\` names it.
//!    Agent Pools checks only the \`Host\`. Codex's own HTTP client sends neither header.
//! 5. A usage body that is not JSON is passed on as Agent Pools does, and the event's message is
//!    serde_json's, not \`JSON.parse\`'s.
//! 6. TLS (\`state/certs\`) is the listener's: \`load_relay_tls\` reads the pair as Agent Pools does,
//!    and the program that serves the app wraps its listeners (corten::serve_on).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use corten::axum::body::Body;
use corten::axum::extract::{FromRequestParts, Request};
use corten::axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use corten::axum::response::{IntoResponse, Response};
use corten::prelude::*;
use corten::upgrade::Upgrade;
use corten::Relay;
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::Out;

pub const BACKEND_PREFIX: &str = "/backend-api";
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Request headers that are the connection's, not the request's, and the encoding the relay
/// sets itself (\`identity\`, so the usage body can be read).
const HOP_HEADERS: [&str; 9] = ["host", "connection", "content-length", "transfer-encoding", "accept-encoding", "keep-alive", "proxy-connection", "te", "upgrade"];
const DROPPED_RESPONSE_HEADERS: [&str; 4] = ["content-encoding", "content-length", "transfer-encoding", "connection"];

/// \`isBackendPath\`: \`/backend-api\` or anything under it.
pub fn is_backend_path(path: &str) -> bool {
    path == BACKEND_PREFIX || path.strip_prefix(BACKEND_PREFIX).is_some_and(|rest| rest.starts_with('/'))
}

/// The desktop relay's TLS pair, as Set-Desktop-Relay.ps1 writes it to \`state/certs\`.
pub struct TlsMaterial {
    pub cert: Vec<u8>,
    pub key: Vec<u8>,
}

/// \`loadRelayTls(dir)\`: the pair, \`None\` when either file is missing (plain HTTP then), or the
/// error when one cannot be read for another reason.
pub fn load_relay_tls(dir: &Path) -> std::io::Result<Option<TlsMaterial>> {
    load_relay_tls_files(dir, "desktop-relay.crt", "desktop-relay.key")
}

pub fn load_relay_tls_files(dir: &Path, cert_file: &str, key_file: &str) -> std::io::Result<Option<TlsMaterial>> {
    let read = |name: &str| match std::fs::read(dir.join(name)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    };
    // Both are read before either is judged, as Promise.all does.
    let (cert, key) = (read(cert_file), read(key_file));
    match (cert?, key?) {
        (Some(cert), Some(key)) => Ok(Some(TlsMaterial { cert, key })),
        _ => Ok(None),
    }
}

/// The usage rewrite: the account's \`/wham/usage\` body in, the body the desktop sees out. An
/// error passes the account's own body on and is reported (\`pool-usage-overlay-error\`).
pub type Rewrite = Arc<dyn Fn(Value) -> Result<Value, String> + Send + Sync>;
pub type Observe = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// createBackendRelay's options, and the listener's port (its hosts name it).
#[derive(Clone)]
pub struct Config {
    /// The upstream's \`/backend-api\`: \`https://chatgpt.com/backend-api\` in service.
    pub upstream: String,
    pub rewrite_usage: Option<Rewrite>,
    pub observe: Observe,
    pub max_body_bytes: usize,
    pub connect_timeout: Duration,
    /// Browser origins whose writes and switches are let through (none by default).
    pub allow_origins: Vec<String>,
}

impl Config {
    pub fn new(upstream: &str) -> Config {
        Config {
            upstream: upstream.into(),
            rewrite_usage: None,
            observe: Arc::new(|_, _| {}),
            max_body_bytes: MAX_BODY_BYTES,
            connect_timeout: CONNECT_TIMEOUT,
            allow_origins: Vec::new(),
        }
    }
}

/// The relay's state, shared by its handlers.
#[derive(Clone)]
pub struct Backend(Arc<Inner>);

struct Inner {
    config: Config,
    /// \`https://chatgpt.com\`
    origin: String,
    /// \`/backend-api\`, without a trailing slash.
    base_path: String,
    http: reqwest::Client,
}

/// The relay's app for a listener on \`port\`: it answers \`localhost:<port>\` and
/// \`127.0.0.1:<port>\` only, as the desktop relay does.
pub fn app(config: Config, port: u16) -> Result<App<Hosts>, String> {
    let url = reqwest::Url::parse(&config.upstream).map_err(|e| format!("upstream: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("upstream: an http or https URL".into());
    }
    let origin = url.origin().ascii_serialization();
    let base_path = url.path().trim_end_matches('/').to_string();
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(config.connect_timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let mut policy = corten::security::Policy::new();
    for o in &config.allow_origins {
        policy = policy.allow_origin(o);
    }
    let backend = Backend(Arc::new(Inner { origin, base_path, http, config }));
    Ok(App::new(url::router(backend))
        .policy(policy)
        // A conversation streams for as long as the model writes, and a tunnel stays open.
        .deadline(None)
        .middleware(Hosts { hosts: vec![format!("localhost:{port}"), format!("127.0.0.1:{port}")] }))
}

/// \`desktopRelayHosts\`: only the listener's own names, else 403 \`{"error":"Invalid host"}\`.
pub struct Hosts {
    hosts: Vec<String>,
}

impl corten::Middleware for Hosts {
    async fn call(&self, req: Request, next: impl corten::Next) -> Response {
        let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
        if self.hosts.iter().any(|h| h == host) {
            next.run(req).await
        } else {
            reply(StatusCode::FORBIDDEN, json!({ "error": "Invalid host" })).into_response()
        }
    }
}

#[corten::routes(state = Backend)]
pub mod url {
    #[route("/backend-api", get = get, post = handle, put = handle, delete = handle, patch = handle, options = handle)]
    pub struct Root;

    #[route("/backend-api/{*rest}", get = get, post = handle, put = handle, delete = handle, patch = handle, options = handle)]
    pub struct Under<'a> {
        pub rest: &'a str,
    }
}

/// \`reply(res, status, data)\`: JSON, never cached.
fn reply(status: StatusCode, data: Value) -> Out {
    let mut res = Response::new(Body::from(data.to_string()));
    *res.status_mut() = status;
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Out::Own(res)
}

/// Whether a request asks to switch protocols (Corten's own test, which its \`Upgrade\` applies).
fn asks_to_switch(method: &Method, headers: &HeaderMap) -> bool {
    *method == Method::GET
        && headers.contains_key(header::UPGRADE)
        && headers
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
}

impl Backend {
    fn c(&self) -> &Config {
        &self.0.config
    }

    fn emit(&self, kind: &str, data: Value) {
        let observe = self.c().observe.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observe(kind, data)));
    }

    /// \`upstreamPath(url)\`: the upstream's base path, the rest after \`/backend-api\`, the query.
    fn upstream_url(&self, uri: &corten::axum::http::Uri) -> String {
        let rest = &uri.path()[BACKEND_PREFIX.len()..];
        let query = uri.query().map_or(String::new(), |q| format!("?{q}"));
        format!("{}{}{rest}{query}", self.0.origin, self.0.base_path)
    }
}

async fn get(State(b): State<Backend>, req: Request) -> Out {
    if asks_to_switch(req.method(), req.headers()) {
        tunnel(b, req).await
    } else {
        relay(b, req).await
    }
}

async fn handle(State(b): State<Backend>, req: Request) -> Out {
    relay(b, req).await
}

/// \`readBody(req, limit)\`: the whole body, \`None\` when empty, or the status and message.
async fn read_body(body: Body, limit: usize) -> Result<Option<Vec<u8>>, (StatusCode, String)> {
    let mut stream = body.into_data_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        if out.len() + chunk.len() > limit {
            return Err((StatusCode::PAYLOAD_TOO_LARGE, format!("Request body exceeds {limit} bytes")));
        }
        out.extend_from_slice(&chunk);
    }
    Ok((!out.is_empty()).then_some(out))
}

/// \`handle(req, res, url)\`: the request to the upstream with the caller's headers, its answer
/// streamed back (the usage poll rewritten).
async fn relay(b: Backend, req: Request) -> Out {
    let (parts, body) = req.into_parts();
    let mut headers = HeaderMap::new();
    for (name, value) in &parts.headers {
        if !HOP_HEADERS.contains(&name.as_str()) {
            headers.append(name.clone(), value.clone());
        }
    }
    headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    let body = match read_body(body, b.c().max_body_bytes).await {
        Ok(body) => body,
        Err((status, message)) => return reply(status, json!({ "error": message })),
    };
    let mut request = b.0.http.request(parts.method.clone(), b.upstream_url(&parts.uri)).headers(headers);
    if let Some(body) = body {
        request = request.body(body);
    }
    let response = match request.send().await {
        Ok(r) => r,
        Err(e) => return reply(StatusCode::BAD_GATEWAY, json!({ "error": format!("Backend unreachable: {}", chain(&e)) })),
    };
    let status = response.status();
    let mut out = response.headers().clone();
    for h in DROPPED_RESPONSE_HEADERS {
        out.remove(h);
    }
    let usage = parts.method == Method::GET && parts.uri.path() == "/backend-api/wham/usage" && status == StatusCode::OK;
    if let (Some(rewrite), true) = (b.c().rewrite_usage.clone(), usage) {
        let payload = match response.bytes().await {
            Ok(p) => p.to_vec(),
            Err(e) => return reply(StatusCode::BAD_GATEWAY, json!({ "error": format!("Backend unreachable: {}", chain(&e)) })),
        };
        let rewritten = serde_json::from_slice::<Value>(&payload)
            .map_err(|e| e.to_string())
            .and_then(|v| std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rewrite(v))).unwrap_or_else(|_| Err("the usage rewrite panicked".into())));
        let payload = match rewritten {
            Ok(v) => {
                out.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
                pool_core::js::stringify(&v).into_bytes()
            }
            Err(message) => {
                // Fail open to the account's real numbers rather than break the poll.
                b.emit("pool-usage-overlay-error", json!({ "message": message }));
                payload
            }
        };
        out.insert(header::CONTENT_LENGTH, HeaderValue::from(payload.len()));
        return Out::Relay(Relay::new(status, out, Body::from(payload)));
    }
    // Dropped with the answer when the client goes, which closes the upstream request.
    Out::Relay(Relay::new(status, out, Body::from_stream(response.bytes_stream())))
}

/// An error and its causes, as one line.
fn chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        text.push_str(": ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    text
}

/// \`upgrade(req, socket, head)\`: the handshake to the upstream with every header but \`Host\`;
/// once the upstream switches, its 101 is relayed and bytes pass both ways until either side
/// closes or the app shuts down.
async fn tunnel(b: Backend, req: Request) -> Out {
    let (mut parts, _) = req.into_parts();
    let upgrade = match Upgrade::from_request_parts(&mut parts, &()).await {
        Ok(u) => u,
        Err(refused) => return Out::Own(refused),
    };
    let mut headers = HeaderMap::new();
    for (name, value) in &parts.headers {
        if name != header::HOST {
            headers.append(name.clone(), value.clone());
        }
    }
    let sent = b.0.http.get(b.upstream_url(&parts.uri)).headers(headers).send();
    let response = match tokio::time::timeout(b.c().connect_timeout, sent).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return reply(StatusCode::BAD_GATEWAY, json!({ "error": format!("Backend unreachable: {}", chain(&e)) })),
        Err(_) => return reply(StatusCode::BAD_GATEWAY, json!({ "error": "Backend unreachable: no answer to the switch in time" })),
    };
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        // The upstream refused to switch: its answer goes back as it is.
        let status = response.status();
        let mut out = response.headers().clone();
        for h in DROPPED_RESPONSE_HEADERS {
            out.remove(h);
        }
        return Out::Relay(Relay::new(status, out, Body::from_stream(response.bytes_stream())));
    }
    let switched = response.headers().clone();
    let mut upstream = match response.upgrade().await {
        Ok(u) => u,
        Err(e) => return reply(StatusCode::BAD_GATEWAY, json!({ "error": format!("Backend unreachable: {}", chain(&e)) })),
    };
    Out::Switch(upgrade.switch(switched, move |mut stream, signal| async move {
        tokio::select! {
            _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream) => {}
            _ = signal.aborted() => {}
        }
    }))
}
