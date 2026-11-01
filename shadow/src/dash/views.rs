//! The dashboard's markup: each part of dashboard.html as a component on Agent Pools' design
//! system ([`crate::design`]) and the UI kit ([`crate::ui`]), with its stylesheet beside it
//! (`dash.css` for the page around the tabs, `attention.css`, `provider_panel.css`, ...).
//!
//! The words, the tooltips and the sizes drawn from data are the live page's, in its order
//! (`golden/dash-text.mjs` holds them to it). Sizes are custom properties each component
//! registers (`style={WIDTH.pct(..)}`, written as `data-vars`), so no page needs style
//! attributes.

use corten::prelude::*;

use super::model::{
    AccountsView, Act, Cell, Detail, Does, Filter, Item, Ladder, PolicyView, ProviderCard, Timeline, Tone, UsageView, View,
};
use super::signin::{Banner, signin_name};
use crate::app::url;
use crate::design::{Chip, ChipSize, DotSize, Eyebrow, Kpi, Meter, ProviderDot, Segmented, Stat, Stats, StatusDot};
use crate::ui::alert::{Alert, AlertActions, AlertRole, AlertVariant};
use crate::ui::badge::{Badge, BadgeVariant};
use crate::ui::button::{Button, ButtonSize, ButtonVariant};
use crate::ui::card::{Card, CardAction, CardContent, CardFooter, CardHeader};
use crate::ui::table::Table;

/// An account's row on the accounts page: `acct-{id}`.
#[derive(Id)]
pub struct AcctId<'a>(pub &'a str);

/// The timeline card, where "Timeline" actions go.
#[derive(Id)]
pub struct TlAnchorId;

/// The tab's body, which `/live` replaces.
#[derive(Id)]
pub struct DashBodyId;

/// The header's count of items needing attention.
#[derive(Id)]
pub struct AttCountId;

/// The header's freshness line.
#[derive(Id)]
pub struct FreshId;

/// The configuration card on the automation page.
#[derive(Id)]
pub struct ConfigId;

/// The sign-in banner under the top bar (`#finish`).
#[derive(Id)]
pub struct FinishId;

/// What the header shows on every page.
pub struct Header {
    pub count: usize,
    pub fresh: String,
    pub bad: bool,
    /// The sign-in banner (a copy's).
    pub finish: Option<Banner>,
}

fn sev(t: Tone) -> &'static str {
    match t {
        Tone::Bad => "Act now",
        Tone::Warn => "Soon",
        _ => "FYI",
    }
}

const READ_ONLY: &str = "Done on the live dashboard (port 8318): the shadow only reads";
const NEXT_TIP: &str = "New sessions route here: the available account whose weekly window resets soonest";

/// A page title: the live service's as given, a shadow's or a copy's marked "(shadow)".
pub fn titled(title: &str, service: bool) -> String {
    if service { title.to_string() } else { format!("{title} (shadow)") }
}

/// The document around every dashboard page: the top bar with its tabs and freshness, and the
/// sign-in banner. On a copy (`run-copy`, `writable`) Refresh posts to `/refresh` and "Add
/// account" opens the sign-in steps; the read-only shadow shows both disabled. The tabs are
/// the shell's nav (`req.nav`): Corten marks the one for this page's path current, here and
/// after each swap, so none is current on the sign-in page. The page is the theme's dark one.
#[component(css)]
pub fn Dash<'a>(req: &'a Req, title: &'a str, header: &'a Header, writable: bool, service: bool, children: impl Render) -> impl Document {
    let accounts = url::Accounts { filter: None, open: None };
    html! {
        <!DOCTYPE html>
        <html lang="en" data-theme="dark">
            <head>
                <meta charset="utf-8">
                <meta name="viewport" content="width=device-width, initial-scale=1">
                <link rel="stylesheet" href={req.asset(crate::app::Static::BASE_CSS)}>
                <title>{title}</title>
                <Corten />
            </head>
            <body>
                <header class={Bar}>
                    <div class={BarIn}>
                        <div class={Brand}>
                            <span class={Logo}></span>"Agent pools"
                            if !service {
                                <span title="Corten shadow: built from the live install's files, read only"><Badge variant={BadgeVariant::Warning}>"shadow"</Badge></span>
                            }
                        </div>
                        <nav class={Tabs} aria-label="Dashboard">
                            <a {..req.nav(&url::Overview)}>
                                "Overview "
                                <Count n={header.count} />
                            </a>
                            <a {..req.nav(&accounts)}>"Accounts"</a>
                            <a {..req.nav(&url::Usage)}>"Usage & value"</a>
                            <a {..req.nav(&url::Policy)}>"Automation"</a>
                        </nav>
                        <div class={Right}>
                            <Fresh header={header} />
                            if !service {
                                <Button variant={ButtonVariant::Ghost} size={ButtonSize::Sm}><a href={url::Parity}>"Parity"</a></Button>
                            }
                            if writable {
                                <form class={Inline} method="post" action={url::RefreshNow}>
                                    <Button variant={ButtonVariant::Outline} size={ButtonSize::Sm}><button type="submit" title="Run a refresh cycle now">"Refresh"</button></Button>
                                </form>
                                <Button variant={ButtonVariant::Primary} size={ButtonSize::Sm}><a href={url::Signin { provider: None, account: None }}>"Add account"</a></Button>
                            } else {
                                <Button variant={ButtonVariant::Outline} size={ButtonSize::Sm}><button type="button" disabled title={READ_ONLY}>"Refresh"</button></Button>
                                <Button variant={ButtonVariant::Primary} size={ButtonSize::Sm}><button type="button" disabled title={READ_ONLY}>"Add account"</button></Button>
                            }
                        </div>
                    </div>
                </header>
                // In <main>, so a navigation (which swaps <main>) brings the banner as it is.
                <main class={Main}><Finish header={header} />{children}</main>
            </body>
        </html>
    }
}

