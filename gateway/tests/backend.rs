//! backend-relay.test.mjs (Agent Pools 5c696f6) on the Corten relay, over real sockets. The fake
//! chatgpt.com is a raw TCP server: usage JSON, an echo, a chunked stream, and a switch that
//! echoes bytes. Where the port answers what Agent Pools drops (a switch it cannot carry), the
//! case records Corten's answer instead.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pools_gateway::backend::{self, Config};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: String,
    upgrade: bool,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

type Seen_ = Arc<Mutex<Vec<Seen>>>;

/// The head of an HTTP/1.1 message and whatever came after it.
async fn read_head(tcp: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).into_owned();
            return Some((head, buf[end + 4..].to_vec()));
        }
        let n = tcp.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Fake chatgpt.com on loopback; \`stream_gap\` is the pause between the stream's two events.
async fn fake_upstream(stream_gap: Duration) -> (u16, Seen_, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen_ = Arc::default();
    let s = seen.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut tcp, _)) = listener.accept().await else { return };
            let s = s.clone();
            tokio::spawn(async move {
                let Some((head, mut rest)) = read_head(&mut tcp).await else { return };
                let mut lines = head.split("\r\n");
                let mut first = lines.next().unwrap_or("").split(' ');
                let (method, url) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
                let headers: Vec<(String, String)> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                let upgrade = headers.iter().any(|(k, _)| k == "upgrade");
                let length: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
                if !upgrade {
                    while rest.len() < length {
                        let mut chunk = [0u8; 4096];
                        let Ok(n) = tcp.read(&mut chunk).await else { return };
                        if n == 0 {
                            break;
                        }
                        rest.extend_from_slice(&chunk[..n]);
                    }
                }
                let body = if upgrade { String::new() } else { String::from_utf8_lossy(&rest).into_owned() };
                s.lock().unwrap().push(Seen { method, url: url.clone(), headers, body, upgrade });
                if upgrade {
                    let _ = tcp.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n").await;
                    if !rest.is_empty() {
                        let _ = tcp.write_all(&rest).await;
                    }
                    let (mut r, mut w) = tcp.into_split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                    return;
                }
                let simple = |status: &str, kind: &str, extra: &str, body: &str| {
                    format!("HTTP/1.1 {status}\r\ncontent-type: {kind}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
                };
                let out = if url == "/backend-api/wham/usage" {
                    simple("200 OK", "application/json", "x-upstream: yes\r\n", r#"{"rate_limit":{"allowed":false}}"#)
                } else if url == "/backend-api/broken-usage" {
                    simple("200 OK", "application/json", "", "not json")
                } else if url.starts_with("/backend-api/stream") {
                    let _ = tcp
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\nb\r\ndata: one\n\n\r\n")
                        .await;
                    tokio::time::sleep(stream_gap).await;
                    let _ = tcp.write_all(b"b\r\ndata: two\n\n\r\n0\r\n\r\n").await;
                    return;
                } else {
                    simple("201 Created", "text/plain", "", "echo")
                };
                let _ = tcp.write_all(out.as_bytes()).await;
            });
        }
    });
    (port, seen, task)
}

/// The relay on a listener of its own, with \`tune\` applied to its options.
async fn front(upstream_port: u16, tune: impl FnOnce(&mut Config)) -> u16 {
    let mut config = Config::new(&format!("http://127.0.0.1:{upstream_port}/backend-api"));
    tune(&mut config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = backend::app(config, port).unwrap();
    tokio::spawn(async move { corten::serve_on(vec![listener], app).await.unwrap() });
    port
}

async fn body(r: reqwest::Response) -> Value {
    serde_json::from_str(&r.text().await.unwrap()).unwrap()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

type Events = Arc<Mutex<Vec<Value>>>;

fn recorder(events: &Events) -> backend::Observe {
    let events = events.clone();
    Arc::new(move |kind: &str, data: Value| {
        let mut e = json!({ "type": kind });
        if let (Value::Object(e), Value::Object(d)) = (&mut e, data) {
            e.extend(d);
        }
        events.lock().unwrap().push(e);
    })
}

#[test]
fn backend_path_matching() {
    assert!(backend::is_backend_path("/backend-api"));
    assert!(backend::is_backend_path("/backend-api/wham/usage"));
    assert!(!backend::is_backend_path("/backend-apix"));
    assert!(!backend::is_backend_path("/api/status"));
}

#[test]
fn relay_tls_material_is_optional_and_only_used_as_a_complete_pair() {
    let base = std::env::temp_dir().join(format!("relay-tls-{}", std::process::id()));
    let (dir, unreadable) = (base.join("a"), base.join("b"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&unreadable).unwrap();
    assert!(backend::load_relay_tls(&dir).unwrap().is_none(), "no files: plain HTTP");
    std::fs::write(dir.join("desktop-relay.crt"), "CERT").unwrap();
    assert!(backend::load_relay_tls(&dir).unwrap().is_none(), "a lone certificate is not enough");
    std::fs::write(dir.join("desktop-relay.key"), "KEY").unwrap();
    let material = backend::load_relay_tls(&dir).unwrap().unwrap();
    assert_eq!(material.cert, b"CERT");
    assert_eq!(material.key, b"KEY");
    std::fs::create_dir_all(unreadable.join("desktop-relay.crt")).unwrap();
    std::fs::write(unreadable.join("desktop-relay.key"), "KEY").unwrap();
    assert!(backend::load_relay_tls(&unreadable).is_err(), "other read errors surface");
    // Only the files this test made, then its own (now empty) directories.
    for f in [dir.join("desktop-relay.crt"), dir.join("desktop-relay.key"), unreadable.join("desktop-relay.key")] {
        std::fs::remove_file(f).unwrap();
    }
    for d in [unreadable.join("desktop-relay.crt"), unreadable, dir, base] {
        std::fs::remove_dir(d).unwrap();
    }
}

#[tokio::test]
async fn relay_rewrites_usage_forwards_other_calls_verbatim_and_streams() {
    let (up, seen, _task) = fake_upstream(Duration::from_millis(300)).await;
    let events: Events = Arc::default();
    let observe = recorder(&events);
    let port = front(up, |c| {
        c.rewrite_usage = Some(Arc::new(|mut u: Value| {
            u["pool"] = json!("rewritten");
            Ok(u)
        }));
        c.observe = observe;
        c.max_body_bytes = 64;
    })
    .await;
    let origin = format!("http://127.0.0.1:{port}");
    let http = client();
    let last = || seen.lock().unwrap().last().cloned().unwrap();

    let usage = http.get(format!("{origin}/backend-api/wham/usage")).header("authorization", "Bearer t").header("chatgpt-account-id", "acct").send().await.unwrap();
    assert_eq!(usage.status(), 200);
    assert_eq!(usage.headers().get("x-upstream").unwrap(), "yes");
    assert_eq!(body(usage).await, json!({ "rate_limit": { "allowed": false }, "pool": "rewritten" }));
    assert_eq!(last().header("authorization"), Some("Bearer t"));
    assert_eq!(last().header("chatgpt-account-id"), Some("acct"));
    assert_eq!(last().header("accept-encoding"), Some("identity"));

    let posted = http.post(format!("{origin}/backend-api/conversation?x=1")).header("content-type", "application/json").body(r#"{"a":1}"#).send().await.unwrap();
    assert_eq!(posted.status(), 201);
    assert_eq!(posted.text().await.unwrap(), "echo");
    assert_eq!(last().method, "POST");
    assert_eq!(last().url, "/backend-api/conversation?x=1");
    assert_eq!(last().body, r#"{"a":1}"#);

    // Streamed as it comes: the first event arrives on its own, before the upstream sends the second.
    let stream = http.get(format!("{origin}/backend-api/stream")).send().await.unwrap();
    assert_eq!(stream.headers().get("content-type").unwrap(), "text/event-stream");
    let mut chunks = stream.bytes_stream();
    use futures_util::StreamExt;
    let first = chunks.next().await.unwrap().unwrap();
    assert_eq!(&first[..], b"data: one\n\n");
    let mut rest = Vec::new();
    while let Some(c) = chunks.next().await {
        rest.extend_from_slice(&c.unwrap());
    }
    assert_eq!(rest, b"data: two\n\n");

    let broken = http.get(format!("{origin}/backend-api/broken-usage")).send().await.unwrap();
    assert_eq!(broken.text().await.unwrap(), "not json", "non-usage paths are never rewritten");

    let big = http.post(format!("{origin}/backend-api/conversation")).body("x".repeat(65)).send().await.unwrap();
    assert_eq!(big.status(), 413);
    assert_eq!(body(big).await, json!({ "error": "Request body exceeds 64 bytes" }));
    let to_conversation = seen.lock().unwrap().iter().filter(|r| r.url.starts_with("/backend-api/conversation")).count();
    assert_eq!(to_conversation, 1, "oversized body never reaches upstream");
    assert!(events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn relay_fails_open_when_the_usage_rewrite_throws_and_reports_it() {
    let (up, _seen, _task) = fake_upstream(Duration::from_millis(20)).await;
    let events: Events = Arc::default();
    let observe = recorder(&events);
    let port = front(up, |c| {
        c.rewrite_usage = Some(Arc::new(|_| Err("overlay broke".into())));
        c.observe = observe;
    })
    .await;
    let usage = client().get(format!("http://127.0.0.1:{port}/backend-api/wham/usage")).send().await.unwrap();
    assert_eq!(body(usage).await, json!({ "rate_limit": { "allowed": false } }));
    assert_eq!(*events.lock().unwrap(), vec![json!({ "type": "pool-usage-overlay-error", "message": "overlay broke" })]);
}

#[tokio::test]
async fn relay_answers_502_when_the_upstream_is_down() {
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up = closed.local_addr().unwrap().port();
    drop(closed);
    let port = front(up, |_| {}).await;
    let response = client().get(format!("http://127.0.0.1:{port}/backend-api/wham/usage")).send().await.unwrap();
    assert_eq!(response.status(), 502);
    let error = body(response).await["error"].as_str().unwrap().to_string();
    assert!(error.contains("unreachable"), "{error}");
}

/// A raw handshake on a new connection: the answer's head, and the connection.
async fn switch(port: u16, path: &str, host: &str, extra: &str) -> (String, TcpStream) {
    let mut tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n{extra}\r\n");
    tcp.write_all(req.as_bytes()).await.unwrap();
    let (head, _) = read_head(&mut tcp).await.expect("an answer");
    (head, tcp)
}

#[tokio::test]
async fn relay_tunnels_websocket_upgrades_and_refuses_others() {
    let (up, seen, _task) = fake_upstream(Duration::from_millis(20)).await;
    let port = front(up, |_| {}).await;
    let host = format!("127.0.0.1:{port}");
    let (head, mut tcp) = switch(port, "/backend-api/wham/remote/control/server?x=1", &host, "Authorization: Bearer t\r\n").await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    let last = seen.lock().unwrap().last().cloned().unwrap();
    assert!(last.upgrade);
    assert_eq!(last.url, "/backend-api/wham/remote/control/server?x=1");
    assert_eq!(last.header("authorization"), Some("Bearer t"));
    assert_eq!(last.header("host"), Some(format!("127.0.0.1:{up}").as_str()));
    tcp.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(5), tcp.read_exact(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf, b"ping");
    drop(tcp);

    // Agent Pools drops these connections (ECONNRESET); here they are answered, and never reach the upstream.
    let before = seen.lock().unwrap().len();
    let (other, _) = switch(port, "/other", &host, "").await;
    assert!(other.starts_with("HTTP/1.1 404"), "{other}");
    let (wrong_host, _) = switch(port, "/backend-api/x", &format!("[::1]:{port}"), "").await;
    assert!(wrong_host.starts_with("HTTP/1.1 403"), "{wrong_host}");
    let (cross_site, _) = switch(port, "/backend-api/x", &host, "Origin: https://evil.example\r\nSec-Fetch-Site: cross-site\r\n").await;
    assert!(cross_site.starts_with("HTTP/1.1 403"), "{cross_site}");
    assert_eq!(seen.lock().unwrap().len(), before);
}

#[tokio::test]
async fn only_the_listeners_own_hosts_and_browser_writes_from_named_origins() {
    let (up, seen, _task) = fake_upstream(Duration::from_millis(20)).await;
    let port = front(up, |c| c.allow_origins = vec!["https://chatgpt.com".into()]).await;
    let http = client();
    let url = format!("http://127.0.0.1:{port}/backend-api/conversation");
    let with_host = |host: &str| {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("host", host.parse().unwrap());
        h
    };
    let localhost = http.get(&url).headers(with_host(&format!("localhost:{port}"))).send().await.unwrap();
    assert_eq!(localhost.status(), 201);
    let v6 = http.get(&url).headers(with_host(&format!("[::1]:{port}"))).send().await.unwrap();
    assert_eq!(v6.status(), 403);
    assert_eq!(body(v6).await, json!({ "error": "Invalid host" }));
    let before = seen.lock().unwrap().len();
    let cross = http.post(&url).header("origin", "https://evil.example").header("sec-fetch-site", "cross-site").body("{}").send().await.unwrap();
    assert_eq!(cross.status(), 403, "a cross-site browser write is Corten's 403");
    assert_eq!(seen.lock().unwrap().len(), before);
    let named = http.post(&url).header("origin", "https://chatgpt.com").header("sec-fetch-site", "cross-site").body("{}").send().await.unwrap();
    assert_eq!(named.status(), 201, "an origin the relay names is let through");
    let unknown = http.get(format!("http://127.0.0.1:{port}/api/status")).send().await.unwrap();
    assert_eq!(unknown.status(), 404);
}
