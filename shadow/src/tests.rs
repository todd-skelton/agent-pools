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
use crate::watch::Shadow;

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