/// The header's count: hidden at none, as the live page leaves it empty.
#[component(css = Dash)]
pub fn Count(n: usize) -> impl Render {
    html! { <span id={AttCountId} class={Count} hidden={n == 0}>{n}</span> }
}

#[component(css = Dash)]
pub fn Fresh<'a>(header: &'a Header) -> impl Render {
    html! {
        <div id={FreshId} class={(Fresh, header.bad.then_some(Stale))}><span class={Pulse}></span><span class={Said}>{&header.fresh}</span></div>
    }
}

/// `renderFinish()`: a sign-in to finish (from this PC by itself, or by pasting the link on the
/// sign-in page), or the one just confirmed (the live page's toast); hidden otherwise.
#[component(css = Dash)]
pub fn Finish<'a>(header: &'a Header) -> impl Render {
    html! {
        match &header.finish {
            None => <div id={FinishId} class={Finish} hidden></div>,
            Some(Banner::Pending(p)) => {
                <div id={FinishId} class={Finish}>
                    <Alert variant={AlertVariant::Info} role={AlertRole::Status}>
                        <p><b>"Finish "{signin_name(p)}" sign-in."</b>" "<span class={Aside}>"On this PC it finishes by itself. Signed in on another device? Paste the localhost link it opened."</span></p>
                        <AlertActions>
                            <Button variant={ButtonVariant::Primary} size={ButtonSize::Sm}><a href={url::Signin { provider: None, account: None }}>"Finish sign-in"</a></Button>
                            <form class={Inline} method="post" action={url::SigninCancel}>
                                <Button variant={ButtonVariant::Outline} size={ButtonSize::Sm}><button type="submit">"Cancel"</button></Button>
                            </form>
                        </AlertActions>
                    </Alert>
                </div>
            }
            Some(Banner::Signed(m)) => {
                <div id={FinishId} class={Finish}>
                    <Alert variant={AlertVariant::Success} role={AlertRole::Status}><p>{m}</p></Alert>
                </div>
            }
        }
    }
}

/// The tab's body, by id, so a live fragment replaces it.
pub fn body(view: impl Render) -> impl Render {
    html! { <div id={DashBodyId}>{view}</div> }
}

pub fn count(n: usize) -> impl Render {
    html! { <Count n={n} /> }
}

pub fn fresh(h: &Header) -> impl Render + '_ {
    html! { <Fresh header={h} /> }
}

pub fn finish(h: &Header) -> impl Render + '_ {
    html! { <Finish header={h} /> }
}

/// An action: a link where it goes somewhere on these pages; where it changes the pool, a
/// refresh form or a link to the sign-in steps on a copy, and a disabled button on the read-only
/// shadow.
#[component(css)]
pub fn Action<'a>(a: &'a Act, sm: bool) -> impl Render {
    let size = if sm { ButtonSize::Sm } else { ButtonSize::Md };
    let look = |primary: bool| if primary { ButtonVariant::Primary } else { ButtonVariant::Outline };
    html! {
        match a {
            Act::Account { text, id } => <Button variant={look(false)} size={size}><a href={url::Accounts { filter: None, open: Some(id) }.at(AcctId(id))}>{text}</a></Button>,
            Act::Credits { text, open: Some(id) } => <Button variant={look(false)} size={size}><a href={url::Accounts { filter: Some("credits"), open: Some(id) }.at(AcctId(id))}>{text}</a></Button>,
            Act::Credits { text, open: None } => <Button variant={look(false)} size={size}><a href={url::Accounts { filter: Some("credits"), open: None }}>{text}</a></Button>,
            Act::Go { text, view: View::Overview, anchor: Some(_) } => <Button variant={look(false)} size={size}><a href={url::Overview.at(TlAnchorId)}>{text}</a></Button>,
            Act::Go { text, view: View::Policy, anchor: Some(_) } => <Button variant={look(false)} size={size}><a href={url::Policy.at(ConfigId)}>{text}</a></Button>,
            Act::Go { text, view: View::Overview, anchor: None } => <Button variant={look(false)} size={size}><a href={url::Overview}>{text}</a></Button>,
            Act::Go { text, view: View::Policy, anchor: None } => <Button variant={look(false)} size={size}><a href={url::Policy}>{text}</a></Button>,
            Act::Live { text, primary, writable: false, .. } => <Button variant={look(*primary)} size={size}><button type="button" disabled title={READ_ONLY}>{text}</button></Button>,
            Act::Live { text, primary, does: Does::Refresh { open }, .. } => {
                <form class={Form} method="post" action={url::RefreshNow}>
                    if let Some(id) = open { <input type="hidden" name="open" value={id}> }
                    <Button variant={look(*primary)} size={size}><button type="submit">{text}</button></Button>
                </form>
            }
            Act::Live { text, primary, does: Does::Signin { provider, account }, .. } => {
                <Button variant={look(*primary)} size={size}><a href={url::Signin { provider: provider.as_deref(), account: account.as_deref() }}>{text}</a></Button>
            }
        }
    }
}

