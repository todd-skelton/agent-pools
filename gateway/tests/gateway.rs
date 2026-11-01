//! public-gateway.test.mjs (Agent Pools 5c696f6) on the Corten gateway, over real sockets: each
//! case starts recording upstreams (the pool and the service) and the gateway on loopback.
//! Where a status differs by design (README, `lib.rs`), the case says so.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use corten::axum::{self, Router, body::Body, extract::Request, response::Response};
use pools_gateway::{Client, Config, app, sha256};
use serde_json::{Value, json};

const HOST: &str = "pool.example.test";
const ORIGIN: &str = "https://pool.example.test";
const MACHINE_KEY: &str = "ap-machine-fixture-key";
const POOL_KEY: &str = "pool-fixture-key";

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: axum::http::HeaderMap,
    body: String,
}

type Handler = Arc<dyn Fn(&Seen) -> Response + Send + Sync>;

struct Upstream {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

async fn upstream(handler: Handler) -> Upstream {
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let s = seen.clone();
    let router = Router::new().fallback(move |req: Request| {
        let (s, handler) = (s.clone(), handler.clone());
        async move {
            let (parts, body) = req.into_parts();
            let body = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
            let seen = Seen {
                path: parts.uri.path_and_query().map(|p| p.to_string()).unwrap_or_default(),
                headers: parts.headers,
                body: String::from_utf8_lossy(&body).into_owned(),
            };
            s.lock().unwrap().push(seen.clone());
            handler(&seen)
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Upstream { url, seen }
}

struct Fixture {
    pool: Upstream,
    service: Upstream,
    events: Arc<Mutex<Vec<(String, Value)>>>,
    clients: Arc<Mutex<Vec<Client>>>,
    clock: Arc<AtomicU64>,
    port: u16,
    http: reqwest::Client,
}

fn sse_pool() -> Handler {
    Arc::new(|_| {
        Response::builder()
            .header("content-type", "text/event-stream")
            .header("set-cookie", "private=1")
            .header("connection", "close")
            .body(Body::from("data: one\n\ndata: two\n\n"))
            .unwrap()
    })
}

fn json_service() -> Handler {
    Arc::new(|s| Response::builder().header("content-type", "application/json").body(Body::from(serde_json::json!({ "path": s.path }).to_string())).unwrap())
}

async fn fixture_with(pool: Handler, service: Handler, tune: impl FnOnce(&mut Config)) -> Fixture {
    let (pool, service) = (upstream(pool).await, upstream(service).await);
    let events: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let clients = Arc::new(Mutex::new(vec![Client { name: "work-pc".into(), key_sha256: sha256(MACHINE_KEY) }]));
    let clock = Arc::new(AtomicU64::new(0));
    let mut config = Config::new(HOST, &pool.url, &service.url);
    let (c, e, k) = (clients.clone(), events.clone(), clock.clone());
    config.clients = Arc::new(move || c.lock().unwrap().clone());
    config.pool_key = Arc::new(|| Some(POOL_KEY.into()));
    config.session_secret = Arc::new(|| Ok("s".repeat(40)));
    config.observe = Arc::new(move |kind, data| e.lock().unwrap().push((kind.to_string(), data)));
    // A clock of 0 is the real one.
    config.now = Arc::new(move || match k.load(Ordering::SeqCst) {
        0 => pools_gateway::now_ms(),
        t => t as f64,
    });
    tune(&mut config);
    let app = app(config).expect("a valid config");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { corten::serve_on(vec![listener], app).await.unwrap() });
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy().build().unwrap();
    Fixture { pool, service, events, clients, clock, port, http }
}

async fn fixture() -> Fixture {
    fixture_with(sse_pool(), json_service(), |_| {}).await
}

struct Answer {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: String,
}

impl Fixture {
    async fn call(&self, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&str>) -> Answer {
        // One value per name: a case's own `host` replaces the public one.
        let mut map = reqwest::header::HeaderMap::new();
        map.insert("host", HOST.parse().unwrap());
        for (k, v) in headers {
            map.insert(reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        let mut req = self.http.request(method.parse().unwrap(), format!("http://127.0.0.1:{}{path}", self.port)).headers(map);
        if let Some(b) = body {
            req = req.body(b.to_string());
        }
        let res = req.send().await.unwrap();
        let (status, headers) = (res.status().as_u16(), res.headers().clone());
        Answer { status, headers, body: res.text().await.unwrap_or_default() }
    }

    async fn login(&self) -> String {
        let body = format!("key={MACHINE_KEY}");
        let r = self.call("POST", "/login", &[("origin", ORIGIN), ("content-type", "application/x-www-form-urlencoded")], Some(&body)).await;
        assert_eq!(r.status, 303, "{}", r.body);
        r.headers["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string()
    }

    fn pool_seen(&self) -> Vec<Seen> {
        self.pool.seen.lock().unwrap().clone()
    }

    fn service_seen(&self) -> Vec<Seen> {
        self.service.seen.lock().unwrap().clone()
    }

    fn events(&self, kind: &str) -> Vec<Value> {
        self.events.lock().unwrap().iter().filter(|(k, _)| k == kind).map(|(_, v)| v.clone()).collect()
    }
}

/// Waits up to a second for `check`.
async fn eventually(mut check: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test]
async fn inference_with_a_machine_key_reaches_the_pool_with_the_pool_key_only() {
    let f = fixture().await;
    let r = f
        .call(
            "POST",
            "/v1/messages?beta=true",
            &[
                ("x-api-key", MACHINE_KEY),
                ("content-type", "application/json"),
                ("anthropic-version", "2023-06-01"),
                ("cookie", "ap_session=x"),
                ("cf-connecting-ip", "203.0.113.9"),
                ("x-forwarded-for", "203.0.113.9"),
            ],
            Some(r#"{"model":"m"}"#),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body, "data: one\n\ndata: two\n\n");
    assert!(!r.headers.contains_key("set-cookie"));
    let got = f.pool_seen().pop().unwrap();
    assert_eq!(got.path, "/v1/messages?beta=true");
    assert_eq!(got.body, r#"{"model":"m"}"#);
    assert_eq!(got.headers["x-api-key"], POOL_KEY);
    assert_eq!(got.headers["anthropic-version"], "2023-06-01");
    for h in ["cookie", "cf-connecting-ip", "x-forwarded-for", "authorization"] {
        assert!(!got.headers.contains_key(h), "{h} reached the pool");
    }
    assert!(eventually(|| f.events("public-request").iter().any(|e| e["client"] == "work-pc" && e["status"] == 200)).await);
}

#[tokio::test]
async fn bearer_keys_are_swapped_as_bearer() {
    let f = fixture().await;
    let bearer = format!("Bearer {MACHINE_KEY}");
    let r = f.call("POST", "/v1/responses", &[("authorization", &bearer)], Some("{}")).await;
    assert_eq!(r.status, 200);
    let got = f.pool_seen().pop().unwrap();
    assert_eq!(got.headers["authorization"], format!("Bearer {POOL_KEY}").as_str());
    assert!(!got.headers.contains_key("x-api-key"));
}

#[tokio::test]
async fn missing_wrong_pool_and_removed_keys_are_refused_before_the_pool() {
    let f = fixture().await;
    for headers in [vec![], vec![("x-api-key", "wrong")], vec![("x-api-key", POOL_KEY)], vec![("authorization", "Bearer wrong")]] {
        let r = f.call("POST", "/v1/messages", &headers, Some("{}")).await;
        assert_eq!(r.status, 401, "{headers:?}");
        let body: Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(body["error"]["type"], "authentication_error", "Anthropic's shape: {}", r.body);
    }
    f.clients.lock().unwrap().clear();
    assert_eq!(f.call("GET", "/v1/models", &[("x-api-key", MACHINE_KEY)], None).await.status, 401);
    assert_eq!(f.pool_seen().len(), 0);
}

#[tokio::test]
async fn a_dashboard_session_cookie_cannot_spend_quota() {
    let f = fixture().await;
    let cookie = f.login().await;
    assert_eq!(f.call("POST", "/v1/messages", &[("cookie", &cookie)], Some("{}")).await.status, 401);
    assert_eq!(f.pool_seen().len(), 0);
}

/// An address or `localhost` gets Agent Pools' 404; another name gets Corten's 421 before routing.
#[tokio::test]
async fn only_the_public_hostname_is_served() {
    let f = fixture().await;
    for (host, status) in [("127.0.0.1:8330", 404), ("localhost", 404), ("evil.test", 421)] {
        let r = f.call("GET", "/v1/models", &[("host", host), ("x-api-key", MACHINE_KEY)], None).await;
        assert_eq!(r.status, status, "{host}: {}", r.body);
    }
    assert_eq!(f.pool_seen().len(), 0);
}

#[tokio::test]
async fn management_unknown_routes_and_wrong_methods_stay_closed() {
    let f = fixture().await;
    let cookie = f.login().await;
    for (path, method) in [
        ("/v0/management/auth-files", "GET"),
        ("/v1/messages", "GET"),
        ("/api/login", "POST"),
        ("/api/callback", "POST"),
        ("/api/shutdown", "POST"),
        ("/v1/models/..%2F..%2Fv0%2Fmanagement", "GET"),
        ("/v1/models/a%20b", "GET"),
    ] {
        let headers = [("x-api-key", MACHINE_KEY), ("cookie", cookie.as_str()), ("origin", ORIGIN), ("content-type", "application/json")];
        let r = f.call(method, path, &headers, (method == "POST").then_some("{}")).await;
        assert!([403, 404, 405].contains(&r.status), "{path} -> {} {}", r.status, r.body);
    }
    assert_eq!(f.pool_seen().len(), 0);
    assert_eq!(f.service_seen().len(), 0);
}

#[tokio::test]
async fn the_dashboard_needs_a_session_and_login_sets_a_strict_cookie() {
    let f = fixture().await;
    let anon = f.call("GET", "/", &[], None).await;
    assert_eq!(anon.status, 303);
    assert_eq!(anon.headers["location"], "/login");
    assert_eq!(f.call("GET", "/api/summary", &[], None).await.status, 401);
    assert_eq!(f.call("GET", "/api/summary", &[("x-api-key", MACHINE_KEY)], None).await.status, 401);
    let page = f.call("GET", "/login", &[], None).await;
    assert_eq!(page.status, 200);
    assert!(page.body.contains(r#"name="key""#), "{}", page.body);
    let form = [("origin", ORIGIN), ("content-type", "application/x-www-form-urlencoded")];
    let bad = f.call("POST", "/login", &form, Some("key=nope")).await;
    assert_eq!(bad.status, 401);
    assert!(bad.body.contains("That key is not registered."), "{}", bad.body);
    let r = f.call("POST", "/login", &form, Some(&format!("key={MACHINE_KEY}"))).await;
    let set = r.headers["set-cookie"].to_str().unwrap().to_string();
    for attr in ["HttpOnly", "Secure", "SameSite=Strict", "Path=/"] {
        assert!(set.contains(attr), "{attr}: {set}");
    }
    let cookie = set.split(';').next().unwrap();
    let s = f.call("GET", "/api/summary", &[("cookie", cookie)], None).await;
    assert_eq!(s.status, 200, "{}", s.body);
    let got = f.service_seen().pop().unwrap();
    assert_eq!(got.headers["host"], f.service.url.trim_start_matches("http://"));
    assert!(!got.headers.contains_key("cookie"));
}

#[tokio::test]
async fn login_and_dashboard_posts_require_the_public_origin() {
    let f = fixture().await;
    let key = format!("key={MACHINE_KEY}");
    let evil = f.call("POST", "/login", &[("origin", "https://evil.test"), ("content-type", "application/x-www-form-urlencoded")], Some(&key)).await;
    assert_eq!(evil.status, 403);
    let cookie = f.login().await;
    let evil = f.call("POST", "/api/refresh", &[("cookie", &cookie), ("origin", "https://evil.test"), ("content-type", "application/json")], Some("{}")).await;
    assert_eq!(evil.status, 403);
    let ok = f.call("POST", "/api/refresh", &[("cookie", &cookie), ("origin", ORIGIN), ("content-type", "application/json")], Some("{}")).await;
    assert_eq!(ok.status, 200, "{}", ok.body);
    assert_eq!(f.service_seen().pop().unwrap().headers["origin"], f.service.url.as_str());
}

/// Agent Pools sends `same-origin`; Corten sends `strict-origin-when-cross-origin`. Either keeps
/// the real `Origin` on the sign-in form's POST, which `no-referrer` would turn into `null`.
#[tokio::test]
async fn gateway_pages_keep_the_origin_header_on_same_site_form_posts() {
    let f = fixture().await;
    let page = f.call("GET", "/login", &[], None).await;
    assert_eq!(page.status, 200);
    let policy = page.headers["referrer-policy"].to_str().unwrap();
    assert!(["same-origin", "strict-origin-when-cross-origin"].contains(&policy), "{policy}");
    assert_eq!(page.headers["cache-control"], "no-store");
}

#[tokio::test]
async fn sessions_end_when_the_machine_is_removed_rekeyed_or_the_cookie_is_forged() {
    let f = fixture().await;
    let cookie = f.login().await;
    let (name, value) = cookie.split_once('=').unwrap();
    let forged = format!("{name}={}.AAAA", value.split('.').next().unwrap());
    assert_eq!(f.call("GET", "/api/status", &[("cookie", &forged)], None).await.status, 401);
    *f.clients.lock().unwrap() = vec![Client { name: "work-pc".into(), key_sha256: sha256("new-key") }];
    assert_eq!(f.call("GET", "/api/status", &[("cookie", &cookie)], None).await.status, 401);
    f.clients.lock().unwrap().clear();
    assert_eq!(f.call("GET", "/api/status", &[("cookie", &cookie)], None).await.status, 401);
}

#[tokio::test]
async fn expired_sessions_are_refused() {
    let f = fixture_with(sse_pool(), json_service(), |c| c.session_days = 1.0).await;
    f.clock.store(1_000_000, Ordering::SeqCst);
    let cookie = f.login().await;
    assert_eq!(f.call("GET", "/api/status", &[("cookie", &cookie)], None).await.status, 200);
    f.clock.fetch_add(86_400_001, Ordering::SeqCst);
    assert_eq!(f.call("GET", "/api/status", &[("cookie", &cookie)], None).await.status, 401);
}

/// Agent Pools reads `CF-Connecting-IP`; the port reads Corten's `ClientIp`, the nearest
/// `X-Forwarded-For` address behind the tunnel on loopback.
#[tokio::test]
async fn repeated_failures_from_one_address_are_throttled() {
    let f = fixture_with(sse_pool(), json_service(), |c| c.max_failures = 3).await;
    let from = |ip| ("x-forwarded-for", ip);
    let body = |a: &Answer| serde_json::from_str::<serde_json::Value>(&a.body).unwrap();
    for _ in 0..3 {
        let a = f.call("GET", "/v1/models", &[("x-api-key", "wrong"), from("198.51.100.7")], None).await;
        assert_eq!(a.status, 401);
        // public-gateway.mjs's words, from the Caller's refusal (corten#177).
        assert_eq!(body(&a), json!({ "type": "error", "error": { "type": "authentication_error", "message": "Invalid API key" } }));
    }
    let locked = f.call("GET", "/v1/models", &[("x-api-key", MACHINE_KEY), from("198.51.100.7")], None).await;
    assert_eq!(locked.status, 429, "the right key is not even looked at");
    assert_eq!(body(&locked), json!({ "type": "error", "error": { "type": "rate_limit_error", "message": "Too many failed attempts; try again later" } }));
    // Agent Pools sends no Retry-After; the port says when the 15-minute window ends.
    let wait: u64 = locked.headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=900).contains(&wait), "{wait}");
    assert_eq!(locked.headers["cache-control"], "no-store");
    assert_eq!(f.call("GET", "/v1/models", &[("x-api-key", MACHINE_KEY), from("198.51.100.8")], None).await.status, 200);
    assert_eq!(f.events("public-auth-rejected").len(), 1);
}

/// A cookie Agent Pools signed is valid here with the same secret (the token is the same bytes).
#[test]
fn a_session_token_is_agent_pools_bytes() {
    let client = Client { name: "work-pc".into(), key_sha256: sha256(MACHINE_KEY) };
    // node -e 'import("./public-gateway.mjs").then(m => console.log(m.sessionToken("s".repeat(40), { name: "work-pc", keySha256: m.sha256("ap-machine-fixture-key") }, 1793000000000)))'
    let want = "eyJuIjoid29yay1wYyIsImsiOiJjNmRmZDY3MzVmN2NiNGJkIiwiZSI6MTc5MzAwMDAwMDAwMH0.9dvjhSjqpppB3ZgLNGMd-AZi3YZN9J_Maf6A4sjE8zY";
    assert_eq!(pools_gateway::session_token(&"s".repeat(40), &client, 1_793_000_000_000.0), want);
}

/// corten#117: when the client leaves a relayed stream, the upstream request is dropped.
#[tokio::test]
async fn a_client_that_leaves_a_stream_ends_the_upstream_request() {
    let ended = Arc::new(AtomicBool::new(false));
    let e = ended.clone();
    let slow: Handler = Arc::new(move |_| {
        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let flag = Flag(e.clone());
        let pieces = futures_util::stream::unfold((0u32, flag), |(i, flag)| async move {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Some((Ok::<_, std::io::Error>(format!("data: {i}\n\n")), (i + 1, flag)))
        });
        Response::builder().header("content-type", "text/event-stream").body(Body::from_stream(pieces)).unwrap()
    });
    let f = fixture_with(slow, json_service(), |_| {}).await;
    let res = f
        .http
        .post(format!("http://127.0.0.1:{}/v1/messages", f.port))
        .header("host", HOST)
        .header("x-api-key", MACHINE_KEY)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let mut stream = res.bytes_stream();
    use futures_util::StreamExt;
    let first = tokio::time::timeout(Duration::from_secs(2), stream.next()).await.expect("a first piece").unwrap().unwrap();
    assert!(first.starts_with(b"data: 0"), "{first:?}");
    assert!(!ended.load(Ordering::SeqCst), "the upstream is still sending");
    drop(stream);
    let gone = {
        let mut ok = false;
        for _ in 0..50 {
            if ended.load(Ordering::SeqCst) {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        ok
    };
    assert!(gone, "the upstream kept streaming 2.5 s after the client left");
    // No public-request event: the answer was not read to its end.
    assert!(f.events("public-request").is_empty());
}

/// What Corten answers itself under the gateway (corten#117 point 2): recorded for the issue.
#[tokio::test]
async fn failures_corten_answers_itself() {
    let f = fixture().await;
    let cookie = f.login().await;
    let cases = [
        ("GET", "/v1/messages", vec![("x-api-key", MACHINE_KEY)], None),
        ("GET", "/v1/nope", vec![("x-api-key", MACHINE_KEY)], None),
        ("POST", "/api/shutdown", vec![("cookie", cookie.as_str()), ("origin", ORIGIN), ("content-type", "application/json")], Some("{}")),
        ("POST", "/v1/messages", vec![("x-api-key", MACHINE_KEY), ("origin", "https://evil.test")], Some("{}")),
        ("GET", "/nope", vec![], None),
    ];
    // Under /v1 (corten#163, Api::errors): the gateway's own shape, with Corten's statuses.
    let shaped = |status: u16, kind: &str| (status, Some(kind.to_string()));
    let want = [
        shaped(405, "invalid_request_error"),
        shaped(404, "not_found_error"),
        (404, None),
        shaped(403, "permission_error"),
        (404, None),
    ];
    for ((method, path, headers, body), (status, kind)) in cases.into_iter().zip(want) {
        let r = f.call(method, path, &headers, body).await;
        assert_eq!(r.status, status, "{method} {path}: {}", r.body);
        if let Some(kind) = kind {
            let v: serde_json::Value = serde_json::from_str(&r.body).unwrap_or_else(|_| panic!("{method} {path}: {}", r.body));
            assert_eq!(v["type"], "error", "{method} {path}: {}", r.body);
            assert_eq!(v["error"]["type"], kind.as_str(), "{method} {path}: {}", r.body);
            assert!(v["error"]["message"].as_str().is_some_and(|m| !m.is_empty()), "{method} {path}: {}", r.body);
        }
    }
}

/// A relayed page keeps its own Content-Security-Policy (corten#164, fixed by corten#172).
#[tokio::test]
async fn a_relayed_page_and_its_csp() {
    let page: Handler = Arc::new(|_| {
        Response::builder()
            .header("content-type", "text/html; charset=utf-8")
            .header("content-security-policy", "default-src 'self'; script-src 'nonce-upstream'")
            .body(Body::from("<!doctype html><script nonce=upstream>1</script>"))
            .unwrap()
    });
    let f = fixture_with(sse_pool(), page, |_| {}).await;
    let cookie = f.login().await;
    let r = f.call("GET", "/", &[("cookie", &cookie)], None).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let csp: Vec<_> = r.headers.get_all("content-security-policy").iter().map(|v| v.to_str().unwrap().to_string()).collect();
    // corten#172: the upstream's own policy, in place of the app's, so the page's scripts run.
    assert_eq!(csp, ["default-src 'self'; script-src 'nonce-upstream'"]);
    // The app's other security headers still apply.
    assert_eq!(r.headers.get("x-content-type-options").map(|v| v.to_str().unwrap()), Some("nosniff"));
}

/// The live service's own dashboard through the gateway, when `AP_GATEWAY_LIVE` names the
/// service (`http://127.0.0.1:8318`): GETs only (`/` and `/api/status`), never a write. The page
/// arrives with its bytes and its own Content-Security-Policy, so its scripts run as they do on
/// the service itself.
#[tokio::test]
async fn the_live_dashboard_through_the_gateway_when_asked() {
    let Ok(service) = std::env::var("AP_GATEWAY_LIVE") else {
        eprintln!("AP_GATEWAY_LIVE not set; skipped");
        return;
    };
    let pool = upstream(sse_pool()).await;
    let mut config = Config::new(HOST, &pool.url, &service);
    config.clients = Arc::new(|| vec![Client { name: "work-pc".into(), key_sha256: sha256(MACHINE_KEY) }]);
    config.session_secret = Arc::new(|| Ok("s".repeat(40)));
    let app = app(config).expect("a valid config");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { corten::serve_on(vec![listener], app).await.unwrap() });
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy().build().unwrap();
    let ask = |path: &str, cookie: Option<&str>| {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("host", HOST.parse().unwrap());
        if let Some(c) = cookie {
            h.insert("cookie", c.parse().unwrap());
        }
        http.get(format!("http://127.0.0.1:{port}{path}")).headers(h)
    };
    let login = http
        .post(format!("http://127.0.0.1:{port}/login"))
        .header("host", HOST)
        .header("origin", ORIGIN)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("key={MACHINE_KEY}"))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
    let cookie = login.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();

    let page = ask("/", Some(&cookie)).send().await.unwrap();
    assert_eq!(page.status(), 200);
    let csp: Vec<String> = page.headers().get_all("content-security-policy").iter().map(|v| v.to_str().unwrap().to_string()).collect();
    let body = page.text().await.unwrap();
    // The page is drawn per request (relative times, sizes from data), so its policy is not the
    // same bytes twice; it is the service's own, one header, allowing the page's style block by
    // its hash and no style attribute.
    assert_eq!(csp.len(), 1, "{csp:?}");
    let direct_csp = csp[0].clone();
    assert!(!direct_csp.contains("style-src-attr"), "{direct_csp}");
    if let Some(style) = body.split("<style>").nth(1).and_then(|s| s.split("</style>").next()) {
        use base64::Engine;
        use sha2::Digest;
        let hash = base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(style.as_bytes()));
        assert!(direct_csp.contains(&format!("'sha256-{hash}'")), "the page's style block is allowed by its hash: {direct_csp}");
    }
    // The other tabs and the page's stylesheets come the same way (reads only).
    for path in ["/accounts", "/usage", "/policy"] {
        assert_eq!(ask(path, Some(&cookie)).send().await.unwrap().status(), 200, "{path}");
    }
    let sheets: Vec<&str> = body.split("href=\"").skip(1).filter_map(|s| s.split('"').next()).filter(|h| h.starts_with("/assets/") && h.ends_with(".css")).collect();
    assert!(!sheets.is_empty(), "the page links its stylesheets");
    for sheet in &sheets {
        let r = ask(sheet, Some(&cookie)).send().await.unwrap();
        assert_eq!(r.status(), 200, "{sheet}");
        assert!(r.headers()["content-type"].to_str().unwrap().starts_with("text/css"), "{sheet}");
        assert_eq!(ask(sheet, None).send().await.unwrap().status(), 404, "{sheet} needs a session");
    }
    let status = ask("/api/status", Some(&cookie)).send().await.unwrap();
    assert_eq!(status.status(), 200);
    let doc: Value = serde_json::from_str(&status.text().await.unwrap()).unwrap();
    assert!(doc["accounts"].as_array().is_some_and(|a| !a.is_empty()));
    println!("live dashboard through the gateway: {} bytes, CSP {direct_csp:?}, {} accounts in /api/status", body.len(), doc["accounts"].as_array().unwrap().len());
}

/// The dashboard's other pages, its live updates and its files reach the service for a session
/// only; without one a page goes to the sign-in form and a file the gateway lacks is a 404. The
/// gateway's own files (the sign-in page's stylesheet) never need one, and the service's other
/// pages (summary, parity) stay closed.
#[tokio::test]
async fn the_dashboard_pages_live_updates_and_files_need_a_session() {
    let f = fixture().await;
    for path in ["/accounts", "/usage", "/policy", "/signin", "/live?tab=overview"] {
        let r = f.call("GET", path, &[], None).await;
        assert_eq!((r.status, r.headers.get("location").map(|v| v.to_str().unwrap().to_string())), (303, Some("/login".into())), "{path}");
    }
    assert_eq!(f.call("GET", "/assets/0123456789ab/base.css", &[], None).await.status, 404);
    let login = f.call("GET", "/login", &[], None).await;
    let own = login.body.split("href=\"").nth(1).and_then(|s| s.split('"').next()).unwrap().to_string();
    assert!(own.starts_with("/assets/") && own.ends_with(".css"), "{own}");
    assert_eq!(f.call("GET", &own, &[], None).await.status, 200, "the sign-in page's own stylesheet, without a session");
    assert_eq!(f.service_seen().len(), 0, "nothing reached the service without a session");

    let cookie = f.login().await;
    let page = f.call("GET", "/accounts?filter=credits&open=a", &[("cookie", &cookie), ("corten", "nav"), ("x-api-key", MACHINE_KEY)], None).await;
    assert_eq!(page.status, 200, "{}", page.body);
    let got = f.service_seen().pop().unwrap();
    assert_eq!(got.path, "/accounts?filter=credits&open=a");
    assert_eq!(got.headers["corten"], "nav", "Corten's runtime asks for a part of the page");
    for gone in ["cookie", "x-api-key", "origin"] {
        assert!(!got.headers.contains_key(gone), "{gone} reached the service");
    }
    for path in ["/usage", "/policy", "/signin?provider=claude", "/live?tab=accounts&filter=credits"] {
        assert_eq!(f.call("GET", path, &[("cookie", &cookie)], None).await.status, 200, "{path}");
        assert_eq!(f.service_seen().pop().unwrap().path, path);
    }
    let file = f.call("GET", "/assets/0123456789ab/base.css", &[("cookie", &cookie)], None).await;
    assert_eq!(file.status, 200);
    assert_eq!(f.service_seen().pop().unwrap().path, "/assets/0123456789ab/base.css");
    let before = f.service_seen().len();
    assert_eq!(f.call("GET", &own, &[("cookie", &cookie)], None).await.status, 200);
    assert_eq!(f.service_seen().len(), before, "the gateway's own file is its own");
    for path in ["/summary", "/parity", "/api/parity"] {
        assert!([404, 405].contains(&f.call("GET", path, &[("cookie", &cookie)], None).await.status), "{path}");
    }
    assert_eq!(f.service_seen().len(), before, "the service's other pages stay closed");
}

/// The dashboard's forms (Refresh and the sign-in steps) need a session, the public origin and a
/// form body; the service gets them from its own origin, without the cookie.
#[tokio::test]
async fn the_dashboard_forms_need_a_session_the_public_origin_and_a_form() {
    let f = fixture().await;
    let form = ("content-type", "application/x-www-form-urlencoded");
    let anon = f.call("POST", "/refresh", &[("origin", ORIGIN), form], Some("open=")).await;
    assert_eq!((anon.status, anon.headers.get("location").map(|v| v.to_str().unwrap().to_string())), (303, Some("/login".into())));
    let cookie = f.login().await;
    let evil = f.call("POST", "/refresh", &[("cookie", &cookie), ("origin", "https://evil.test"), form], Some("open=")).await;
    assert_eq!(evil.status, 403);
    let none = f.call("POST", "/refresh", &[("cookie", &cookie), form], Some("open=")).await;
    assert_eq!(none.status, 403, "no Origin is not the public origin");
    let json = f.call("POST", "/refresh", &[("cookie", &cookie), ("origin", ORIGIN), ("content-type", "application/json")], Some("{}")).await;
    assert_eq!(json.status, 415);
    assert_eq!(f.service_seen().len(), 0);

    for (path, body) in [("/refresh", "open="), ("/signin", "provider=codex"), ("/signin/finish", "provider=codex&link=http%3A%2F%2Flocalhost%3A1455%2Fcallback%3Fcode%3Dx"), ("/signin/cancel", "")] {
        let r = f.call("POST", path, &[("cookie", &cookie), ("origin", ORIGIN), ("corten", "action"), form], Some(body)).await;
        assert_eq!(r.status, 200, "{path}: {}", r.body);
        let got = f.service_seen().pop().unwrap();
        assert_eq!((got.path.as_str(), got.body.as_str()), (path, body));
        assert_eq!(got.headers["origin"], f.service.url.as_str(), "a write from the service's own origin");
        assert_eq!(got.headers["host"], f.service.url.trim_start_matches("http://"));
        assert_eq!(got.headers["content-type"], "application/x-www-form-urlencoded");
        assert_eq!(got.headers["corten"], "action");
        assert!(!got.headers.contains_key("cookie"));
    }
    let refreshes = f.call("POST", "/api/shutdown", &[("cookie", &cookie), ("origin", ORIGIN), form], Some("")).await;
    assert!([404, 405].contains(&refreshes.status), "{}", refreshes.status);
}

/// A form's answer is the service's own: its redirect reaches the browser, which then asks for
/// the page itself, rather than the gateway following it and leaving the browser at the form.
#[tokio::test]
async fn a_forms_redirect_reaches_the_browser() {
    let service: Handler = Arc::new(|s| {
        if s.path == "/refresh" {
            Response::builder().status(303).header("location", "/accounts?open=a#acct-a").body(Body::empty()).unwrap()
        } else {
            Response::builder().header("content-type", "text/html").body(Body::from("page")).unwrap()
        }
    });
    let f = fixture_with(sse_pool(), service, |_| {}).await;
    let cookie = f.login().await;
    let r = f.call("POST", "/refresh", &[("cookie", &cookie), ("origin", ORIGIN), ("content-type", "application/x-www-form-urlencoded")], Some("open=a")).await;
    assert_eq!((r.status, r.headers["location"].to_str().unwrap()), (303, "/accounts?open=a#acct-a"));
    assert_eq!(f.service_seen().iter().map(|s| s.path.as_str()).collect::<Vec<_>>(), ["/refresh"], "the gateway did not follow it");
}

