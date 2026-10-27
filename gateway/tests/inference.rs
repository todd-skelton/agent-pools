//! inference-relay.test.mjs (Agent Pools 5c696f6) on the Corten relay, over real sockets. The
//! pool is a plain hyper server on loopback that answers HTTP and, on `/v1/responses`, a
//! WebSocket echo. Cases for Codex's config file (`inference-config.mjs`) are not here: that
//! file is not part of the relay.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use http_body_util_shim::Full;
use pools_gateway::inference::{self, Authorize, Config};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, protocol::Role};

const AUTH: &str = "Bearer desktop-fixture";
const POOL_KEY: &str = "pool-fixture";
const BODY: &str = r#"{"model":"fixture-model","reasoning":{"effort":"high"},"input":[]}"#;

mod http_body_util_shim {
    pub type Full = corten::axum::body::Body;
}

#[derive(Clone, Debug)]
struct Seen {
    headers: corten::axum::http::HeaderMap,
    body: String,
    path: String,
}

type Handler = Arc<dyn Fn(Seen) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<corten::axum::response::Response>> + Send>> + Send + Sync>;

/// The pool on `port` (0 for any): HTTP answers from `handler` (`None`: drop the connection),
/// and a WebSocket echo (with `echo_prefix`) on upgrade.
async fn pool(port: u16, handler: Option<Handler>, echo_prefix: &'static str) -> (u16, Arc<Mutex<Vec<Seen>>>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let s = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else { return };
            let (s, handler) = (s.clone(), handler.clone());
            tokio::spawn(async move {
                // Peek: a WebSocket handshake goes to tungstenite, anything else to hyper.
                let mut buf = [0u8; 2048];
                let n = tcp.peek(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                if head.contains("upgrade: websocket") {
                    let headers = Arc::new(Mutex::new(corten::axum::http::HeaderMap::new()));
                    let h = headers.clone();
                    #[allow(clippy::result_large_err)] // tungstenite's callback type
                    let cb = move |req: &tokio_tungstenite::tungstenite::handshake::server::Request, res| {
                        *h.lock().unwrap() = req.headers().clone();
                        Ok(res)
                    };
                    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(tcp, cb).await else { return };
                    while let Some(Ok(m)) = ws.next().await {
                        if let Message::Text(t) = m {
                            s.lock().unwrap().push(Seen { headers: headers.lock().unwrap().clone(), body: t.to_string(), path: "/v1/responses".into() });
                            if ws.send(Message::text(format!("{echo_prefix}{t}"))).await.is_err() {
                                return;
                            }
                        }
                    }
                    return;
                }
                let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let (s, handler) = (s.clone(), handler.clone());
                    async move {
                        let (parts, body) = req.into_parts();
                        let path = parts.uri.path_and_query().map(|p| p.to_string()).unwrap_or_default();
                        let body = corten::axum::body::to_bytes(corten::axum::body::Body::new(body), usize::MAX).await.unwrap_or_default();
                        let seen = Seen { headers: parts.headers, body: String::from_utf8_lossy(&body).into_owned(), path };
                        s.lock().unwrap().push(seen.clone());
                        let res: Option<corten::axum::response::Response> = match &handler {
                            Some(h) => h(seen).await,
                            None => Some(
                                corten::axum::response::Response::builder()
                                    .header("content-type", "text/event-stream")
                                    .header("set-cookie", "upstream-private")
                                    .body(Full::from(format!("data: {echo_prefix}fixture\n\n")))
                                    .unwrap(),
                            ),
                        };
                        match res {
                            Some(r) => Ok::<_, std::io::Error>(r),
                            None => Err::<corten::axum::response::Response, _>(std::io::Error::other("drop")),
                        }
                    }
                });
                let io = hyper_util::rt::TokioIo::new(tcp);
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    (port, seen)
}

struct Relay {
    port: u16,
    inf: inference::Inf,
    http: reqwest::Client,
}

fn fixed_auth() -> Authorize {
    Arc::new(|a: Option<String>| Box::pin(async move { Ok((a.as_deref() == Some(AUTH)).then(|| POOL_KEY.to_string())) }))
}

async fn relay(pool_port: u16, tune: impl FnOnce(&mut Config)) -> Relay {
    let mut config = Config::new(&format!("http://127.0.0.1:{pool_port}/v1"), fixed_auth());
    tune(&mut config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (app, inf) = inference::app(config, port).unwrap();
    tokio::spawn(async move { corten::serve_on(vec![listener], app).await.unwrap() });
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    Relay { port, inf, http }
}

async fn fixture(handler: Option<Handler>, tune: impl FnOnce(&mut Config)) -> (Relay, Arc<Mutex<Vec<Seen>>>) {
    let (port, seen) = pool(0, handler, "").await;
    (relay(port, tune).await, seen)
}

impl Relay {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    async fn post(&self, path: &str, body: impl Into<reqwest::Body>, headers: &[(&str, &str)]) -> reqwest::Response {
        let mut map = reqwest::header::HeaderMap::new();
        map.insert("authorization", AUTH.parse().unwrap());
        for (k, v) in headers {
            map.insert(reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        self.http.post(self.url(path)).headers(map).body(body).send().await.unwrap()
    }

    async fn health(&self) -> serde_json::Value {
        serde_json::from_str(&self.http.get(self.url("/health")).send().await.unwrap().text().await.unwrap()).unwrap()
    }

    async fn socket(&self, headers: &[(&str, &str)]) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, String> {
        let mut req = format!("ws://127.0.0.1:{}/v1/responses", self.port).into_client_request().unwrap();
        for (k, v) in headers {
            req.headers_mut().insert(reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        tokio_tungstenite::connect_async(req).await.map(|(ws, _)| ws).map_err(|e| e.to_string())
    }

    async fn idle(&self) -> bool {
        for _ in 0..50 {
            if self.inf.active() == 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }
}

fn handler<F, Fut>(f: F) -> Option<Handler>
where
    F: Fn(Seen) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Option<corten::axum::response::Response>> + Send + 'static,
{
    Some(Arc::new(move |s| Box::pin(f(s))))
}

#[tokio::test]
async fn http_inference_and_compaction_preserve_bodies_and_separate_credentials() {
    let (r, seen) = fixture(None, |_| {}).await;
    for route in ["/v1/responses", "/v1/responses/compact"] {
        let res = r
            .post(route, BODY, &[("chatgpt-account-id", "private"), ("cookie", "secret"), ("x-api-key", "attacker"), ("session_id", "fixture-thread")])
            .await;
        assert_eq!(res.status(), 200);
        assert!(res.headers().get("set-cookie").is_none());
        res.text().await.unwrap();
        let got = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!((got.body.as_str(), got.path.as_str()), (BODY, route));
        assert_eq!(got.headers["x-api-key"], POOL_KEY);
        assert_eq!(got.headers["session_id"], "fixture-thread");
        for k in ["authorization", "chatgpt-account-id", "cookie"] {
            assert!(!got.headers.contains_key(k), "{k}");
        }
    }
}

#[tokio::test]
async fn websocket_inference_preserves_frames_but_not_caller_credentials() {
    let (r, seen) = fixture(None, |_| {}).await;
    let mut ws = r.socket(&[("authorization", AUTH), ("cookie", "secret"), ("chatgpt-account-id", "private")]).await.unwrap();
    ws.send(Message::text(BODY)).await.unwrap();
    let back = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(back.into_text().unwrap().as_str(), BODY);
    let got = seen.lock().unwrap()[0].clone();
    assert_eq!(got.headers["x-api-key"], POOL_KEY);
    for k in ["authorization", "chatgpt-account-id", "cookie"] {
        assert!(!got.headers.contains_key(k), "{k}");
    }
}

#[tokio::test]
async fn zstd_bodies_are_decoded_and_checked_before_forwarding() {
    let (r, seen) = fixture(None, |_| {}).await;
    for route in ["/v1/responses", "/v1/responses/compact"] {
        let res = r.post(route, zstd::encode_all(BODY.as_bytes(), 3).unwrap(), &[("content-encoding", " ZsTd "), ("cookie", "secret")]).await;
        assert_eq!(res.status(), 200);
        res.text().await.unwrap();
        let got = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!(got.body, BODY);
        assert!(!got.headers.contains_key("content-encoding"));
    }
}

#[tokio::test]
async fn compressed_bodies_enforce_decoded_size_and_json_before_forwarding() {
    let (r, seen) = fixture(None, |c| c.max_body_bytes = 1024).await;
    let oversized = zstd::encode_all(format!(r#"{{"model":"fixture-model","input":"{}"}}"#, "x".repeat(16 * 1024)).as_bytes(), 3).unwrap();
    assert!(oversized.len() < 1024);
    for route in ["/v1/responses", "/v1/responses/compact"] {
        let cases: Vec<(Vec<u8>, &str, u16)> = vec![
            (oversized.clone(), "zstd", 413),
            (vec![0; 1025], "zstd", 413),
            (b"invalid-zstd".to_vec(), "zstd", 400),
            (zstd::encode_all(&b"not-json"[..], 3).unwrap(), "zstd", 400),
            (zstd::encode_all(&b"{}"[..], 3).unwrap(), "zstd", 400),
            (BODY.as_bytes().to_vec(), "gzip", 415),
            (BODY.as_bytes().to_vec(), "zstd, zstd", 415),
        ];
        for (payload, encoding, status) in cases {
            let res = r.post(route, payload, &[("content-encoding", encoding)]).await;
            assert_eq!(res.status(), status, "{route} {encoding}");
        }
    }
    assert_eq!(seen.lock().unwrap().len(), 0);
    assert!(r.idle().await);
    assert_eq!(r.post("/v1/responses", BODY, &[]).await.status(), 200);
}

#[tokio::test]
async fn wrong_credentials_browsers_hosts_and_other_routes_fail_closed() {
    let (r, seen) = fixture(None, |_| {}).await;
    for headers in [vec![("authorization", "")], vec![("authorization", "Bearer wrong")], vec![("origin", "http://localhost")], vec![("sec-fetch-site", "cross-site")]] {
        let status = r.post("/v1/responses", BODY, &headers).await.status().as_u16();
        assert!([401, 403].contains(&status), "{headers:?}: {status}");
    }
    // Corten refuses a name it does not answer to with 421 before routing (Agent Pools: 403).
    assert_eq!(r.post("/v1/responses", BODY, &[("host", "attacker.invalid")]).await.status(), 421);
    // A name Corten answers to (`localhost`, without TLS) is the relay's to refuse.
    let localhost = format!("localhost:{}", r.port);
    assert_eq!(r.post("/v1/responses", BODY, &[("host", &localhost)]).await.status(), 403);
    for (path, ok) in [("/v1/responses?target=elsewhere", [404]), ("/backend-api/wham/usage", [404]), ("/v1/models", [404]), ("/v1/responses/other", [404])] {
        let status = r.post(path, BODY, &[]).await.status().as_u16();
        assert!(ok.contains(&status), "{path}: {status}");
    }
    assert_eq!(seen.lock().unwrap().len(), 0);
}

/// A `GET /v1/responses` that does not ask to switch is not the socket: the relay's own 404 (the
/// HTTP side takes only POSTs), signed in or not, as inference-relay.mjs answers it.
#[tokio::test]
async fn a_plain_get_on_the_socket_route_is_the_relays_404() {
    let (r, seen) = fixture(None, |_| {}).await;
    for auth in [AUTH, ""] {
        let res = r.http.get(r.url("/v1/responses")).header("authorization", auth).send().await.unwrap();
        assert_eq!(res.status(), 404, "{auth:?}");
        assert_eq!(res.headers()["cache-control"], "no-store");
        let body: serde_json::Value = serde_json::from_str(&res.text().await.unwrap()).unwrap();
        assert_eq!(body, serde_json::json!({ "error": { "message": "Inference relay HTTP 404", "type": "relay_error" } }));
    }
    assert_eq!(seen.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn unauthenticated_and_browser_upgrades_never_reach_the_pool() {
    let (r, seen) = fixture(None, |_| {}).await;
    for headers in [vec![], vec![("authorization", AUTH), ("origin", "http://localhost")]] {
        let err = r.socket(&headers).await.expect_err("refused");
        assert!(err.contains("401") || err.contains("403"), "{err}");
    }
    assert_eq!(seen.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn credential_store_errors_fail_closed_without_their_contents() {
    let failing: Authorize = Arc::new(|_| Box::pin(async { Err("sensitive detail".to_string()) }));
    let (r, seen) = fixture(None, |c| c.authorize = failing).await;
    let res = r.post("/v1/responses", BODY, &[]).await;
    assert_eq!(res.status(), 503);
    assert!(!res.text().await.unwrap().contains("sensitive"));
    assert_eq!(seen.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn file_authentication_follows_refresh_account_changes_rotation_and_logout() {
    let dir = std::env::temp_dir().join(format!("inference-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (auth, secrets) = (dir.join("auth.json"), dir.join("secrets.json"));
    let write_auth = |token: &str| std::fs::write(&auth, serde_json::json!({ "auth_mode": "chatgpt", "tokens": { "access_token": token } }).to_string()).unwrap();
    write_auth("first");
    std::fs::write(&secrets, r#"{"clientKey":"pool1"}"#).unwrap();
    let check = inference::file_credentials(auth.clone(), secrets.clone());
    let ask = |v: &str| check(Some(v.to_string()));
    assert_eq!(ask("Bearer first").await.unwrap().as_deref(), Some("pool1"));
    assert_eq!(ask("Bearer forged").await.unwrap(), None);
    write_auth("second");
    assert_eq!(ask("Bearer first").await.unwrap(), None);
    assert_eq!(ask("Bearer second").await.unwrap().as_deref(), Some("pool1"));
    std::fs::write(&secrets, r#"{"clientKey":"pool2"}"#).unwrap();
    assert_eq!(ask("Bearer second").await.unwrap().as_deref(), Some("pool2"));
    std::fs::write(&auth, "{}").unwrap();
    assert_eq!(ask("Bearer second").await.unwrap(), None);
    std::fs::remove_file(&auth).unwrap();
    assert!(ask("Bearer second").await.is_err());
    // API-key mode, never an arbitrary bearer.
    std::fs::write(&auth, r#"{"auth_mode":"apikey","OPENAI_API_KEY":"api-fixture"}"#).unwrap();
    assert_eq!(ask("Bearer api-fixture").await.unwrap().as_deref(), Some("pool2"));
    assert_eq!(ask("Bearer other").await.unwrap(), None);
    std::fs::remove_file(&auth).unwrap();
    std::fs::remove_file(&secrets).unwrap();
    std::fs::remove_dir(&dir).unwrap();
}

#[tokio::test]
async fn existing_websockets_cannot_submit_more_after_logout_or_rotation() {
    let valid = Arc::new(AtomicBool::new(true));
    let v = valid.clone();
    let auth: Authorize = Arc::new(move |a| {
        let ok = v.load(Ordering::SeqCst) && a.as_deref() == Some(AUTH);
        Box::pin(async move { Ok(ok.then(|| POOL_KEY.to_string())) })
    });
    let (r, seen) = fixture(None, |c| c.authorize = auth).await;
    let mut ws = r.socket(&[("authorization", AUTH)]).await.unwrap();
    ws.send(Message::text(BODY)).await.unwrap();
    ws.next().await.unwrap().unwrap();
    valid.store(false, Ordering::SeqCst);
    ws.send(Message::text(BODY)).await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(m)) = ws.next().await {
            if m.is_close() {
                break;
            }
        }
    })
    .await;
    assert!(ended.is_ok(), "the socket stayed open after sign-out");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn malformed_binary_and_oversized_inference_is_rejected_before_forwarding() {
    let (r, seen) = fixture(None, |c| c.max_body_bytes = 1024).await;
    for (payload, code) in [("not-json".to_string(), 400), ("{}".to_string(), 400), ("x".repeat(1025), 413)] {
        assert_eq!(r.post("/v1/responses", payload, &[]).await.status(), code);
    }
    let mut ws = r.socket(&[("authorization", AUTH)]).await.unwrap();
    ws.send(Message::binary(BODY.as_bytes().to_vec())).await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), async { while let Some(Ok(_)) = ws.next().await {} }).await;
    assert!(ended.is_ok());
    assert_eq!(seen.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn arbitrary_upstreams_and_redirect_escape_are_rejected() {
    for url in ["https://example.com/v1", "http://127.0.0.1/v1?x=1", "http://x:y@127.0.0.1/v1", "http://localhost/v1", "http://127.0.0.1:1/v2"] {
        let err = inference::app(Config::new(url, fixed_auth()), 1).err().expect(url);
        assert!(err.contains("loopback"), "{url}: {err}");
    }
    let redirect = handler(|_| async {
        Some(corten::axum::response::Response::builder().status(307).header("location", "http://example.invalid/").body(Full::empty()).unwrap())
    });
    let (r, _) = fixture(redirect, |_| {}).await;
    assert_eq!(r.post("/v1/responses", BODY, &[]).await.status(), 502);
}

#[tokio::test]
async fn http_cancellation_propagates_upstream_and_releases_admission() {
    let closed = Arc::new(AtomicBool::new(false));
    let c = closed.clone();
    let slow = handler(move |_| {
        let c = c.clone();
        async move {
            struct Flag(Arc<AtomicBool>);
            impl Drop for Flag {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let flag = Flag(c);
            let pieces = futures_util::stream::unfold((0, flag), |(i, flag)| async move {
                if i > 0 {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                Some((Ok::<_, std::io::Error>("data: started\n\n"), (i + 1, flag)))
            });
            Some(corten::axum::response::Response::builder().header("content-type", "text/event-stream").body(Full::from_stream(pieces)).unwrap())
        }
    });
    let (r, _) = fixture(slow, |_| {}).await;
    let res = r.post("/v1/responses", BODY, &[]).await;
    let mut stream = res.bytes_stream();
    stream.next().await.unwrap().unwrap();
    assert_eq!(r.inf.active(), 1);
    drop(stream);
    for _ in 0..100 {
        if closed.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(closed.load(Ordering::SeqCst), "the pool's stream outlived the client");
    assert!(r.idle().await);
}

#[tokio::test]
async fn delayed_streams_stay_open_within_the_stream_budget() {
    let delayed = handler(|_| async {
        let pieces = futures_util::stream::unfold(0, |i| async move {
            (i < 2).then_some(())?;
            tokio::time::sleep(Duration::from_millis(150)).await;
            Some((Ok::<_, std::io::Error>(if i == 0 { "first" } else { "second" }), i + 1))
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        Some(corten::axum::response::Response::builder().body(Full::from_stream(pieces)).unwrap())
    });
    let (r, _) = fixture(delayed, |c| {
        c.header_timeout = Duration::from_millis(50);
        c.stream_timeout = Duration::from_secs(2);
    })
    .await;
    assert_eq!(r.post("/v1/responses", BODY, &[]).await.text().await.unwrap(), "firstsecond");
}

#[tokio::test]
async fn the_stream_deadline_cancels_a_stalled_upstream_without_replay() {
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    let stall = handler(move |_| {
        c.fetch_add(1, Ordering::SeqCst);
        async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            None
        }
    });
    let (r, _) = fixture(stall, |c| c.stream_timeout = Duration::from_millis(75)).await;
    assert_eq!(r.post("/v1/responses", BODY, &[]).await.status(), 502);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(r.idle().await);
}

async fn free_port() -> u16 {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    probe.local_addr().unwrap().port()
}

/// Windows takes about 2 s to report a refused loopback connect (measured: 2,076 ms), so the
/// pool comes up after 3 s, once the first attempt has been refused; Agent Pools' test waits 0.6 s.
#[tokio::test]
async fn http_waits_for_a_restarting_pool_instead_of_failing() {
    let port = free_port().await;
    let r = relay(port, |_| {}).await;
    let pending = tokio::spawn({
        let (http, url) = (r.http.clone(), r.url("/v1/responses"));
        async move { http.post(url).header("authorization", AUTH).body(BODY).send().await.unwrap() }
    });
    tokio::time::sleep(Duration::from_millis(3000)).await;
    let (_, seen) = pool(port, None, "late:").await;
    let res = pending.await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.text().await.unwrap(), "data: late:fixture\n\n");
    assert_eq!(seen.lock().unwrap().iter().map(|s| s.body.clone()).collect::<Vec<_>>(), [BODY]);
    let h = r.health().await;
    assert_eq!((h["connectWaits"].as_u64(), h["waiting"].as_i64()), (Some(1), Some(0)));
    assert!(r.idle().await);
}

#[tokio::test]
async fn a_pool_that_stays_down_past_the_budget_still_fails() {
    let port = free_port().await;
    let r = relay(port, |c| c.connect_retry = Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    assert_eq!(r.post("/v1/responses", BODY, &[]).await.status(), 502);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(r.idle().await);
}

#[tokio::test]
async fn websocket_waits_for_a_restarting_pool_then_relays() {
    let port = free_port().await;
    let r = relay(port, |_| {}).await;
    let mut ws = r.socket(&[("authorization", AUTH)]).await.unwrap();
    ws.send(Message::text(BODY)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (_, seen) = pool(port, None, "echo:").await;
    let back = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(back.into_text().unwrap().as_str(), format!("echo:{BODY}"));
    assert_eq!(seen.lock().unwrap().len(), 1);
    ws.close(None).await.unwrap();
    assert!(r.idle().await);
}

#[tokio::test]
async fn a_connection_the_pool_accepted_then_dropped_is_not_retried() {
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    let drop_it = handler(move |_| {
        c.fetch_add(1, Ordering::SeqCst);
        async { None }
    });
    let (r, _) = fixture(drop_it, |_| {}).await;
    assert_eq!(r.post("/v1/responses", BODY, &[]).await.status(), 502);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(r.health().await["connectWaits"], 0);
}

#[tokio::test]
async fn websocket_cancellation_releases_the_slot() {
    let (r, _) = fixture(None, |_| {}).await;
    let mut ws = r.socket(&[("authorization", AUTH)]).await.unwrap();
    ws.send(Message::text(BODY)).await.unwrap();
    ws.next().await.unwrap().unwrap();
    drop(ws);
    assert!(r.idle().await);
}

#[tokio::test]
async fn the_admission_cap_rejects_overload_then_recovers() {
    let (r, _) = fixture(None, |c| c.max_active = 1).await;
    let mut ws = r.socket(&[("authorization", AUTH)]).await.unwrap();
    assert_eq!(r.post("/v1/responses", BODY, &[]).await.status(), 503);
    ws.close(None).await.unwrap();
    assert!(r.idle().await);
    assert_eq!(r.post("/v1/responses", BODY, &[]).await.status(), 200);
}

/// A handshake missing `Sec-WebSocket-Key`: Corten refuses it (400) and the slot taken for it
/// is released with the work that never ran.
#[tokio::test]
async fn malformed_handshakes_do_not_leak_slots() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (r, _) = fixture(None, |c| c.max_active = 1).await;
    let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", r.port)).await.unwrap();
    let req = format!("GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: {AUTH}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\r\n", r.port);
    tcp.write_all(req.as_bytes()).await.unwrap();
    let mut answer = vec![0u8; 512];
    let n = tokio::time::timeout(Duration::from_secs(5), tcp.read(&mut answer)).await.unwrap().unwrap();
    let answer = String::from_utf8_lossy(&answer[..n]).to_string();
    assert!(!answer.starts_with("HTTP/1.1 101"), "{answer}");
    drop(tcp);
    assert!(r.idle().await);
    assert_eq!(r.post("/v1/responses", BODY, &[]).await.status(), 200);
}

#[tokio::test]
async fn empty_frame_floods_are_bounded_while_credential_reads_are_pending() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let slow: Authorize = Arc::new(move |_| {
        let n = c.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if n > 0 {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            Ok(Some(POOL_KEY.to_string()))
        })
    });
    let (r, seen) = fixture(None, |c| c.authorize = slow).await;
    let ws = r.socket(&[("authorization", AUTH)]).await.unwrap();
    let (mut tx, mut rx) = ws.split();
    for _ in 0..140 {
        if tx.send(Message::text("")).await.is_err() {
            break;
        }
    }
    let ended = tokio::time::timeout(Duration::from_secs(5), async { while let Some(Ok(_)) = rx.next().await {} }).await;
    assert!(ended.is_ok(), "the flood did not end the socket");
    assert_eq!(seen.lock().unwrap().len(), 0);
    let _ = Role::Client;
}

/// A client that never finishes its request head is closed after `header_timeout` (Node:
/// `server.headersTimeout`), with no answer, and frees nothing it never took.
#[tokio::test]
async fn a_request_head_that_never_ends_is_closed_after_the_header_timeout() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (r, _) = fixture(None, |c| c.header_timeout = Duration::from_millis(300)).await;
    let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", r.port)).await.unwrap();
    tcp.write_all(format!("POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n", r.port).as_bytes()).await.unwrap();
    let started = std::time::Instant::now();
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), tcp.read_to_end(&mut buf)).await;
    assert!(read.is_ok(), "still open after 5 s");
    assert!(started.elapsed() >= Duration::from_millis(250), "closed at {:?}", started.elapsed());
    assert!(buf.is_empty(), "no answer: {}", String::from_utf8_lossy(&buf));
    assert_eq!(r.health().await["active"], 0);
}