/// `attentionHtml`: what needs attention, worst first, and what is healthy. `names` are the
/// providers' names for the dots' tooltips, an item's at its place.
#[component(css)]
pub fn Attention<'a>(items: &'a [Item], names: Vec<Option<String>>, healthy: &'a [String]) -> impl Render {
    let rail = |t: Tone| match t {
        Tone::Bad => Some(Danger),
        Tone::Warn => Some(Warning),
        Tone::Info => Some(Info),
        _ => None,
    };
    html! {
        <Card>
            <CardHeader>
                <h2>"Needs attention"</h2>
                <p>
                    match items.len() {
                        0 => "nothing",
                        1 => "1 item",
                        n => {{n}" items"}
                    }
                </p>
            </CardHeader>
            if !items.is_empty() {
                <ul class={List}>
                    for (i, name) in items.iter().zip(names.iter()) {
                        <li class={(Item, rail(i.tone))}>
                            <span class={Rail}></span>
                            <div>
                                <h3>
                                    if let Some(p) = &i.provider { <ProviderDot provider={p} size={DotSize::Sm} title={name.as_deref()} /> }
                                    {&i.title}" "
                                    <Chip tone={i.tone} dot={false} size={ChipSize::Sm} title={None}>{sev(i.tone)}</Chip>
                                </h3>
                                <p>{&i.detail}</p>
                            </div>
                            <div class={Acts}>for a in &i.acts { <Action a={a} sm={true} /> }</div>
                            if let Some(why) = &i.why {
                                <details class={Why}><summary>"Why this matters"</summary><p>{why}</p></details>
                            }
                        </li>
                    }
                </ul>
            }
            <CardFooter>
                <span class={Healthy}><Eyebrow title={None}>"Healthy"</Eyebrow></span>
                for c in healthy { <Chip tone={Tone::Ok} dot={true} size={ChipSize::Md} title={None}>{c}</Chip> }
            </CardFooter>
        </Card>
    }
}

fn provider_names(items: &[Item], pname: impl Fn(&str) -> String) -> Vec<Option<String>> {
    items.iter().map(|i| i.provider.as_deref().map(&pname)).collect()
}

pub fn attention<'a>(items: &'a [Item], healthy: &'a [String], pname: impl Fn(&str) -> String) -> impl Render + 'a {
    let names = provider_names(items, pname);
    html! { <Attention items={items} names={names} healthy={healthy} /> }
}

/// The overview tab: what needs attention, the two provider cards and the next 14 days.
#[component(css)]
pub fn Overview<'a>(items: &'a [Item], names: Vec<Option<String>>, healthy: &'a [String], cards: &'a [ProviderCard], tl: &'a Timeline) -> impl Render {
    html! {
        <div class={Overview}>
            <div class={Wide}><Attention items={items} names={names.clone()} healthy={healthy} /></div>
            for card in cards { <div class={Half}><ProviderPanel c={card} /></div> }
            <div class={Wide}><TimelinePanel t={tl} /></div>
        </div>
    }
}

/// A forecast's rows: the scenario or pace, what it comes to, and a note.
#[component(css)]
pub fn LadderBox<'a>(l: &'a Ladder) -> impl Render {
    html! {
        <div class={LadderBox}>
            <div class={Heading}><Eyebrow title={l.title.as_deref()}>{&l.heading}</Eyebrow></div>
            for r in &l.rows {
                <div class={(Row, r.main.then_some(Main), r.assume.then_some(Assume))} title={r.title.as_deref()}>
                    <span>{&r.label}</span>
                    <span class={match r.value_tone { Some(Tone::Bad) => Some(Bad), Some(Tone::Ok) => Some(Good), _ => None }}>{&r.value}</span>
                    <span class={Note}>{&r.note}</span>
                </div>
            }
            if let Some(n) = &l.note { <p class={Foot}>{n}</p> }
        </div>
    }
}

