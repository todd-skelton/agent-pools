//! Offline parity checks through real authenticated CONNECT + TLS + Corten sockets.
use base64::Engine;
use corten::axum::{
    Router,
    body::{Body, to_bytes},
    extract::Request,
    http::HeaderMap,
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use pools_gateway::claude_split::{self, Config, Handle};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::{
    TlsAcceptor, TlsConnector,
    rustls::{
        self,
        pki_types::{PrivatePkcs8KeyDer, ServerName},
    },
};

const LOCAL_KEY: &str = "fixture-local-proxy-key-32-characters";
#[derive(Clone)]
struct Seen {
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
    dropped: Arc<AtomicBool>,
}
struct Fixture {
    port: u16,
    client: reqwest::Client,
    pool: Arc<Mutex<Vec<Seen>>>,
    control: Arc<Mutex<Vec<Seen>>>,
    events: Arc<Mutex<Vec<Value>>>,
    handle: Handle,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    cert: Vec<u8>,
}
struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.handle.shutdown();
        for t in &self.tasks {
            t.abort();
        }
    }
}

fn pair() -> (TlsAcceptor, Vec<u8>) {
    let pair = rcgen::generate_simple_self_signed(vec!["api.anthropic.com".into()]).unwrap();
    let der = pair.cert.der().to_vec();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![pair.cert.der().clone()], PrivatePkcs8KeyDer::from(pair.key_pair.serialize_der()).into())
        .unwrap();
    (TlsAcceptor::from(Arc::new(config)), der)
}

async fn upstream() -> (u16, Arc<Mutex<Vec<Seen>>>, tokio::task::JoinHandle<()>) {
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let log = seen.clone();
    let router = Router::new().fallback(move |mut req: Request| {
        let log = log.clone();
        async move {
            let upgrade = req.headers().contains_key("upgrade").then(|| hyper::upgrade::on(&mut req));
            let (p, b) = req.into_parts();
            let mode = p.headers.get("x-fixture-mode").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            let bytes = to_bytes(b, 64 * 1024 * 1024).await.unwrap();
            let dropped = Arc::new(AtomicBool::new(false));
            log.lock().unwrap().push(Seen {
                path: p.uri.to_string(),
                headers: p.headers.clone(),
                body: bytes.to_vec(),
                dropped: dropped.clone(),
            });
            if let Some(on) = upgrade {
                let key = p.headers["sec-websocket-key"].as_bytes();
                let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key);
                tokio::spawn(async move {
                    let io = hyper_util::rt::TokioIo::new(on.await.unwrap());
                    let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(io, tokio_tungstenite::tungstenite::protocol::Role::Server, None).await;
                    while let Some(Ok(message)) = ws.next().await {
                        if ws.send(message).await.is_err() {
                            break;
                        }
                    }
                });
                return Response::builder()
                    .status(101)
                    .header("connection", "Upgrade")
                    .header("upgrade", "websocket")
                    .header("sec-websocket-accept", accept)
                    .body(Body::empty())
                    .unwrap();
            }
            if mode == "headers-slow" {
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            if mode == "long" {
                // Deltas for 32 s, then message_stop: a turn longer than the 30 s head timeout.
                let body = futures_util::stream::unfold(0, |n| async move {
                    if n > 8 {
                        return None;
                    }
                    if n > 0 {
                        tokio::time::sleep(Duration::from_secs(4)).await;
                    }
                    let chunk: &[u8] = if n == 8 {
                        b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
                    } else {
                        b"event: content_block_delta\ndata: {}\n\n"
                    };
                    Some((Ok::<_, std::io::Error>(chunk.to_vec()), n + 1))
                });
                return Response::builder().header("content-type", "text/event-stream").body(Body::from_stream(body)).unwrap();
            }
            if matches!(mode.as_str(), "sse" | "truncated" | "sse-error" | "idle" | "cancel") {
                let first = "event: content_block_delta\ndata: {\"delta\":\"hello 🦀\"}\n\n".as_bytes().to_vec();
                let stop = if mode == "sse-error" {
                    b"event: error\ndata: {\"type\":\"error\"}\n\n".to_vec()
                } else {
                    b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_vec()
                };
                let body = futures_util::stream::unfold((0, mode, Dropped(dropped)), move |(n, mode, guard)| {
                    let (first, stop) = (first.clone(), stop.clone());
                    async move {
                        let chunk = match n {
                            0 => first,
                            1 if mode == "cancel" => {
                                std::future::pending::<()>().await;
                                unreachable!()
                            }
                            1 if mode == "truncated" => return None,
                            1 => {
                                tokio::time::sleep(Duration::from_millis(if mode == "idle" { 600 } else { 150 })).await;
                                stop
                            }
                            _ => return None,
                        };
                        Some((Ok::<_, std::io::Error>(chunk), (n + 1, mode, guard)))
                    }
                });
                return Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(body))
                    .unwrap();
            }
            if mode == "rate-limit" {
                return Response::builder()
                    .status(429)
                    .header("retry-after", "2")
                    .body(Body::from("{\"limited\":true}"))
                    .unwrap();
            }
            Response::builder()
                .header("content-type", "application/json")
                .header("set-cookie", "fixture-upstream-cookie=1")
                .body(Body::from("{\"data\":[{\"id\":\"fixture\"}]}"))
                .unwrap()
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        corten::axum::serve(listener, router).await.unwrap();
    });
    (port, seen, task)
}

