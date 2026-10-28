//! The shadow's pages and JSON: the dashboard and `/api/summary` built by `pool-core` from the
//! live install's files, and the parity checks against the live service.
//!
//! | Route | Answer |
//! |---|---|
//! | `GET /`, `/accounts`, `/usage`, `/policy` | the live dashboard's four tabs ([`crate::dash`]) |
//! | `GET /summary` | `/api/summary` as a page: per provider cost, credits, forecast and scenarios; per account plan, windows and value |
//! | `GET /parity` | the parity checks: counts per kind, the last difference, the recent checks |
//! | `GET /api/summary` | `agent-pools-summary/v1`, as `JSON.stringify` writes it |
//! | `GET /api/status`, `/api/capacity`, `/api/ladders`, `/api/value` | the service's documents, from the files |
//! | `GET /api/parity` | the parity counts and recent checks as JSON |
//! | `GET /api/health`, `POST /api/refresh`, `/api/login`, `/api/callback` | on a copy only (`run-copy`): the write side, through the stand-in proxy; 404 on the read-only shadow |
//! | `POST /api/shutdown` | on a copy only: with the run's management key as a Bearer, 202 and the copy stops (`corten::shutdown`); other writes get 503 while it does |

use corten::axum::http::StatusCode;
use corten::prelude::*;
use pool_core::js;
use serde_json::{Value, json};

use crate::dash::signin::{cancel as signin_cancel, finish as signin_finish, open as signin_open, page as signin_page};
use crate::dash::{accounts, live, overview, policy, usage};
use crate::parity::{self, Kind, Outcome};
use crate::tailnet::Owner;
use crate::watch::{Shared, now_ms};

/// How often the live service's memory (`/api/status`) is read for the dashboard.
const MEMORY_EVERY: std::time::Duration = std::time::Duration::from_secs(5);
/// How often the dashboard's files' modification times are looked at.
const SAVES_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

/// The app's security policy: style attributes allowed (the dashboard sizes meters, bars and
/// timeline marks from data: corten#139), and the tailnet view's name and origin when it is set.
fn security(shadow: &Shared) -> corten::security::Policy {
    let p = corten::security::Policy::new().allow_style_attributes();
    match shadow.tailnet.get() {
        Some(t) => t.policy(p),
        None => p,
    }
}

/// The app on a copy of the install (`pools-shadow run-copy`): the same pages over the copy's
/// files, and the write side's refresh loop in place of the parity checks.
pub fn app_copy(
    shadow: Shared,
    assets: std::path::PathBuf,
    copy: std::sync::Arc<crate::writer::Copy>,
    delays: crate::writer::Delays,
    options: pool_core::refresh::Options,
) -> App {
    let security = security(&shadow);
    let _ = shadow.writer.set(copy.trigger.clone());
    let _ = shadow.copy.set(copy.clone());
    let (n, w) = (shadow.clone(), shadow.clone());
    let (watch, drain, d, value, closing) = (copy.clone(), copy.clone(), shadow.clone(), copy.clone(), copy.clone());
    let models = copy.models.clone().map(|m| (m, copy.io.root.clone(), shadow.clone()));
    let app = App::new(url::router(shadow))
        .assets(assets)
        .policy(security)
        .shutdown_grace(std::time::Duration::from_secs(20))
        // Open pages redraw when the write side's memory changes (a save, a publication, a
        // report); no file is looked at.
        .task("notice-memory", move |signal| {
            crate::watch::notice_memory(n, signal)
        })
        .task("refresh", move |signal| {
            crate::writer::run(copy, delays, options, move || w.changed_inputs(), signal)
        })
        .task("check-proxy", move |signal| {
            crate::writer::health::run(watch.io.clone(), watch.health.clone(), watch.trigger.clone(), watch.supervisor.clone(), crate::writer::health::EVERY, signal)
        })
        .task("usage-ledger", move |signal| {
            crate::writer::ledger::run(drain.ledger.clone(), drain.io.clone(), drain.health.clone(), std::time::Duration::from_secs(5), move || d.changed_inputs(), signal)
        })
        .task("value-report", move |signal| {
            crate::writer::report::run(value.io.clone(), crate::writer::report::FIRST, signal)
        })
        // A signal (Ctrl-C, Ctrl-Break, the console closing) stops the copy as `stop("signal")`
        // does; Corten does not say which signal.
        .task("closing", move |signal| async move {
            signal.aborted().await;
            closing.stop("signal");
        });
    // model-refresh.mjs's two scheduled tasks; the pool's status is the copy's own /api/status.
    match models {
        Some(((prober, delays), root, s)) => app.task("model-refresh", move |signal| {
            let (status, changed) = (s.clone(), s);
            crate::writer::models::run(
                root,
                prober,
                move || {
                    let now = now_ms();
                    status.built(now).map(|b| (*b.status(now)).clone())
                },
                move || changed.changed_inputs(),
                delays,
                signal,
            )
        }),
        None => app,
    }
}

