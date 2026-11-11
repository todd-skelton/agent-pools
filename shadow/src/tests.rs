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

#[tokio::test]
async fn claude_project_status_is_opt_in_metadata_with_staleness() {
    let t = snapshot("claude-project-status", false);
    let (code, body) = get(router(&t), "/api/claude/projects").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["state"], "disabled");
    let profile = t.install().root().join("state/claude-split");
    std::fs::create_dir_all(&profile).unwrap();
    let install_path = t.install().root().join("state/install.json");
    let mut settings: Value = serde_json::from_slice(&std::fs::read(&install_path).unwrap()).unwrap();
    settings["claudeCode"] = serde_json::json!({"enabled":true});
    std::fs::write(install_path,settings.to_string()).unwrap();
    std::fs::write(profile.join("claude-project-status.json"),serde_json::json!({"schema":"agent-pools-claude-projects/v1","updatedAtMs":1,"state":"running","projects":[]}).to_string()).unwrap();
    let (code,body) = get(router(&t),"/api/claude/projects").await;
    assert_eq!(code,StatusCode::OK);
    let doc: Value=serde_json::from_str(&body).unwrap();
    assert_eq!(doc["state"],"running");
    assert_eq!(doc["stale"],true);
    assert!(!body.contains("proxyKey"));
    std::fs::remove_file(profile.join("claude-project-status.json")).unwrap();
    std::fs::remove_dir(profile).unwrap();
}

fn router(t: &crate::testing::TempInstall) -> corten::axum::Router {
    let shadow = Shadow::new(
        t.install(),
        "http://127.0.0.1:9".into(),
        t.dir.join("parity.jsonl"),
    );
    crate::app::app(
        shadow,
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
async fn the_service_pages_drop_the_shadow_marks() {
    let t = snapshot("service-marks", false);
    let (_, shadow_page) = get(router(&t), "/accounts").await;
    assert!(shadow_page.contains("Corten shadow:") && shadow_page.contains("(shadow)</title>") && shadow_page.contains("href=\"/parity\""));
    let shadow = Shadow::new(t.install(), "(the service: not compared)".into(), t.dir.join("parity.jsonl"));
    let _ = shadow.service.set(());
    let app = crate::app::app(shadow, Duration::from_secs(5), Duration::from_secs(60)).router();
    let (status, page) = get(app, "/accounts").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("<title>Accounts · Agent pools</title>"), "{page}");
    for mark in ["Corten shadow:", "(shadow)", "href=\"/parity\""] {
        assert!(!page.contains(mark), "the service's page has no {mark}");
    }
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

/// A form value as a browser encodes it.
fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

async fn send_form(app: corten::axum::Router, path: &str, body: &str) -> (StatusCode, String, Option<String>) {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "localhost")
        .header("origin", "http://localhost")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let location = res.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_string);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap(), location)
}