/// `providerHtml(p)`: a provider's supply, its forecast and its accounts.
#[component(css)]
pub fn ProviderPanel<'a>(c: &'a ProviderCard) -> impl Render {
    let fill = match c.id.as_str() {
        "claude" => Some(Claude),
        "codex" => Some(Codex),
        _ => None,
    };
    html! {
        <Card>
            <div class={Body}>
                <div class={Head}>
                    <ProviderDot provider={&c.id} size={DotSize::Lg} title={None} />
                    <h2>{&c.name}</h2>
                    <span class={Quiet}>{&c.accounts}</span>
                    if let Some((cost, tip)) = &c.cost { <span class={Quiet} title={tip}>"· "{cost}</span> }
                    <span class={Push}><Chip tone={c.state.0} dot={true} size={ChipSize::Md} title={Some(&c.state_title)}>{c.state.1}</Chip></span>
                </div>
                <div class={Big}><b>{&c.remaining}</b><span class={Quiet}>"of "{&c.total}" account-weeks left this cycle"</span></div>
                <div class={Supply} title="Remaining weekly quota per account">
                    for s in &c.supply {
                        <span class={Slice} style={FLEX.set(s.weight)} title={&s.title}><i class={(Left, fill)} style={WIDTH.pct(s.left)}></i></span>
                    }
                </div>
                <Stats>
                    for s in &c.stats { <Stat label={&s.label} value={&s.value} sub={&s.sub} tone={s.tone} /> }
                </Stats>
                if let Some(l) = &c.ladder { <LadderBox l={l} /> }
                if let Some(l) = &c.pace { <LadderBox l={l} /> }
                <div>
                    for a in &c.lines {
                        <div class={Acct}>
                            <div class={Name}>
                                <span title={&a.email}>{&a.name}</span>
                                if a.next { <span class={Tag} title={NEXT_TIP}><Badge variant={BadgeVariant::Success}>"next"</Badge></span> }
                            </div>
                            match a.used {
                                None => <span class={Dim}>"n/a"</span>,
                                Some(u) => <Meter used={u} tone={super::model::level(u)} tick={None} />,
                            }
                            <div class={When} title={&a.reset_title}>{&a.resets}</div>
                            <div class={Credit}>
                                match &a.credits {
                                    Some((n, t)) => {<b>{n}</b>" "{t}}
                                    None => <span class={Dim}>{&a.no_credits}</span>,
                                }
                            </div>
                        </div>
                    }
                </div>
                if c.empty { <p class={Quiet}>"No accounts connected."</p> }
            </div>
        </Card>
    }
}

pub fn provider(c: &ProviderCard) -> impl Render + '_ {
    html! { <ProviderPanel c={c} /> }
}

/// `timelineHtml()`: each account's resets and credits over the next 14 days, and each
/// provider's projected shortfall.
#[component(css)]
pub fn TimelinePanel<'a>(t: &'a Timeline) -> impl Render {
    use super::model::Mark;
    let mark = |m: Mark| match m {
        Mark::Reset => (Reset, None),
        Mark::ResetNext => (Reset, Some(Proj)),
        Mark::Used => (Used, None),
        Mark::Lapse => (Lapse, None),
        Mark::Credit => (Credit, None),
    };
    html! {
        <Card>
            <CardHeader>
                <h2 id={TlAnchorId}>"Next 14 days"</h2>
                <CardAction>
                    <div class={Legend}>
                        <span><i class={(Key, Reset)}></i>"Weekly reset"</span>
                        <span><i class={(Key, Reset, Proj)}></i>"Following reset"</span>
                        <span><i class={(Key, Credit)}></i>"Credit expires"</span>
                        <span><i class={(Key, Used)}></i>"Credit used (projected)"</span>
                        <span><i class={(Key, Lapse)}></i>"Credit lapses"</span>
                        <span><i class={(Key, Swatch)}></i>"Projected shortfall"</span>
                    </div>
                </CardAction>
            </CardHeader>
            <CardContent>
                <div class={Tl}>
                    <div class={Gridlines}>
                        for (x, _, wk) in &t.ticks {
                            if *wk {
                                <span class={(Gl, Wknd)} style={(LEFT.pct(*x), WIDTH.pct(100.0 / 14.0))}></span>
                            } else {
                                <span class={Gl} style={LEFT.pct(*x)}></span>
                            }
                        }
                        <span class={Nowline}></span>
                    </div>
                    <div class={Axis}><div></div><div class={Ticks}>for (x, label, _) in &t.ticks { <span class={Tick} style={LEFT.pct(*x)}>{label}</span> }</div></div>
                    for r in &t.rows {
                        match &r.provider {
                            Some(p) => {
                                <div class={(TlRow, Group)}>
                                    <div class={Lbl}><ProviderDot provider={p} size={DotSize::Sm} title={None} />{&r.label}</div>
                                    <div class={Lane}>
                                        if let Some((l, w, text, title)) = &r.band {
                                            <span class={Band} style={(LEFT.pct(*l), WIDTH.pct(*w))} title={title}>{text}</span>
                                        }
                                    </div>
                                </div>
                            }
                            None => {
                                <div class={TlRow}>
                                    <div class={Lbl} title={&r.title}>{&r.label}</div>
                                    <div class={(Lane, Dashed)}>
                                        for (m, x, title) in &r.marks {
                                            <span class={(Mk, mark(*m).0, mark(*m).1)} style={LEFT.pct(*x)} title={title}></span>
                                        }
                                    </div>
                                </div>
                            }
                        }
                    }
                </div>
            </CardContent>
        </Card>
    }
}

