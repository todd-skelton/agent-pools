//! Agent Pools' inference relay (inference-relay.mjs at 5c696f6) on Corten: Codex on this
//! machine sends `/v1/responses` here, over HTTP or a WebSocket, with its own sign-in as the
//! Bearer; the relay checks it (`authorize`, read again on every admission and every WebSocket
//! frame) and sends the request to the pool with the pool's key. Inference only: no other route,
//! no redirect, no replay. The one retry is a refused connect while the pool restarts.
//!
//! | Route | Answer |
//! |---|---|
//! | `POST /v1/responses`, `/v1/responses/compact` | the pool's answer, streamed; the body checked (size, zstd, JSON with a `model`) first |
//! | `GET /v1/responses` (WebSocket) | a socket whose text frames go to the pool's socket, each admitted again |
//! | `GET /health` | counters |
//!
//! # Where it differs from inference-relay.mjs
//!
//! 1. A WebSocket's handshake is Corten's `Upgrade`: a `GET /v1/responses` that does not ask to
//!    switch gets Corten's 426 (Agent Pools: 404), and a refused one gets this relay's JSON body
//!    with its status (Agent Pools: the bare status line).
//! 2. Unknown routes and wrong methods get Corten's 404 and 405 (Agent Pools: 404), and a `Host`
//!    that is not `localhost` or an address gets Corten's 421 (Agent Pools: 403).
//! 3. The server's limits are Corten's (corten#173): `App::header_timeout` closes a connection
//!    whose request head takes longer than `header_timeout` without answering it (Node answers
//!    408), and at `max_connections` further connections wait in the operating system's queue
//!    (Node closes them). The body read keeps its own header timeout, as Agent Pools' does.
//! 4. A client frame's backpressure is tungstenite's write buffer (`max_write_buffer_size`), where
//!    Agent Pools checks `bufferedAmount` before each send; both end the socket past the limit.

use std::future::Future;
use std::io::Read;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use corten::axum::body::Body;
use corten::axum::extract::Request;
use corten::axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use corten::axum::response::Response;
use corten::prelude::*;
use corten::upgrade::Upgrade;
use corten::{ClientIp, Relay};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest, protocol::Role, protocol::WebSocketConfig};

use crate::Out;

/// The pool key for a caller's `Authorization`, `None` for a caller it does not know, or an
/// error (the credential store could not be read: a 503, its message never shown).
pub type Authorize = Arc<dyn Fn(Option<String>) -> Pin<Box<dyn Future<Output = Result<Option<String>, String>> + Send>> + Send + Sync>;

pub type Observe = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// startInferenceRelay's options.
#[derive(Clone)]
pub struct Config {
    /// The pool's `/v1`: `http://127.0.0.1:<port>/v1`, nothing else.
    pub pool_base_url: String,
    pub authorize: Authorize,
    /// Served over TLS, so `localhost` is a host it answers to too.
    pub tls: bool,
    pub observe: Observe,
    pub max_body_bytes: usize,
    /// Connections held open at once (`maxConnections`).
    pub max_connections: usize,
    pub max_active: i64,
    pub header_timeout: Duration,
    pub stream_timeout: Duration,
    pub connect_retry: Duration,
}

impl Config {
    pub fn new(pool_base_url: &str, authorize: Authorize) -> Config {
        Config {
            pool_base_url: pool_base_url.into(),
            authorize,
            tls: false,
            observe: Arc::new(|_, _| {}),
            max_body_bytes: 32 * 1024 * 1024,
            max_connections: 128,
            max_active: 64,
            header_timeout: Duration::from_secs(30),
            stream_timeout: Duration::from_secs(30 * 60),
            connect_retry: Duration::from_secs(120),
        }
    }
}

#[derive(Default)]
struct Counters {
    http_requests: AtomicU64,
    websocket_requests: AtomicU64,
    rejected: AtomicU64,
    upstream_failures: AtomicU64,
    connect_waits: AtomicU64,
    waiting: AtomicI64,
}

/// The relay's state, shared by its handlers.
#[derive(Clone)]
pub struct Inf(Arc<Inner>);

