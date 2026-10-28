//! model-benchmark-sources.test.mjs's checks of the two snapshots, with a stand-in for both sites.

use std::sync::{Arc, Mutex};

use corten::axum::{Router, extract::{Path as UrlPath, Query, State}, http::StatusCode, response::IntoResponse, routing::get};
use serde_json::{Value, json};

use super::*;

const SHA: &str = "4c39810eebe1cb20145d0a81756a95b8f057cd03";

fn ids() -> Value {
    json!({ "claude-opus-5-5": {}, "claude-fable-5-1": {}, "gpt-6.1-sol": {}, "gpt-6-astra": {}, "claude-sonnet-5-5": {} })
}

fn arow(name: &str, category: &str, rating: f64, lower: f64, upper: f64, votes: f64) -> Value {
    json!({ "model_name": name, "organization": "x", "rating": rating, "rating_lower": lower, "rating_upper": upper, "vote_count": votes, "rank": 1, "category": category, "leaderboard_publish_date": "2026-09-25" })
}

fn m(model: &str, effort: &'static str) -> Option<(String, &'static str)> {
    Some((model.to_string(), effort))
}

#[test]
fn arena_names_map_only_as_exact_id_and_effort() {
    let ids = ids();
    let at = |n: &str| match_arena_name(Some(&json!(n)), &ids);
    assert_eq!(at("claude-opus-5.5-high"), m("claude-opus-5-5", "high"));
    assert_eq!(at("gpt-6.1-sol-max"), m("gpt-6.1-sol", "max"));
    assert_eq!(at("gpt-6-1-sol-xhigh"), m("gpt-6.1-sol", "xhigh"));
    assert_eq!(at("gpt-6-astra-max (codex-harness)"), None, "harness variants are a different configuration");
    assert_eq!(at("claude-opus-5.5"), None, "no effort means unknown effort");
    assert_eq!(at("claude-opus-5.5-high-32k"), None);
    assert_eq!(at("claude-opus-5-high"), None, "a different version never matches");
    assert_eq!(at("Claude-Opus-5.5-high"), None, "names are exact, not case-folded");
    assert_eq!(match_arena_name(None, &ids), None);
    assert_eq!(match_arena_name(Some(&json!("gpt-6.1-sol-high")), &json!({ "gpt-6.1-sol": {}, "gpt-6-1-sol": {} })), None, "two identities that normalise alike are ambiguous");
}

/// The display names the dataset has used since 2026-10-21, as both boards list them on 2026-10-28.
#[test]
fn arena_display_names_map_to_id_and_effort_only_when_the_parentheses_hold_one_effort() {
    let ids = json!({ "claude-opus-5-5": {}, "claude-fable-5-1": {}, "claude-sonnet-5-5": {}, "claude-opus-4-8": {}, "claude-opus-4-5-20251101": {}, "gpt-6.1-sol": {}, "gpt-6-astra": {}, "gpt-5.6-sol": {} });
    let at = |n: &str| match_arena_name(Some(&json!(n)), &ids);
    assert_eq!(at("Claude Opus 5.5 (Max)"), m("claude-opus-5-5", "max"));
    assert_eq!(at("Claude Opus 5.5 (High)"), m("claude-opus-5-5", "high"));
    assert_eq!(at("Claude Fable 5.1 (Max)"), m("claude-fable-5-1", "max"));
    assert_eq!(at("Claude Sonnet 5.5 (xHigh)"), m("claude-sonnet-5-5", "xhigh"));
    assert_eq!(at("Claude Opus 4.8 (High)"), m("claude-opus-4-8", "high"));
    assert_eq!(at("GPT-6.1 Sol (Max)"), m("gpt-6.1-sol", "max"));
    assert_eq!(at("GPT-6 Astra (Max)"), m("gpt-6-astra", "max"));
    assert_eq!(at("GPT-5.6 Sol (xHigh)"), m("gpt-5.6-sol", "xhigh"));
    assert_eq!(at("GPT-5.6 Sol (xHigh, Codex Harness)"), None, "harness variants are a different configuration");
    assert_eq!(at("Claude Opus 4.5 (High, 32K, 20251101)"), None, "a budget or date qualifier is a different configuration");
    assert_eq!(at("Claude Opus 4.5 (20251101)"), None, "a date is not an effort");
    assert_eq!(at("Claude Opus 4.8"), None, "no effort means unknown effort");
    assert_eq!(at("Claude Opus 5 (High)"), None, "a different version never matches");
    assert_eq!(at("Claude Opus 5.5 (Thinking)"), None, "only the registry's efforts");
    assert_eq!(at("Claude  Opus 5.5 (Max)"), None);
    assert_eq!(at("Claude Opus 5.5 (Max) "), None);
    assert_eq!(match_arena_name(Some(&json!("GPT-6.1 Sol (Max)")), &json!({ "gpt-6.1-sol": {}, "gpt-6-1-sol": {} })), None, "two identities that normalise alike are ambiguous");
}

