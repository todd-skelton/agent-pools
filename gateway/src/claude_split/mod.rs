//! Claude's first-party control identity with pooled inference. The authenticated loopback
//! CONNECT listener and decrypted API are Corten apps; only api.anthropic.com is intercepted.
//! Control credentials/bodies and response streams pass through unchanged. Pooled requests
//! replace auth; the opt-in catalog also resolves explicit model/effort pairs and adds current
//! runtime identity to the system prompt without changing signed conversation blocks.
pub mod policy;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use base64::Engine;
use corten::axum::{
    Router,
    body::{Body, to_bytes},
    extract::{FromRequestParts, Request},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use corten::{App, Middleware, Next, Relay, upgrade::Upgrade};
use futures_util::{StreamExt, stream};
use hyper_util::rt::TokioIo;
use policy::Route;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::{
    net::TcpStream,
    sync::{Semaphore, watch},
};
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;

pub const MAX_BODY: usize = 32 * 1024 * 1024;
pub type Observe = Arc<dyn Fn(Value) + Send + Sync>;

/// No control credentials are stored here: each request supplies its own first-party login.
#[derive(Clone)]
pub struct Config {
    pub pool_origin: String,
    pub pool_key: String,
    pub proxy_key: String,
    pub control_origin: String,
    pub tls: TlsAcceptor,
    /// Unix seconds; health and new CONNECTs fail closed after expiration.
    pub certificate_expires_at: i64,
    pub additional_tunnel_hosts: Vec<String>,
    pub max_body_bytes: usize,
    pub header_timeout: Duration,
    pub idle_timeout: Duration,
    pub observe: Observe,
    /// Additional trust for offline fixture upstreams. Verification is never disabled.
    pub upstream_ca: Option<reqwest::Certificate>,
    /// Explicit opt-in: identity metadata from the sanitized pool model catalog.
    pub model_catalog: Option<crate::model_catalog::Catalog>,
}

impl Config {
    pub fn new(pool_origin: &str, pool_key: &str, proxy_key: &str, tls: TlsAcceptor, certificate_expires_at: i64) -> Self {
        Self {
            pool_origin: pool_origin.into(),
            pool_key: pool_key.into(),
            proxy_key: proxy_key.into(),
            tls,
            certificate_expires_at,
            control_origin: "https://api.anthropic.com".into(),
            additional_tunnel_hosts: Vec::new(),
            max_body_bytes: MAX_BODY,
            header_timeout: Duration::from_secs(180),
            idle_timeout: Duration::from_secs(300),
            observe: Arc::new(|_| {}),
            upstream_ca: None,
            model_catalog: None,
        }
    }
}

struct Shared {
    cfg: Config,
    http: reqwest::Client,
    pool: String,
    control: String,
    auth_hash: [u8; 32],
    tls: RwLock<(TlsAcceptor, i64)>,
    stop: watch::Sender<bool>,
    connections: Arc<Semaphore>,
    active: AtomicUsize,
    instance: String,
}

#[derive(Clone)]
pub struct Handle(Arc<Shared>);

impl Handle {
    pub fn instance_id(&self) -> &str {
        &self.0.instance
    }
    pub fn shutdown(&self) {
        self.0.stop.send_replace(true);
    }
    pub async fn stopped(&self) {
        stopped(self.0.stop.subscribe()).await;
    }
    pub fn replace_tls(&self, tls: TlsAcceptor, expires_at: i64) {
        *self.0.tls.write().unwrap_or_else(|e| e.into_inner()) = (tls, expires_at);
    }
}

#[derive(Clone)]
pub struct Proxy {
    shared: Arc<Shared>,
    decrypted: Router,
}
#[derive(Clone)]
struct Decrypted(Arc<Shared>);

fn origin(raw: &str) -> Result<String, String> {
    let u = reqwest::Url::parse(raw).map_err(|_| "Invalid splitter upstream origin")?;
    let loopback = u
        .host_str()
        .is_some_and(|h| h == "localhost" || h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok_and(|a| a.is_loopback()));
    if !u.username().is_empty()
        || u.password().is_some()
        || u.path() != "/"
        || u.query().is_some()
        || u.fragment().is_some()
        || !(u.scheme() == "https" || (u.scheme() == "http" && loopback))
    {
        return Err("Use HTTPS or loopback HTTP origins without credentials, paths or queries".into());
    }
    Ok(u.origin().ascii_serialization())
}

/// The caller binds loopback only. The transport can also be run on a remote *client*
/// machine with an HTTPS public pool origin and that machine's registered key.
pub fn app(cfg: Config) -> Result<(App<Proxy>, Handle), String> {
    if let Some(catalog) = &cfg.model_catalog { catalog.validate().map_err(str::to_string)?; }
    if cfg.pool_key.is_empty() || cfg.proxy_key.len() < 32 || cfg.pool_key == cfg.proxy_key {
        return Err("Independent pool and local proxy credentials required".into());
    }
    let pool = origin(&cfg.pool_origin)?;
    let control = origin(&cfg.control_origin)?;
    if cfg.additional_tunnel_hosts.iter().any(|h| !policy::dns_name(h)) {
        return Err("Additional tunnel hosts must be exact lowercase DNS names".into());
    }
    let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("probe:{}", cfg.proxy_key)));
    let mut http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(cfg.idle_timeout);
    if let Some(ca) = &cfg.upstream_ca {
        http = http.add_root_certificate(ca.clone());
    }
    let http = http.build().map_err(|_| "Could not initialize verified upstream TLS")?;
    let (stop, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        tls: RwLock::new((cfg.tls.clone(), cfg.certificate_expires_at)),
        cfg,
        http,
        pool,
        control,
        auth_hash: Sha256::digest(auth.as_bytes()).into(),
        stop,
        connections: Arc::new(Semaphore::new(128)),
        active: AtomicUsize::new(0),
        instance: format!("{:x}", Sha256::digest(format!("{}:{}", std::process::id(), crate::now_ms()).as_bytes())),
    });
    let decrypted = App::new(corten::Routes::new())
        .policy(corten::security::Policy::new().host("api.anthropic.com"))
        .deadline(None)
        .middleware(Decrypted(shared.clone()))
        .router();
    let mut policy = corten::security::Policy::new();
    for d in policy::TUNNEL_DOMAINS {
        policy = policy.host(&format!(".{d}"));
    }
    for d in &shared.cfg.additional_tunnel_hosts {
        policy = policy.host(d);
    }
    let app = App::new(corten::Routes::new())
        .policy(policy)
        .deadline(None)
        // No head timeout on the CONNECT listener: Corten's clock keeps only a 101 busy, so it
        // counts a tunnel's 200 as an answered request and cut every tunnel 30 s after it opened,
        // however busy (corten 3b19711, watch.rs). The listener is loopback and authenticated,
        // max_connections bounds it, and the decrypted connection inside each tunnel keeps its
        // own header read timeout.
        .header_timeout(None)
        .max_connections(256)
        .middleware(Proxy {
            shared: shared.clone(),
            decrypted,
        });
    Ok((app, Handle(shared)))
}