struct Inner {
    config: Config,
    /// `127.0.0.1:<port>`, the pool's origin.
    pool: String,
    hosts: Vec<String>,
    http: reqwest::Client,
    active: AtomicI64,
    counters: Counters,
    stopping: AtomicBool,
}

/// One admitted request or socket; released when dropped.
struct Slot(Inf);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The relay's app on `port` (the listener's, bound first: the hosts it answers to name it).
pub fn app(config: Config, port: u16) -> Result<(App, Inf), String> {
    let rest = config.pool_base_url.strip_prefix("http://").ok_or("Only a loopback pool /v1 endpoint is allowed")?;
    let (authority, path) = rest.split_once('/').ok_or("Only a loopback pool /v1 endpoint is allowed")?;
    let (host, pool_port) = authority.split_once(':').ok_or("Only a loopback pool /v1 endpoint is allowed")?;
    if host != "127.0.0.1" || path != "v1" || pool_port.is_empty() || !pool_port.bytes().all(|b| b.is_ascii_digit()) {
        return Err("Only a loopback pool /v1 endpoint is allowed".into());
    }
    let mut hosts = vec![format!("127.0.0.1:{port}")];
    if config.tls {
        hosts.push(format!("localhost:{port}"));
    }
    let http = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().map_err(|e| e.to_string())?;
    let inf = Inf(Arc::new(Inner {
        pool: authority.to_string(),
        hosts,
        http,
        active: AtomicI64::new(0),
        counters: Counters::default(),
        stopping: AtomicBool::new(false),
        config,
    }));
    let app = App::new(url::router(inf.clone()))
        .deadline(None)
        // `server.headersTimeout` and `server.maxConnections`.
        .header_timeout(inf.c().header_timeout)
        .max_connections(inf.c().max_connections.max(1));
    Ok((app, inf))
}

#[corten::routes(state = Inf)]
pub mod url {
    #[route("/health", get = health)]
    pub struct Health;

    #[route("/v1/responses", get = socket, post = http)]
    pub struct Responses;

    #[route("/v1/responses/compact", post = http)]
    pub struct Compact;
}

/// `reply(res, status)`: `{"error":{"message":"Inference relay HTTP <status>","type":"relay_error"}}`.
fn reply(status: StatusCode) -> Out {
    let body = json!({ "error": { "message": format!("Inference relay HTTP {}", status.as_u16()), "type": "relay_error" } }).to_string();
    let mut res = Response::new(Body::from(body));
    *res.status_mut() = status;
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Out::Own(res)
}

/// `validBody`: a JSON object with a `model` of 1 to 128 characters.
fn valid_body(data: &[u8]) -> bool {
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(data) else { return false };
    body.get("model").and_then(Value::as_str).is_some_and(|m| !m.is_empty() && m.encode_utf16().count() <= 128)
}

/// Whether an error is a refused connect anywhere in its chain: the pool never got the request.
fn refused(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut e: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(err) = e {
        if err.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::ConnectionRefused) {
            return true;
        }
        e = err.source();
    }
    false
}

impl Inf {
    fn c(&self) -> &Config {
        &self.0.config
    }

