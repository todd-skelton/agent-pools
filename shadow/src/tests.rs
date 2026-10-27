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
async fn pages_render() {
    let t = snapshot("pages", false);
    let (status, home) = get(router(&t), "/").await;
    assert_eq!(status, StatusCode::OK, "{home}");
    for text in ["Codex", "Claude", "bankedCredits", "Monthly cost"] {
        assert!(home.contains(text), "the dashboard shows {text}");
    }
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