#[test]
fn an_arena_snapshot_drops_bad_intervals_and_ambiguous_duplicates_and_lists_unmatched_vendor_names() {
    let ids = ids();
    let snap = arena_snapshot(
        &[
            ("text_style_control", vec![
                arow("claude-opus-5.5-high", "creative_writing", 1521.0, 1492.0, 1550.0, 487.0),
                arow("claude-fable-5.1-max", "creative_writing", 1484.0, 1490.0, 1497.0, 2160.0), // rating outside its own interval
                arow("gpt-6-astra-max", "creative_writing", 1453.0, 1437.0, 1469.0, 1484.0),
                arow("gpt-6-astra-max", "creative_writing", 1460.0, 1440.0, 1470.0, 10.0),
                arow("claude-opus-4-8", "creative_writing", 1467.0, 1460.0, 1474.0, 100.0),
                arow("GPT-6.1 Sol (Max)", "creative_writing", 1470.0, 1460.0, 1480.0, 900.0),
                arow("GPT-5.6 Sol (xHigh, Codex Harness)", "creative_writing", 1400.0, 1390.0, 1410.0, 50.0),
                arow("Gemini 3 Pro", "creative_writing", 1400.0, 1390.0, 1410.0, 50.0),
            ]),
            ("webdev", vec![arow("Claude Opus 5.5 (Max)", "overall", 1826.0, 1808.0, 1845.0, 1607.0)]),
        ],
        &ids,
        "t",
        SHA,
    )
    .unwrap();
    let rows: Vec<String> = js::values(snap.get("rows")).iter().map(|r| format!("{}|{}|{}|{}", text(r.get("board")), text(r.get("model")), text(r.get("effort")), text(r.get("votes")))).collect();
    assert_eq!(rows, ["text_style_control/creative_writing|claude-opus-5-5|high|487", "text_style_control/creative_writing|gpt-6.1-sol|max|900", "webdev/overall|claude-opus-5-5|max|1607"]);
    assert_eq!(snap["ambiguous"], json!(["gpt-6-astra|max|text_style_control/creative_writing"]));
    assert_eq!(snap["unmatched"], json!(["GPT-5.6 Sol (xHigh, Codex Harness)", "claude-opus-4-8"]));
    assert_eq!(js::values(snap.get("rows")).last().unwrap()["arenaName"], json!("Claude Opus 5.5 (Max)"), "the row keeps the name the dataset wrote");
    assert_eq!((&snap["revision"], &snap["publishDate"]), (&json!(SHA), &json!("2026-09-25")));
    let bad = arena_snapshot(
        &[("webdev", vec![arow("claude-opus-5.5-max", "overall", 1826.0, 1808.0, 1845.0, 1607.0), arow("claude-opus-5.5-max", "overall", 1500.0, 1600.0, 1400.0, 10.0)])],
        &ids,
        "t",
        SHA,
    )
    .unwrap();
    assert_eq!((js::values(bad.get("rows")).len(), &bad["ambiguous"]), (0, &json!(["claude-opus-5-5|max|webdev/overall"])), "an invalid second row still makes the configuration ambiguous");
    let mut missing = arow("claude-opus-5.5-high", "overall", 1.0, 1.0, 1.0, 1.0);
    missing.as_object_mut().unwrap().remove("rating_upper");
    assert_eq!(arena_snapshot(&[("webdev", vec![missing])], &ids, "t", SHA).unwrap_err(), "Arena webdev is missing column rating_upper");
}