pub fn timeline(t: &Timeline) -> impl Render + '_ {
    html! { <TimelinePanel t={t} /> }
}

/// A cell of an account's row.
#[component(css = AccountsTab)]
fn AcctCell<'a>(c: &'a Cell, filter: Filter, open: bool, id: &'a str) -> impl Render {
    let toggle = url::Accounts { filter: (filter != Filter::All).then_some(filter.key()), open: (!open).then_some(id) };
    html! {
        match c {
            Cell::Who { tone: t, status, name, email, next, notes, reauth } => {
                <div class={Who}>
                    <StatusDot tone={*t} title={status} />
                    <a class={Nm} href={toggle.at(AcctId(id))} title={email} aria-expanded={if open { "true" } else { "false" }}>{name}</a>
                    if *next { <span class={Tag} title="New sessions route here: soonest weekly reset among available accounts"><Badge variant={BadgeVariant::Success}>"next"</Badge></span> }
                    if *t != Tone::Ok { <Chip tone={*t} dot={true} size={ChipSize::Md} title={None}>{*status}</Chip> }
                </div>
                for (t, n) in notes {
                    <div class={(Note, match t { Tone::Bad => Some(Bad), Tone::Warn => Some(Warn), _ => None })}>{n}</div>
                }
                if let Some(r) = reauth { <div class={Note}><Action a={r} sm={true} /></div> }
            }
            Cell::Plan { name, weight, sub, tip } => {
                <span title={tip}>{name}if let Some(w) = weight { " "<span class={Dim}>{w}</span> }</span>
                if let Some(s) = sub { <div class={Sub}>{s}</div> }
            }
            Cell::Meter { used, tick, sub } => {
                <Meter used={*used} tone={super::model::level(*used)} tick={*tick} />
                if let Some(s) = sub { <div class={Sub}>{s}</div> }
            }
            Cell::Text { text, title, sub, tone: t, dim, mono } => {
                <span class={(dim.then_some(Dim), mono.then_some(Mono), (*t == Some(Tone::Warn)).then_some(Warn))} title={title.as_deref()}>{text}</span>
                if let Some(s) = sub { <div class={Sub}>{s}</div> }
            }
            Cell::Credits { pips, count, title, next } => {
                <div class={Pips} title={title}>for _ in 0..*pips { <i></i> }<b>{count}</b></div>
                if let Some((text, title, soon)) = next { <div class={(Sub, soon.then_some(Soon))} title={title}>{text}</div> }
            }
        }
    }
}

/// An open row's details: its windows, models, plan and actions.
#[component(css = AccountsTab)]
fn AcctDetail<'a>(d: &'a Detail) -> impl Render {
    html! {
        <div class={Dgrid}>
            <div>
                <h4>"Quota windows"</h4>
                <dl class={Kv}>for w in &d.windows { <dt>{&w.key}</dt><dd>{&w.value}</dd> }</dl>
                <h4 class={Later}>{&d.models_ready}</h4>
                <div class={Models}>for (m, ok) in &d.models { <span class={(!*ok).then_some(Down)}>{m}</span> }</div>
                <p class={Fine}>{&d.list_read}</p>
            </div>
            <div>
                <h4>"Plan, credits & value"</h4>
                <dl class={Kv}>
                    for kv in &d.plan { <dt>{&kv.key}</dt><dd>{&kv.value}if let Some(n) = &kv.note { " "<span class={Dim}>{n}</span> }</dd> }
                </dl>
            </div>
            <div>
                <h4>"Actions"</h4>
                <div class={Dacts}>
                    for a in &d.acts { <Action a={a} sm={true} /> }
                </div>
                if !d.notes.is_empty() { <p class={Notes}>for n in &d.notes { {n}<br> }</p> }
                <p class={Fine}>"Id "<code>{&d.id}</code></p>
            </div>
        </div>
    }
}