    fn emit(&self, kind: &str, data: Value) {
        let observe = self.c().observe.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observe(kind, data)));
    }

    /// Stops admitting (shutdown).
    pub fn stop(&self) {
        self.0.stopping.store(true, Ordering::SeqCst);
    }

    pub fn active(&self) -> i64 {
        self.0.active.load(Ordering::SeqCst)
    }

    /// `validHost`: not stopping, no browser (`Origin`, `Sec-Fetch-Site`), one of the relay's
    /// own hosts, from this machine.
    fn valid_host(&self, headers: &HeaderMap, peer: Option<ClientIp>) -> bool {
        !self.0.stopping.load(Ordering::SeqCst)
            && !headers.contains_key(header::ORIGIN)
            && !headers.contains_key("sec-fetch-site")
            && headers.get(header::HOST).and_then(|v| v.to_str().ok()).is_some_and(|h| self.0.hosts.iter().any(|x| x == h))
            && peer.is_some_and(|ClientIp(ip)| ip == std::net::IpAddr::from([127, 0, 0, 1]))
    }

    /// `admission(req)`: the pool key, or the status refusing it.
    async fn admission(&self, headers: &HeaderMap, peer: Option<ClientIp>) -> Result<String, StatusCode> {
        if !self.valid_host(headers, peer) {
            return Err(StatusCode::FORBIDDEN);
        }
        let authorization = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).map(String::from);
        match (self.c().authorize)(authorization).await {
            Ok(Some(key)) if !key.is_empty() => Ok(key),
            Ok(_) => Err(StatusCode::UNAUTHORIZED),
            Err(_) => Err(StatusCode::SERVICE_UNAVAILABLE),
        }
    }

    /// A slot, or `None` while stopping or at the cap.
    fn slot(&self) -> Option<Slot> {
        if self.0.stopping.load(Ordering::SeqCst) {
            return None;
        }
        let max = self.c().max_active;
        let mut current = self.0.active.load(Ordering::SeqCst);
        loop {
            if current >= max {
                return None;
            }
            match self.0.active.compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(Slot(self.clone())),
                Err(now) => current = now,
            }
        }
    }

    /// The pool's request headers: JSON, the pool key, Codex's user agent, and a plain `session_id`.
    fn upstream_headers(&self, from: &HeaderMap, key: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Ok(k) = HeaderValue::from_str(key) {
            h.insert("x-api-key", k);
        }
        h.insert(header::USER_AGENT, HeaderValue::from_static("codex_cli_rs/0.155.1"));
        if let Some(id) = from.get("session_id").and_then(|v| v.to_str().ok())
            && (1..=128).contains(&id.len())
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            h.insert("session_id", HeaderValue::from_str(id).expect("checked"));
        }
        h
    }

    /// `whenListening(attempt)`: retries a refused connect, backing off from 250 ms to 2 s, until
    /// `connect_retry` has passed. Any other failure is final.
    async fn when_listening<T, E: std::error::Error + 'static, F: Future<Output = Result<T, E>>>(&self, mut attempt: impl FnMut() -> F, transport: &str) -> Result<T, E> {
        let deadline = Instant::now() + self.c().connect_retry;
        let mut waited = false;
        let mut delay = Duration::from_millis(250);
        let result = loop {
            match attempt().await {
                Ok(v) => break Ok(v),
                Err(e) => {
                    if !refused(&e) || self.0.stopping.load(Ordering::SeqCst) || Instant::now() + delay > deadline {
                        break Err(e);
                    }
                    if !waited {
                        waited = true;
                        self.0.counters.connect_waits.fetch_add(1, Ordering::SeqCst);
                        self.0.counters.waiting.fetch_add(1, Ordering::SeqCst);
                        self.emit("inference-upstream-wait", json!({ "transport": transport }));
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(2));
                }
            }
        };
        if waited {
            self.0.counters.waiting.fetch_sub(1, Ordering::SeqCst);
        }
        result
    }
}

async fn health(State(inf): State<Inf>, peer: Option<ClientIp>, headers: HeaderMap) -> Out {
    if !inf.valid_host(&headers, peer) {
        return reply(StatusCode::NOT_FOUND);
    }
    let c = &inf.0.counters;
    let doc = json!({
        "service": "agent-pools-inference", "pid": std::process::id(), "active": inf.active(),
        "httpRequests": c.http_requests.load(Ordering::SeqCst), "websocketRequests": c.websocket_requests.load(Ordering::SeqCst),
        "rejected": c.rejected.load(Ordering::SeqCst), "upstreamFailures": c.upstream_failures.load(Ordering::SeqCst),
        "connectWaits": c.connect_waits.load(Ordering::SeqCst), "waiting": c.waiting.load(Ordering::SeqCst),
    });
    let mut res = Response::new(Body::from(doc.to_string()));
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Out::Own(res)
}

/// The request body, at most `max` bytes: `Err(true)` past it, `Err(false)` when it broke off.
async fn read_limited(body: Body, max: usize) -> Result<Vec<u8>, bool> {
    let mut out = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(piece) = stream.next().await {
        let piece = piece.map_err(|_| false)?;
        if out.len() + piece.len() > max {
            return Err(true);
        }
        out.extend_from_slice(&piece);
    }
    Ok(out)
}

