//! The live dashboard (`dashboard.html`) on Corten: the same four tabs, built on the server from
//! the shadow's `/api/status` and the usage value report on disk, read only. Its actions are
//! shown but disabled (sign-in, refresh): they change the pool, which the shadow never does.
//!
//! | Route | Tab |
//! |---|---|
//! | `GET /` | Overview: needs attention, the provider cards, the next 14 days |
//! | `GET /accounts?filter=&open=` | Accounts: a table per provider; `open` expands a row |
//! | `GET /usage` | Usage & value |
//! | `GET /policy` | Automation |
//! | `GET /live?tab=&filter=&open=` | the open tab drawn again every 15 s, for its `<corten-live>` |

pub mod fmt;
pub mod model;
pub mod views;

use corten::axum::http::StatusCode;
use corten::prelude::*;
use serde_json::Value;

use corten::{LiveResponse, LiveSince};

use crate::app::url;
use crate::watch::{Shared, now_ms};
use model::{Ctx, Filter, View};
use views::{Dash, Header};

/// What every tab is drawn from: `S`, `V` and `NOW` of the live page.
pub struct Data {
    pub s: Value,
    pub v: Option<Value>,
    pub now: f64,
    pub z: fmt::Zone,
}

impl Data {
    pub fn ctx(&self) -> Ctx<'_> {
        Ctx { s: &self.s, v: self.v.as_ref(), now: self.now, z: self.z }
    }
}

/// The status as the shadow builds it (the live service's memory, `processes`, `holds` and
/// `usageLedger`, from its last `/api/status` answer when there is one) and the value report.
pub async fn load(shadow: &Shared) -> Result<Data> {
    let s = shadow.clone();
    let live = shadow.live_memory();
    let (status, value, now) = tokio::task::spawn_blocking(move || {
        let now = now_ms();
        (crate::parity::status_now(&s.install, &live, now), s.install.json("state/usage-value.json"), now)
    })
    .await?;
    let s = status.ok_or_else(|| {
        corten::Error::new("state/pool-state.json did not read").with_status(StatusCode::SERVICE_UNAVAILABLE)
    })?;
    Ok(Data { s, v: value.filter(|v| v.is_object()), now, z: fmt::Zone::from_env() })
}

/// Each part of the dashboard as the live page fills its containers (`#attention`,
/// `#prov-claude`, ...), for `pools-shadow render-dash`, which sets them beside the JavaScript's.
pub fn fragments(d: &Data, filter: Option<&str>, open: Option<&str>) -> serde_json::Map<String, Value> {
    fn html(r: impl Render) -> Value {
        let mut out = String::new();
        r.render(&mut out);
        Value::String(out)
    }
    let c = d.ctx();
    let items = c.attention_items();
    let healthy = c.healthy();
    let (fresh, _) = c.freshness();
    let mut out = serde_json::Map::new();
    out.insert("attention".into(), html(views::attention(&items, &healthy, |p| c.pname(p))));
    out.insert("count".into(), Value::from(c.attention_count(&items)));
    out.insert("fresh".into(), Value::String(fresh));
    for p in ["claude", "codex"] {
        out.insert(format!("prov-{p}"), html(views::provider(&c.provider_card(p))));
    }
    out.insert("timeline".into(), html(views::timeline(&c.timeline())));
    out.insert("accounts".into(), html(views::accounts(&c.accounts_view(Filter::parse(filter), open))));
    out.insert("usage".into(), html(views::usage(&c.usage())));
    out.insert("policy".into(), html(views::policy(&c.policy())));
    out
}

/// A tab of the dashboard, as `/live?tab=` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Accounts,
    Usage,
    Policy,
}

impl Tab {
    fn parse(s: Option<&str>) -> Tab {
        match s {
            Some("accounts") => Tab::Accounts,
            Some("usage") => Tab::Usage,
            Some("policy") => Tab::Policy,
            _ => Tab::Overview,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Tab::Overview => "overview",
            Tab::Accounts => "accounts",
            Tab::Usage => "usage",
            Tab::Policy => "policy",
        }
    }

    fn view(self) -> View {
        match self {
            Tab::Overview => View::Overview,
            Tab::Accounts => View::Accounts,
            Tab::Usage => View::Usage,
            Tab::Policy => View::Policy,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Agent pools (shadow)",
            Tab::Accounts => "Accounts · Agent pools (shadow)",
            Tab::Usage => "Usage & value · Agent pools (shadow)",
            Tab::Policy => "Automation · Agent pools (shadow)",
        }
    }
}