async fn stopped(mut rx: watch::Receiver<bool>) {
    if *rx.borrow() {
        return;
    }
    let _ = rx.wait_for(|s| *s).await;
}

fn emit(s: &Shared, event: Value) {
    // Observability must not interrupt transport or disclose a request/URL/credential.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (s.cfg.observe)(event)));
}

fn refuse(status: u16, message: &'static str) -> Response {
    let mut res = Response::new(Body::from(
        json!({"type":"error","error":{"type":"splitter_error","message":message}}).to_string(),
    ));
    *res.status_mut() = StatusCode::from_u16(status).unwrap();
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

impl Middleware for Proxy {
    async fn call(&self, mut req: Request, _next: impl Next) -> Response {
        let received = req.headers().get(header::PROXY_AUTHORIZATION).map(|h| h.as_bytes()).unwrap_or_default();
        let hash: [u8; 32] = Sha256::digest(received).into();
        if !bool::from(hash.ct_eq(&self.shared.auth_hash)) {
            let mut r = refuse(407, "Local proxy authentication required");
            r.headers_mut()
                .insert(header::PROXY_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Claude splitter\""));
            return r;
        }
        if req.method() == Method::GET && req.uri().path() == "/health" {
            let exp = self.shared.tls.read().unwrap_or_else(|e| e.into_inner()).1;
            let mut res = Response::new(Body::from(json!({"schema":"claude-splitter/v1","service":"agent-pools-claude-split","instanceId":self.shared.instance,
                "poolOrigin":self.shared.pool,"certificateExpiresAt":exp,"certificateValid":exp > (crate::now_ms()/1000.0) as i64,"activeRequests":self.shared.active.load(Ordering::Relaxed)}).to_string()));
            res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return res;
        }
        if req.method() == Method::POST
            && req.uri().path() == "/shutdown"
            && req
                .headers()
                .get("x-splitter-instance")
                .is_some_and(|v| v.as_bytes() == self.shared.instance.as_bytes())
        {
            let stop = self.shared.stop.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(25)).await;
                stop.send_replace(true);
            });
            return Response::new(Body::from("{\"stopping\":true}"));
        }
        if req.method() != Method::CONNECT {
            return refuse(405, "CONNECT only");
        }
        let authority = req.uri().authority().map(|a| a.as_str()).unwrap_or_default();
        let Some(host) = authority
            .strip_suffix(":443")
            .map(str::to_ascii_lowercase)
            .filter(|h| policy::tunnel_host(h, &self.shared.cfg.additional_tunnel_hosts))
        else {
            return refuse(403, "Tunnel host or port not permitted");
        };
        if req.uri().scheme().is_some() || req.uri().path_and_query().is_some() {
            return refuse(400, "CONNECT requires an authority");
        }
        let permit = match self.shared.connections.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => return refuse(503, "Too many active tunnels"),
        };
        let tls = self.shared.tls.read().unwrap_or_else(|e| e.into_inner()).clone();
        if host == "api.anthropic.com" && tls.1 <= (crate::now_ms() / 1000.0) as i64 {
            return refuse(503, "Splitter certificate expired; renew before reconnecting");
        }
        let mut remote = if host != "api.anthropic.com" {
            match tokio::time::timeout(Duration::from_secs(15), TcpStream::connect((host.as_str(), 443))).await {
                Ok(Ok(stream)) => Some(stream),
                _ => return refuse(502, "Tunnel upstream unavailable"),
            }
        } else {
            None
        };
        let on = hyper::upgrade::on(&mut req);
        let (router, shared) = (self.decrypted.clone(), self.shared.clone());
        tokio::spawn(async move {
            let _permit = permit;
            let connected = async {
                let Ok(Ok(upgraded)) = tokio::time::timeout(Duration::from_secs(10), on).await else {
                    return;
                };
                let mut io = TokioIo::new(upgraded);
                if let Some(ref mut remote) = remote {
                    let _ = tokio::io::copy_bidirectional(&mut io, remote).await;
                } else {
                    let Ok(Ok(tls)) = tokio::time::timeout(Duration::from_secs(10), tls.0.accept(io)).await else {
                        return;
                    };
                    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                        let router = router.clone();
                        async move { router.oneshot(req.map(Body::new)).await }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .timer(hyper_util::rt::TokioTimer::new())
                        .header_read_timeout(Duration::from_secs(30))
                        .max_buf_size(64 * 1024)
                        .serve_connection(TokioIo::new(tls), service)
                        .with_upgrades()
                        .await;
                }
            };
            tokio::select! { _ = connected => {}, _ = stopped(shared.stop.subscribe()) => {} }
        });
        // Successful CONNECT must have neither a content length nor a transfer encoding.
        Response::new(Body::empty())
    }
}