async fn http(State(inf): State<Inf>, peer: Option<ClientIp>, req: Request) -> Out {
    let (parts, body) = req.into_parts();
    // Exactly the two routes: a query could steer the pool elsewhere.
    if parts.uri.query().is_some() {
        return reply(StatusCode::NOT_FOUND);
    }
    let key = match inf.admission(&parts.headers, peer).await {
        Ok(k) => k,
        Err(status) => {
            inf.0.counters.rejected.fetch_add(1, Ordering::SeqCst);
            return reply(status);
        }
    };
    let Some(slot) = inf.slot() else { return reply(StatusCode::SERVICE_UNAVAILABLE) };
    let c = inf.c().clone();
    let failed = |inf: &Inf| {
        inf.0.counters.upstream_failures.fetch_add(1, Ordering::SeqCst);
        reply(StatusCode::BAD_GATEWAY)
    };
    let mut body = match tokio::time::timeout(c.header_timeout, read_limited(body, c.max_body_bytes)).await {
        Ok(Ok(b)) => b,
        Ok(Err(true)) => return reply(StatusCode::PAYLOAD_TOO_LARGE),
        Ok(Err(false)) | Err(_) => return failed(&inf),
    };
    let encoding = parts.headers.get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()).unwrap_or("identity").trim().to_ascii_lowercase();
    if encoding == "zstd" {
        let decoded = zstd::stream::read::Decoder::new(&body[..]).and_then(|d| {
            let mut out = Vec::new();
            d.take(c.max_body_bytes as u64 + 1).read_to_end(&mut out).map(|_| out)
        });
        match decoded {
            Ok(d) if d.len() > c.max_body_bytes => return reply(StatusCode::PAYLOAD_TOO_LARGE),
            Ok(d) => body = d,
            Err(_) => return reply(StatusCode::BAD_REQUEST),
        }
    } else if encoding != "identity" && !encoding.is_empty() {
        return reply(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    if !valid_body(&body) {
        return reply(StatusCode::BAD_REQUEST);
    }
    let deadline = tokio::time::Instant::now() + c.stream_timeout;
    let route = parts.uri.path().to_string();
    inf.0.counters.http_requests.fetch_add(1, Ordering::SeqCst);
    inf.emit("inference-request", json!({ "transport": "http", "route": route }));
    let url = format!("http://{}{route}", inf.0.pool);
    let headers = inf.upstream_headers(&parts.headers, &key);
    let send = inf.when_listening(|| inf.0.http.post(&url).headers(headers.clone()).body(body.clone()).send(), "http");
    let upstream = match tokio::time::timeout_at(deadline, send).await {
        Ok(Ok(res)) if !res.status().is_redirection() => res,
        _ => return failed(&inf),
    };
    let mut out = HeaderMap::new();
    let kind = upstream.headers().get(header::CONTENT_TYPE).cloned().unwrap_or(HeaderValue::from_static("application/json"));
    out.insert(header::CONTENT_TYPE, kind);
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(r) = upstream.headers().get(header::RETRY_AFTER) {
        out.insert(header::RETRY_AFTER, r.clone());
    }
    let status = upstream.status();
    let mut pieces = upstream.bytes_stream();
    let g = inf.clone();
    let stream = crate::async_stream(move |send: tokio::sync::mpsc::Sender<Result<corten::axum::body::Bytes, std::io::Error>>| async move {
        // Held while the answer streams: released when it ends or the client leaves.
        let _slot = slot;
        loop {
            match tokio::time::timeout_at(deadline, pieces.next()).await {
                Ok(Some(Ok(piece))) => {
                    if send.send(Ok(piece)).await.is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Ok(Some(Err(_))) | Err(_) => {
                    g.0.counters.upstream_failures.fetch_add(1, Ordering::SeqCst);
                    let _ = send.send(Err(std::io::Error::other("the pool's stream ended early"))).await;
                    return;
                }
            }
        }
    });
    Out::Relay(Relay::new(status, out, Body::from_stream(stream)))
}

async fn socket(State(inf): State<Inf>, peer: Option<ClientIp>, headers: HeaderMap, uri: corten::axum::http::Uri, upgrade: Upgrade) -> Out {
    let key = match inf.admission(&headers, peer).await {
        Ok(k) => k,
        Err(status) => {
            inf.0.counters.rejected.fetch_add(1, Ordering::SeqCst);
            return reply(status);
        }
    };
    if uri.query().is_some() {
        inf.0.counters.rejected.fetch_add(1, Ordering::SeqCst);
        return reply(StatusCode::NOT_FOUND);
    }
    let Some(slot) = inf.slot() else {
        inf.0.counters.rejected.fetch_add(1, Ordering::SeqCst);
        return reply(StatusCode::SERVICE_UNAVAILABLE);
    };
    let g = inf.clone();
    // The slot moves into the work: a handshake that never switches drops it, and so frees it.
    match upgrade.websocket(move |stream, signal| async move {
        let _slot = slot;
        tokio::select! {
            () = relay_socket(g, stream, headers, peer, key) => {}
            _ = signal.aborted() => {}
        }
    }) {
        Ok(switch) => Out::Switch(switch),
        Err(e) => Out::Own(corten::axum::response::IntoResponse::into_response(e)),
    }
}

fn ws_config(max: usize) -> WebSocketConfig {
    WebSocketConfig::default().max_message_size(Some(max)).max_frame_size(Some(max)).max_write_buffer_size(max.max(1) + 128 * 1024).write_buffer_size(0)
}

/// One socket's frames both ways, until either side ends, a frame is refused, or the socket has
/// been idle for `stream_timeout`. Returning drops both connections (`drop()`).
async fn relay_socket(inf: Inf, stream: corten::upgrade::Stream, headers: HeaderMap, peer: Option<ClientIp>, key: String) {
    let c = inf.c().clone();
    let client = tokio_tungstenite::WebSocketStream::from_raw_socket(stream, Role::Server, Some(ws_config(c.max_body_bytes))).await;
    let (mut client_tx, mut client_rx) = client.split();
    let touched = Arc::new(std::sync::Mutex::new(tokio::time::Instant::now()));
    let touch = || *touched.lock().unwrap() = tokio::time::Instant::now();
    let (sink_tx, sink_rx) = tokio::sync::oneshot::channel();
    let (up_tx, up_rx) = tokio::sync::oneshot::channel();
    let queued_bytes = Arc::new(AtomicI64::new(0));
    let queued_frames = Arc::new(AtomicI64::new(0));
    let (queue_tx, mut queue_rx) = tokio::sync::mpsc::unbounded_channel::<tungstenite::Utf8Bytes>();

    // `opened`: the pool's socket, waiting for it to listen while it restarts.
    let connect = async {
        let url = format!("ws://{}/v1/responses", inf.0.pool);
        let upstream_headers = inf.upstream_headers(&headers, &key);
        let attempt = || {
            let (url, h, cfg) = (url.clone(), upstream_headers.clone(), ws_config(c.max_body_bytes));
            async move {
                let mut req = url.into_client_request()?;
                req.headers_mut().extend(h);
                let open = tokio_tungstenite::connect_async_with_config(req, Some(cfg), true);
                match tokio::time::timeout(c.header_timeout, open).await {
                    Ok(r) => r.map(|(ws, _)| ws),
                    Err(_) => Err(tungstenite::Error::Io(std::io::Error::other("handshake timed out"))),
                }
            }
        };
        match inf.when_listening(attempt, "websocket").await {
            Ok(ws) => {
                let (tx, rx) = ws.split();
                let _ = sink_tx.send(tx);
                let _ = up_tx.send(rx);
                std::future::pending::<()>().await;
            }
            Err(_) => {
                inf.0.counters.upstream_failures.fetch_add(1, Ordering::SeqCst);
            }
        }
    };

    // The caller's frames: text only, at most 128 waiting and `max_body_bytes` in all.
    let read = async {
        while let Some(message) = client_rx.next().await {
            let text = match message {
                Ok(Message::Text(t)) => t,
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                _ => return,
            };
            let bytes = queued_bytes.fetch_add(text.len() as i64, Ordering::SeqCst) + text.len() as i64;
            let frames = queued_frames.fetch_add(1, Ordering::SeqCst) + 1;
            if bytes > c.max_body_bytes as i64 || frames > 128 || queue_tx.send(text).is_err() {
                return;
            }
        }
    };

    // Each frame admitted again (sign-out or a new key ends the socket), then sent on in order.
    let send = async {
        let mut sink_rx = Some(sink_rx);
        let mut sink = None;
        while let Some(text) = queue_rx.recv().await {
            match inf.admission(&headers, peer).await {
                Ok(current) if current == key => {}
                _ => {
                    inf.0.counters.rejected.fetch_add(1, Ordering::SeqCst);
                    return;
                }
            }
            if sink.is_none() {
                match sink_rx.take() {
                    Some(rx) => match rx.await {
                        Ok(s) => sink = Some(s),
                        Err(_) => return,
                    },
                    None => return,
                }
            }
            if !valid_body(text.as_bytes()) {
                return;
            }
            touch();
            inf.0.counters.websocket_requests.fetch_add(1, Ordering::SeqCst);
            inf.emit("inference-request", json!({ "transport": "websocket", "route": "/v1/responses" }));
            let len = text.len() as i64;
            let Some(s) = sink.as_mut() else { return };
            if s.send(Message::Text(text)).await.is_err() {
                inf.0.counters.upstream_failures.fetch_add(1, Ordering::SeqCst);
                return;
            }
            queued_bytes.fetch_sub(len, Ordering::SeqCst);
            queued_frames.fetch_sub(1, Ordering::SeqCst);
        }
    };

    // The pool's frames back to the caller.
    let back = async {
        let Ok(mut up) = up_rx.await else { return std::future::pending::<()>().await };
        while let Some(message) = up.next().await {
            let message = match message {
                Ok(m @ (Message::Text(_) | Message::Binary(_))) => m,
                Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => continue,
                Ok(Message::Close(_)) => return,
                Err(_) => {
                    inf.0.counters.upstream_failures.fetch_add(1, Ordering::SeqCst);
                    return;
                }
            };
            touch();
            if client_tx.send(message).await.is_err() {
                return;
            }
        }
    };

    let idle = async {
        loop {
            let at = *touched.lock().unwrap() + c.stream_timeout;
            if tokio::time::Instant::now() >= at {
                return;
            }
            tokio::time::sleep_until(at).await;
        }
    };

    tokio::select! {
        () = connect => {}
        () = read => {}
        () = send => {}
        () = back => {}
        () = idle => {}
    }
}

/// inference-auth.mjs `sameSecret`: both non-empty and equal, compared as SHA-256 digests in
/// constant time.
pub fn same_secret(a: &str, b: &str) -> bool {
    use sha2::{Digest, Sha256};
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let (x, y) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    x.iter().zip(y.iter()).fold(0u8, |acc, (p, q)| acc | (p ^ q)) == 0
}

fn read_json(path: &std::path::Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())
}

/// inference-auth.mjs `fileCredentials(authFile, secretsFile)`: the caller must present Codex's
/// own current sign-in (`auth.json`: a ChatGPT access token, or an API key); the pool key is
/// `clientKey` from `secrets`. Both are read on every check, never cached.
pub fn file_credentials(auth_file: std::path::PathBuf, secrets_file: std::path::PathBuf) -> Authorize {
    Arc::new(move |authorization: Option<String>| {
        let (auth_file, secrets_file) = (auth_file.clone(), secrets_file.clone());
        Box::pin(async move {
            tokio::task::spawn_blocking(move || -> Result<Option<String>, String> {
                let auth = read_json(&auth_file)?;
                let mode = auth.get("auth_mode").and_then(Value::as_str);
                let api_key = auth.get("OPENAI_API_KEY").and_then(Value::as_str).filter(|k| !k.is_empty());
                let token = match mode {
                    Some("chatgpt") => auth.pointer("/tokens/access_token").and_then(Value::as_str),
                    Some("apikey") => api_key,
                    None => api_key,
                    Some(_) => None,
                };
                let Some(token) = token.filter(|t| !t.is_empty()) else { return Ok(None) };
                if !same_secret(authorization.as_deref().unwrap_or(""), &format!("Bearer {token}")) {
                    return Ok(None);
                }
                match read_json(&secrets_file)?.get("clientKey").and_then(Value::as_str) {
                    Some(k) if !k.is_empty() => Ok(Some(k.to_string())),
                    _ => Err("Pool credential unavailable".into()),
                }
            })
            .await
            .map_err(|e| e.to_string())?
        })
    })
}