/// The app: its routes and the two tasks that keep the parity checks running.
pub fn app(
    shadow: Shared,
    assets: std::path::PathBuf,
    watch_every: std::time::Duration,
    summary_every: std::time::Duration,
) -> App {
    let security = security(&shadow);
    let (w, s, st, n) = (shadow.clone(), shadow.clone(), shadow.clone(), shadow.clone());
    App::new(url::router(shadow))
        .assets(assets)
        .policy(security)
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

    #[route("/signin", get = signin_page, post = signin_open)]
    pub struct Signin<'a> {
        pub provider: Option<&'a str>,
        pub account: Option<&'a str>,
    }

    #[route("/signin/finish", post = signin_finish)]
    pub struct SigninFinish;

    #[route("/signin/cancel", post = signin_cancel)]
    pub struct SigninCancel;

    #[route("/api/summary", get = summary_json)]
    pub struct Summary;

    #[route("/api/status", get = status_json)]
    pub struct Status;

    #[route("/api/ladders", get = ladders_json)]
    pub struct Ladders;

    #[route("/api/parity", get = parity_json)]
    pub struct ParityJson;

    #[route("/api/capacity", get = capacity_json)]
    pub struct Capacity;

    #[route("/api/value", get = value_json)]
    pub struct ValueJson;

    #[route("/api/health", get = health_json)]
    pub struct Health;

    #[route("/api/refresh", post = api_refresh)]
    pub struct ApiRefresh;

    #[route("/api/login", post = api_login)]
    pub struct ApiLogin;

    #[route("/api/callback", post = api_callback)]
    pub struct ApiCallback;

    #[route("/api/shutdown", post = api_shutdown)]
    pub struct ApiShutdown;
}

/// pool-service.mjs `send(res, status, data)`.
fn send(status: StatusCode, data: Value) -> JsonStatus {
    JsonStatus(status, JsJson(data))
}

fn error(status: StatusCode, message: &str) -> JsonStatus {
    send(status, json!({ "error": message }))
}

/// `/api/capacity`: the forecast as last published (`capacity.current()`), or 503.
async fn capacity_json(State(shadow): State<Shared>) -> RawJson {
    let doc = match shadow.copy.get() {
        Some(c) => c.io.memory.forecast(),
        None => {
            let s = shadow.clone();
            tokio::task::spawn_blocking(move || s.install.doc("state/capacity-forecast.json"))
                .await
                .ok()
                .flatten()
                .filter(|d| d.get("format").and_then(Value::as_str) == Some(pool_core::forecast::FORECAST_FORMAT))
        }
    };
    match doc {
        Some(doc) => RawJson(StatusCode::OK, shadow.capacity_bytes.bytes(&doc)),
        None => RawJson::error(StatusCode::SERVICE_UNAVAILABLE, "No capacity forecast yet"),
    }
}