/// `accountsHtml(filter, openRow)`: a table per provider, a row per account; a row's name opens
/// its details.
#[component(css)]
pub fn AccountsTab<'a>(v: &'a AccountsView) -> impl Render {
    html! {
        <div class={Toolbar}>
            <Segmented label="Show accounts">
                for (f, key, label) in Filter::ALL {
                    <a href={url::Accounts { filter: (f != Filter::All).then_some(key), open: None }} aria-current={(v.filter == f).then_some("page")}>{label}</a>
                }
            </Segmented>
            <span class={Shown}>{&v.shown}</span>
        </div>
        <div class={AccountsTab}>
            for t in &v.tables {
                <Card>
                    <CardHeader>
                        <ProviderDot provider={&t.provider} size={DotSize::Lg} title={None} />
                        <h2>{&t.name}</h2>
                        <p>{&t.summary}</p>
                        <Chip tone={t.state.0} dot={true} size={ChipSize::Md} title={None}>{t.state.1}</Chip>
                        <CardAction>
                            <span class={(Order, HideSm)}>"Routing order: soonest weekly reset first"</span>
                            <Action a={&t.connect} sm={true} />
                        </CardAction>
                    </CardHeader>
                    <Table>
                        <table class={Grid}>
                            <colgroup>for c in &t.cols { <col class={c.sm.then_some(HideSm)} style={WIDTH.pct(f64::from(c.w))}> }</colgroup>
                            <thead><tr>for c in &t.cols { <th class={c.sm.then_some(HideSm)} title={c.tip.as_deref()}>{&c.head}</th> }</tr></thead>
                            <tbody>
                                for r in &t.rows {
                                    <tr class={Row} id={AcctId(&r.id)} aria-expanded={if r.open { "true" } else { "false" }}>
                                        for (i, (c, col)) in r.cells.iter().zip(&t.cols).enumerate() {
                                            <td class={col.sm.then_some(HideSm)} data-h={(i > 0).then_some(col.head.as_str())}><AcctCell c={c} filter={v.filter} open={r.open} id={&r.id} /></td>
                                        }
                                    </tr>
                                    if let Some(d) = &r.detail { <tr class={DetailRow}><td colspan={t.cols.len()}><AcctDetail d={d} /></td></tr> }
                                }
                                if t.rows.is_empty() { <tr><td colspan={t.cols.len()} class={Empty}>"No "{&t.name}" accounts match this filter."</td></tr> }
                            </tbody>
                        </table>
                    </Table>
                </Card>
            }
        </div>
    }
}

pub fn accounts(v: &AccountsView) -> impl Render + '_ {
    html! { <AccountsTab v={v} /> }
}