const HOP: &[&str] = &[
    "connection",
    "proxy-authorization",
    "proxy-authenticate",
    "proxy-connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "host",
];
fn clean(h: &HeaderMap) -> HeaderMap {
    let mut out = h.clone();
    for name in h.get_all(header::CONNECTION).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')) {
        out.remove(name.trim());
    }
    for h in HOP {
        out.remove(*h);
    }
    out
}

impl Middleware for Decrypted {
    async fn call(&self, req: Request, _next: impl Next) -> Response {
        let s = &self.0;
        let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or_default();
        if !(host.eq_ignore_ascii_case("api.anthropic.com") || host.eq_ignore_ascii_case("api.anthropic.com:443"))
            || req.uri().scheme().is_some()
            || req.uri().authority().is_some()
            || !policy::canonical_path(req.uri().path())
        {
            return refuse(400, "Expected a canonical api.anthropic.com origin-form request");
        }
        let route = policy::classify(req.method(), req.uri().path());
        if route == Route::Unsupported {
            let method = match req.method().as_str() {
                "GET" => "GET", "POST" => "POST", "PUT" => "PUT", "DELETE" => "DELETE",
                "PATCH" => "PATCH", "HEAD" => "HEAD", "OPTIONS" => "OPTIONS", _ => "OTHER",
            };
            emit(s, json!({"route":"unsupported","status":501,"method":method,"endpoint":policy::diagnostic_endpoint(req.uri().path()),"endpointTags":policy::diagnostic_tags(req.uri().path(), &s.cfg.proxy_key)}));
            return refuse(501, "Unsupported API route; no control-account inference fallback");
        }
        let target = format!(
            "{}{}",
            if route == Route::Pool { &s.pool } else { &s.control },
            req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/")
        );
        if req.headers().contains_key(header::UPGRADE) {
            if route != Route::Control
                || req.method() != Method::GET
                || !req
                    .headers()
                    .get(header::UPGRADE)
                    .is_some_and(|h| h.as_bytes().eq_ignore_ascii_case(b"websocket"))
            {
                return refuse(501, "Only control WebSockets are supported");
            }
            return websocket(s.clone(), req, target).await;
        }
        let (parts, body) = req.into_parts();
        let mut body = match tokio::time::timeout(Duration::from_secs(120), to_bytes(body, s.cfg.max_body_bytes)).await {
            Ok(Ok(b)) => b,
            Ok(Err(_)) => return refuse(413, "Request exceeds splitter body limit"),
            Err(_) => return refuse(408, "Request body timed out"),
        };
        let mut identity = None;
        let mut rewritten = false;
        if route == Route::Pool && parts.method == Method::POST {
            let encoding = parts.headers.get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()).unwrap_or("identity");
            let mut parsed = match policy::decode(&body, encoding, s.cfg.max_body_bytes) {
                Ok(p) => p,
                Err((code, message)) => return refuse(code, message),
            };
            if let Some(catalog) = &s.cfg.model_catalog {
                identity = match catalog.annotate(&mut parsed) {
                    Ok(i) => i,
                    Err(message) => return refuse(422, message),
                };
                if identity.is_some() {
                    let encoded = serde_json::to_vec(&parsed).unwrap_or_default();
                    if encoded.len() > s.cfg.max_body_bytes { return refuse(413, "Identity metadata exceeds splitter body limit"); }
                    body = encoded.into();
                    rewritten = true;
                }
            }
        }
        let mut headers = clean(&parts.headers);
        if rewritten {
            headers.remove(header::CONTENT_ENCODING);
            headers.remove(header::CONTENT_LENGTH);
        }
        if route == Route::Pool {
            for name in ["authorization", "x-api-key", "cookie", "x-forwarded-for", "x-real-ip"] {
                headers.remove(name);
            }
            let Ok(auth) = HeaderValue::from_str(&format!("Bearer {}", s.cfg.pool_key)) else {
                return refuse(503, "Pool credential invalid");
            };
            headers.insert(header::AUTHORIZATION, auth);
            headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        }
        let mut tracker = Tracker::new(s.clone(), route, policy::control_endpoint(&parts.method, parts.uri.path()));
        tracker.identity = identity;
        // Exactly one upstream attempt. Account failover is the pool's responsibility.
        let sent = s.http.request(parts.method, target).headers(headers).body(body).send();
        let response = match tokio::time::timeout(s.cfg.header_timeout, sent).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => {
                tracker.fail(502, "UPSTREAM_UNAVAILABLE");
                return refuse(502, "Splitter upstream unavailable; no fallback or replay attempted");
            }
            Err(_) => {
                tracker.fail(504, "HEADER_TIMEOUT");
                return refuse(504, "Splitter upstream timed out; no fallback or replay attempted");
            }
        };
        let status = response.status();
        let mut headers = clean(response.headers());
        if route == Route::Pool {
            headers.remove(header::SET_COOKIE);
        }
        let sse = route == Route::Pool
            && status == StatusCode::OK
            && parts.uri.path() == "/v1/messages"
            && headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("text/event-stream"));
        let tracker = tracker.response(status.as_u16(), sse);
        let out = stream::unfold((response.bytes_stream(), tracker, false), |(mut upstream, mut tracker, ended)| async move {
            if ended {
                return None;
            }
            match upstream.next().await {
                Some(Ok(chunk)) => {
                    tracker.observe(&chunk);
                    Some((Ok::<_, std::io::Error>(chunk), (upstream, tracker, false)))
                }
                Some(Err(_)) => {
                    tracker.finish(false, Some("UPSTREAM_STREAM_ERROR"));
                    Some((Err(std::io::Error::other("Splitter upstream stream interrupted")), (upstream, tracker, true)))
                }
                None => {
                    tracker.observe(b"\n");
                    let valid = !tracker.sse || tracker.terminal;
                    tracker.finish(
                        valid && !tracker.api_error,
                        if tracker.api_error {
                            Some("SSE_API_ERROR")
                        } else if !valid {
                            Some("INCOMPLETE_SSE")
                        } else {
                            None
                        },
                    );
                    if valid || tracker.api_error {
                        None
                    } else {
                        Some((Err(std::io::Error::other("Missing message_stop event")), (upstream, tracker, true)))
                    }
                }
            }
        });
        Relay::new(status, headers, Body::from_stream(out)).into_response()
    }
}