/// `/api/value`: `state/usage-value.json` as written (a copy's as its report task wrote it), or
/// 503.
async fn value_json(State(shadow): State<Shared>) -> RawJson {
    if let Some(c) = shadow.copy.get() {
        return match c.io.memory.value() {
            Some((_, bytes)) => RawJson(StatusCode::OK, bytes),
            None => RawJson::error(StatusCode::SERVICE_UNAVAILABLE, "No usage value report yet"),
        };
    }
    let path = shadow.install.root().join("state/usage-value.json");
    match tokio::fs::read(path).await {
        Ok(bytes) => RawJson(StatusCode::OK, bytes.into()),
        Err(_) => RawJson::error(StatusCode::SERVICE_UNAVAILABLE, "No usage value report yet"),
    }
}

/// A JSON body passed on as its bytes.
pub struct RawJson(StatusCode, corten::axum::body::Bytes);

impl RawJson {
    /// pool-service.mjs `send(res, status, { error })`.
    fn error(status: StatusCode, message: &str) -> RawJson {
        RawJson(status, js::stringify(&json!({ "error": message })).into())
    }
}

impl corten::axum::response::IntoResponse for RawJson {
    fn into_response(self) -> corten::axum::response::Response {
        let mut res = corten::axum::response::Response::new(corten::axum::body::Body::from(self.1));
        *res.status_mut() = self.0;
        res.headers_mut().insert(corten::axum::http::header::CONTENT_TYPE, corten::axum::http::HeaderValue::from_static("application/json"));
        res
    }
}

impl corten::answer::Answer for RawJson {
    const FORMAT: corten::answer::Format = corten::answer::Format::Json;
    type Inputs = corten::answer::AnyInputs;
}

fn copy_of(shadow: &Shared) -> Result<std::sync::Arc<crate::writer::Copy>, JsonStatus> {
    shadow.copy.get().cloned().ok_or_else(|| error(StatusCode::NOT_FOUND, "Not found"))
}

/// `/api/health` on a copy: the service, its process, and whether the stand-in proxy answers.
async fn health_json(State(shadow): State<Shared>) -> JsonStatus {
    match copy_of(&shadow) {
        Ok(copy) => send(StatusCode::OK, json!({ "service": "agent-pools", "pid": std::process::id(), "proxyHealthy": copy.health.healthy(), "closing": copy.closing() })),
        Err(e) => e,
    }
}

/// `POST /api/shutdown` (the guardian's graceful stop): the run's management key as a Bearer, or
/// 403; then 202, sent before the connection closes, and the copy stops.
async fn api_shutdown(State(shadow): State<Shared>, headers: corten::axum::http::HeaderMap) -> JsonStatus {
    let copy = match copy_of(&shadow) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let presented = headers.get(corten::axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if presented != Some(format!("Bearer {}", copy.management_key).as_str()) {
        return error(StatusCode::FORBIDDEN, "Unauthorized");
    }
    copy.stop("api-shutdown");
    send(StatusCode::ACCEPTED, json!({ "ok": true }))
}

/// pool-service.mjs `if (closing && req.method === "POST")`: a write while the copy stops.
fn refuse_while_closing(copy: &crate::writer::Copy) -> Result<(), JsonStatus> {
    if copy.closing() {
        return Err(error(StatusCode::SERVICE_UNAVAILABLE, "Service stopping"));
    }
    Ok(())
}

/// The tailnet page's origin when the owner writes from it (tailnet-dashboard-proxy.mjs passes
/// such a write on with the service's own origin).
fn tailnet_origin(shadow: &Shared, owner: Owner) -> Option<&str> {
    (owner == Owner::Tailnet).then(|| shadow.tailnet.get().map(|t| t.origin.as_str())).flatten()
}

/// The checks pool-service.mjs puts before every write: from this origin (or `also`, the
/// tailnet page's), as JSON, at most 8 KiB, parsed (`{}` when empty). Its own origin is
/// `http://<the Host it was asked for>`.
fn json_input(headers: &corten::axum::http::HeaderMap, body: &[u8], also: Option<&str>) -> Result<Value, JsonStatus> {
    let header = |name: corten::axum::http::HeaderName| headers.get(name).and_then(|v| v.to_str().ok());
    let own = header(corten::axum::http::header::HOST).map(|h| format!("http://{h}"));
    let origin = header(corten::axum::http::header::ORIGIN);
    let same_origin = (own.is_some() && origin == own.as_deref()) || (also.is_some() && origin.map(|o| o.trim_end_matches('/')) == also);
    if !same_origin || !header(corten::axum::http::header::CONTENT_TYPE).is_some_and(|t| t.starts_with("application/json")) {
        return Err(error(StatusCode::FORBIDDEN, "Same-origin JSON required"));
    }
    if body.len() > 8192 {
        return Err(error(StatusCode::INTERNAL_SERVER_ERROR, "Request too large"));
    }
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    serde_json::from_slice(body).map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))
}