#[test]
fn arena_shard_lists_and_publish_dates_are_checked() {
    let n = |i: u32, t: u32| format!("webdev/latest-0000{i}-of-0000{t}.parquet");
    assert_eq!(latest_shards(&[n(1, 2), n(0, 2), "webdev/full-00000-of-00001.parquet".into()], "webdev").unwrap(), [n(0, 2), n(1, 2)]);
    assert!(latest_shards(&[n(0, 2)], "webdev").unwrap_err().contains("incomplete"));
    assert!(latest_shards(&[n(0, 2), n(1, 3)], "webdev").unwrap_err().contains("incomplete"));
    assert!(latest_shards(&[n(0, 1), n(0, 1)], "webdev").unwrap_err().contains("incomplete"));
    assert_eq!(latest_shards(&[], "webdev").unwrap_err(), "Arena webdev has no latest parquet");
    for (date, want) in [("2026-09-25", Some("2026-09-25")), ("Mon Sep 28 2026", None), ("2026-02-30", None), ("2026-04-31", None), ("2028-02-29", Some("2028-02-29"))] {
        assert_eq!(iso_date(Some(&json!(date))).as_deref(), want, "{date}");
    }
    let mut later = arow("claude-opus-5.5-max", "overall", 1826.0, 1808.0, 1845.0, 1607.0);
    later["leaderboard_publish_date"] = json!("2026-09-28");
    let snap = arena_snapshot(&[("webdev", vec![later, arow("gpt-6-astra-max", "overall", 1791.0, 1780.0, 1803.0, 4908.0)])], &ids(), "t", SHA).unwrap();
    assert_eq!(snap["publishDate"], "2026-09-28");
}

fn aa_row(id: &str, slug: &str, name: &str) -> Value {
    json!({ "id": id, "slug": slug, "name": name, "evaluations": { "artificial_analysis_intelligence_index": 50 }, "pricing": {} })
}

fn cost_row(id: &str, slug: &str, name: &str, usd: f64) -> Value {
    json!({ "id": id, "slug": slug, "name": name, "artificial_analysis_intelligence_index_cost": { "cost_per_task": { "total_cost": usd } } })
}

#[test]
fn an_aa_task_cost_joins_on_id_with_the_same_slug_and_name_and_ambiguous_rows_are_dropped() {
    let data = [
        aa_row("a", "gpt-6-1-sol-high", "GPT-6.1 Sol (high)"),
        aa_row("b", "gpt-6-1-sol-xhigh", "GPT-6.1 Sol (xhigh)"),
        aa_row("c", "gpt-6-astra-high", "GPT-6 Astra (high)"),
        aa_row("d", "claude-opus-5-5-high", "Claude Opus 5.5 (Adaptive Reasoning, High Effort, Default Fallback)"),
        aa_row("e", "claude-opus-5-5-high", "Claude Opus 5.5 (Adaptive Reasoning, High Effort)"),
    ];
    let costs = [
        cost_row("a", "gpt-6-1-sol-high", "GPT-6.1 Sol (high)", 0.3191),
        cost_row("b", "gpt-6-1-sol-xhigh", "GPT-6.1 Sol (low)", 0.13),
        cost_row("c", "gpt-6-astra-high", "GPT-6 Astra (high)", 0.0),
        cost_row("d", "claude-opus-5-5-high", "x", 1.8),
    ];
    let snap = snapshot(&data, &ids(), "t", &costs, json!(4.3));
    let by: Vec<(String, Value)> = js::values(snap.get("rows")).iter().map(|r| (format!("{}/{}", text(r.get("model")), text(r.get("effort"))), r["usdPerTask"].clone())).collect();
    assert_eq!(by, [("gpt-6-astra/high".to_string(), Value::Null), ("gpt-6.1-sol/high".into(), json!(0.3191)), ("gpt-6.1-sol/xhigh".into(), Value::Null)], "zero is not a cost, a relabelled cost row is not trusted, and two AA rows for one configuration are ambiguous");
    assert_eq!(snap["ambiguous"], json!(["claude-opus-5-5|high"]));
    assert_eq!(snap["intelligenceIndexVersion"], json!(4.3));
    assert!(task_costs(&[]).is_empty());
    let dupe = task_costs(&[cost_row("a", "s", "n", 1.0), cost_row("a", "s", "n", 0.1), cost_row("b", "t", "m", 2.0), cost_row("b", "t", "m", 2.0)]);
    assert!(!dupe.contains_key("a"), "a conflicting duplicate id gets no cost");
    assert_eq!(dupe["b"].usd, 2.0, "an identical repeat is harmless");
    let invalid = task_costs(&[cost_row("c", "s", "n", 1.0), json!({ "id": "c", "slug": "s", "name": "n" }), cost_row("d", "s", "n", 1.0), cost_row("d", "s", "n", 0.0)]);
    assert!(invalid.is_empty(), "a conflicting repeat without a valid cost still makes the id ambiguous");
}