struct Tracker {
    shared: Arc<Shared>,
    route: Route,
    endpoint: Option<&'static str>,
    started: Instant,
    status: u16,
    sse: bool,
    terminal: bool,
    api_error: bool,
    line: Vec<u8>,
    reported: bool,
    identity: Option<Value>,
}
impl Tracker {
    fn new(shared: Arc<Shared>, route: Route, endpoint: Option<&'static str>) -> Self {
        shared.active.fetch_add(1, Ordering::Relaxed);
        Self {
            shared,
            route,
            endpoint,
            started: Instant::now(),
            status: 502,
            sse: false,
            terminal: false,
            api_error: false,
            line: Vec::new(),
            reported: false,
            identity: None,
        }
    }
    fn response(mut self, status: u16, sse: bool) -> Self {
        self.status = status;
        self.sse = sse;
        self
    }
    fn fail(mut self, status: u16, error: &str) {
        self.status = status;
        self.finish(false, Some(error));
    }
    fn observe(&mut self, bytes: &[u8]) {
        if !self.sse {
            return;
        }
        for &b in bytes {
            if b != b'\n' {
                if self.line.len() < 1024 * 1024 {
                    self.line.push(b);
                }
                continue;
            }
            let line = String::from_utf8_lossy(&self.line);
            if let Some(event) = line.strip_prefix("event:").map(str::trim) {
                self.terminal |= event == "message_stop";
                self.api_error |= event == "error";
            }
            if let Some(data) = line.strip_prefix("data:").and_then(|v| serde_json::from_str::<Value>(v).ok()) {
                self.terminal |= data.get("type").and_then(Value::as_str) == Some("message_stop");
                self.api_error |= data.get("type").and_then(Value::as_str) == Some("error");
            }
            self.line.clear();
        }
    }
    fn finish(&mut self, complete: bool, error: Option<&str>) {
        if self.reported {
            return;
        }
        self.reported = true;
        emit(
            &self.shared,
            json!({"route":if self.route == Route::Pool {"pool-inference"} else {"anthropic-control"},"endpoint":self.endpoint,"status":self.status,"complete":complete,"elapsedMs":self.started.elapsed().as_millis() as u64,"error":error,"requestedIdentity":self.identity}),
        );
    }
}
impl Drop for Tracker {
    fn drop(&mut self) {
        self.finish(false, Some("CLIENT_CANCELLED"));
        self.shared.active.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn websocket(s: Arc<Shared>, req: Request, target: String) -> Response {
    let (mut parts, _) = req.into_parts();
    let upgrade = match Upgrade::from_request_parts(&mut parts, &()).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let mut headers = clean(&parts.headers);
    headers.remove(header::CONTENT_LENGTH);
    headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    let response = match tokio::time::timeout(s.cfg.header_timeout, s.http.get(target).headers(headers).send()).await {
        Ok(Ok(r)) => r,
        _ => return refuse(502, "Control WebSocket upstream unavailable"),
    };
    let status = response.status();
    let headers = clean(response.headers());
    if status != StatusCode::SWITCHING_PROTOCOLS {
        return Relay::new(status, headers, Body::from_stream(response.bytes_stream())).into_response();
    }
    let Ok(mut remote) = response.upgrade().await else {
        return refuse(502, "Control WebSocket upgrade failed");
    };
    let mut headers = headers;
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    upgrade
        .switch(headers, move |mut local, signal| async move {
            tokio::select! {
                _ = tokio::io::copy_bidirectional(&mut local, &mut remote) => {},
                _ = signal.aborted() => {},
                _ = stopped(s.stop.subscribe()) => {},
            }
            emit(&s, json!({"route":"anthropic-control-upgrade","status":101}));
        })
        .into_response()
}

/// Corten's server owns the listener and HTTP limits; the handle also stops upgraded sockets.
pub async fn serve(listener: tokio::net::TcpListener, config: Config) -> Result<(), std::io::Error> {
    if !listener.local_addr()?.ip().is_loopback() {
        return Err(std::io::Error::other("Claude splitter must bind loopback only"));
    }
    let (app, handle) = app(config).map_err(std::io::Error::other)?;
    tokio::select! { r = corten::serve_on(vec![listener], app) => r, _ = handle.stopped() => Ok(()) }
}