async fn fixture(tune: impl FnOnce(&mut Config)) -> Fixture {
    let (pool_port, pool, ptask) = upstream().await;
    let (control_port, control, ctask) = upstream().await;
    let (tls, cert) = pair();
    let events: Arc<Mutex<Vec<Value>>> = Arc::default();
    let e = events.clone();
    let mut config = Config::new(&format!("http://127.0.0.1:{pool_port}"), "fixture-pool-key", LOCAL_KEY, tls, i64::MAX);
    config.control_origin = format!("http://127.0.0.1:{control_port}");
    config.observe = Arc::new(move |v| e.lock().unwrap().push(v));
    tune(&mut config);
    let (app, handle) = claude_split::app(config).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        corten::serve_on(vec![listener], app).await.unwrap();
    });
    let proxy = reqwest::Proxy::all(format!("http://127.0.0.1:{port}")).unwrap().basic_auth("probe", LOCAL_KEY);
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .add_root_certificate(reqwest::Certificate::from_der(&cert).unwrap())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    Fixture {
        port,
        client,
        pool,
        control,
        events,
        handle,
        tasks: vec![ptask, ctask, task],
        cert,
    }
}

#[tokio::test]
async fn inference_and_control_have_separate_credentials_and_unchanged_bodies() {
    let f = fixture(|_| {}).await;
    let body = r#"{"model":"fixture","messages":[{"role":"assistant","content":[{"type":"thinking","signature":"signed-context","thinking":"private"},{"type":"tool_use","id":"toolu_kept","input":{"file_id":"local-data"}}]}],"tools":[{"name":"local","input_schema":{"properties":{"container":{"type":"string"}}}}],"context_management":{"edits":[{"type":"compact_20260112"}]}}"#;
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        let r = f
            .client
            .post(format!("https://api.anthropic.com{path}"))
            .header("authorization", "Bearer control-secret")
            .header("x-api-key", "control-api-key")
            .header("cookie", "control-cookie=1")
            .header("anthropic-beta", "fixture-beta")
            .header("x-claude-code-session-id", "one-session")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
        assert!(!r.headers().contains_key("set-cookie"));
        r.bytes().await.unwrap();
    }
    {
        let pool = f.pool.lock().unwrap();
        assert_eq!(pool.len(), 2);
        for p in pool.iter() {
            assert_eq!(p.body, body.as_bytes());
            assert_eq!(p.headers["authorization"], "Bearer fixture-pool-key");
            assert!(!p.headers.contains_key("cookie"));
            assert!(!p.headers.contains_key("x-api-key"));
            assert!(!p.headers.contains_key("proxy-authorization"));
            assert_eq!(p.headers["anthropic-beta"], "fixture-beta");
            assert_eq!(p.headers["x-claude-code-session-id"], "one-session");
        }
    }
    for path in ["/api/oauth/profile", "/v1/code/sessions/one?private=query"] {
        let r = f
            .client
            .get(format!("https://api.anthropic.com{path}"))
            .bearer_auth("control-secret")
            .header("cookie", "control-cookie=1")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.headers().contains_key("set-cookie"));
        r.bytes().await.unwrap();
    }
    let control = f.control.lock().unwrap();
    assert_eq!(control.len(), 2);
    assert_eq!(control[1].path, "/v1/code/sessions/one?private=query");
    for p in control.iter() {
        assert_eq!(p.headers["authorization"], "Bearer control-secret");
        assert_eq!(p.headers["cookie"], "control-cookie=1");
    }
    let logs = serde_json::to_string(&*f.events.lock().unwrap()).unwrap();
    for secret in ["control-secret", "fixture-pool-key", "control-cookie", "signed-context", "private=query"] {
        assert!(!logs.contains(secret));
    }
}

