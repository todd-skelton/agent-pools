//! The live dashboard (`dashboard.html`) on Corten: the same four tabs, built on the server from
//! the shadow's `/api/status` and the usage value report on disk. On the read-only shadow its
//! actions are shown but disabled (sign-in, refresh): they change the pool. On a copy
//! (`run-copy`) they work: refresh is a form, sign-in is [`signin`]'s steps.
//!
//! | Route | Tab |
//! |---|---|
//! | `GET /` | Overview: needs attention, the provider cards, the next 14 days |
//! | `GET /accounts?filter=&open=` | Accounts: a table per provider; `open` expands a row |
//! | `GET /usage` | Usage & value |
//! | `GET /policy` | Automation |
//! | `GET /live?tab=&filter=&open=` | the open tab, sent again whenever it changes, for its `<corten-live>` |
//! | `GET /signin?provider=&account=`, `POST /signin`, `/signin/finish`, `/signin/cancel` | on a copy: the sign-in steps ([`signin`]) |

pub mod fmt;
pub mod model;
pub mod signin;
pub mod views;

/// The dashboard's components with a stylesheet: the tabs' and the sign-in steps'.
pub const COMPONENTS: &[corten::Component] = &[
    views::Dash,
    views::Action,
    views::Attention,
    views::Overview,
    views::LadderBox,
    views::ProviderPanel,
    views::TimelinePanel,
    views::AccountsTab,
    views::UsageTab,
    views::PolicyTab,
    signin::SigninSteps,
];

use corten::prelude::*;
use serde_json::Value;

use corten::{LiveResponse, LiveSince};

use crate::app::url;
use crate::watch::{Shared, now_ms};
use model::{AccountsView, Ctx, Filter, Item, PolicyView, ProviderCard, Timeline, UsageView};
use views::{Dash, Header};

/// What every tab is drawn from: `S`, `V` and `NOW` of the live page.
pub struct Data {
    pub s: std::sync::Arc<Value>,
    pub v: Option<std::sync::Arc<Value>>,
    pub now: f64,
    pub z: fmt::Zone,
    /// Whether the actions work (a copy).
    pub writable: bool,
    /// The sign-in banner (a copy's).
    pub signin: Option<signin::Banner>,
    /// The live service's pages, without the shadow's marks.
    pub service: bool,
}

impl Data {
    pub fn ctx(&self) -> Ctx<'_> {
        Ctx { s: &self.s, v: self.v.as_deref(), now: self.now, z: self.z, writable: self.writable }
    }
}

/// The status as the shadow builds it (the service's memory, `processes`, `holds` and
/// `usageLedger`: the live service's last `/api/status` answer, or a copy's own) and the value
/// report: the answers kept for the inputs ([`crate::served`]), so every open page shares one
/// build.
pub async fn load(shadow: &Shared) -> corten::Result<Data> {
    let now = now_ms();
    let built = crate::app::built(shadow, now).await?;
    let s = built.status(now);
    let value = built.inputs.value.clone();
    let writable = shadow.copy.get().is_some();
    if writable && shadow.signin.observe(&s, now).is_some() {
        shadow.changed_inputs();
    }
    let banner = shadow.signin.banner(now).filter(|_| writable);
    Ok(Data { s, v: value.filter(|v| v.is_object()), now, z: fmt::Zone::from_env(), writable, signin: banner, service: shadow.service.get().is_some() })
}

/// Between changes, a copy's open page takes the status kept for this moment (the same one
/// until an account's reading turns stale), so a reading that goes stale shows at once.
fn restatus(shadow: &Shared, d: &mut Data) {
    if shadow.copy.get().is_some()
        && let Some(b) = shadow.built(d.now)
    {
        d.s = b.status(d.now);
    }
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

    fn title(self, service: bool) -> String {
        views::titled(
            match self {
                Tab::Overview => "Agent pools",
                Tab::Accounts => "Accounts · Agent pools",
                Tab::Usage => "Usage & value · Agent pools",
                Tab::Policy => "Automation · Agent pools",
            },
            service,
        )
    }
}