/// `usageHtml()`: what the pool's use would cost at API prices against what the plans cost.
#[component(css)]
pub fn UsageTab<'a>(u: &'a Result<UsageView, String>) -> impl Render {
    html! {
        match u {
            Err(ledger) => {
                <Card><CardContent>
                    <p class={Quiet}>"No usage value report yet. It is written about two minutes after the service starts, then hourly."</p>
                    <p class={Dim}>{ledger}</p>
                </CardContent></Card>
            }
            Ok(u) => {
                <div class={UsageTab}>
                    <div class={Kpis}>
                        for k in &u.kpis { <Kpi label={k.label} value={&k.value} sub={&k.sub} /> }
                    </div>
                    <Card>
                        <CardHeader><h2>"Quota rates"</h2><CardAction>"what one account-week buys"</CardAction></CardHeader>
                        <CardContent><ul class={Fits}>for (p, f) in &u.fits { <li><b>{p}":"</b>" "{f}</li> }</ul></CardContent>
                    </Card>
                    <Card>
                        <CardHeader><h2>"By model"</h2><CardAction>"API-equivalent vs actual · "{&u.since}</CardAction></CardHeader>
                        <Table>
                            <table class={Costs}>
                                <colgroup><col class={ModelCol}><col><col><col><col><col><col class={HideSm}></colgroup>
                                <thead><tr><th>"Model"</th><th class={Num}>"Requests"</th><th class={Num}>"API cost"</th><th class={Num}>"Actual cost"</th><th class={Num}>"API / request"</th><th class={Num}>"Actual / request"</th><th class={(Num, HideSm)}>"Quota share"</th></tr></thead>
                                <tbody>
                                    for m in &u.models {
                                        <tr>
                                            <td title={&m.model}><ProviderDot provider={&m.provider} size={DotSize::Sm} title={None} />" "{&m.model}</td>
                                            <td class={Num}>{&m.requests}</td>
                                            <td class={Num}>{&m.api}</td>
                                            <td class={Num} title={m.actual_title.as_deref()}>{&m.actual}</td>
                                            <td class={Num}>{&m.api_per}</td>
                                            <td class={Num}>{&m.actual_per}</td>
                                            <td class={(Num, HideSm, Dim)}>{&m.share}</td>
                                        </tr>
                                    }
                                    for t in &u.totals {
                                        <tr>
                                            <td><ProviderDot provider={&t.provider} size={DotSize::Sm} title={None} />" "<b>{&t.name}</b></td>
                                            <td class={Num}>{&t.requests}</td>
                                            <td class={Num}>{&t.api}</td>
                                            <td class={Num} title={&t.actual_title}>{&t.actual}if let Some(i) = &t.idle { <div class={Sub}>{i}</div> }</td>
                                            <td class={Num}>{&t.api_per}</td>
                                            <td class={Num}>{&t.actual_per}</td>
                                            <td class={(Num, HideSm)}><span class={Dim}>"—"</span></td>
                                        </tr>
                                    }
                                </tbody>
                            </table>
                        </Table>
                    </Card>
                    <Card>
                        <CardHeader><h2>"By account"</h2><CardAction>{&u.since}</CardAction></CardHeader>
                        <Table>
                            <table class={Costs}>
                                <colgroup><col class={AccountCol}><col><col><col><col><col><col class={HideSm}><col class={HideSm}><col></colgroup>
                                <thead><tr><th>"Account"</th><th class={Num}>"Requests"</th><th class={Num}>"API cost"</th><th class={Num}>"Actual cost"</th><th class={Num}>"API / request"</th><th class={Num}>"Actual / request"</th><th class={(Num, HideSm)}>"$ / acct-week"</th><th class={(Num, HideSm)}>"Plan / mo"</th><th class={Num}>"Value vs plan"</th></tr></thead>
                                <tbody>
                                    for a in &u.accounts {
                                        <tr>
                                            <td title={&a.email}><ProviderDot provider={&a.provider} size={DotSize::Sm} title={None} />" "{&a.name}</td>
                                            <td class={Num}>{&a.requests}</td>
                                            <td class={Num}>{&a.usd}</td>
                                            <td class={Num} title="plan cost over the window">{&a.actual}if a.idle { <div class={Sub}>"idle"</div> }</td>
                                            <td class={Num}>{&a.api_per}</td>
                                            <td class={Num}>{&a.actual_per}</td>
                                            <td class={(Num, HideSm, Dim)}>{&a.per_week}</td>
                                            <td class={(Num, HideSm, Dim)} title={&a.plan_title}>{&a.plan}</td>
                                            <td class={(Num, Dim)}>{&a.multiple}</td>
                                        </tr>
                                    }
                                </tbody>
                            </table>
                        </Table>
                    </Card>
                    <div class={Pair}>
                        <Card>
                            <CardHeader><h2>"Largest sessions"</h2><CardAction>"API-equivalent · actual"</CardAction></CardHeader>
                            <CardContent><ul class={Bars}>
                                for s in &u.sessions {
                                    <li>
                                        <div class={Lbl}><ProviderDot provider={&s.provider} size={DotSize::Sm} title={None} /><span title={&s.title}>{&s.label}</span></div>
                                        <div class={Hbar}><i class={match s.provider.as_str() { "claude" => Some(Claude), "codex" => Some(Codex), _ => None }} style={WIDTH.pct(s.width)}></i></div>
                                        <div class={When}>{&s.right}<span class={(Dim, WhenSub)}>{&s.sub}</span></div>
                                    </li>
                                }
                            </ul></CardContent>
                        </Card>
                        <Card>
                            <CardHeader><h2>"Quota left on the table"</h2></CardHeader>
                            <CardContent>
                                <p class={Intro}>"Weekly used % when each recent window ended. Short bars are quota that expired unused."</p>
                                <div>
                                    for e in &u.ends {
                                        <div class={EndsRow}>
                                            <div class={Lbl}><ProviderDot provider={&e.provider} size={DotSize::Sm} title={None} /><span>{&e.name}</span></div>
                                            <div class={Ends} title={&e.title}>for (h, lo) in &e.bars { <i class={lo.then_some(Lo)} style={HEIGHT.px((h * 0.3).max(3.0))}></i> }</div>
                                            <div class={When}>{&e.wasted}</div>
                                        </div>
                                    }
                                </div>
                                if u.ends.is_empty() { <p class={Dim}>"No windows have ended yet."</p> }
                                <p class={Fine}>{&u.projected}</p>
                            </CardContent>
                        </Card>
                    </div>
                    <p class={Fine}>{&u.footnote}</p>
                </div>
            }
        }
    }
}

pub fn usage(u: &Result<UsageView, String>) -> impl Render + '_ {
    html! { <UsageTab u={u} /> }
}

/// An endpoint and what it returns; `file`, when given, is the document's file on disk, set as code
/// in the middle of the text.
#[component(css = PolicyTab)]
fn Endpoint<'a>(path: &'a str, what: &'a str, file: Option<(&'a str, &'a str)>) -> impl Render {
    html! {
        <tr>
            <td class={Wrap}><code>{path}</code></td>
            <td class={Wrap}>{what}if let Some((file, rest)) = file { <code>{file}</code>{rest} }</td>
        </tr>
    }
}

