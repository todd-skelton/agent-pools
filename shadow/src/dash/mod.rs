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

pub mod fmt;
pub mod model;
pub mod views;

use corten::axum::http::StatusCode;
use corten::prelude::*;
use serde_json::Value;

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

fn header(c: &Ctx<'_>, items: &[model::Item]) -> Header {
    let (fresh, bad) = c.freshness();
    Header { count: c.attention_count(items), fresh, bad }
}

pub async fn overview(req: Req, State(shadow): State<Shared>) -> HtmlResult {
    let d = load(&shadow).await?;
    let c = d.ctx();
    let items = c.attention_items();
    let header = header(&c, &items);
    let healthy = c.healthy();
    let cards = ["claude", "codex"].map(|p| c.provider_card(p));
    let tl = c.timeline();
    req.page(html! {
        <Dash req={&req} title="Agent pools (shadow)" view={View::Overview} header={&header}>
            <div class="grid">
                <div class="card span-12">{views::attention(&items, &healthy, |p| c.pname(p))}</div>
                for card in &cards { <div class="card span-6">{views::provider(card)}</div> }
                <div class="card span-12">{views::timeline(&tl)}</div>
            </div>
        </Dash>
    })
}

#[derive(serde::Deserialize)]
pub struct AccountsQuery {
    filter: Option<String>,
    open: Option<String>,
}

pub async fn accounts(req: Req, State(shadow): State<Shared>, Query(q): Query<AccountsQuery>) -> HtmlResult {
    let d = load(&shadow).await?;
    let c = d.ctx();
    let items = c.attention_items();
    let header = header(&c, &items);
    let view = c.accounts_view(Filter::parse(q.filter.as_deref()), q.open.as_deref());
    req.page(html! {
        <Dash req={&req} title="Accounts · Agent pools (shadow)" view={View::Accounts} header={&header}>
            {views::accounts(&view)}
        </Dash>
    })
}

pub async fn usage(req: Req, State(shadow): State<Shared>) -> HtmlResult {
    let d = load(&shadow).await?;
    let c = d.ctx();
    let items = c.attention_items();
    let header = header(&c, &items);
    let view = c.usage();
    req.page(html! {
        <Dash req={&req} title="Usage & value · Agent pools (shadow)" view={View::Usage} header={&header}>
            {views::usage(&view)}
        </Dash>
    })
}

pub async fn policy(req: Req, State(shadow): State<Shared>) -> HtmlResult {
    let d = load(&shadow).await?;
    let c = d.ctx();
    let items = c.attention_items();
    let header = header(&c, &items);
    let view = c.policy();
    req.page(html! {
        <Dash req={&req} title="Automation · Agent pools (shadow)" view={View::Policy} header={&header}>
            {views::policy(&view)}
        </Dash>
    })
}
