//! The shadow over an install made from the golden inputs: each parity check, run through the
//! files as the live install lays them out, matches the golden outputs (which the JavaScript
//! wrote), and the app serves pool-core's summary through Corten.

use std::time::Duration;

use corten::axum::body::Body;
use corten::axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pool_core::js;
use serde_json::Value;
use tower::ServiceExt;

use crate::parity::{self, Outcome};
use crate::testing::{read, snapshot};
use crate::watch::{Shadow, now_ms};

fn assert_match(c: &parity::Check) {
    assert_eq!(
        c.outcome,
        Outcome::Match,
        "{:?}: {}",
        c.kind,
        c.detail.as_deref().unwrap_or("")
    );
}

#[test]
fn summary_check_matches_the_golden_summary() {
    let t = snapshot("summary", false);
    let i = t.install();
    let c = parity::check_summary(
        &read("outputs/summary/now.json"),
        i.json("state/pool-state.json").as_ref(),
        i.json("state/capacity-forecast.json").as_ref(),
        i.json("state/usage-value.json").as_ref(),
        0.0,
    );
    assert_match(&c);
    // A summary from another moment is not compared.
    let mut later = read("outputs/summary/now.json");
    later["forecastGeneratedAt"] = Value::from("2026-10-27T02:43:33.487Z");
    let c = parity::check_summary(
        &later,
        i.json("state/pool-state.json").as_ref(),
        i.json("state/capacity-forecast.json").as_ref(),
        i.json("state/usage-value.json").as_ref(),
        0.0,
    );
    assert_eq!(c.outcome, Outcome::Unaligned);
}

#[test]
fn forecast_check_matches_the_golden_forecast() {
    let t = snapshot("forecast", false);
    let install = t.install();
    let pool = read("inputs/pool-state.json");
    let previous = read("inputs/capacity-forecast.json");
    let inp = parity::ForecastInputs {
        install: &install,
        pool: &pool,
        previous: Some(&previous),
    };
    assert_match(&parity::check_forecast(
        &read("outputs/forecast/now.json"),
        &inp,
        0.0,
    ));
    // Without its memory the document is another one, and the check says where.
    let inp = parity::ForecastInputs {
        install: &install,
        pool: &pool,
        previous: None,
    };
    let c = parity::check_forecast(&read("outputs/forecast/now.json"), &inp, 0.0);
    assert_eq!(c.outcome, Outcome::Differs);
    assert!(c.detail.unwrap().starts_with("forecast."));
}

#[test]
fn value_check_matches_the_golden_report() {
    let t = snapshot("value", true);
    let install = t.install();
    let pool = read("inputs/pool-state.json");
    let forecast = read("outputs/forecast/now.json");
    let inp = parity::ValueInputs {
        install: &install,
        pool: &pool,
        forecast: Some(&forecast),
        ledger_until: f64::INFINITY,
        samples_until: f64::INFINITY,
        ledger_rows: None,
    };
    assert_match(&parity::check_value(
        &read("outputs/value/deployed.json"),
        &inp,
        0.0,
    ));
}