/// `POST /api/refresh`: the stand-in's live model lists re-read (ignored when refused) and a
/// refresh cycle started; 202 at once.
async fn api_refresh(State(shadow): State<Shared>, Caller(owner): Caller<Owner>, headers: corten::axum::http::HeaderMap, body: corten::axum::body::Bytes) -> JsonStatus {
    let copy = match copy_of(&shadow) {
        Ok(c) => c,
        Err(e) => return e,
    };
    if let Err(e) = refuse_while_closing(&copy) {
        return e;
    }
    if let Err(e) = json_input(&headers, &body, tailnet_origin(&shadow, owner)) {
        return e;
    }
    let io = copy.io.clone();
    tokio::spawn(async move {
        let _ = io.call("live-models/refresh", Some(json!({})), None).await;
    });
    copy.trigger.request();
    send(StatusCode::ACCEPTED, json!({ "ok": true }))
}

/// `POST /api/login` `{ provider }`: the stand-in's sign-in address for that provider.
async fn api_login(State(shadow): State<Shared>, Caller(owner): Caller<Owner>, headers: corten::axum::http::HeaderMap, body: corten::axum::body::Bytes) -> JsonStatus {
    let copy = match copy_of(&shadow) {
        Ok(c) => c,
        Err(e) => return e,
    };
    if let Err(e) = refuse_while_closing(&copy) {
        return e;
    }
    let input = match json_input(&headers, &body, tailnet_origin(&shadow, owner)) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let Some(adapter) = input.get("provider").and_then(Value::as_str).and_then(pool_core::providers::provider) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Unknown provider");
    };
    match copy.io.call(&format!("{}-auth-url?is_webui=true", adapter.login_id), None, None).await {
        Ok(result) => send(StatusCode::OK, result),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.message),
    }
}

/// `POST /api/callback` `{ provider (a loginId), redirect_url, ... }`: passed to the stand-in's
/// `oauth-callback`, which signs nothing in.
async fn api_callback(State(shadow): State<Shared>, Caller(owner): Caller<Owner>, headers: corten::axum::http::HeaderMap, body: corten::axum::body::Bytes) -> JsonStatus {
    let copy = match copy_of(&shadow) {
        Ok(c) => c,
        Err(e) => return e,
    };
    if let Err(e) = refuse_while_closing(&copy) {
        return e;
    }
    let input = match json_input(&headers, &body, tailnet_origin(&shadow, owner)) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let login = input.get("provider").and_then(Value::as_str);
    let known = login.is_some_and(|l| pool_core::providers::PROVIDERS.iter().any(|p| p.login_id == l));
    if !known || !input.get("redirect_url").is_some_and(Value::is_string) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Invalid callback");
    }
    match copy.io.call("oauth-callback", Some(input), None).await {
        Ok(result) => send(StatusCode::OK, result),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.message),
    }
}

/// A JSON document written as `JSON.stringify` writes it (key order, number format), so the
/// shadow's bytes can be set beside the live service's.
pub struct JsJson(pub Value);

impl corten::json::ToJson for JsJson {
    fn write_json(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(js::stringify(&self.0).as_bytes());
    }
}

/// The answers for now: on a copy from its memory at once; on the read-only shadow off the
/// async threads (its inputs may be read from files).
pub async fn built(shadow: &Shared, now: f64) -> corten::Result<std::sync::Arc<crate::served::Built>> {
    let b = if shadow.copy.get().is_some() {
        shadow.built(now)
    } else {
        let s = shadow.clone();
        tokio::task::spawn_blocking(move || s.built(now)).await?
    };
    b.ok_or_else(|| corten::Error::new("state/pool-state.json did not read").with_status(StatusCode::SERVICE_UNAVAILABLE))
}

