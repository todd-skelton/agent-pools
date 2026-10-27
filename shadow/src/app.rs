//! The shadow's pages and JSON: the dashboard and `/api/summary` built by `pool-core` from the
//! live install's files, and the parity checks against the live service.
//!
//! | Route | Answer |
//! |---|---|
//! | `GET /`, `/accounts`, `/usage`, `/policy` | the live dashboard's four tabs ([`crate::dash`]) |
//! | `GET /summary` | `/api/summary` as a page: per provider cost, credits, forecast and scenarios; per account plan, windows and value |
//! | `GET /parity` | the parity checks: counts per kind, the last difference, the recent checks |
//! | `GET /api/summary` | `agent-pools-summary/v1`, as `JSON.stringify` writes it |
//! | `GET /api/parity` | the parity counts and recent checks as JSON |

use corten::axum::http::StatusCode;
use corten::prelude::*;
use pool_core::js;
use serde_json::{Value, json};

use crate::dash::{accounts, live, overview, policy, usage};
use crate::parity::{self, Kind, Outcome};
use crate::watch::{Shared, now_ms};

/// How often the live service's memory (`/api/status`) is read for the dashboard.
const MEMORY_EVERY: std::time::Duration = std::time::Duration::from_secs(5);
/// How often the dashboard's files' modification times are looked at.
const SAVES_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

/// The app on a copy of the install (`pools-shadow run-copy`): the same pages over the copy's
/// files, and the write side's refresh loop in place of the parity checks.
pub fn app_copy(
    shadow: Shared,
    assets: std::path::PathBuf,
    io: std::sync::Arc<crate::writer::io::FileIo>,
    delays: crate::writer::Delays,
    options: pool_core::refresh::Options,
) -> App {
    let trigger = shadow.writer.get_or_init(Default::default).clone();
    let (n, w) = (shadow.clone(), shadow.clone());
    App::new(url::router(shadow))
        .assets(assets)
        .policy(corten::security::Policy::new().allow_style_attributes())
        .shutdown_grace(std::time::Duration::from_secs(20))
        .task("notice-saves", move |signal| {
            crate::watch::notice_saves(n, SAVES_EVERY, signal)
        })
        .task("refresh", move |signal| {
            crate::writer::run(io, trigger, delays, options, move || w.changed_inputs(), signal)
        })
}

/// The app: its routes and the two tasks that keep the parity checks running.
pub fn app(
    shadow: Shared,
    assets: std::path::PathBuf,
    watch_every: std::time::Duration,
    summary_every: std::time::Duration,
) -> App {
    let (w, s, st, n) = (shadow.clone(), shadow.clone(), shadow.clone(), shadow.clone());
    App::new(url::router(shadow))
        .assets(assets)
        // The dashboard sizes meters, bars and timeline marks from data (corten#139: no way to write one without style attributes).
        .policy(corten::security::Policy::new().allow_style_attributes())
        .task("watch-files", move |signal| {
            crate::watch::watch(w, watch_every, signal)
        })
        .task("compare-summary", move |signal| {
            crate::watch::compare_summary(s, summary_every, signal)
        })
        .task("compare-status", move |signal| {
            crate::watch::compare_status(st, MEMORY_EVERY, summary_every, signal)
        })
        .task("notice-saves", move |signal| {
            crate::watch::notice_saves(n, SAVES_EVERY, signal)
        })
}

#[corten::routes(state = Shared)]
pub mod url {
    #[route("/", get = overview)]
    pub struct Overview;

    #[route("/accounts", get = accounts)]
    pub struct Accounts<'a> {
        pub filter: Option<&'a str>,
        pub open: Option<&'a str>,
    }

    #[route("/usage", get = usage)]
    pub struct Usage;

    #[route("/policy", get = policy)]
    pub struct Policy;

    #[route("/live", get = live)]
    pub struct Live<'a> {
        pub tab: Option<&'a str>,
        pub filter: Option<&'a str>,
        pub open: Option<&'a str>,
    }

    #[route("/summary", get = summary_page)]
    pub struct SummaryPage;

    #[route("/parity", get = parity_page)]
    pub struct Parity;

    #[route("/refresh", post = refresh_now)]
    pub struct RefreshNow;

    #[route("/api/summary", get = summary_json)]
    pub struct Summary;

    #[route("/api/status", get = status_json)]
    pub struct Status;

    #[route("/api/parity", get = parity_json)]
    pub struct ParityJson;
}