/// What a tab shows, worked out once from the model and drawn twice: as text, for the live
/// stream's fragment and its cursor, and inside a full page's own render, so the sizes it sets
/// (`data-vars`) reach the page's style block. Drawn ahead as text and set in with `raw`, they
/// would not, and a page would have no sizes until its first live update.
pub enum Body {
    Overview { items: Vec<Item>, names: Vec<Option<String>>, healthy: Vec<String>, cards: Box<[ProviderCard; 2]>, tl: Timeline },
    Accounts(AccountsView),
    Usage(Result<UsageView, String>),
    Policy(PolicyView),
}

impl Body {
    pub fn view(&self) -> impl Render + '_ {
        html! {
            match self {
                Body::Overview { items, names, healthy, cards, tl } => {
                    <views::Overview items={items} names={names.clone()} healthy={healthy} cards={&cards[..]} tl={tl} />
                }
                Body::Accounts(v) => <views::AccountsTab v={v} />,
                Body::Usage(u) => <views::UsageTab u={u} />,
                Body::Policy(p) => <views::PolicyTab p={p} />,
            }
        }
    }
}

/// A tab drawn from `d`: its body (in `#dash-body`) and the header's count and freshness.
pub fn draw(d: &Data, tab: Tab, filter: Option<&str>, open: Option<&str>) -> (Body, Header) {
    let c = d.ctx();
    let items = c.attention_items();
    let (fresh, bad) = c.freshness();
    let header = Header { count: c.attention_count(&items), fresh, bad, finish: d.signin.clone() };
    let body = match tab {
        Tab::Overview => {
            let names = items.iter().map(|i| i.provider.as_deref().map(|p| c.pname(p))).collect();
            Body::Overview { names, healthy: c.healthy(), cards: Box::new(["claude", "codex"].map(|p| c.provider_card(p))), tl: c.timeline(), items }
        }
        Tab::Accounts => Body::Accounts(c.accounts_view(Filter::parse(filter), open)),
        Tab::Usage => Body::Usage(c.usage()),
        Tab::Policy => Body::Policy(c.policy()),
    };
    (body, header)
}

/// The fragment `/live` sends: the body, the header's two live parts and the sign-in banner,
/// each by id.
fn fragment(body: &Body, header: &Header) -> String {
    let mut out = views::body(body.view()).to_html();
    out.push_str(&views::count(header.count).to_html());
    out.push_str(&views::fresh(header).to_html());
    out.push_str(&views::finish(header).to_html());
    out
}