/// A tab drawn from `d`: its body (in `#dash-body`) and the header's count and freshness.
pub fn draw(d: &Data, tab: Tab, filter: Option<&str>, open: Option<&str>) -> (String, Header) {
    let c = d.ctx();
    let items = c.attention_items();
    let (fresh, bad) = c.freshness();
    let header = Header { count: c.attention_count(&items), fresh, bad };
    let body = match tab {
        Tab::Overview => {
            let healthy = c.healthy();
            let cards = ["claude", "codex"].map(|p| c.provider_card(p));
            let tl = c.timeline();
            html! {
                <div class="grid">
                    <div class="card span-12">{views::attention(&items, &healthy, |p| c.pname(p))}</div>
                    for card in &cards { <div class="card span-6">{views::provider(card)}</div> }
                    <div class="card span-12">{views::timeline(&tl)}</div>
                </div>
            }
            .to_html()
        }
        Tab::Accounts => views::accounts(&c.accounts_view(Filter::parse(filter), open)).to_html(),
        Tab::Usage => views::usage(&c.usage()).to_html(),
        Tab::Policy => views::policy(&c.policy()).to_html(),
    };
    (body, header)
}

/// The fragment `/live` sends: the body and the header's two live parts, each by id.
fn fragment(body: &str, header: &Header) -> String {
    let mut out = views::body(body).to_html();
    out.push_str(&views::count(header.count).to_html());
    out.push_str(&views::fresh(header).to_html());
    out
}

/// A short hash of a fragment: the stream's cursor, so a page (or a reconnect) that already shows
/// it is sent nothing until it changes.
fn cursor(fragment: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    fragment.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[derive(serde::Deserialize, Default)]
pub struct DashQuery {
    filter: Option<String>,
    open: Option<String>,
    tab: Option<String>,
}

async fn page(req: Req, shadow: Shared, tab: Tab, q: DashQuery) -> HtmlResult {
    let d = load(&shadow).await?;
    let (body, header) = draw(&d, tab, q.filter.as_deref(), q.open.as_deref());
    let since = cursor(&fragment(&body, &header));
    let src = url::Live { tab: Some(tab.key()), filter: q.filter.as_deref(), open: q.open.as_deref() };
    req.page(html! {
        <Dash req={&req} title={tab.title()} view={tab.view()} header={&header}>
            <corten-live src={src} since={&since}>{views::body(&body)}</corten-live>
        </Dash>
    })
}

pub async fn overview(req: Req, State(shadow): State<Shared>) -> HtmlResult {
    page(req, shadow, Tab::Overview, DashQuery::default()).await
}

pub async fn accounts(req: Req, State(shadow): State<Shared>, Query(q): Query<DashQuery>) -> HtmlResult {
    page(req, shadow, Tab::Accounts, q).await
}

pub async fn usage(req: Req, State(shadow): State<Shared>) -> HtmlResult {
    page(req, shadow, Tab::Usage, DashQuery::default()).await
}

pub async fn policy(req: Req, State(shadow): State<Shared>) -> HtmlResult {
    page(req, shadow, Tab::Policy, DashQuery::default()).await
}

/// How often an open tab is drawn again: the live page polls its status every 15 s.
const REDRAW: std::time::Duration = std::time::Duration::from_secs(15);

/// `GET /live?tab=&filter=&open=`: the tab drawn again every 15 s, sent when it changed (the
/// countdowns change it at least each minute). Read only, like the pages.
pub async fn live(State(shadow): State<Shared>, Query(q): Query<DashQuery>, LiveSince(since): LiveSince) -> LiveResponse {
    let tab = Tab::parse(q.tab.as_deref());
    let mut tick = tokio::time::interval(REDRAW);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    corten::live(futures_util::stream::unfold((shadow, q, since, tick), move |(shadow, q, mut last, mut tick)| async move {
        loop {
            tick.tick().await;
            let Ok(d) = load(&shadow).await else { continue };
            let (body, header) = draw(&d, tab, q.filter.as_deref(), q.open.as_deref());
            let html = fragment(&body, &header);
            let c = cursor(&html);
            if last.as_deref() == Some(c.as_str()) {
                continue;
            }
            last = Some(c.clone());
            return Some((corten::Fragment::new(html).cursor(c), (shadow, q, last, tick)));
        }
    }))
}