/// A JSON document written as `JSON.stringify` writes it (key order, number format), so the
/// shadow's bytes can be set beside the live service's.
pub struct JsJson(pub Value);

impl corten::json::ToJson for JsJson {
    fn write_json(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(js::stringify(&self.0).as_bytes());
    }
}

async fn build_summary(shadow: &Shared) -> corten::Result<Value> {
    let s = shadow.clone();
    let doc =
        tokio::task::spawn_blocking(move || parity::summary_now(&s.install, now_ms())).await?;
    doc.ok_or_else(|| {
        corten::Error::new("state/pool-state.json did not read")
            .with_status(StatusCode::SERVICE_UNAVAILABLE)
    })
}

/// `POST /refresh`: a refresh cycle now, on a copy (`run-copy`); the read-only shadow has no
/// write side and answers 404.
async fn refresh_now(req: Req, State(shadow): State<Shared>) -> HtmlResult {
    let Some(trigger) = shadow.writer.get() else {
        return Err(corten::Error::new("This shadow only reads; refresh on the live dashboard").with_status(StatusCode::NOT_FOUND));
    };
    trigger.request();
    req.redirect(&url::Overview)
}

async fn summary_json(State(shadow): State<Shared>) -> JsonResult<JsJson> {
    Ok(Json(JsJson(build_summary(&shadow).await?)))
}

/// `/api/status` from the files. `processes`, `holds` and `usageLedger` are the live service's
/// memory, so they are `null` here.
async fn status_json(State(shadow): State<Shared>) -> JsonResult<JsJson> {
    let s = shadow.clone();
    let doc = tokio::task::spawn_blocking(move || {
        parity::status_now(&s.install, &Default::default(), now_ms())
    })
    .await?;
    let doc = doc.ok_or_else(|| {
        corten::Error::new("state/pool-state.json did not read")
            .with_status(StatusCode::SERVICE_UNAVAILABLE)
    })?;
    Ok(Json(JsJson(doc)))
}

async fn parity_json(State(shadow): State<Shared>) -> JsonResult<Value> {
    let crate::watch::Snapshot {
        tallies,
        recent,
        live_summary: live,
        live_error,
    } = shadow.snapshot();
    let mut kinds = serde_json::Map::new();
    for (kind, tally) in tallies {
        kinds.insert(kind.name().into(), serde_json::to_value(tally)?);
    }
    Ok(Json(json!({
        "format": "agent-pools-shadow-parity/v1",
        "startedAt": js::iso(shadow.started_at),
        "liveUrl": shadow.live_url,
        "liveSummaryAt": live.map(|(at, _)| js::iso(at)),
        "liveError": live_error,
        "kinds": kinds,
        "recent": recent,
    })))
}