async fn build_summary(shadow: &Shared) -> corten::Result<Value> {
    let now = now_ms();
    let bytes = built(shadow, now).await?.summary_bytes(now);
    Ok(serde_json::from_slice(&bytes)?)
}

/// What a refresh form sends: the account whose row it was on, to go back to.
#[derive(serde::Deserialize)]
struct RefreshInput {
    open: Option<String>,
}

/// `POST /refresh`: a refresh cycle now, on a copy (`run-copy`), then back to the overview, or
/// to the account's row when the form was on one; the read-only shadow has no write side and
/// answers 404.
async fn refresh_now(req: Req, State(shadow): State<Shared>, _owner: Caller<Owner>, Form(input): Form<RefreshInput>) -> HtmlResult {
    let Some(trigger) = shadow.writer.get() else {
        return Err(corten::Error::new("This shadow only reads; refresh on the live dashboard").with_status(StatusCode::NOT_FOUND));
    };
    if shadow.copy.get().is_some_and(|c| c.closing()) {
        return Err(corten::Error::new("Service stopping").with_status(StatusCode::SERVICE_UNAVAILABLE));
    }
    trigger.request();
    match input.open.as_deref().filter(|id| !id.is_empty()) {
        Some(id) => req.redirect(&url::Accounts { filter: None, open: Some(id) }.at(crate::dash::views::AcctId(id))),
        None => req.redirect(&url::Overview),
    }
}

/// `/api/ladders`: the routing ladders last published, as the service keeps them (the file it
/// wrote: the live install's for the read-only shadow, the copy's own on `run-copy`), or 503
/// with the live service's body before there are any.
async fn ladders_json(State(shadow): State<Shared>) -> RawJson {
    let doc = match shadow.copy.get() {
        Some(c) => c.io.memory.ladders(),
        None => {
            let s = shadow.clone();
            tokio::task::spawn_blocking(move || s.install.doc("state/routing-ladders.json"))
                .await
                .ok()
                .flatten()
                .filter(|d| d.get("format").and_then(Value::as_str) == Some("routing-ladders/v1"))
        }
    };
    match doc {
        Some(doc) => RawJson(StatusCode::OK, shadow.ladders_bytes.bytes(&doc)),
        None => RawJson::error(StatusCode::SERVICE_UNAVAILABLE, "No routing ladders yet"),
    }
}

/// A JSON body with a status of its own, as pool-service.mjs `send(res, status, doc)` answers.
pub struct JsonStatus(StatusCode, JsJson);

impl corten::axum::response::IntoResponse for JsonStatus {
    fn into_response(self) -> corten::axum::response::Response {
        (self.0, Json(self.1)).into_response()
    }
}

impl corten::answer::Answer for JsonStatus {
    const FORMAT: corten::answer::Format = corten::answer::Format::Json;
    type Inputs = corten::answer::AnyInputs;
}

/// `/api/summary` at the request's moment, from the answers kept for the inputs.
async fn summary_json(State(shadow): State<Shared>) -> corten::Result<RawJson> {
    let now = now_ms();
    Ok(RawJson(StatusCode::OK, built(&shadow, now).await?.summary_bytes(now)))
}

/// `/api/status` from the files. `processes`, `holds` and `usageLedger` are the live service's
/// memory, so they are `null` here; on a copy, `holds` is its proxy watch's last reading and
/// `usageLedger` its ledger's stats.
async fn status_json(State(shadow): State<Shared>) -> corten::Result<RawJson> {
    let now = now_ms();
    Ok(RawJson(StatusCode::OK, built(&shadow, now).await?.status_bytes(now)))
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
                        <a {..req.nav(&url::SummaryPage)}>"Summary"</a>
                        <a {..req.nav(&url::Parity)}>"Parity"</a>
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