/// A short hash of a fragment: the stream's cursor, so a page (or a reconnect) that already shows
/// it is sent nothing until it changes. Numbers in sizes (`data-vars`) count to one decimal: the
/// timeline's marks are placed from `now` to the millisecond, so they move a little each second,
/// and 0.1% of fourteen days is twenty minutes, under a pixel.
fn cursor(fragment: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut rest = fragment;
    while let Some(at) = rest.find(SIZES) {
        let (before, after) = rest.split_at(at + SIZES.len());
        before.hash(&mut h);
        let end = after.find('"').unwrap_or(after.len());
        coarse(&after[..end]).hash(&mut h);
        rest = &after[end..];
    }
    rest.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Where a component writes the sizes it draws from data.
const SIZES: &str = " data-vars=\"";

/// A style value with each decimal number rounded to one place.
fn coarse(style: &str) -> String {
    let mut out = String::with_capacity(style.len());
    let mut chars = style.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c.is_ascii_digit() {
            let mut end = i + 1;
            while let Some(&(j, d)) = chars.peek() {
                if !(d.is_ascii_digit() || d == '.' || d == 'e' || d == '-' && style[..j].ends_with('e')) {
                    break;
                }
                end = j + d.len_utf8();
                chars.next();
            }
            match style[i..end].parse::<f64>() {
                Ok(x) => out.push_str(&format!("{x:.1}")),
                Err(_) => out.push_str(&style[i..end]),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
#[test]
fn a_mark_that_moves_under_a_tenth_of_a_percent_is_the_same_fragment() {
    let a = r#"<span class="mk reset" data-vars="--timeline-panel__left:42.123456%" title="Weekly reset Mon">x</span>"#;
    let b = r#"<span class="mk reset" data-vars="--timeline-panel__left:42.131%" title="Weekly reset Mon">x</span>"#;
    let c = r#"<span class="mk reset" data-vars="--timeline-panel__left:42.2%" title="Weekly reset Mon">x</span>"#;
    let d = r#"<span class="mk reset" data-vars="--timeline-panel__left:42.123456%" title="Weekly reset Tue">x</span>"#;
    assert_eq!(cursor(a), cursor(b));
    assert_ne!(cursor(a), cursor(c));
    assert_ne!(cursor(a), cursor(d));
    assert_eq!(coarse("--a__left:12.34%;--a__width:1e-7%"), "--a__left:12.3%;--a__width:0.0%");
}

#[derive(Default)]
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
        <Dash req={&req} title={&tab.title(d.service)} header={&header} writable={d.writable} service={d.service}>
            <corten-live src={src} since={&since}>{views::body(body.view())}</corten-live>
        </Dash>
    })
}

pub async fn overview(req: Req, State(shadow): State<Shared>) -> HtmlResult {
    page(req, shadow, Tab::Overview, DashQuery::default()).await
}

pub async fn accounts(req: Req, State(shadow): State<Shared>, Params(q): Params<url::Accounts<'_>>) -> HtmlResult {
    page(req, shadow, Tab::Accounts, DashQuery { filter: q.filter, open: q.open, tab: None }).await
}

pub async fn usage(req: Req, State(shadow): State<Shared>) -> HtmlResult {
    page(req, shadow, Tab::Usage, DashQuery::default()).await
}

pub async fn policy(req: Req, State(shadow): State<Shared>) -> HtmlResult {
    page(req, shadow, Tab::Policy, DashQuery::default()).await
}

/// `GET /live?tab=&filter=&open=`: the tab, sent again whenever it changes. The files and the
/// live service's memory are read again when the shadow notices a change (a save, within a
/// quarter second); in between, the tab is drawn again from them each second, so countdowns and
/// "ago" times move. A redraw that comes out the same is not sent. Read only, like the pages.
pub async fn live(State(shadow): State<Shared>, Params(q): Params<url::Live<'_>>, LiveSince(since): LiveSince) -> LiveResponse {
    let q = DashQuery { filter: q.filter, open: q.open, tab: q.tab };
    let tab = Tab::parse(q.tab.as_deref());
    let changes = shadow.changes.subscribe();
    let state = Stream { shadow, q, last: since, changes, data: None };
    corten::live(futures_util::stream::unfold(state, move |mut st| async move {
        loop {
            // `changed()` marks the change seen as it returns, so it is noted here.
            let mut reread = st.data.is_none();
            if st.data.is_some() {
                let next_second = 1000.0 - now_ms() % 1000.0;
                tokio::select! {
                    r = st.changes.changed() => match r {
                        Ok(()) => reread = true,
                        Err(_) => return None,
                    },
                    _ = tokio::time::sleep(std::time::Duration::from_millis(next_second as u64 + 1)) => {}
                }
            }
            if reread {
                st.changes.borrow_and_update();
                match load(&st.shadow).await {
                    Ok(d) => st.data = Some(d),
                    Err(_) if st.data.is_some() => {}
                    Err(_) => {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        continue;
                    }
                }
            }
            let Some(d) = st.data.as_mut() else { continue };
            d.now = now_ms();
            restatus(&st.shadow, d);
            let (body, header) = draw(d, tab, st.q.filter.as_deref(), st.q.open.as_deref());
            let html = fragment(&body, &header);
            let c = cursor(&html);
            if st.last.as_deref() == Some(c.as_str()) {
                continue;
            }
            st.last = Some(c.clone());
            return Some((corten::Fragment::new(html).cursor(c), st));
        }
    }))
}

/// One open tab's stream: what it last sent and the inputs it draws from.
struct Stream {
    shadow: Shared,
    q: DashQuery,
    last: Option<String>,
    changes: tokio::sync::watch::Receiver<u64>,
    data: Option<Data>,
}