async fn summary_page(req: Req, State(shadow): State<Shared>) -> HtmlResult {
    let doc = build_summary(&shadow).await?;
    let view = SummaryView::of(&doc);
    req.page(html! {
        <Layout req={&req} title="Agent Pools (shadow)">
            <h1>"Agent Pools"</h1>
            <p>
                "Read-only shadow: pool-core's summary of the live install's files, built "{&view.generated_at}
                ". Pool state checked "{&view.checked_at}"; forecast "{&view.forecast_at}"; value report "{&view.value_at}"."
            </p>
            <p>"Monthly cost: "<strong>{&view.monthly_cost}</strong></p>
            for p in &view.providers {
                <section>
                    <h2>{&p.name}</h2>
                    <dl>
                        for (k, v) in &p.facts {
                            <div><dt>{k}</dt><dd>{v}</dd></div>
                        }
                    </dl>
                    <table>
                        <caption>{&p.name}" forecast scenarios"</caption>
                        <thead>
                            <tr><th>"Scenario"</th><th>"First short"</th><th>"Short hours"</th><th>"Credits used"</th><th>"Credits lapsing"</th></tr>
                        </thead>
                        <tbody>
                            for s in &p.scenarios {
                                <tr><td>{s.name}</td><td>{&s.first_short}</td><td>{&s.short_hours}</td><td>{&s.used}</td><td>{&s.lapsing}</td></tr>
                            }
                        </tbody>
                    </table>
                </section>
            }
            <section>
                <h2>"Accounts"</h2>
                <table>
                    <caption>"Accounts, plans and quota windows"</caption>
                    <thead>
                        <tr><th>"Account"</th><th>"Status"</th><th>"Plan"</th><th>"Price"</th><th>"Windows"</th><th>"Credits"</th><th>"Value (7 d)"</th></tr>
                    </thead>
                    <tbody>
                        for a in &view.accounts {
                            <tr data-status={&a.status}>
                                <td>{&a.email}<br><small>{&a.provider}</small></td>
                                <td>{&a.status}</td>
                                <td>{&a.plan}</td>
                                <td>{&a.price}</td>
                                <td>
                                    for w in &a.windows {
                                        <div>{w}</div>
                                    }
                                </td>
                                <td>{&a.credits}</td>
                                <td>{&a.value}</td>
                            </tr>
                        }
                    </tbody>
                </table>
            </section>
        </Layout>
    })
}

async fn parity_page(req: Req, State(shadow): State<Shared>) -> HtmlResult {
    let crate::watch::Snapshot {
        tallies,
        recent,
        live_summary: live,
        live_error,
    } = shadow.snapshot();
    let rows: Vec<TallyView> = tallies
        .into_iter()
        .map(|(kind, t)| TallyView::of(kind, &t))
        .collect();
    let checks: Vec<CheckView> = recent.iter().map(CheckView::of).collect();
    let live_at = live.map_or_else(|| "not yet".to_string(), |(at, _)| when(&js::iso(at)));
    req.page(html! {
        <Layout req={&req} title="Parity (shadow)">
            <h1>"Parity with the live service"</h1>
            <p>
                "Watching since "{when(&js::iso(shadow.started_at))}". Live "<code>{&shadow.live_url}</code>
                " answered /api/summary last at "{&live_at}"."
            </p>
            if let Some(e) = &live_error {
                <p role="alert">"The live service did not answer: "{e}</p>
            }
            <table>
                <caption>"Checks by kind (unaligned: the shadow could not read the inputs the live service used)"</caption>
                <thead>
                    <tr><th>"Kind"</th><th>"Match"</th><th>"Differs"</th><th>"Unaligned"</th><th>"Last match"</th><th>"Last difference"</th></tr>
                </thead>
                <tbody>
                    for t in &rows {
                        <tr data-outcome={t.state}>
                            <td>{t.kind}</td><td>{&t.matched}</td><td>{&t.differed}</td><td>{&t.unaligned}</td><td>{&t.last_match}</td><td>{&t.last_difference}</td>
                        </tr>
                    }
                </tbody>
            </table>
            <table>
                <caption>"Recent checks, newest first"</caption>
                <thead>
                    <tr><th>"Checked"</th><th>"Kind"</th><th>"Live document"</th><th>"Outcome"</th><th>"Build"</th><th>"Detail"</th></tr>
                </thead>
                <tbody>
                    for c in &checks {
                        <tr data-outcome={c.outcome}>
                            <td>{&c.checked_at}</td><td>{c.kind}</td><td>{&c.subject}</td><td>{c.outcome}</td><td>{&c.build}</td><td>{&c.detail}</td>
                        </tr>
                    }
                </tbody>
            </table>
        </Layout>
    })
}