/// `policyHtml()`: how sessions are routed and credits used, and where agents read the data.
#[component(css)]
pub fn PolicyTab<'a>(p: &'a PolicyView) -> impl Render {
    html! {
        <div class={PolicyTab}>
            <div class={Wide}>
                <Card>
                    <CardHeader><h2>"How sessions are routed"</h2></CardHeader>
                    <CardContent><ol class={Rules}>
                        <li><b>"New sessions go to the available account whose weekly window resets soonest"</b>", until it is drained. That account is marked "<Badge variant={BadgeVariant::Success}>"next"</Badge>"."</li>
                        <li><b>"Short windows and model caps spill over."</b>" The proxy enforces 5-hour limits and model caps such as Fable (a cap inside the all-models quota, not extra quota) and moves that request to the next account."</li>
                        <li><b>"Sessions are sticky."</b>" Active sessions stay on their account; sessions idle for over an hour rebind."</li>
                        <li><b>"The client picks the provider."</b>" The pool routes among accounts of the provider a request names and never moves requests across providers. The capacity forecast publishes a balance signal (now: "{&p.balance}") on "<code>"/api/capacity"</code>" for lane dispatchers; nothing in the pool enforces it."</li>
                    </ol></CardContent>
                </Card>
            </div>
            for c in &p.cards {
                <Card>
                    <CardHeader>
                        <ProviderDot provider={&c.provider} size={DotSize::Sm} title={None} />
                        <h2>{&c.title}</h2>
                        <CardAction>
                            if c.on {
                                <Chip tone={Tone::Ok} dot={true} size={ChipSize::Md} title={None}>"Automatic"</Chip>
                            } else {
                                <Chip tone={Tone::Neutral} dot={true} size={ChipSize::Md} title={None}>"Off"</Chip>
                            }
                        </CardAction>
                    </CardHeader>
                    <CardContent><dl class={Settings}>
                        for s in &c.settings {
                            <div class={Setting}>
                                <dt>{&s.key}</dt>
                                <dd class={s.bad.then_some(Bad)}>{&s.value}if let Some(chip) = &s.chip { " "<Chip tone={Tone::Neutral} dot={true} size={ChipSize::Md} title={None}>{chip}</Chip> }</dd>
                            </div>
                        }
                    </dl></CardContent>
                </Card>
            }
            <Card>
                <CardHeader><h2>"Which credit goes first"</h2></CardHeader>
                <CardContent><ol class={Rules}>
                    <li><b>"Credits expiring within 24 hours"</b>" are used first."</li>
                    <li>"Otherwise the account whose "<b>"natural weekly reset is furthest away"</b>"."</li>
                    <li>"A refused credit "<b>"waits"</b>" while other accounts stay eligible."</li>
                </ol></CardContent>
            </Card>
            <Card>
                <CardHeader><h2 id={ConfigId}>"Configuration"</h2></CardHeader>
                <CardContent><dl class={Settings}>
                    <div class={Setting}><dt>"Plan prices"</dt><dd>"Read from each provider twice a day. Codex: the subscription and ChatGPT's own price table (billed price). Claude: Anthropic reports the tier (Max 20x) but not the price to Claude Code sign-ins, so the claude.com list price for that tier is used. To override, set USD per month under "<code>"usageLedger.planPrices"</code>" in "<code>"state/install.json"</code>"; an override beats a list price, not a provider-billed one."</dd></div>
                    <div class={Setting}><dt>"Credit automation"</dt><dd><code>"resetPolicy"</code>" in "<code>"state/install.json"</code>"; master switch "<code>"resetAutomation"</code>" in "<code>"state/pool-state.json"</code>" ("{p.automation}")."</dd></div>
                    <div class={Setting}><dt>"Cadence"</dt><dd>{&p.cadence}</dd></div>
                </dl></CardContent>
            </Card>
            <div class={Wide}>
                <Card>
                    <CardHeader><h2>"Agent access"</h2><CardAction>"read-only, no tokens"</CardAction></CardHeader>
                    <CardContent><p class={Intro}>"Agents and scripts on this machine read the same data this page shows. From WSL use the Windows host address on port 8318."</p></CardContent>
                    <Table>
                        <table class={Endpoints}>
                            <thead><tr><th class={Path}>"Endpoint"</th><th>"What it returns"</th></tr></thead>
                            <tbody>
                                <Endpoint path="GET http://127.0.0.1:8318/api/summary" what="One compact document: per provider cost, credits, forecast with three scenarios (resets only, banked credits, +1 credit/week) and per-account plan, price, windows and credit expiries. Start here." file={None} />
                                <Endpoint path="GET /api/status" what="Full live pool state: accounts, windows, routing, reset journal, policies." file={None} />
                                <Endpoint path="GET /api/capacity" what="capacity-forecast/v1 (also " file={Some(("state/capacity-forecast.json", "): states, balance signal, projections, scenarios."))} />
                                <Endpoint path="GET /api/value" what="usage-value/v1 (also " file={Some(("state/usage-value.json", "): API-equivalent dollars, quota rates, value vs plan."))} />
                            </tbody>
                        </table>
                    </Table>
                </Card>
            </div>
        </div>
    }
}

pub fn policy(p: &PolicyView) -> impl Render + '_ {
    html! { <PolicyTab p={p} /> }
}