#[tokio::test]
async fn opt_in_model_identity_is_current_and_does_not_change_control_or_signed_history() {
    let f = fixture(|c| c.model_catalog = Some(pools_gateway::model_catalog::Catalog::from_registry(&json!({"models":{
        "gpt-fixture":{"provider":"codex","efforts":["low","high"],"admittedEfforts":["low","high"]},
        "claude-fixture":{"provider":"claude","efforts":["medium"],"admittedEfforts":["medium"]}
    }})))).await;
    for (model, effort, provider) in [("gpt-fixture","high","openai"),("claude-fixture","medium","anthropic")] {
        let body = json!({"model":model,"system":[{"type":"text","text":"original"}],"output_config":{"effort":effort},
            "messages":[{"role":"assistant","content":[{"type":"thinking","signature":"private-signature","thinking":"private-thought"},
            {"type":"tool_use","id":"private-tool-id","name":"Read","input":{}}]}]});
        let response = f.client.post("https://api.anthropic.com/v1/messages").bearer_auth("control-secret")
            .body(body.to_string()).send().await.unwrap();
        assert_eq!(response.status(), 200);
        response.bytes().await.unwrap();
        let seen = f.pool.lock().unwrap().last().unwrap().clone();
        let forwarded: Value = serde_json::from_slice(&seen.body).unwrap();
        assert_eq!(forwarded["messages"], body["messages"]);
        assert_eq!(forwarded["output_config"], body["output_config"]);
        assert_eq!(forwarded["system"][0], body["system"][0]);
        assert!(forwarded["system"][1]["text"].as_str().unwrap().contains(provider));
    }
    let response = f.client.post("https://api.anthropic.com/v1/messages")
        .body(r#"{"model":"gpt-fixture","output_config":{"effort":"ultra"}}"#).send().await.unwrap();
    assert_eq!(response.status(), 422);
    assert_eq!(f.pool.lock().unwrap().len(), 2);
    let control_body = r#"{"model":"gpt-fixture","private":"control-body"}"#;
    let response = f.client.post("https://api.anthropic.com/api/oauth/fixture").bearer_auth("control-secret")
        .body(control_body).send().await.unwrap();
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    assert_eq!(f.control.lock().unwrap()[0].body, control_body.as_bytes());
    let logs = serde_json::to_string(&*f.events.lock().unwrap()).unwrap();
    assert!(logs.contains("openai") && logs.contains("anthropic") && logs.contains("requestedEffort"));
    for secret in ["control-secret","private-signature","private-thought","private-tool-id","control-body"] { assert!(!logs.contains(secret)); }
}

#[tokio::test]
async fn thread_continuation_resolves_selection_without_resending_inherited_system() {
    let f = fixture(|c| c.model_catalog = Some(pools_gateway::model_catalog::Catalog::from_registry(&json!({"models":{
        "claude-fixture":{"provider":"claude","efforts":["medium"],"admittedEfforts":["medium"]}
    }})))).await;
    let body = json!({"model":"pool--claude-fixture--medium",
        "thread":{"type":"continue","previous_message_id":"msg_private"},
        "cache_control":{"type":"ephemeral"},"messages":[{"role":"user","content":"delta"}]});
    let response = f.client.post("https://api.anthropic.com/v1/messages")
        .body(body.to_string()).send().await.unwrap();
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    let seen = f.pool.lock().unwrap().last().unwrap().clone();
    let forwarded: Value = serde_json::from_slice(&seen.body).unwrap();
    let mut expected = body;
    expected["model"] = json!("claude-fixture");
    expected["output_config"] = json!({"effort":"medium"});
    assert_eq!(forwarded, expected);
    assert!(!serde_json::to_string(&*f.events.lock().unwrap()).unwrap().contains("msg_private"));
}

#[tokio::test]
async fn compressed_picker_selection_resolves_for_messages_and_token_counting() {
    let f = fixture(|c| c.model_catalog = Some(pools_gateway::model_catalog::Catalog::from_registry(&json!({"models":{
        "gpt-fixture":{"provider":"codex","efforts":["low","high"],"admittedEfforts":["high"]}
    }})))).await;
    use std::io::Write;
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        let body = json!({"model":"pool--gpt-fixture--high","messages":[],"output_config":{"effort":"low"}});
        let mut packed = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        packed.write_all(body.to_string().as_bytes()).unwrap();
        let response = f.client.post(format!("https://api.anthropic.com{path}"))
            .header("content-encoding", "gzip").body(packed.finish().unwrap()).send().await.unwrap();
        assert_eq!(response.status(), 200);
        response.bytes().await.unwrap();
        let seen = f.pool.lock().unwrap().last().unwrap().clone();
        assert!(!seen.headers.contains_key("content-encoding"));
        let forwarded: Value = serde_json::from_slice(&seen.body).unwrap();
        assert_eq!(forwarded["model"], "gpt-fixture");
        assert_eq!(forwarded["output_config"]["effort"], "high");
        assert!(forwarded["system"][0]["text"].as_str().unwrap().contains("provider=openai"));
    }
    let response = f.client.post("https://api.anthropic.com/v1/messages")
        .body(r#"{"model":"pool--gpt-fixture--low"}"#).send().await.unwrap();
    assert_eq!(response.status(), 422);
    assert_eq!(f.pool.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn remote_control_bridge_lifecycle_preserves_control_auth_without_logging_handles() {
    let f = fixture(|_| {}).await;
    let body = r#"{"directory":"private-directory","max_sessions":4}"#;
    for (method, path) in [
        (reqwest::Method::GET, "/api/directory/plugins?private=query"),
        (reqwest::Method::GET, "/api/frame/contract/latest"),
        (reqwest::Method::GET, "/mcp-registry/v0/servers"),
        (reqwest::Method::GET, "/v2/ccr-sessions/session_private/mcp"),
        (reqwest::Method::POST, "/v2/ccr-sessions/session_private/mcp"),
        (reqwest::Method::DELETE, "/v2/ccr-sessions/session_private/mcp"),
        (reqwest::Method::GET, "/api/claude_code_grove"),
        (reqwest::Method::POST, "/api/claude_code/metrics"),
        (reqwest::Method::GET, "/v1/session_ingress/session/session_private"),
        (reqwest::Method::POST, "/v1/sessions/session_private/archive"),
        (reqwest::Method::POST, "/v1/environments/bridge"),
        (reqwest::Method::DELETE, "/v1/environments/bridge/env_private"),
        (reqwest::Method::POST, "/v1/environments/bridge/env_private/offline"),
        (reqwest::Method::POST, "/v1/environments/env_private/bridge/reconnect"),
        (reqwest::Method::GET, "/v1/environments/env_private/work/poll?private=query"),
        (reqwest::Method::POST, "/v1/environments/env_private/work/work_private/ack"),
        (reqwest::Method::POST, "/v1/environments/env_private/work/work_private/stop"),
        (reqwest::Method::POST, "/v1/environments/env_private/work/work_private/heartbeat"),
    ] {
        let response = f.client.request(method, format!("https://api.anthropic.com{path}"))
            .bearer_auth("control-secret").header("cookie", "control-cookie=1").body(body).send().await.unwrap();
        assert_eq!(response.status(), 200, "{path}");
        response.bytes().await.unwrap();
    }
    assert!(f.pool.lock().unwrap().is_empty());
    let control = f.control.lock().unwrap();
    assert_eq!(control.len(), 18);
    for request in control.iter() {
        assert_eq!(request.headers["authorization"], "Bearer control-secret");
        assert_eq!(request.headers["cookie"], "control-cookie=1");
        assert!(!request.headers.contains_key("proxy-authorization"));
        assert_eq!(request.body, body.as_bytes());
    }
    drop(control);
    let events = f.events.lock().unwrap();
    assert!(events.iter().any(|e| e["endpoint"] == "/v1/environments/bridge" && e["status"] == 200));
    let logs = serde_json::to_string(&*events).unwrap();
    for secret in ["control-secret", "fixture-pool-key", "env_private", "work_private", "session_private", "private=query", "private-directory"] {
        assert!(!logs.contains(secret));
    }
}

#[tokio::test]
async fn unsupported_resources_paths_encodings_and_oversized_bodies_never_reach_upstream() {
    let f = fixture(|c| c.max_body_bytes = 1024).await;
    for (path, body, encoding, status) in [
        ("/v1/files", "{}", "identity", 501),
        ("/v1/code/agent-proxy", "{}", "identity", 501),
        ("/api/oauth/files/x", "{}", "identity", 501),
        ("/v1/environments/env_private/work/work_private/inference", "{}", "identity", 501),
        ("/v1/environments/env_private/work/poll", "{}", "identity", 501),
        ("/v1/sessions", "{}", "identity", 501),
        ("/v2/ccr-sessions/session_private/agent-proxy", "{}", "identity", 501),
        ("/v2/ccr-sessions/session_private/teleport/conversations/conversation_private/completion", "{}", "identity", 501),
        ("/v2/ccr-sessions/session_private/mcp/messages", "{}", "identity", 501),
        ("/api/frame/contract/latest", "{}", "identity", 501),
        ("/api/directory/plugins", "{}", "identity", 501),
        ("/mcp-registry/v0/servers", "{}", "identity", 501),
        ("/v1/environments/env_private/work/work_private/new-private-api?secret=query", "{}", "identity", 501),
        ("/v1/messages", "{\"container\":\"x\"}", "identity", 422),
        ("/v1/messages", "[]", "identity", 400),
        ("/v1/messages", "{}", "zstd", 415),
    ] {
        let r = f
            .client
            .post(format!("https://api.anthropic.com{path}"))
            .header("content-encoding", encoding)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), status);
        r.bytes().await.unwrap();
    }
    let r = f
        .client
        .post("https://api.anthropic.com/v1/messages")
        .body(" ".repeat(2048))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    assert!(f.pool.lock().unwrap().is_empty());
    assert!(f.control.lock().unwrap().is_empty());
    let logs = serde_json::to_string(&*f.events.lock().unwrap()).unwrap();
    for secret in ["env_private", "work_private", "new-private-api", "secret=query"] {
        assert!(!logs.contains(secret));
    }
}

#[tokio::test]
async fn compressed_inline_attachments_are_forwarded_byte_for_byte() {
    use std::io::Write;
    let f = fixture(|_| {}).await;
    let body = json!({"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}},{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"cGRm"}}]}]}).to_string();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(body.as_bytes()).unwrap();
    let gzip = gzip.finish().unwrap();
    let mut deflate = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    deflate.write_all(body.as_bytes()).unwrap();
    let deflate = deflate.finish().unwrap();
    let mut br = Vec::new();
    {
        let mut w = brotli::CompressorWriter::new(&mut br, 4096, 4, 22);
        w.write_all(body.as_bytes()).unwrap();
    }
    for (encoding, bytes) in [("gzip", gzip), ("deflate", deflate), ("br", br)] {
        let r = f
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("content-encoding", encoding)
            .body(bytes.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.bytes().await.unwrap();
        let p = f.pool.lock().unwrap().last().unwrap().clone();
        assert_eq!(p.body, bytes);
        assert_eq!(p.headers["content-encoding"], encoding);
    }
}

#[tokio::test]
async fn sse_streams_incrementally_and_rejects_truncation_without_replay() {
    let f = fixture(|_| {}).await;
    let r = f
        .client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-fixture-mode", "sse")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let mut chunks = r.bytes_stream();
    let first = tokio::time::timeout(Duration::from_millis(100), chunks.next()).await.unwrap().unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("hello 🦀"));
    let mut all = first.to_vec();
    while let Some(c) = chunks.next().await {
        all.extend_from_slice(&c.unwrap());
    }
    assert!(String::from_utf8_lossy(&all).contains("message_stop"));
    for mode in ["truncated", "sse-error"] {
        let r = f
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-fixture-mode", mode)
            .body("{}")
            .send()
            .await
            .unwrap();
        let result = r.bytes().await;
        if mode == "truncated" {
            assert!(result.is_err());
        } else {
            assert!(String::from_utf8_lossy(&result.unwrap()).contains("event: error"));
        }
    }
    assert_eq!(f.pool.lock().unwrap().len(), 3);
    assert!(f.control.lock().unwrap().is_empty());
    let events = f.events.lock().unwrap();
    assert!(events.iter().any(|e| e["error"] == "INCOMPLETE_SSE" && e["complete"] == false));
    assert!(events.iter().any(|e| e["error"] == "SSE_API_ERROR" && e["complete"] == false));
}

#[tokio::test]
async fn rate_limits_header_timeouts_and_idle_streams_do_not_fall_back_or_retry() {
    let f = fixture(|c| {
        c.header_timeout = Duration::from_millis(150);
        c.idle_timeout = Duration::from_millis(200);
    })
    .await;
    let r = f
        .client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-fixture-mode", "rate-limit")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers()["retry-after"], "2");
    r.bytes().await.unwrap();
    let r = f
        .client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-fixture-mode", "headers-slow")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 504);
    r.bytes().await.unwrap();
    let r = f
        .client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-fixture-mode", "idle")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert!(r.bytes().await.is_err());
    assert_eq!(f.pool.lock().unwrap().len(), 3);
    assert!(f.control.lock().unwrap().is_empty());
}

async fn head(socket: &mut (impl tokio::io::AsyncRead + Unpin)) -> String {
    let mut bytes = Vec::new();
    loop {
        bytes.push(socket.read_u8().await.unwrap());
        if bytes.ends_with(b"\r\n\r\n") {
            return String::from_utf8(bytes).unwrap();
        }
        assert!(bytes.len() < 16384);
    }
}

async fn tls_connection(f: &Fixture) -> tokio_rustls::client::TlsStream<TcpStream> {
    let mut tcp = TcpStream::connect(("127.0.0.1", f.port)).await.unwrap();
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("probe:{LOCAL_KEY}"));
    tcp.write_all(format!("CONNECT api.anthropic.com:443 HTTP/1.1\r\nHost: api.anthropic.com:443\r\nProxy-Authorization: Basic {auth}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    assert!(head(&mut tcp).await.starts_with("HTTP/1.1 200"));
    let mut roots = rustls::RootCertStore::empty();
    roots.add(f.cert.clone().into()).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("api.anthropic.com").unwrap(), tcp)
        .await
        .unwrap()
}

#[tokio::test]
async fn control_websocket_preserves_auth_and_bidirectional_bytes_and_stops() {
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
    let f = fixture(|_| {}).await;
    let tls = tls_connection(&f).await;
    let mut request = "wss://api.anthropic.com/v1/code/sessions/one/events?token=fixture-query"
        .into_client_request()
        .unwrap();
    request.headers_mut().insert("authorization", "Bearer control-secret".parse().unwrap());
    let (mut ws, response) = tokio_tungstenite::client_async(request, tls).await.unwrap();
    assert_eq!(response.status(), 101);
    ws.send(Message::text("unchanged 🦀")).await.unwrap();
    assert_eq!(ws.next().await.unwrap().unwrap().into_text().unwrap(), "unchanged 🦀");
    assert!(f.pool.lock().unwrap().is_empty());
    assert_eq!(f.control.lock().unwrap()[0].headers["authorization"], "Bearer control-secret");
    f.handle.shutdown();
    assert!(tokio::time::timeout(Duration::from_secs(1), ws.next()).await.is_ok());
    let log = serde_json::to_string(&*f.events.lock().unwrap()).unwrap();
    assert!(!log.contains("fixture-query"));
    assert!(!log.contains("control-secret"));
}

#[tokio::test]
async fn noncanonical_inner_requests_and_host_rebinding_never_reach_upstream() {
    let f = fixture(|_| {}).await;
    for (path, host) in [
        ("/v1/%2e%2e/messages", "api.anthropic.com"),
        ("/v1%2fmessages", "api.anthropic.com"),
        ("//v1/messages", "api.anthropic.com"),
        ("/v1/messages", "localhost"),
        ("https://api.anthropic.com/v1/messages", "api.anthropic.com"),
    ] {
        let mut tls = tls_connection(&f).await;
        tls.write_all(format!("POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").as_bytes())
            .await
            .unwrap();
        let response = head(&mut tls).await;
        assert!(!response.starts_with("HTTP/1.1 200"), "{path}: {response}");
    }
    assert!(f.pool.lock().unwrap().is_empty());
    assert!(f.control.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_tunnel_outlives_the_head_timeout_while_it_carries_a_stream() {
    let f = fixture(|_| {}).await;
    let mut tls = tls_connection(&f).await;
    tls.write_all(b"POST /v1/messages HTTP/1.1\r\nHost: api.anthropic.com\r\nx-fixture-mode: long\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
        .await
        .unwrap();
    let mut all = Vec::new();
    let mut buf = [0u8; 4096];
    let ended = tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            match tls.read(&mut buf).await {
                Ok(0) => return "eof",
                Ok(n) => all.extend_from_slice(&buf[..n]),
                Err(_) => return "error",
            }
            if String::from_utf8_lossy(&all).contains("message_stop") {
                return "message_stop";
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(ended, "message_stop", "the tunnel ended early: {}", String::from_utf8_lossy(&all));
    assert!(String::from_utf8_lossy(&all).starts_with("HTTP/1.1 200"));
    drop(tls);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(f.events.lock().unwrap().iter().any(|e| e["route"] == "pool-inference" && e["complete"] == true));
}

#[tokio::test]
async fn client_cancellation_closes_the_pool_stream_without_replay() {
    let f = fixture(|_| {}).await;
    let r = f
        .client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-fixture-mode", "cancel")
        .body("{}")
        .send()
        .await
        .unwrap();
    let mut chunks = r.bytes_stream();
    assert!(chunks.next().await.unwrap().is_ok());
    drop(chunks);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if f.pool.lock().unwrap()[0].dropped.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.pool.lock().unwrap().len(), 1);
    assert!(f.control.lock().unwrap().is_empty());
    assert!(
        f.events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e["error"] == "CLIENT_CANCELLED" && e["complete"] == false)
    );
}

#[tokio::test]
async fn compressed_expansion_is_bounded_before_forwarding() {
    use std::io::Write;
    let f = fixture(|c| c.max_body_bytes = 4096).await;
    let body = json!({"value":"x".repeat(1024 * 1024)}).to_string();
    let mut w = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    w.write_all(body.as_bytes()).unwrap();
    let packed = w.finish().unwrap();
    assert!(packed.len() < 4096);
    let r = f
        .client
        .post("https://api.anthropic.com/v1/messages")
        .header("content-encoding", "gzip")
        .body(packed)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    assert!(f.pool.lock().unwrap().is_empty());
    assert!(f.control.lock().unwrap().is_empty());
}

#[tokio::test]
async fn hot_leaf_reload_preserves_profile_trust_and_existing_connections() {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign];
    let ca = params.self_signed(&key).unwrap();
    let leaf = || {
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["api.anthropic.com".into()])
            .unwrap()
            .signed_by(&leaf_key, &ca, &key)
            .unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into())
            .unwrap();
        TlsAcceptor::from(Arc::new(config))
    };
    let f = fixture(|c| c.tls = leaf()).await;
    let make_client = || {
        reqwest::Client::builder()
            .proxy(
                reqwest::Proxy::all(format!("http://127.0.0.1:{}", f.port))
                    .unwrap()
                    .basic_auth("probe", LOCAL_KEY),
            )
            .add_root_certificate(reqwest::Certificate::from_der(ca.der()).unwrap())
            .build()
            .unwrap()
    };
    let first = make_client();
    assert_eq!(first.get("https://api.anthropic.com/v1/models").send().await.unwrap().status(), 200);
    f.handle.replace_tls(leaf(), i64::MAX);
    assert_eq!(first.get("https://api.anthropic.com/v1/models").send().await.unwrap().status(), 200);
    assert_eq!(make_client().get("https://api.anthropic.com/v1/models").send().await.unwrap().status(), 200);
}

#[tokio::test]
async fn an_unavailable_pool_recovers_on_the_next_request_without_control_fallback() {
    let vacant = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = vacant.local_addr().unwrap().port();
    drop(vacant);
    let f = fixture(|c| c.pool_origin = format!("http://127.0.0.1:{port}")).await;
    assert_eq!(f.client.get("https://api.anthropic.com/v1/models").send().await.unwrap().status(), 502);
    let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        head(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"data\":[]}")
            .await
            .unwrap();
    });
    let r = f.client.get("https://api.anthropic.com/v1/models").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "{\"data\":[]}");
    assert!(f.control.lock().unwrap().is_empty());
    task.await.unwrap();
}