#[component]
fn Layout<'a>(req: &'a Req, title: &'a str, children: impl Render) -> impl Document {
    html! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8">
                <meta name="viewport" content="width=device-width, initial-scale=1">
                <link rel="stylesheet" href={req.asset("style.css")}>
                <title>{title}</title>
                <Corten />
            </head>
            <body>
                <header>
                    <nav>
                        <a href={url::Overview}>"Dashboard"</a>
                        <a href={url::SummaryPage} data-nav aria-current={req.current(&url::SummaryPage)}>"Summary"</a>
                        <a href={url::Parity} data-nav aria-current={req.current(&url::Parity)}>"Parity"</a>
                        <a href={url::Summary}>"/api/summary"</a>
                    </nav>
                </header>
                <main>{children}</main>
            </body>
        </html>
    }
}

/// An ISO time as the page shows it: `2026-10-27 04:19:02Z`.
fn when(iso: &str) -> String {
    let s = iso.replacen('T', " ", 1);
    match s.find('.') {
        Some(dot) if s.ends_with('Z') => format!("{}Z", &s[..dot]),
        _ => s,
    }
}

fn when_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str)
        .map_or_else(|| "none".into(), when)
}

fn num(v: Option<&Value>) -> String {
    js::finite(v).map_or_else(|| "-".into(), js::number_to_string)
}

fn usd(v: Option<&Value>) -> String {
    js::finite(v).map_or_else(|| "-".into(), |f| format!("${}", js::number_to_string(f)))
}

fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => "-".into(),
        Some(v) => js::to_string(Some(v)),
    }
}

pub struct SummaryView {
    generated_at: String,
    checked_at: String,
    forecast_at: String,
    value_at: String,
    monthly_cost: String,
    providers: Vec<ProviderView>,
    accounts: Vec<AccountView>,
}