async fn get(app: corten::axum::Router, path: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .uri(path)
        .header("host", "localhost")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn router(t: &crate::testing::TempInstall) -> corten::axum::Router {
    let shadow = Shadow::new(
        t.install(),
        "http://127.0.0.1:9".into(),
        t.dir.join("parity.jsonl"),
    );
    crate::app::app(
        shadow,
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static"),
        Duration::from_secs(5),
        Duration::from_secs(60),
    )
    .router()
}

#[tokio::test]
async fn api_summary_is_pool_cores_summary_in_javascripts_bytes() {
    let t = snapshot("api", false);
    let (status, body) = get(router(&t), "/api/summary").await;
    assert_eq!(status, StatusCode::OK);
    let doc: Value = serde_json::from_str(&body).unwrap();
    // Built at the request's moment: rebuilt here at that moment, it is the same, byte for byte.
    let now = js::date_parse_value(doc.get("generatedAt"));
    let again = parity::summary_now(&t.install(), now).unwrap();
    assert_eq!(body, js::stringify(&again));
    assert_eq!(doc["format"], "agent-pools-summary/v1");
    assert_eq!(doc["accounts"].as_array().unwrap().len(), 7);
}

#[tokio::test]
async fn api_status_leaves_out_credit_ids_and_auth() {
    let t = snapshot("status", false);
    let (status, body) = get(router(&t), "/api/status").await;
    assert_eq!(status, StatusCode::OK);
    let doc: Value = serde_json::from_str(&body).unwrap();
    let accounts = doc["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 7);
    for a in accounts {
        for k in ["credits", "auth_index", "accountId"] {
            assert!(a.get(k).is_none(), "{k} is left out");
        }
        assert!(a["creditExpiries"].is_array());
    }
    for k in ["processes", "holds", "usageLedger"] {
        assert_eq!(doc[k], Value::Null, "{k} is the live service's memory");
    }
    assert_eq!(doc["providers"]["claude"]["loginId"], "anthropic");
    assert_eq!(doc["resetPolicy"]["claude"]["redemption"], "refill");
    assert_eq!(doc["capacity"]["format"], "capacity-forecast/v1");
}

#[tokio::test]
async fn pages_render() {
    let t = snapshot("pages", false);
    let (status, home) = get(router(&t), "/summary").await;
    assert_eq!(status, StatusCode::OK, "{home}");
    for text in ["Codex", "Claude", "bankedCredits", "Monthly cost"] {
        assert!(home.contains(text), "the summary page shows {text}");
    }
    // The live dashboard's four tabs.
    for (path, texts) in [
        ("/", &["Needs attention", "account-weeks left this cycle", "Next 14 days", "Healthy"][..]),
        ("/accounts", &["Weekly reset", "Routing order: soonest weekly reset first", " accounts</span>"][..]),
        ("/accounts?filter=credits", &["aria-current=\"page\">Has credits</a>"][..]),
        ("/usage", &["Requests", "By model", "Quota left on the table"][..]),
        ("/policy", &["How sessions are routed", "Which credit goes first", "id=\"config\""][..]),
    ] {
        let (status, page) = get(router(&t), path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {page}");
        for text in texts {
            assert!(page.contains(text), "{path} shows {text}");
        }
        // Nothing that changes the pool is live here.
        assert!(!page.contains("<form"), "{path} has no forms");
    }
    // An account opened from its link shows its detail row.
    let status_doc = parity::status_now(&t.install(), &Default::default(), crate::watch::now_ms()).unwrap();
    let id = status_doc["accounts"][0]["id"].as_str().unwrap().to_string();
    let (status, page) = get(router(&t), &format!("/accounts?open={id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Quota windows") && page.contains(&format!("<code>{id}</code>")), "{id} is open");
    let (status, page) = get(router(&t), "/parity").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("Recent checks"));
    let (status, json) = get(router(&t), "/api/parity").await;
    assert_eq!(status, StatusCode::OK);
    let doc: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(doc["format"], "agent-pools-shadow-parity/v1");
    assert_eq!(doc["kinds"]["forecast"]["matched"], 0);
}

#[tokio::test]
async fn a_tab_is_kept_current_over_live() {
    let t = snapshot("live", false);
    // The page names its stream and the cursor it was drawn at.
    let (status, page) = get(router(&t), "/usage").await;
    assert_eq!(status, StatusCode::OK);
    let at = page.find("<corten-live src=\"/live?tab=usage\" since=\"").expect("the body is live");
    let since = &page[at..].split('"').nth(3).unwrap().to_string();
    assert_eq!(since.len(), 16);
    // Without a cursor the stream draws the tab at once: the body and the header's two parts.
    let res = router(&t)
        .oneshot(Request::builder().uri("/live?tab=usage").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let mut body = res.into_body();
    let frame = tokio::time::timeout(Duration::from_secs(10), body.frame()).await.unwrap().unwrap().unwrap();
    let event = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
    assert!(event.starts_with("event: patch\n"), "{event}");
    for id in ["id=\"dash-body\"", "id=\"att-count\"", "id=\"fresh\"", "By model"] {
        assert!(event.contains(id), "the fragment has {id}");
    }
    // At the page's cursor nothing is sent while the tab is unchanged.
    let res = router(&t)
        .oneshot(Request::builder().uri("/live?tab=usage").header("last-event-id", since.as_str()).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let mut body = res.into_body();
    let quiet = tokio::time::timeout(Duration::from_millis(1500), body.frame()).await;
    assert!(quiet.is_err(), "nothing until the tab changes");
}

#[tokio::test]
async fn a_save_reaches_an_open_tab_at_once() {
    let t = snapshot("realtime", false);
    let shadow = Shadow::new(t.install(), "http://127.0.0.1:9".into(), t.dir.join("parity.jsonl"));
    let app = crate::app::app(
        shadow.clone(),
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static"),
        Duration::from_secs(5),
        Duration::from_secs(60),
    )
    .router();
    let (_, page) = get(app.clone(), "/policy").await;
    let at = page.find("since=\"").unwrap() + 7;
    let since = page[at..at + 16].to_string();
    let res = app
        .oneshot(Request::builder().uri("/live?tab=policy").header("host", "localhost").header("last-event-id", since.as_str()).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let mut body = res.into_body();
    // The stream reads its inputs and finds the page current: nothing is sent.
    assert!(tokio::time::timeout(Duration::from_millis(1500), body.frame()).await.is_err());
    // The pool's automation switch is turned off and saved: the open tab says so at once.
    let path = t.dir.join("state/pool-state.json");
    let mut pool: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(pool["resetAutomation"], Value::Bool(true));
    pool["resetAutomation"] = Value::Bool(false);
    std::fs::write(&path, serde_json::to_string(&pool).unwrap()).unwrap();
    let started = std::time::Instant::now();
    shadow.changed_inputs();
    let frame = tokio::time::timeout(Duration::from_secs(5), body.frame()).await.unwrap().unwrap().unwrap();
    let event = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
    assert!(event.contains("resetAutomation is false in state/pool-state.json"), "{event}");
    assert!(started.elapsed() < Duration::from_secs(1), "sent in {:?}", started.elapsed());
}

#[tokio::test]
async fn no_pool_state_is_unavailable() {
    let t = snapshot("missing", false);
    std::fs::remove_file(t.dir.join("state/pool-state.json")).unwrap();
    let (status, _) = get(router(&t), "/api/summary").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn a_rebound_host_is_refused() {
    let t = snapshot("host", false);
    let req = Request::builder()
        .uri("/api/summary")
        .header("host", "attacker.example")
        .body(Body::empty())
        .unwrap();
    let res = router(&t).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::MISDIRECTED_REQUEST);
}

async fn send_json(app: corten::axum::Router, method: &str, path: &str, origin: Option<&str>, body: &str) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path).header("host", "localhost").header("content-type", "application/json");
    if let Some(o) = origin {
        req = req.header("origin", o);
    }
    let res = app.oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// The copy's own API (pool-service.mjs's): health and holds from its proxy watch, and the
/// writes (refresh, login, callback) through the stand-in proxy, behind the same-origin JSON check.
#[tokio::test]
async fn a_copy_answers_the_services_api_through_the_stand_in() {
    let t = snapshot("copy-api", false);
    let key = "test-key".to_string();
    let proxy = crate::writer::fake_proxy::start(t.dir.clone(), key.clone(), None).await.unwrap();
    let io = std::sync::Arc::new(crate::writer::io::FileIo::new(t.dir.clone(), proxy, key));
    let copy = std::sync::Arc::new(crate::writer::Copy::new(io));
    let shadow = Shadow::new(t.install(), "(a copy)".into(), t.dir.join("parity.jsonl"));
    let app = crate::app::app_copy(
        shadow,
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static"),
        copy.clone(),
        Default::default(),
        Default::default(),
    )
    .router();

    let (status, body) = get(app.clone(), "/api/health").await;
    assert_eq!(status, StatusCode::OK);
    let doc: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(doc["service"], "agent-pools");
    assert_eq!(doc["proxyHealthy"], false, "not checked yet");
    crate::writer::health::check(&copy.io, &copy.health, &copy.trigger).await;
    let (_, body) = get(app.clone(), "/api/health").await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["proxyHealthy"], true);
    let (_, body) = get(app.clone(), "/api/status").await;
    let holds = &serde_json::from_str::<Value>(&body).unwrap()["holds"];
    assert_eq!((&holds["enabled"], &holds["active"], &holds["turns"]), (&Value::Bool(true), &Value::from(0), &Value::from(0)));

    let origin = Some("http://localhost");
    let (status, body) = send_json(app.clone(), "POST", "/api/refresh", None, "{}").await;
    assert_eq!((status, body), (StatusCode::FORBIDDEN, serde_json::json!({ "error": "Same-origin JSON required" })));
    let (status, body) = send_json(app.clone(), "POST", "/api/refresh", origin, "").await;
    assert_eq!((status, body), (StatusCode::ACCEPTED, serde_json::json!({ "ok": true })));

    let (status, body) = send_json(app.clone(), "POST", "/api/login", origin, r#"{"provider":"codex"}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["url"], "https://example.invalid/stand-in-sign-in");
    let (status, body) = send_json(app.clone(), "POST", "/api/login", origin, r#"{"provider":"nope"}"#).await;
    assert_eq!((status, body), (StatusCode::INTERNAL_SERVER_ERROR, serde_json::json!({ "error": "Unknown provider" })));

    let (status, body) = send_json(app.clone(), "POST", "/api/callback", origin, r#"{"provider":"anthropic","redirect_url":"http://localhost/cb?code=x"}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    let (status, body) = send_json(app.clone(), "POST", "/api/callback", origin, r#"{"provider":"claude","redirect_url":"x"}"#).await;
    assert_eq!((status, body), (StatusCode::INTERNAL_SERVER_ERROR, serde_json::json!({ "error": "Invalid callback" })), "a provider id is not a loginId");
    let (status, _) = send_json(app.clone(), "POST", "/api/login", origin, &format!(r#"{{"provider":"{}"}}"#, "x".repeat(9000))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "over 8 KiB");

    // The stand-in saw the sign-in calls; nothing else was written outside the temp install.
    let log = std::fs::read_to_string(t.dir.join("logs/fake-proxy.jsonl")).unwrap();
    assert!(log.contains("codex-auth-url") && log.contains("oauth-callback"));
    std::fs::remove_file(t.dir.join("logs/fake-proxy.jsonl")).unwrap();
}

/// The copy's ledger end to end: the stand-in's queue drained into the day's raw file (the row
/// attributed to the account the stand-in named, with the quota from its headers), the stats in
/// `/api/status`, and a value report written from the ledger.
#[tokio::test]
async fn a_copy_drains_the_stand_ins_queue_into_its_ledger_and_reports_value_from_it() {
    let t = snapshot("copy-ledger", false);
    let key = "test-key".to_string();
    let proxy = crate::writer::fake_proxy::start(t.dir.clone(), key.clone(), None).await.unwrap();
    let io = std::sync::Arc::new(crate::writer::io::FileIo::new(t.dir.clone(), proxy, key));
    let copy = std::sync::Arc::new(crate::writer::Copy::new(io));
    let shadow = Shadow::new(t.install(), "(a copy)".into(), t.dir.join("parity.jsonl"));
    let app = crate::app::app_copy(
        shadow,
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static"),
        copy.clone(),
        Default::default(),
        Default::default(),
    )
    .router();

    let (_, body) = get(app.clone(), "/api/status").await;
    let before = serde_json::from_str::<Value>(&body).unwrap()["usageLedger"].clone();
    assert_eq!((&before["rows"], &before["enabled"], &before["drainedAt"]), (&Value::from(0), &Value::Bool(true), &Value::Null));

    // The stand-in's first made-up request is due at its start.
    let stats = copy.ledger.drain(&*copy.io, now_ms).await;
    assert_eq!((&stats["rows"], &stats["dropped"], &stats["pending"], &stats["lastError"]), (&Value::from(1), &Value::from(0), &Value::from(0), &Value::Null), "{stats}");
    let (_, body) = get(app.clone(), "/api/status").await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["usageLedger"]["rows"], 1);

    let state: Value = serde_json::from_str(&std::fs::read_to_string(t.dir.join("state/pool-state.json")).unwrap()).unwrap();
    let first = &state["accounts"][0];
    let day: String = pool_core::js::iso(now_ms()).chars().take(10).collect();
    let file = t.dir.join(format!("logs/usage-ledger/raw-{day}.jsonl"));
    let text = std::fs::read_to_string(&file).unwrap();
    let row: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(row["acct"], first["id"]);
    assert_eq!(row.get("ai"), None, "the account is known");
    assert_eq!((&row["client"], &row["prov"], &row["ok"], &row["st"]), (&Value::from("stand-in"), &first["provider"], &Value::Bool(true), &Value::from(200)));
    assert!(row["quota"].is_object(), "quota from the stand-in's headers: {row}");

    let rows = crate::writer::report::write_value_report(&t.dir, now_ms()).unwrap();
    assert_eq!(rows, 1.0, "the report read the one row");
    let report: Value = serde_json::from_str(&std::fs::read_to_string(t.dir.join("state/usage-value.json")).unwrap()).unwrap();
    assert_eq!(report["format"], "usage-value/v1");
    let session: Value = serde_json::from_str(&std::fs::read_to_string(t.dir.join("state/session-usage.json")).unwrap()).unwrap();
    assert_eq!(session["format"], "session-usage/v1");

    for f in [file, t.dir.join("state/session-usage.json"), t.dir.join("logs/fake-proxy.jsonl")] {
        std::fs::remove_file(f).unwrap();
    }
}

/// The copy's inference relay and public gateway in front of the stand-in: Codex's sign-in over
/// HTTP and over a WebSocket, a registered machine's key at the gateway, wrong keys refused, and
/// every answered request in the ledger with the client that sent it.
#[tokio::test]
async fn the_copys_relay_and_gateway_answer_through_the_stand_in_into_the_ledger() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
    let t = snapshot("copy-edge", false);
    let keys = crate::writer::edge::Keys::new();
    keys.write(&t.dir).unwrap();
    let key = "test-key".to_string();
    let proxy = crate::writer::fake_proxy::start(t.dir.clone(), key.clone(), Some(keys.client_key.clone())).await.unwrap();
    let io = std::sync::Arc::new(crate::writer::io::FileIo::new(t.dir.clone(), proxy.clone(), key));
    let copy = crate::writer::Copy::new(io.clone());
    let relay = crate::writer::edge::start_inference(io.clone(), &proxy, 0).await.unwrap();
    let gateway = crate::writer::edge::start_gateway(io.clone(), &keys, &proxy, "http://127.0.0.1:9", 0).await.unwrap();
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let body = r#"{"model":"gpt-6.1-sol","stream":true,"input":[{"role":"user","content":"hi"}]}"#;

    let post = |port: u16, path: &str, auth: (&str, String)| {
        let mut r = http.post(format!("http://127.0.0.1:{port}{path}")).header("content-type", "application/json").header(auth.0, auth.1);
        if port == gateway {
            r = r.header("host", crate::writer::edge::GATEWAY_HOST).header("user-agent", "claude-cli/2.1.300 (external)");
        }
        r
    };
    let res = post(relay, "/v1/responses", ("authorization", format!("Bearer {}", keys.desktop_token))).body(body).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let text = res.text().await.unwrap();
    assert!(text.contains("event: response.completed") && text.contains("stand-in: no model was called"), "{text}");
    let res = post(relay, "/v1/responses", ("authorization", "Bearer nope".into())).body(body).send().await.unwrap();
    assert_eq!(res.status(), 401);
    // The stand-in takes only the pool's key, which the relay swapped in.
    let direct = http.post(format!("{proxy}/v1/responses")).header("authorization", format!("Bearer {}", keys.desktop_token)).body(body).send().await.unwrap();
    assert_eq!(direct.status(), 401);

    let mut req = format!("ws://127.0.0.1:{relay}/v1/responses").into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {}", keys.desktop_token).parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::text(r#"{"type":"response.create","model":"gpt-6-luna","input":[]}"#)).await.unwrap();
    let mut kinds = Vec::new();
    while kinds.len() < 3 {
        let Some(Ok(Message::Text(frame))) = ws.next().await else { panic!("the socket closed after {kinds:?}") };
        kinds.push(serde_json::from_str::<Value>(&frame).unwrap()["type"].as_str().unwrap().to_string());
    }
    assert_eq!(kinds, ["response.created", "response.output_text.delta", "response.completed"]);
    drop(ws);

    let messages = r#"{"model":"claude-opus-5-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#;
    let res = post(gateway, "/v1/messages", ("x-api-key", keys.machine_key.clone())).body(messages).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let answer: Value = serde_json::from_str(&res.text().await.unwrap()).unwrap();
    assert_eq!((&answer["type"], &answer["model"]), (&Value::from("message"), &Value::from("claude-opus-5-5")));
    let res = post(gateway, "/v1/messages", ("x-api-key", keys.client_key.clone())).body(messages).send().await.unwrap();
    assert_eq!(res.status(), 401, "the pool's own key is not a machine's");

    // Three answers queued, besides the stand-in's own first made-up request.
    let stats = copy.ledger.drain(&*copy.io, now_ms).await;
    assert_eq!(stats["lastError"], Value::Null, "{stats}");
    let day: String = pool_core::js::iso(now_ms()).chars().take(10).collect();
    let file = t.dir.join(format!("logs/usage-ledger/raw-{day}.jsonl"));
    let rows: Vec<Value> = std::fs::read_to_string(&file).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let served: Vec<(String, String, String)> = rows
        .iter()
        .filter(|r| r["client"] != "stand-in")
        .map(|r| (r["client"].as_str().unwrap().into(), r["prov"].as_str().unwrap().into(), r["model"].as_str().unwrap().into()))
        .collect();
    let s = |a: &str, b: &str, c: &str| (a.to_string(), b.to_string(), c.to_string());
    assert_eq!(served, [s("codex_cli_rs", "codex", "gpt-6.1-sol"), s("codex_cli_rs", "codex", "gpt-6-luna"), s("claude-cli", "claude", "claude-opus-5-5")]);
    assert!(rows.iter().filter(|r| r["client"] != "stand-in").all(|r| r["acct"].is_string()), "each on a known account");

    for f in [file, t.dir.join("logs/fake-proxy.jsonl"), t.dir.join("logs/inference-events.jsonl"), t.dir.join("logs/public-gateway-events.jsonl")] {
        let _ = std::fs::remove_file(f);
    }
    for f in ["codex-auth.json", "pool-keys.json", "keys.json"] {
        std::fs::remove_file(t.dir.join("stand-in").join(f)).unwrap();
    }
    std::fs::remove_dir(t.dir.join("stand-in")).unwrap();
}

#[tokio::test]
async fn the_read_only_shadow_serves_documents_but_no_writes() {
    let t = snapshot("ro-api", false);
    let app = router(&t);
    let (status, body) = get(app.clone(), "/api/capacity").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["format"], "capacity-forecast/v1");
    let (status, body) = get(app.clone(), "/api/value").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, std::fs::read_to_string(t.dir.join("state/usage-value.json")).unwrap(), "the file's own bytes");
    let (status, _) = get(app.clone(), "/api/health").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send_json(app.clone(), "POST", "/api/login", Some("http://localhost"), r#"{"provider":"codex"}"#).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