#[tokio::test]
async fn connect_requires_auth_and_exact_allowed_host_port_and_certificate_health() {
    let f = fixture(|_| {}).await;
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("probe:{LOCAL_KEY}"));
    for (authority, credentials, expected) in [
        ("api.anthropic.com:443", "", "407"),
        ("api.anthropic.com:444", auth.as_str(), "403"),
        ("api.anthropic.com.evil.test:443", auth.as_str(), "421"),
    ] {
        let mut tcp = TcpStream::connect(("127.0.0.1", f.port)).await.unwrap();
        tcp.write_all(format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Authorization: Basic {credentials}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        assert!(head(&mut tcp).await.contains(expected));
    }
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://127.0.0.1:{}/health", f.port))
        .header("proxy-authorization", format!("Basic {auth}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let health: Value = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
    assert_eq!(health["certificateValid"], true);
    let (tls, _) = pair();
    f.handle.replace_tls(tls, 0);
    assert!(f.client.get("https://api.anthropic.com/v1/models").send().await.is_err());
}

#[tokio::test]
async fn upstream_tls_is_verified_and_a_failed_pool_never_reaches_control() {
    let (tls, _) = pair();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let tls = tls.clone();
            tokio::spawn(async move {
                let _ = tls.accept(tcp).await;
            });
        }
    });
    let f = fixture(|c| c.pool_origin = format!("https://127.0.0.1:{port}")).await;
    let r = f.client.get("https://api.anthropic.com/v1/models").send().await.unwrap();
    assert_eq!(r.status(), 502);
    assert!(f.control.lock().unwrap().is_empty());
    task.abort();
}