struct ProviderView {
    name: String,
    facts: Vec<(&'static str, String)>,
    scenarios: Vec<ScenarioView>,
}

struct ScenarioView {
    name: &'static str,
    first_short: String,
    short_hours: String,
    used: String,
    lapsing: String,
}

struct AccountView {
    email: String,
    provider: String,
    status: String,
    plan: String,
    price: String,
    windows: Vec<String>,
    credits: String,
    value: String,
}

impl SummaryView {
    pub fn of(doc: &Value) -> SummaryView {
        let g = |k: &str| doc.get(k);
        let providers = js::entries(g("providers"))
            .into_iter()
            .map(|(_, p)| {
                let p = Some(p);
                let f = js::get(p, "forecast");
                let value = js::get(p, "value");
                let mut facts = vec![
                    (
                        "Accounts",
                        format!(
                            "{} ({} routable)",
                            num(js::get(p, "accounts")),
                            num(js::get(p, "routable"))
                        ),
                    ),
                    ("Monthly cost", usd(js::get(p, "monthlyCostUsd"))),
                    ("Credits banked", num(js::get(p, "creditsBanked"))),
                    (
                        "Next credit expiry",
                        when_of(js::get(p, "nextCreditExpiry")),
                    ),
                ];
                if js::truthy(f) {
                    facts.extend([
                        (
                            "Forecast state",
                            format!(
                                "{}{}",
                                text(js::get(f, "state")),
                                if js::truthy(js::get(f, "stale")) {
                                    " (stale)"
                                } else {
                                    ""
                                }
                            ),
                        ),
                        ("Demand (units/h)", num(js::get(f, "demandUnitsPerHour"))),
                        ("Remaining units", num(js::get(f, "remainingUnits"))),
                        ("Next reset", when_of(js::get(f, "nextResetAt"))),
                        (
                            "Pace: max units",
                            num(js::get(js::get(f, "pace"), "maxUnits")),
                        ),
                    ]);
                }
                if js::truthy(value) {
                    facts.push((
                        "API-equivalent value",
                        format!(
                            "{} over {} days",
                            usd(js::get(value, "apiEquivalentUsd")),
                            num(js::get(value, "windowDays"))
                        ),
                    ));
                }
                let scenarios = ["naturalOnly", "bankedCredits", "weeklyGrant"]
                    .into_iter()
                    .filter(|k| js::truthy(js::get(f, k)))
                    .map(|k| {
                        let s = js::get(f, k);
                        ScenarioView {
                            name: k,
                            first_short: when_of(js::get(s, "firstShortAt")),
                            short_hours: num(js::get(s, "shortHours")),
                            used: num(js::get(s, "creditsUsed")),
                            lapsing: num(js::get(s, "creditsLapsing")),
                        }
                    })
                    .collect();
                ProviderView {
                    name: text(js::get(p, "name")),
                    facts,
                    scenarios,
                }
            })
            .collect();
        let accounts = match g("accounts") {
            Some(Value::Array(a)) => a.iter().map(AccountView::of).collect(),
            _ => Vec::new(),
        };
        SummaryView {
            generated_at: when_of(g("generatedAt")),
            checked_at: when_of(g("checkedAt")),
            forecast_at: when_of(g("forecastGeneratedAt")),
            value_at: when_of(g("valueGeneratedAt")),
            monthly_cost: usd(g("monthlyCostUsd")),
            providers,
            accounts,
        }
    }
}

impl AccountView {
    fn of(a: &Value) -> AccountView {
        let a = Some(a);
        let windows = js::get(a, "windows")
            .and_then(Value::as_array)
            .map(|ws| {
                ws.iter()
                    .map(|w| {
                        let w = Some(w);
                        let reset = js::get(w, "resetAt")
                            .and_then(Value::as_str)
                            .map(|r| format!(", resets {}", when(r)))
                            .unwrap_or_default();
                        format!(
                            "{} {}: {}%{reset}",
                            text(js::get(w, "scope")),
                            text(js::get(w, "label")),
                            num(js::get(w, "usedPercent"))
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let credits = js::get(a, "credits");
        let value = js::get(a, "value");
        AccountView {
            email: text(js::get(a, "email")),
            provider: text(js::get(a, "provider")),
            status: format!(
                "{}{}",
                text(js::get(a, "status")),
                if js::truthy(js::get(a, "routable")) {
                    ""
                } else {
                    ", not routable"
                }
            ),
            plan: text(js::get(js::get(a, "plan"), "label")),
            price: format!(
                "{} ({})",
                usd(js::get(a, "priceUsdPerMonth")),
                text(js::get(a, "priceSource"))
            ),
            windows,
            credits: if js::truthy(js::get(credits, "known")) {
                num(js::get(credits, "count"))
            } else {
                "unknown".into()
            },
            value: if js::truthy(value) {
                format!(
                    "{} ({}x)",
                    usd(js::get(value, "apiEquivalentUsd")),
                    num(js::get(value, "valueMultiple"))
                )
            } else {
                "-".into()
            },
        }
    }
}

struct TallyView {
    kind: &'static str,
    state: &'static str,
    matched: String,
    differed: String,
    unaligned: String,
    last_match: String,
    last_difference: String,
}

impl TallyView {
    fn of(kind: Kind, t: &crate::watch::Tally) -> TallyView {
        TallyView {
            kind: kind.name(),
            state: if t.differed > 0 {
                "differs"
            } else if t.matched > 0 {
                "match"
            } else {
                "unaligned"
            },
            matched: t.matched.to_string(),
            differed: t.differed.to_string(),
            unaligned: t.unaligned.to_string(),
            last_match: t.last_match.as_deref().map_or_else(|| "-".into(), when),
            last_difference: t.last_difference.as_ref().map_or_else(
                || "-".into(),
                |c| {
                    format!(
                        "{}: {}",
                        c.subject.as_deref().map_or_else(|| "-".into(), when),
                        c.detail.as_deref().unwrap_or("")
                    )
                },
            ),
        }
    }
}

struct CheckView {
    checked_at: String,
    kind: &'static str,
    subject: String,
    outcome: &'static str,
    build: String,
    detail: String,
}

impl CheckView {
    fn of(c: &parity::Check) -> CheckView {
        CheckView {
            checked_at: when(&c.checked_at),
            kind: c.kind.name(),
            subject: c.subject.as_deref().map_or_else(|| "-".into(), when),
            outcome: match c.outcome {
                Outcome::Match => "match",
                Outcome::Differs => "differs",
                Outcome::Unaligned => "unaligned",
            },
            build: c
                .build_ms
                .map_or_else(|| "-".into(), |ms| format!("{ms} ms")),
            detail: c.detail.clone().unwrap_or_default(),
        }
    }
}