/// The dashboard's actions on a copy: "Add account" and each sign-in action open the sign-in
/// steps, which get the stand-in's sign-in page, check a pasted localhost link as the live page
/// does and pass it to the stand-in's `oauth-callback`; the banner says a sign-in is pending
/// until it is cancelled; a refresh form goes back to the row it was on.
#[tokio::test]
async fn a_copys_dashboard_signs_in_and_refreshes_through_the_stand_in() {
    let t = snapshot("copy-signin", false);
    let key = "test-key".to_string();
    let proxy = crate::writer::fake_proxy::start(t.dir.clone(), key.clone(), None).await.unwrap();
    let io = std::sync::Arc::new(crate::writer::io::FileIo::new(t.dir.clone(), proxy, key));
    let copy = std::sync::Arc::new(crate::writer::Copy::new(io));
    let shadow = Shadow::new(t.install(), "(a copy)".into(), t.dir.join("parity.jsonl"));
    let app = crate::app::app_copy(
        shadow.clone(),
        copy.clone(),
        Default::default(),
        Default::default(),
    )
    .router();

    let (status, home) = get(app.clone(), "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(home.contains(r#"<a href="/signin">Add account</a>"#), "{home}");
    assert!(!home.contains(" disabled "), "no action is disabled on a copy");
    assert!(home.contains(r#"<main class="dash__main"><div id="finish" class="dash__finish" hidden></div>"#), "in <main>, which a navigation swaps: {home}");
    let (_, accounts) = get(app.clone(), "/accounts").await;
    assert!(accounts.contains(r#"href="/signin?provider=codex">Connect Codex</a>"#), "{accounts}");

    let (status, page) = get(app.clone(), "/signin").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains(r#"<li class="signin-steps__step signin-steps__active"><span class="signin-steps__n">1</span>"#) && page.contains(r#"href="/signin?provider=claude""#), "{page}");
    let (_, page) = get(app.clone(), "/signin?provider=codex").await;
    assert!(page.contains(r#"<li class="signin-steps__step signin-steps__active"><span class="signin-steps__n">2</span>"#) && page.contains("Open ChatGPT (Codex) sign-in"));

    let (status, page, _) = send_form(app.clone(), "/signin", "provider=codex").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains(r#"href="https://example.invalid/stand-in-sign-in">Open the sign-in page</a>"#), "{page}");
    assert!(page.contains("Open the sign-in page and complete ChatGPT (Codex) sign-in in the new tab."));
    let (_, home) = get(app.clone(), "/").await;
    assert!(home.contains("Finish ChatGPT (Codex) sign-in."), "the banner says a sign-in is pending");

    for (link, says) in [
        ("not a link", "That is not the sign-in link."),
        ("https://example.com/cb?code=x", "That is not the localhost sign-in link."),
        ("http://localhost:1455/auth/callback?error=access_denied", "The provider refused the sign-in: access_denied. Start again."),
        ("http://localhost:1455/auth/callback?state=s", "That link has no sign-in code."),
    ] {
        let body = format!("link={}", enc(link));
        let (status, page, _) = send_form(app.clone(), "/signin/finish", &body).await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains(says), "{link}: {page}");
    }
    let (_, page, _) = send_form(app.clone(), "/signin/finish", "link=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback%3Fcode%3Dabc").await;
    assert!(page.contains("ChatGPT (Codex) sign-in submitted; waiting for the account to reconnect..."), "{page}");
    let log = std::fs::read_to_string(t.dir.join("logs/fake-proxy.jsonl")).unwrap();
    assert!(log.contains("codex-auth-url") && log.contains(r#""provider":"codex""#), "{log}");

    let (_, page, _) = send_form(app.clone(), "/signin/cancel", "").await;
    assert!(page.contains("Sign-in cancelled."));
    let (_, home) = get(app.clone(), "/").await;
    assert!(home.contains(r#"<div id="finish" class="dash__finish" hidden></div>"#), "cancelled");

    let id = shadow.install.json("state/pool-state.json").unwrap()["accounts"].as_array().unwrap()[0]["id"].as_str().unwrap().to_string();
    let (status, _, location) = send_form(app.clone(), "/refresh", &format!("open={}", enc(&id))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(location.as_deref().is_some_and(|l| l.starts_with("/accounts?open=") && l.contains("#acct-")), "{location:?}");
    let (status, _, location) = send_form(app.clone(), "/refresh", "").await;
    assert_eq!((status, location.as_deref()), (StatusCode::SEE_OTHER, Some("/")));
    std::fs::remove_file(t.dir.join("logs/fake-proxy.jsonl")).unwrap();
}

/// The tailnet view (tailnet-dashboard-proxy.mjs): the tailnet name is answered and its pages
/// read by anyone on it; a write from it (a form, or the service's JSON) needs the owner's
/// Tailscale login, and the tailnet page's origin passes the JSON write's origin check. Without a
/// tailnet set, a request carrying a Tailscale identity is refused a write.
#[tokio::test]
async fn the_tailnet_reads_and_only_its_owner_writes() {
    let t = snapshot("copy-tailnet", false);
    let key = "test-key".to_string();
    let proxy = crate::writer::fake_proxy::start(t.dir.clone(), key.clone(), None).await.unwrap();
    let io = std::sync::Arc::new(crate::writer::io::FileIo::new(t.dir.clone(), proxy, key));
    let copy = std::sync::Arc::new(crate::writer::Copy::new(io));
    let app_with = |tailnet: Option<crate::tailnet::Tailnet>| {
        let shadow = Shadow::new(t.install(), "(a copy)".into(), t.dir.join("parity.jsonl"));
        if let Some(tn) = tailnet {
            shadow.tailnet.set(tn).unwrap();
        }
        crate::app::app_copy(shadow, copy.clone(), Default::default(), Default::default()).router()
    };
    let app = app_with(Some(crate::tailnet::Tailnet::new("https://box.example.ts.net:8443", "owner@example.com").unwrap()));
    let send = |method: &'static str, path: &'static str, login: Option<&'static str>, kind: &'static str, body: &'static str| {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "box.example.ts.net:8443")
            .header("origin", "https://box.example.ts.net:8443")
            .header("content-type", kind);
        if let Some(l) = login {
            req = req.header("tailscale-user-login", l);
        }
        req.body(Body::from(body)).unwrap()
    };
    let status = |app: corten::axum::Router, req: Request<Body>| async move { app.oneshot(req).await.unwrap().status() };
    let form = "application/x-www-form-urlencoded";

    assert_eq!(status(app.clone(), send("GET", "/", Some("guest@example.com"), form, "")).await, StatusCode::OK, "anyone on the tailnet reads");
    assert_eq!(status(app.clone(), send("GET", "/accounts", None, form, "")).await, StatusCode::OK);
    for login in [None, Some("guest@example.com")] {
        assert_eq!(status(app.clone(), send("POST", "/refresh", login, form, "")).await, StatusCode::FORBIDDEN, "{login:?}");
        assert_eq!(status(app.clone(), send("POST", "/signin", login, form, "provider=codex")).await, StatusCode::FORBIDDEN, "{login:?}");
        assert_eq!(status(app.clone(), send("POST", "/api/refresh", login, "application/json", "{}")).await, StatusCode::FORBIDDEN, "{login:?}");
    }
    assert_eq!(status(app.clone(), send("POST", "/refresh", Some("owner@example.com"), form, "")).await, StatusCode::SEE_OTHER);
    assert_eq!(status(app.clone(), send("POST", "/signin", Some("owner@example.com"), form, "provider=codex")).await, StatusCode::OK);
    assert_eq!(status(app.clone(), send("POST", "/signin/cancel", Some("owner@example.com"), form, "")).await, StatusCode::OK);
    assert_eq!(status(app.clone(), send("POST", "/api/refresh", Some("owner@example.com"), "application/json", "{}")).await, StatusCode::ACCEPTED, "the tailnet page's origin is taken from the owner");

    let local = Request::builder().method("POST").uri("/refresh").header("host", "localhost").header("origin", "http://localhost").header("content-type", form).body(Body::empty()).unwrap();
    assert_eq!(status(app.clone(), local).await, StatusCode::SEE_OTHER, "this PC writes as before");

    let without = app_with(None);
    assert_eq!(status(without.clone(), send("GET", "/", None, form, "")).await, StatusCode::MISDIRECTED_REQUEST, "no tailnet: its name is not answered");
    let forged = Request::builder().method("POST").uri("/refresh").header("host", "localhost").header("origin", "http://localhost").header("tailscale-user-login", "owner@example.com").header("content-type", form).body(Body::empty()).unwrap();
    assert_eq!(status(without, forged).await, StatusCode::FORBIDDEN, "a Tailscale identity with no tailnet set");
    let _ = std::fs::remove_file(t.dir.join("logs/fake-proxy.jsonl"));
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

/// The desktop relay's port taken by something else (pool-service.mjs `listenDesktopRelay`): said
/// once (an event and a toast), tried again until it frees up, then served and said so.
#[tokio::test]
async fn a_busy_desktop_relay_port_is_said_once_and_retried_until_it_frees() {
    let t = snapshot("relay-busy", false);
    let key = "test-key".to_string();
    let proxy = crate::writer::fake_proxy::start(t.dir.clone(), key.clone(), None).await.unwrap();
    let io = std::sync::Arc::new(crate::writer::io::FileIo::new(t.dir.clone(), proxy.clone(), key));
    let holder = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = holder.local_addr().unwrap().port();
    let started = crate::writer::desktop::start(io, &format!("{proxy}/backend-api"), port, Duration::from_millis(100)).await.unwrap();
    assert_eq!(started, port);
    tokio::time::sleep(Duration::from_millis(450)).await;
    let events = || std::fs::read_to_string(t.dir.join("logs/pool-events.jsonl")).unwrap_or_default();
    let count = |kind: &str, text: &str| text.lines().filter(|l| l.contains(&format!("\"type\":\"{kind}\""))).count();
    let before = events();
    assert_eq!(count("desktop-relay-error", &before), 1, "said once while busy: {before}");
    assert_eq!(count("desktop-relay-listening", &before), 0);
    let toasts = std::fs::read_to_string(t.dir.join("logs/toasts.jsonl")).unwrap();
    assert!(toasts.contains("desktop relay port busy"), "{toasts}");

    drop(holder);
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut answered = None;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(r) = http.get(format!("http://127.0.0.1:{port}/backend-api/wham/usage")).send().await {
            answered = Some(r.status());
            break;
        }
    }
    assert_eq!(answered, Some(StatusCode::OK), "the relay serves once the port is free");
    let after = events();
    assert_eq!((count("desktop-relay-error", &after), count("desktop-relay-listening", &after)), (1, 1), "{after}");

    for f in ["logs/pool-events.jsonl", "logs/toasts.jsonl", "logs/fake-proxy.jsonl"] {
        let _ = std::fs::remove_file(t.dir.join(f));
    }
}

/// The claim path on a copy (refresh.mjs's processReset through the stand-in): with every Codex
/// account out, a cycle journals a claim, sends it, and the stand-in refuses it (no_credit), so
/// the credit is backed off and the next cycle claims a different one. Nothing is ever spent.
#[tokio::test]
async fn an_exhausted_pool_claims_a_reset_on_the_copy_and_the_refusal_backs_that_credit_off() {
    let t = snapshot("copy-claims", false);
    let file = t.dir.join("state/pool-state.json");
    let mut saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let credit_expiry = js::iso(crate::writer::io::now_ms() + 7.0 * 86_400_000.0);
    for a in saved["accounts"].as_array_mut().unwrap().iter_mut().filter(|a| a["provider"] == "codex") {
        // This exercises a refused claim, not expiration of the recorded October bank.
        for credit in a["credits"].as_array_mut().unwrap() {
            credit["expires_at"] = Value::from(credit_expiry.clone());
        }
        for key in ["windows", "quotaWindows"] {
            for w in a[key].as_array_mut().unwrap() {
                w["used"] = Value::from(100);
            }
        }
    }
    std::fs::write(&file, saved.to_string()).unwrap();
    let key = "test-key".to_string();
    let proxy = crate::writer::fake_proxy::start(t.dir.clone(), key.clone(), None).await.unwrap();
    let io = crate::writer::io::FileIo::new(t.dir.clone(), proxy, key);
    let mut state = crate::writer::load_state(&t.dir);
    let mut refresh = pool_core::refresh::Refresh::new(Default::default(), &state);

    let first = refresh.refresh(&mut state, &io).await;
    assert!(first.follow_up, "an answered claim asks for a follow-up read");
    let slot = &state["resets"]["codex"];
    assert_eq!((&slot["pending"], &slot["last"]["outcome"]), (&Value::Null, &Value::from("no_credit")), "{slot}");
    let claimed = |slot: &Value| js::to_string(slot["last"].get("creditId"));
    let once = claimed(slot);
    assert_eq!(slot["refused"].as_object().unwrap().len(), 1, "{slot}");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved["resets"]["codex"]["last"]["outcome"], "no_credit", "the outcome is saved");

    refresh.refresh(&mut state, &io).await;
    let slot = &state["resets"]["codex"];
    assert_eq!(slot["last"]["outcome"], "no_credit");
    assert_ne!(claimed(slot), once, "the refused credit is skipped while it is backed off");
    assert_eq!(slot["refused"].as_object().unwrap().len(), 2, "{slot}");

    let events = std::fs::read_to_string(t.dir.join("logs/pool-events.jsonl")).unwrap();
    let kinds: Vec<Value> = events.lines().map(|l| serde_json::from_str::<Value>(l).unwrap()).filter(|e| e["type"].as_str().is_some_and(|k| k.starts_with("reset-"))).collect();
    let summary: Vec<(String, String)> = kinds.iter().map(|e| (js::to_string(e.get("type")), js::to_string(e.get("outcome")))).collect();
    let expected: Vec<(String, String)> = [("reset-attempt", "undefined"), ("reset-result", "no_credit")].repeat(2).into_iter().map(|(a, b)| (a.into(), b.into())).collect();
    assert_eq!(summary, expected, "{events}");
    let log = std::fs::read_to_string(t.dir.join("logs/fake-proxy.jsonl")).unwrap();
    let consumes: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|l| l["body"]["url"].as_str().is_some_and(|u| u.ends_with("/consume")))
        .collect();
    assert_eq!(consumes.len(), 2, "one claim per cycle, each answered by the stand-in");
    let sent: Value = serde_json::from_str(consumes[0]["body"]["data"].as_str().unwrap()).unwrap();
    assert_eq!(sent["credit_id"], Value::from(once), "{sent}");

    for f in ["logs/pool-events.jsonl", "logs/toasts.jsonl", "logs/fake-proxy.jsonl", "state/pool-state.json.tmp"] {
        let _ = std::fs::remove_file(t.dir.join(f));
    }
}

/// pool-service.mjs's `/api/shutdown` and `closing`: only the management key stops the copy, once;
/// from then on health says `closing` and every write is refused with 503.
#[tokio::test]
async fn the_management_key_stops_the_copy_once_and_writes_are_refused_while_it_stops() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let t = snapshot("copy-stop", false);
    let key = "test-key".to_string();
    let proxy = crate::writer::fake_proxy::start(t.dir.clone(), key.clone(), None).await.unwrap();
    let io = std::sync::Arc::new(crate::writer::io::FileIo::new(t.dir.clone(), proxy, key));
    let begun = std::sync::Arc::new(AtomicUsize::new(0));
    let b = begun.clone();
    let copy = crate::writer::Copy::new(io).with_management_key("the-key".into()).with_shutdown(move || {
        b.fetch_add(1, Ordering::SeqCst);
    });
    let copy = std::sync::Arc::new(copy);
    let shadow = Shadow::new(t.install(), "(a copy)".into(), t.dir.join("parity.jsonl"));
    let app = crate::app::app_copy(shadow, copy.clone(), Default::default(), Default::default()).router();
    let shutdown = |auth: Option<&str>| {
        let mut req = Request::builder().method("POST").uri("/api/shutdown").header("host", "localhost");
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        let app = app.clone();
        async move {
            let res = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
            let status = res.status();
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null))
        }
    };
    let closing = || async { serde_json::from_str::<Value>(&get(app.clone(), "/api/health").await.1).unwrap()["closing"].clone() };
    assert_eq!(closing().await, false);

    for auth in [None, Some("Bearer wrong"), Some("the-key")] {
        assert_eq!(shutdown(auth).await, (StatusCode::FORBIDDEN, serde_json::json!({ "error": "Unauthorized" })), "{auth:?}");
    }
    assert_eq!(begun.load(Ordering::SeqCst), 0);
    let origin = Some("http://localhost");
    let (status, _) = send_json(app.clone(), "POST", "/api/refresh", origin, "{}").await;
    assert_eq!(status, StatusCode::ACCEPTED, "writes go on before a stop");

    assert_eq!(shutdown(Some("Bearer the-key")).await, (StatusCode::ACCEPTED, serde_json::json!({ "ok": true })));
    assert_eq!(begun.load(Ordering::SeqCst), 1);
    assert_eq!(closing().await, true);
    for path in ["/api/refresh", "/api/login", "/api/callback"] {
        let (status, body) = send_json(app.clone(), "POST", path, origin, r#"{"provider":"codex"}"#).await;
        assert_eq!((status, body), (StatusCode::SERVICE_UNAVAILABLE, serde_json::json!({ "error": "Service stopping" })), "{path}");
    }
    assert_eq!(shutdown(Some("Bearer the-key")).await.0, StatusCode::ACCEPTED);
    assert_eq!(begun.load(Ordering::SeqCst), 1, "a second request begins nothing more");
    let events = std::fs::read_to_string(t.dir.join("logs/pool-events.jsonl")).unwrap();
    let stops: Vec<Value> = events.lines().map(|l| serde_json::from_str::<Value>(l).unwrap()).filter(|e| e["type"] == "supervisor-stopping").collect();
    assert_eq!(stops.len(), 1, "{events}");
    assert_eq!((&stops[0]["reason"], &stops[0]["pid"]), (&Value::from("api-shutdown"), &Value::from(std::process::id())));

    for f in ["logs/pool-events.jsonl", "logs/fake-proxy.jsonl"] {
        let _ = std::fs::remove_file(t.dir.join(f));
    }
}

#[tokio::test]
async fn the_read_only_shadow_serves_documents_but_no_writes() {
    {
        let t = snapshot("read-only-signin", false);
        let (status, _) = get(router(&t), "/signin").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "sign-in is the live dashboard's");
        let (_, home) = get(router(&t), "/").await;
        assert!(home.contains(r#"<button type="button" disabled title="Done on the live dashboard (port 8318): the shadow only reads">Add account</button>"#), "Add account is disabled: {home}");
        assert!(!home.contains("/signin"));
    }
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


/// model-refresh.mjs on a copy: the plan's efforts probed through the stand-in (Codex with the
/// pool log's routing evidence, Claude with its model usage), the evidence appended, the registry
/// and lineage written under the lock; a second run finds fresh evidence and probes nothing.
#[tokio::test]
async fn a_copy_admits_models_through_the_stand_in_and_publishes_its_registry() {
    use crate::writer::models;
    let t = snapshot("copy-models", false);
    let client = "client-key".to_string();
    let proxy = crate::writer::fake_proxy::start(t.dir.clone(), "test-key".into(), Some(client.clone())).await.unwrap();
    let families = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../reference/agent-pools/model-families.json")).unwrap();
    std::fs::write(t.dir.join("model-families.json"), families).unwrap();
    let (codex, claude) = ("codex-4a53e220-user1@example.invalid-pro.json", "claude-550c2ce1-user4@example.invalid.json");
    let levels = r#"{"levels":["low","medium","high","xhigh","max"]}"#;
    let at = pool_core::js::iso(now_ms());
    let lists = serde_json::json!({ "format": 1, "lists": {
        codex: { "provider": "codex", "account": codex, "fetched_at": at, "models": [{ "id": "gpt-6.1-sol", "thinking": levels }] },
        claude: { "provider": "claude", "account": claude, "fetched_at": at, "models": [{ "id": "claude-opus-5-5", "thinking": levels }] },
    } });
    std::fs::write(t.dir.join("state/live-models.json"), lists.to_string()).unwrap();
    let status = serde_json::json!({ "checkedAt": at, "accounts": [
        { "id": codex, "disabled": false, "routingModels": { "gpt-6.1-sol": { "status": "ready" } } },
        { "id": claude, "disabled": false, "routingModels": { "claude-opus-5-5": { "status": "ready" } } },
    ] });
    let mut prober = models::StandIn::new(t.dir.clone(), &proxy, client);
    prober.flush = std::time::Duration::ZERO;
    let lines = std::sync::Mutex::new(Vec::<String>::new());
    let log = |l: &str| lines.lock().unwrap().push(l.to_string());

    let registry = models::refresh(&t.dir, &prober, || Some(status.clone()), true, None, &log).await.unwrap();
    let rows = models::read_probes(&t.dir).unwrap();
    let tried: Vec<String> = rows.iter().map(|r| format!("{} {}", r["model"].as_str().unwrap(), r["effort"].as_str().unwrap())).collect();
    assert_eq!(
        tried,
        ["gpt-6.1-sol medium", "gpt-6.1-sol high", "gpt-6.1-sol xhigh", "claude-opus-5-5 low", "claude-opus-5-5 medium", "claude-opus-5-5 high"],
        "the required efforts of each family's model, in plan order"
    );
    for r in &rows {
        assert_eq!((&r["ok"], &r["standIn"], &r["reason"]), (&Value::Bool(true), &Value::Bool(true), &Value::Null), "{r}");
        if r["provider"] == "codex" {
            assert_eq!(r["account"], codex, "the account the pool log bound it to: {r}");
            assert!(r["poolRequestId"].as_str().is_some_and(|id| id.len() == 8), "{r}");
        } else {
            assert_eq!((&r["servedModel"], &r["account"]), (&Value::from("claude-opus-5-5"), &Value::Null), "{r}");
        }
    }
    assert_eq!(registry["families"]["sol"]["current"], "gpt-6.1-sol");
    assert_eq!(registry["families"]["opus"]["current"], "claude-opus-5-5");
    assert_eq!(registry["models"]["gpt-6.1-sol"]["usableAccountsByEffort"]["high"], serde_json::json!([codex]));
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(t.dir.join("state/model-registry.json")).unwrap()).unwrap();
    assert_eq!(pool_core::js::stringify(&saved), pool_core::js::stringify(&registry), "the registry written is the one built");
    let lineage: Value = serde_json::from_str(&std::fs::read_to_string(t.dir.join("state/model-lineage.json")).unwrap()).unwrap();
    assert_eq!(lineage["format"], "model-lineage/v1");
    assert!(!t.dir.join("state/model-registry.json.lock").exists(), "the lock is released");
    assert!(lines.lock().unwrap().iter().any(|l| l.starts_with("sol     current=gpt-6.1-sol")), "{:?}", lines.lock().unwrap());

    // Fresh evidence: nothing to probe.
    models::refresh(&t.dir, &prober, || Some(status.clone()), true, None, &log).await.unwrap();
    assert_eq!(models::read_probes(&t.dir).unwrap().len(), 6);

    for f in ["model-families.json", "state/live-models.json", "state/model-lineage.json", "state/model-admission.jsonl", "logs/main.log", "logs/fake-proxy.jsonl"] {
        std::fs::remove_file(t.dir.join(f)).unwrap();
    }
}


/// A document is parsed once and served from memory while its file is unchanged; a save (a write
/// and a rename, as the writer saves) is read again at once.
#[test]
fn a_document_is_kept_parsed_until_its_file_changes() {
    let dir = std::env::temp_dir().join(format!("pools-shadow-parsed-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("state")).unwrap();
    let file = dir.join("state/doc.json");
    std::fs::write(&file, r#"{"n":1}"#).unwrap();
    let install = crate::sources::Install::new(&dir);
    let first = install.doc("state/doc.json").unwrap();
    assert!(std::sync::Arc::ptr_eq(&first, &install.clone().doc("state/doc.json").unwrap()), "unchanged: the same parse, shared by clones");
    std::fs::write(dir.join("state/doc.json.tmp"), r#"{"n":22}"#).unwrap();
    std::fs::rename(dir.join("state/doc.json.tmp"), &file).unwrap();
    assert_eq!(install.doc("state/doc.json").unwrap()["n"], 22, "a save is read again");
    std::fs::remove_file(&file).unwrap();
    assert_eq!(install.doc("state/doc.json"), None, "a removed file is gone, not served from memory");
    std::fs::remove_dir(dir.join("state")).unwrap();
    std::fs::remove_dir(&dir).unwrap();
}