#[test]
fn an_effort_is_read_from_an_aa_name() {
    for (name, want) in [("GPT-6.1 Sol (high)", Some("high")), ("Claude Opus 5.5 (Adaptive Reasoning, Max Effort)", Some("max")), ("GPT-6 Luna (xhigh)", Some("xhigh")), ("GPT-6 (Non-reasoning)", None), ("GPT-6", None)] {
        assert_eq!(effort_from_name(Some(&json!(name))), want, "{name}");
    }
    assert_eq!(match_row(&json!({ "name": "GPT-6.1 Sol (max)", "slug": "gpt-6-1-sol" }), &ids()), m("gpt-6.1-sol", "max"));
    assert_eq!(match_row(&json!({ "name": "GPT-6.1 Sol (high)", "slug": "gpt-6-1-sol" }), &ids()), None, "the slug must carry a non-maximum effort too");
}

/// What the stand-in answers and what it was asked.
#[derive(Default)]
struct Sites {
    calls: Mutex<Vec<String>>,
    keys: Mutex<Vec<String>>,
    /// The cost endpoint's pages: (status, body).
    pages: Mutex<Vec<(u16, Value)>>,
    sha: Mutex<String>,
    files: Mutex<Vec<String>>,
    parquet: Mutex<Vec<u8>>,
}

type Shared = Arc<Sites>;

async fn aa_data(State(s): State<Shared>, h: corten::axum::http::HeaderMap) -> impl IntoResponse {
    s.calls.lock().unwrap().push("data".into());
    s.keys.lock().unwrap().push(h.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string());
    let data = json!({ "data": [aa_row("a", "gpt-6-1-sol-high", "GPT-6.1 Sol (high)"), aa_row("b", "gpt-6-1-sol-xhigh", "GPT-6.1 Sol (xhigh)")] });
    corten::axum::Json(data)
}

async fn aa_cost(State(s): State<Shared>, Query(q): Query<std::collections::HashMap<String, String>>) -> impl IntoResponse {
    let page: usize = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(0);
    s.calls.lock().unwrap().push(format!("cost {page}"));
    let pages = s.pages.lock().unwrap();
    let (status, body) = pages.get(page - 1).cloned().unwrap_or((404, Value::Null));
    (StatusCode::from_u16(status).unwrap(), corten::axum::Json(body))
}

async fn hub_info(State(s): State<Shared>) -> impl IntoResponse {
    corten::axum::Json(json!({ "sha": *s.sha.lock().unwrap() }))
}

async fn hub_revision(State(s): State<Shared>, UrlPath((_, _, rev)): UrlPath<(String, String, String)>) -> impl IntoResponse {
    s.calls.lock().unwrap().push(format!("revision {rev}"));
    let siblings: Vec<Value> = s.files.lock().unwrap().iter().map(|f| json!({ "rfilename": f })).collect();
    corten::axum::Json(json!({ "siblings": siblings }))
}

async fn hub_resolve(State(s): State<Shared>, UrlPath((_, _, rev, rest)): UrlPath<(String, String, String, String)>) -> impl IntoResponse {
    s.calls.lock().unwrap().push(format!("resolve {rev}/{rest}"));
    s.parquet.lock().unwrap().clone()
}

async fn sites(s: Shared) -> String {
    let app = Router::new()
        .route("/aa/data", get(aa_data))
        .route("/aa/cost", get(aa_cost))
        .route("/api/datasets/{org}/{name}", get(hub_info))
        .route("/api/datasets/{org}/{name}/revision/{rev}", get(hub_revision))
        .route("/datasets/{org}/{name}/resolve/{rev}/{*rest}", get(hub_resolve))
        .with_state(s);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = corten::axum::serve(listener, app).await;
    });
    url
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pools-shadow-bench-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("state")).unwrap();
    dir
}

fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    names.sort();
    names
}

fn page(version: Value, data: Vec<Value>, more: Option<bool>) -> (u16, Value) {
    let mut body = json!({ "intelligence_index_version": version, "data": data });
    if let Some(m) = more {
        body["pagination"] = json!({ "has_more": m });
    }
    (200, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_aa_fetch_pages_through_the_costs_and_publishes_nothing_partial() {
    let s: Shared = Default::default();
    let url = sites(s.clone()).await;
    let f = Fetcher::at(&format!("{url}/aa/data"), &format!("{url}/aa/cost"), &url);
    let root = scratch("aa");
    let only = json!({ "gpt-6.1-sol": {} });
    assert!(f.benchmarks(&root, &only, f64::INFINITY).await.unwrap_err().starts_with("state/secrets.json: "), "no secrets file is an error");
    std::fs::write(root.join("state/secrets.json"), r#"{"artificialAnalysisKey":""}"#).unwrap();
    assert_eq!(f.benchmarks(&root, &only, f64::INFINITY).await.unwrap(), json!({ "skipped": "no artificialAnalysisKey in state/secrets.json" }));
    std::fs::write(root.join("state/secrets.json"), r#"{"artificialAnalysisKey":"test-only"}"#).unwrap();
    *s.pages.lock().unwrap() = vec![
        page(json!(4.3), vec![cost_row("a", "gpt-6-1-sol-high", "GPT-6.1 Sol (high)", 0.3191)], Some(true)),
        page(json!(4.3), vec![cost_row("b", "gpt-6-1-sol-xhigh", "GPT-6.1 Sol (xhigh)", 0.3929)], Some(false)),
    ];
    let r = f.benchmarks(&root, &only, f64::INFINITY).await.unwrap();
    assert_eq!((&r["rows"], &r["withCost"]), (&json!(2), &json!(2)));
    assert!(s.keys.lock().unwrap().iter().all(|k| k == "test-only"), "the key goes as x-api-key");
    let snap: Value = serde_json::from_str(&std::fs::read_to_string(text(r.get("file"))).unwrap()).unwrap();
    assert_eq!(js::values(snap.get("rows")).iter().map(|x| x["usdPerTask"].clone()).collect::<Vec<_>>(), [json!(0.3191), json!(0.3929)]);
    let bench = root.join("state/benchmarks");
    let latest: Value = serde_json::from_str(&std::fs::read_to_string(bench.join("latest.json")).unwrap()).unwrap();
    let name = Path::new(&text(r.get("file"))).file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!((&latest["file"], &latest["rows"]), (&json!(name), &json!(2)));
    assert!(name.starts_with("aa-") && name.ends_with(".json") && name.len() == "aa-20261028T091506463Z-a531aa.json".len(), "{name}");
    let before = listing(&bench);

    let fails = |pages: Vec<(u16, Value)>, want: &'static str| {
        let (s, f, root, only) = (s.clone(), &f, &root, &only);
        async move {
            *s.pages.lock().unwrap() = pages;
            let e = f.benchmarks(root, only, f64::INFINITY).await.unwrap_err();
            assert!(e.contains(want), "{e} (wanted {want})");
        }
    };
    fails(vec![page(json!(4.3), vec![], Some(true)), (429, json!({}))], "AA cost API 429").await;
    fails(vec![page(json!(4.3), vec![], Some(true)), page(json!(4.4), vec![], Some(false))], "index version changed mid-fetch (4.3 -> 4.4)").await;
    fails(vec![page(json!(4.3), vec![], None)], "no pagination.has_more").await;
    for bad in [json!(""), json!(" "), json!("v4"), Value::Null] {
        fails(vec![page(bad, vec![], Some(false))], "no intelligence_index_version").await;
    }
    fails((0..25).map(|_| page(json!("4.3"), vec![], Some(true))).collect(), "still has more after 25 pages").await;
    assert_eq!(listing(&bench), before, "nothing is published on a failure");

    // A deadline already past: no request is made.
    let n = s.calls.lock().unwrap().len();
    assert!(f.benchmarks(&root, &only, now_ms() - 1.0).await.unwrap_err().contains("deadline reached"));
    assert!(f.arena(&root, &only, now_ms() - 1.0).await.unwrap_err().contains("deadline reached"));
    assert_eq!(s.calls.lock().unwrap().len(), n);
    let _ = std::fs::remove_dir_all(&root);
}

/// The dataset's own WebDev file of 2026-10-28 (a 30 KB Parquet file, Snappy), read as
/// hyparquet read it: 651 rows; Claude Opus 5.5 (Max) first.
#[test]
fn the_arena_parquet_reads_as_rows() {
    let bytes = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../golden/inputs/arena-webdev-latest.parquet")).unwrap();
    let rows = read_parquet(bytes::Bytes::from(bytes)).unwrap();
    assert_eq!(rows.len(), 651);
    let first = &rows[0];
    assert_eq!((&first["model_name"], &first["vote_count"], &first["category"], &first["leaderboard_publish_date"]), (&json!("Claude Opus 5.5 (Max)"), &json!(3451.0), &json!("overall"), &json!("2026-10-27")));
    assert!(first["rating"].as_f64().is_some_and(|r| (r - 1808.744584468963).abs() < 1e-9));
    assert!(REQUIRED.iter().all(|k| first.get(*k).is_some()));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_arena_fetch_pins_every_download_to_the_sha_it_read() {
    let s: Shared = Default::default();
    let url = sites(s.clone()).await;
    let f = Fetcher::at(&format!("{url}/aa/data"), &format!("{url}/aa/cost"), &url);
    let root = scratch("arena");
    *s.sha.lock().unwrap() = SHA.into();
    *s.files.lock().unwrap() = ["text_style_control/latest-00000-of-00001.parquet", "webdev/latest-00000-of-00001.parquet", "webdev/full-00000-of-00001.parquet"].map(String::from).to_vec();
    *s.parquet.lock().unwrap() = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../golden/inputs/arena-webdev-latest.parquet")).unwrap();
    let ids = json!({ "claude-opus-5-5": {} });
    let a = f.arena(&root, &ids, f64::INFINITY).await.unwrap();
    let b = f.arena(&root, &ids, f64::INFINITY).await.unwrap();
    assert_ne!(a["file"], b["file"]);
    assert_eq!((&a["revision"], &a["rows"]), (&json!(SHA), &json!(10)), "Claude Opus 5.5 (Max) on five WebDev categories, the file served for both configs");
    let calls = s.calls.lock().unwrap().clone();
    let downloads: Vec<&String> = calls.iter().filter(|c| c.starts_with("resolve ")).collect();
    assert_eq!(downloads.len(), 4, "latest files only, both configs, both fetches: {calls:?}");
    assert!(downloads.iter().all(|d| d.starts_with(&format!("resolve {SHA}/")) && d.contains("/latest-")));
    assert!(calls.contains(&format!("revision {SHA}")), "the file list is read at the same revision");
    let latest: Value = serde_json::from_str(&std::fs::read_to_string(root.join("state/benchmarks/latest-arena.json")).unwrap()).unwrap();
    assert_eq!((&latest["revision"], &latest["file"]), (&json!(SHA), &json!(Path::new(&text(b.get("file"))).file_name().unwrap().to_string_lossy())));
    *s.sha.lock().unwrap() = "main".into();
    assert_eq!(f.arena(&root, &ids, f64::INFINITY).await.unwrap_err(), "Arena dataset has no commit sha");
    *s.sha.lock().unwrap() = SHA.into();
    *s.files.lock().unwrap() = vec!["webdev/latest-00000-of-00001.parquet".into()];
    assert_eq!(f.arena(&root, &ids, f64::INFINITY).await.unwrap_err(), "Arena text_style_control has no latest parquet");
    *s.files.lock().unwrap() = vec!["text_style_control/latest-00000-of-00002.parquet".into(), "webdev/latest-00000-of-00001.parquet".into()];
    assert!(f.arena(&root, &ids, f64::INFINITY).await.unwrap_err().contains("shards are incomplete"));
    let after: Value = serde_json::from_str(&std::fs::read_to_string(root.join("state/benchmarks/latest-arena.json")).unwrap()).unwrap();
    assert_eq!(after, latest, "failures leave the pointer alone");
    let _ = std::fs::remove_dir_all(&root);
}
