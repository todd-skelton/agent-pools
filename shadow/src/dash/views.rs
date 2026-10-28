//! The dashboard's markup: each part of dashboard.html as an `html!` view of the model.
//!
//! The live page sizes meters, bars and timeline marks with `style="width:..%"`; so does this
//! one, which is why the app allows style attributes (see the README).

use corten::prelude::*;

use super::model::{
    AccountsView, Act, Cell, Detail, Does, Filter, Item, Ladder, PolicyView, ProviderCard, Timeline, Tone, UsageView, View,
};
use super::signin::{Banner, signin_name};
use crate::app::url;

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

pub fn tone(t: Tone) -> &'static str {
    match t {
        Tone::Ok => "ok",
        Tone::Warn => "warn",
        Tone::Bad => "bad",
        Tone::Info => "info",
        Tone::Neutral => "neutral",
    }
}

fn opt_tone(t: Option<Tone>) -> &'static str {
    t.map_or("", tone)
}

fn sev(t: Tone) -> &'static str {
    match t {
        Tone::Bad => "Act now",
        Tone::Warn => "Soon",
        _ => "FYI",
    }
}

fn pct(x: f64) -> f64 {
    x.clamp(0.0, 100.0)
}

const READ_ONLY: &str = "Done on the live dashboard (port 8318): the shadow only reads";

/// The document around every dashboard page: the top bar with its tabs and freshness, and the
/// sign-in banner. On a copy (`run-copy`, `writable`) Refresh posts to `/refresh` and "Add
/// account" opens the sign-in steps; the read-only shadow shows both disabled. The tabs are
/// the shell's nav (`req.nav`): Corten marks the one for this page's path current, here and
/// after each swap, so none is current on the sign-in page.
/// A page title: the live service's as given, a shadow's or a copy's marked "(shadow)".
pub fn titled(title: &str, service: bool) -> String {
    if service { title.to_string() } else { format!("{title} (shadow)") }
}

#[component]
pub fn Dash<'a>(req: &'a Req, title: &'a str, header: &'a Header, writable: bool, service: bool, children: impl Render) -> impl Document {
    let accounts = url::Accounts { filter: None, open: None };
    html! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8">
                <meta name="viewport" content="width=device-width, initial-scale=1">
                <link rel="stylesheet" href={req.asset("dash.css")}>
                <title>{title}</title>
                <Corten />
            </head>
            <body>
                <header class="bar">
                    <div class="bar-in">
                        <div class="brand">
                            <span class="logo"></span>"Agent pools"
                            if !service {
                                <span class="shadow-tag" title="Corten shadow: built from the live install's files, read only">"shadow"</span>
                            }
                        </div>
                        <nav class="tabs">
                            <a {..req.nav(&url::Overview)}>
                                "Overview "
                                {count(header.count)}
                            </a>
                            <a {..req.nav(&accounts)}>"Accounts"</a>
                            <a {..req.nav(&url::Usage)}>"Usage & value"</a>
                            <a {..req.nav(&url::Policy)}>"Automation"</a>
                        </nav>
                        <div class="bar-right">
                            {fresh(header)}
                            if !service {
                                <a class="btn ghost" href={url::Parity}>"Parity"</a>
                            }
                            if writable {
                                <form class="inline" method="post" action={url::RefreshNow}><button type="submit" class="btn" title="Run a refresh cycle now">"Refresh"</button></form>
                                <a class="btn primary" href={url::Signin { provider: None, account: None }}>"Add account"</a>
                            } else {
                                <button type="button" class="btn" disabled title={READ_ONLY}>"Refresh"</button>
                                <button type="button" class="btn primary" disabled title={READ_ONLY}>"Add account"</button>
                            }
                        </div>
                    </div>
                </header>
                // In <main>, so a navigation (which swaps <main>) brings the banner as it is.
                <main>{finish(header)}{children}</main>
            </body>
        </html>
    }
}

/// The tab's body, by id, so a live fragment replaces it.
pub fn body(html: &str) -> impl Render + '_ {
    html! { <div id={DashBodyId}>{raw(html)}</div> }
}

/// The header's count: hidden at none, as the live page leaves it empty.
pub fn count(n: usize) -> impl Render {
    html! { <span id={AttCountId} class="tab-count" hidden={n == 0}>{n}</span> }
}

/// `renderFinish()`: a sign-in to finish (from this PC by itself, or by pasting the link on the
/// sign-in page), or the one just confirmed (the live page's toast); hidden otherwise.
pub fn finish(h: &Header) -> impl Render + '_ {
    html! {
        match &h.finish {
            None => <div id={FinishId} class="finish" hidden></div>,
            Some(Banner::Pending(p)) => {
                <div id={FinishId} class="finish">
                    <div class="gap">
                        <div>
                            <p><b>"Finish "{signin_name(p)}" sign-in."</b>" "<span class="mut">"On this PC it finishes by itself. Signed in on another device? Paste the localhost link it opened."</span></p>
                        </div>
                        <div class="acts">
                            <a class="btn sm primary" href={url::Signin { provider: None, account: None }}>"Finish sign-in"</a>
                            <form class="inline" method="post" action={url::SigninCancel}><button type="submit" class="btn sm">"Cancel"</button></form>
                        </div>
                    </div>
                </div>
            }
            Some(Banner::Signed(m)) => <div id={FinishId} class="finish"><div class="gap ok"><p role="status">{m}</p></div></div>,
        }
    }
}

pub fn fresh(h: &Header) -> impl Render + '_ {
    html! { <div id={FreshId} class={if h.bad { "fresh bad" } else { "fresh" }}><span class="pulse"></span><span class="t">{&h.fresh}</span></div> }
}

/// An action: a link where it goes somewhere on these pages; where it changes the pool, a
/// refresh form or a link to the sign-in steps on a copy, and a disabled button on the read-only
/// shadow.
pub fn act(a: &Act, sm: bool) -> impl Render + '_ {
    let base = if sm { "btn sm" } else { "btn" };
    html! {
        match a {
            Act::Account { text, id } => <a class={base} href={url::Accounts { filter: None, open: Some(id) }.at(AcctId(id))}>{text}</a>,
            Act::Credits { text, open: Some(id) } => <a class={base} href={url::Accounts { filter: Some("credits"), open: Some(id) }.at(AcctId(id))}>{text}</a>,
            Act::Credits { text, open: None } => <a class={base} href={url::Accounts { filter: Some("credits"), open: None }}>{text}</a>,
            Act::Go { text, view: View::Overview, anchor: Some(_) } => <a class={base} href={url::Overview.at(TlAnchorId)}>{text}</a>,
            Act::Go { text, view: View::Policy, anchor: Some(_) } => <a class={base} href={url::Policy.at(ConfigId)}>{text}</a>,
            Act::Go { text, view: View::Overview, anchor: None } => <a class={base} href={url::Overview}>{text}</a>,
            Act::Go { text, view: View::Policy, anchor: None } => <a class={base} href={url::Policy}>{text}</a>,
            Act::Live { text, primary, writable: false, .. } => <button type="button" class={(base, if *primary { " primary" } else { "" })} disabled title={READ_ONLY}>{text}</button>,
            Act::Live { text, primary, does: Does::Refresh { open }, .. } => {
                <form class="inline" method="post" action={url::RefreshNow}>
                    if let Some(id) = open { <input type="hidden" name="open" value={id}> }
                    <button type="submit" class={(base, if *primary { " primary" } else { "" })}>{text}</button>
                </form>
            }
            Act::Live { text, primary, does: Does::Signin { provider, account }, .. } => {
                <a class={(base, if *primary { " primary" } else { "" })} href={url::Signin { provider: provider.as_deref(), account: account.as_deref() }}>{text}</a>
            }
        }
    }
}

/// `attentionHtml`
pub fn attention<'a>(items: &'a [Item], healthy: &'a [String], pname: impl Fn(&str) -> String + 'a) -> impl Render + 'a {
    html! {
        <div class="card-h">
            <h2>"Needs attention"</h2>
            <span class="mut fs13">
                match items.len() {
                    0 => "nothing",
                    1 => "1 item",
                    n => {{n}" items"}
                }
            </span>
        </div>
        if !items.is_empty() {
            <ul class="att-list">
                for i in items {
                    <li class={("att ", tone(i.tone))}>
                        <span class="rail"></span>
                        <div>
                            <h3>
                                if let Some(p) = &i.provider { <span class={("pdot ", p.as_str())} title={pname(p)}></span> }
                                {&i.title}" "
                                <span class={("chip plain small ", tone(i.tone))}>{sev(i.tone)}</span>
                            </h3>
                            <p>{&i.detail}</p>
                        </div>
                        <div class="acts">for a in &i.acts { {act(a, true)} }</div>
                        if let Some(why) = &i.why {
                            <details><summary>"Why this matters"</summary><p>{why}</p></details>
                        }
                    </li>
                }
            </ul>
        }
        <div class="clear">
            <span class="eyebrow healthy">"Healthy"</span>
            for c in healthy { <span class="chip ok">{c}</span> }
        </div>
    }
}

fn ladder(l: &Ladder) -> impl Render + '_ {
    html! {
        <div class="ladder">
            <div class="eyebrow lh" title={l.title.as_deref()}>{&l.heading}</div>
            for r in &l.rows {
                <div class={("lrow", if r.main { " main" } else { "" }, if r.assume { " assume" } else { "" })} title={r.title.as_deref()}>
                    <span>{&r.label}</span>
                    <span class={match r.value_tone { Some(Tone::Bad) => "bad-t", Some(Tone::Ok) => "ok-t", _ => "" }}>{&r.value}</span>
                    <span class="dim">{&r.note}</span>
                </div>
            }
            if let Some(n) = &l.note { <p class="dim lnote">{n}</p> }
        </div>
    }
}

fn meter(used: Option<f64>) -> impl Render {
    html! {
        match used {
            None => <span class="dim">"n/a"</span>,
            Some(u) => {
                <div class="meter">
                    <div class="track"><div class={("fill ", opt_tone(super::model::level(u)))} style={format!("width:{}%", pct(u))}></div></div>
                    <span class="pct">{pool_core::js::round(u)}"%"</span>
                </div>
            }
        }
    }
}

/// `providerHtml(p)`
pub fn provider(c: &ProviderCard) -> impl Render + '_ {
    html! {
        <div class="card-b">
            <div class="prov-h">
                <span class={("pdot big-dot ", c.id.as_str())}></span>
                <h2>{&c.name}</h2>
                <span class="mut">{&c.accounts}</span>
                if let Some((cost, tip)) = &c.cost { <span class="mut" title={tip}>"· "{cost}</span> }
                <span class={("chip push ", tone(c.state.0))} title={&c.state_title}>{c.state.1}</span>
            </div>
            <div class="big"><b class="mono">{&c.remaining}</b><span class="mut">"of "{&c.total}" account-weeks left this cycle"</span></div>
            <div class="supply" title="Remaining weekly quota per account">
                for s in &c.supply {
                    <span style={format!("flex:{}", s.weight)} title={&s.title}><i style={format!("width:{}%;background:var(--{})", s.left, c.id)}></i></span>
                }
            </div>
            <div class="stats">
                for s in &c.stats {
                    <div class={("stat ", opt_tone(s.tone))}><div class="eyebrow">{&s.label}</div><div class="v">{&s.value}</div><div class="s">{&s.sub}</div></div>
                }
            </div>
            if let Some(l) = &c.ladder { {ladder(l)} }
            if let Some(l) = &c.pace { {ladder(l)} }
            for a in &c.lines {
                <div class="acct">
                    <div class="name">
                        <span title={&a.email}>{&a.name}</span>
                        if a.next { <b class="next" title="New sessions route here: the available account whose weekly window resets soonest">"next"</b> }
                    </div>
                    {meter(a.used)}
                    <div class="when" title={&a.reset_title}>{&a.resets}</div>
                    <div class="credit">
                        match &a.credits {
                            Some((n, t)) => {<b>{n}</b>" "{t}}
                            None => <span class="dim">{&a.no_credits}</span>,
                        }
                    </div>
                </div>
            }
            if c.empty { <p class="mut">"No accounts connected."</p> }
        </div>
    }
}

/// `timelineHtml()`
pub fn timeline(t: &Timeline) -> impl Render + '_ {
    use super::model::Mark;
    let mark = |m: Mark| match m {
        Mark::Reset => "mk reset",
        Mark::ResetNext => "mk reset proj",
        Mark::Used => "mk used",
        Mark::Lapse => "mk lapse",
        Mark::Credit => "mk credit",
    };
    html! {
        <div class="card-h" id={TlAnchorId}>
            <h2>"Next 14 days"</h2>
            <div class="right legend hide-sm">
                <span><i class="mk reset"></i>"Weekly reset"</span>
                <span><i class="mk reset proj"></i>"Following reset"</span>
                <span><i class="mk credit"></i>"Credit expires"</span>
                <span><i class="mk used"></i>"Credit used (projected)"</span>
                <span><i class="mk lapse"></i>"Credit lapses"</span>
                <span><i class="band bad swatch"></i>"Projected shortfall"</span>
            </div>
        </div>
        <div class="card-b">
            <div class="tl">
                <div class="gridlines">
                    for (x, _, wk) in &t.ticks {
                        <span class={if *wk { "gl wknd" } else { "gl" }} style={if *wk { format!("left:{x}%;width:{}%", 100.0 / 14.0) } else { format!("left:{x}%") }}></span>
                    }
                    <span class="nowline"></span>
                </div>
                <div class="tl-axis"><div></div><div class="ticks">for (x, label, _) in &t.ticks { <span class="tick" style={format!("left:{x}%")}>{label}</span> }</div></div>
                for r in &t.rows {
                    match &r.provider {
                        Some(p) => {
                            <div class="tl-row group">
                                <div class="lbl"><span class={("pdot ", p.as_str())}></span>{&r.label}</div>
                                <div class="track2">
                                    if let Some((l, w, text, title)) = &r.band {
                                        <span class="band bad" style={format!("left:{l}%;width:{w}%")} title={title}>{text}</span>
                                    }
                                </div>
                            </div>
                        }
                        None => {
                            <div class="tl-row">
                                <div class="lbl" title={&r.title}>{&r.label}</div>
                                <div class="track2">
                                    for (m, x, title) in &r.marks { <span class={mark(*m)} style={format!("left:{x}%")} title={title}></span> }
                                </div>
                            </div>
                        }
                    }
                }
            </div>
        </div>
    }
}

fn cell<'a>(c: &'a Cell, filter: Filter, open: bool, id: &'a str) -> impl Render + 'a {
    let toggle = url::Accounts { filter: (filter != Filter::All).then_some(filter.key()), open: (!open).then_some(id) };
    html! {
        match c {
            Cell::Who { tone: t, status, name, email, next, notes, reauth } => {
                <div class="who">
                    <span class={("sdot ", tone(*t))} title={*status}></span>
                    <a class="nm" href={toggle.at(AcctId(id))} title={email} aria-expanded={if open { "true" } else { "false" }}>{name}</a>
                    if *next { <b class="next" title="New sessions route here: soonest weekly reset among available accounts">"next"</b> }
                    if *t != Tone::Ok { <span class={("chip ", tone(*t))}>{*status}</span> }
                </div>
                for (t, n) in notes { <div class={("note ", tone(*t))}>{n}</div> }
                if let Some(r) = reauth { <div class="note">{act(r, true)}</div> }
            }
            Cell::Plan { name, weight, sub, tip } => {
                <span title={tip}>{name}if let Some(w) = weight { " "<span class="dim">{w}</span> }</span>
                if let Some(s) = sub { <div class="sub">{s}</div> }
            }
            Cell::Meter { used, tick, sub } => {
                <div class="meter">
                    <div class="track">
                        <div class={("fill ", opt_tone(super::model::level(*used)))} style={format!("width:{}%", pct(*used))}></div>
                        if let Some(t) = tick { <span class="mtick" style={format!("left:{t}%")}></span> }
                    </div>
                    <span class="pct">{pool_core::js::round(*used)}"%"</span>
                </div>
                if let Some(s) = sub { <div class="sub">{s}</div> }
            }
            Cell::Text { text, title, sub, tone: t, dim, mono } => {
                <span class={(if *dim { "dim " } else { "" }, if *mono { "mono " } else { "" }, match t { Some(Tone::Warn) => "warn-t", _ => "" })} title={title.as_deref()}>{text}</span>
                if let Some(s) = sub { <div class="sub">{s}</div> }
            }
            Cell::Credits { pips, count, title, next } => {
                <div class="pips" title={title}>for _ in 0..*pips { <i></i> }<b>{count}</b></div>
                if let Some((text, title, soon)) = next { <div class={if *soon { "sub soon" } else { "sub" }} title={title}>{text}</div> }
            }
        }
    }
}

fn detail(d: &Detail) -> impl Render + '_ {
    html! {
        <div class="dgrid">
            <div>
                <h4>"Quota windows"</h4>
                <dl class="kv">for w in &d.windows { <dt>{&w.key}</dt><dd>{&w.value}</dd> }</dl>
                <h4 class="mt16">{&d.models_ready}</h4>
                <div class="models">for (m, ok) in &d.models { <span class={if *ok { "" } else { "down" }}>{m}</span> }</div>
                <p class="dim fs12 mt8">{&d.list_read}</p>
            </div>
            <div>
                <h4>"Plan, credits & value"</h4>
                <dl class="kv">
                    for kv in &d.plan { <dt>{&kv.key}</dt><dd>{&kv.value}if let Some(n) = &kv.note { " "<span class="dim">{n}</span> }</dd> }
                </dl>
            </div>
            <div>
                <h4>"Actions"</h4>
                <div class="dacts">
                    for a in &d.acts { {act(a, true)} }
                </div>
                if !d.notes.is_empty() { <p class="fs12 mt10">for n in &d.notes { {n}<br> }</p> }
                <p class="dim fs12 mt10">"Id "<code>{&d.id}</code></p>
            </div>
        </div>
    }
}

/// `accountsHtml(filter, openRow)`
pub fn accounts(v: &AccountsView) -> impl Render + '_ {
    html! {
        <div class="toolbar">
            <div class="seg">
                for (f, key, label) in Filter::ALL {
                    <a href={url::Accounts { filter: (f != Filter::All).then_some(key), open: None }} aria-current={(v.filter == f).then_some("page")}>{label}</a>
                }
            </div>
            <span class="mut fs13">{&v.shown}</span>
        </div>
        <div class="grid" id="acct-tables">
            for t in &v.tables {
                <div class="card span-12">
                    <div class="card-h">
                        <span class={("pdot big-dot ", t.provider.as_str())}></span>
                        <h2>{&t.name}</h2>
                        <span class="mut fs13">{&t.summary}</span>
                        <span class={("chip ", tone(t.state.0))}>{t.state.1}</span>
                        <div class="right">
                            <span class="dim hide-sm fs12">"Routing order: soonest weekly reset first"</span>
                            {act(&t.connect, true)}
                        </div>
                    </div>
                    <div class="tw">
                        <table class="t">
                            <colgroup>for c in &t.cols { <col class={c.sm.then_some("hide-sm")} style={format!("width:{}%", c.w)}> }</colgroup>
                            <thead><tr>for c in &t.cols { <th class={c.sm.then_some("hide-sm")} title={c.tip.as_deref()}>{&c.head}</th> }</tr></thead>
                            <tbody>
                                for r in &t.rows {
                                    <tr class="row" id={AcctId(&r.id)} aria-expanded={if r.open { "true" } else { "false" }}>
                                        for (i, (c, col)) in r.cells.iter().zip(&t.cols).enumerate() {
                                            <td class={col.sm.then_some("hide-sm")} data-h={(i > 0).then_some(col.head.as_str())}>{cell(c, v.filter, r.open, &r.id)}</td>
                                        }
                                    </tr>
                                    if let Some(d) = &r.detail { <tr class="detail"><td colspan={t.cols.len()}>{detail(d)}</td></tr> }
                                }
                                if t.rows.is_empty() { <tr><td colspan={t.cols.len()} class="mut">"No "{&t.name}" accounts match this filter."</td></tr> }
                            </tbody>
                        </table>
                    </div>
                </div>
            }
        </div>
    }
}

/// `usageHtml()`
pub fn usage<'a>(u: &'a Result<UsageView, String>) -> impl Render + 'a {
    html! {
        match u {
            Err(ledger) => {
                <div class="card"><div class="card-b">
                    <p class="mut">"No usage value report yet. It is written about two minutes after the service starts, then hourly."</p>
                    <p class="dim">{ledger}</p>
                </div></div>
            }
            Ok(u) => {
                <div class="grid">
                    <div class="span-12 kpis">
                        for k in &u.kpis { <div class="card kpi"><div class="eyebrow">{k.label}</div><div class="v mono">{&k.value}</div><div class="s">{&k.sub}</div></div> }
                    </div>
                    <div class="card span-12">
                        <div class="card-h"><h2>"Quota rates"</h2><span class="right mut fs12">"what one account-week buys"</span></div>
                        <div class="card-b"><ul class="plain">for (p, f) in &u.fits { <li><b>{p}":"</b>" "{f}</li> }</ul></div>
                    </div>
                    <div class="card span-7">
                        <div class="card-h"><h2>"By model"</h2><span class="right mut fs12">"API-equivalent · per request"</span></div>
                        <div class="card-b"><ul class="barlist">
                            for m in &u.models {
                                <li>
                                    <div class="lbl"><span class={("pdot ", m.provider.as_str())}></span><span title={&m.title}>{&m.label}</span></div>
                                    <div class="hbar">
                                        <i style={format!("width:{}%;background:var(--{})", m.width, m.provider)}></i>
                                        <em class={(m.width < 30.0).then_some("out")} style={(m.width < 30.0).then(|| format!("left:calc({}% + 6px)", m.width))}>{&m.inside}</em>
                                    </div>
                                    <div class="when">{&m.right}</div>
                                </li>
                            }
                        </ul></div>
                    </div>
                    <div class="card span-5">
                        <div class="card-h"><h2>"Largest sessions"</h2></div>
                        <div class="card-b"><ul class="barlist sessions">
                            for s in &u.sessions {
                                <li>
                                    <div class="lbl"><span class={("pdot ", s.provider.as_str())}></span><span title={&s.title}>{&s.label}</span></div>
                                    <div class="hbar"><i style={format!("width:{}%;background:var(--{})", s.width, s.provider)}></i></div>
                                    <div class="when">{&s.right}</div>
                                </li>
                            }
                        </ul></div>
                    </div>
                    <div class="card span-8 tw">
                        <div class="card-h"><h2>"By account"</h2><span class="right mut fs12">{&u.since}</span></div>
                        <table class="t">
                            <colgroup><col class="w30"><col><col><col class="hide-sm"><col class="hide-sm"><col></colgroup>
                            <thead><tr><th>"Account"</th><th class="num">"Requests"</th><th class="num">"API-equiv."</th><th class="num hide-sm">"$ / acct-week"</th><th class="num hide-sm">"Plan / mo"</th><th class="num">"Value vs plan"</th></tr></thead>
                            <tbody>
                                for a in &u.accounts {
                                    <tr>
                                        <td title={&a.email}><span class={("pdot ", a.provider.as_str())}></span>" "{&a.name}</td>
                                        <td class="num">{&a.requests}</td>
                                        <td class="num">{&a.usd}</td>
                                        <td class="num hide-sm dim">{&a.per_week}</td>
                                        <td class="num hide-sm dim" title={&a.plan_title}>{&a.plan}</td>
                                        <td class="num dim">{&a.multiple}</td>
                                    </tr>
                                }
                            </tbody>
                        </table>
                    </div>
                    <div class="card span-4">
                        <div class="card-h"><h2>"Quota left on the table"</h2></div>
                        <div class="card-b">
                            <p class="mut fs13 ends-intro">"Weekly used % when each recent window ended. Short bars are quota that expired unused."</p>
                            for e in &u.ends {
                                <div class="acct ends-row">
                                    <div class="name"><span class={("pdot ", e.provider.as_str())}></span><span>{&e.name}</span></div>
                                    <div class="ends" title={&e.title}>for (h, lo) in &e.bars { <i class={lo.then_some("lo")} style={format!("height:{}px", (h * 0.3).max(3.0))}></i> }</div>
                                    <div class="when">{&e.wasted}</div>
                                </div>
                            }
                            if u.ends.is_empty() { <p class="dim">"No windows have ended yet."</p> }
                            <p class="dim fs12 mt12">{&u.projected}</p>
                        </div>
                    </div>
                    <p class="span-12 dim fs12 m0">{&u.footnote}</p>
                </div>
            }
        }
    }
}

/// An endpoint and what it returns; `file`, when given, is the document's file on disk, set as code
/// in the middle of the text.
fn endpoint<'a>(path: &'a str, what: &'a str, file: Option<(&'a str, &'a str)>) -> impl Render + 'a {
    html! {
        <tr>
            <td class="wrap"><code>{path}</code></td>
            <td class="wrap">{what}if let Some((file, rest)) = file { <code>{file}</code>{rest} }</td>
        </tr>
    }
}

/// `policyHtml()`
pub fn policy(p: &PolicyView) -> impl Render + '_ {
    html! {
        <div class="grid">
            <div class="card span-12">
                <div class="card-h"><h2>"How sessions are routed"</h2></div>
                <div class="card-b"><ol class="rules">
                    <li><b>"New sessions go to the available account whose weekly window resets soonest"</b>", until it is drained. That account is marked "<span class="next">"next"</span>"."</li>
                    <li><b>"Short windows and model caps spill over."</b>" The proxy enforces 5-hour limits and model caps such as Fable (a cap inside the all-models quota, not extra quota) and moves that request to the next account."</li>
                    <li><b>"Sessions are sticky."</b>" Active sessions stay on their account; sessions idle for over an hour rebind."</li>
                    <li><b>"The client picks the provider."</b>" The pool routes among accounts of the provider a request names and never moves requests across providers. The capacity forecast publishes a balance signal (now: "{&p.balance}") on "<code>"/api/capacity"</code>" for lane dispatchers; nothing in the pool enforces it."</li>
                </ol></div>
            </div>
            for c in &p.cards {
                <div class="card span-6">
                    <div class="card-h">
                        <span class={("pdot ", c.provider.as_str())}></span>
                        <h2>{&c.title}</h2>
                        <div class="right"><span class={if c.on { "chip ok" } else { "chip neutral" }}>{if c.on { "Automatic" } else { "Off" }}</span></div>
                    </div>
                    <div class="card-b"><dl>
                        for s in &c.settings {
                            <div class="setting"><dt>{&s.key}</dt><dd class={s.bad.then_some("bad-t")}>{&s.value}if let Some(chip) = &s.chip { " "<span class="chip neutral chip-after">{chip}</span> }</dd></div>
                        }
                    </dl></div>
                </div>
            }
            <div class="card span-6">
                <div class="card-h"><h2>"Which credit goes first"</h2></div>
                <div class="card-b"><ol class="rules">
                    <li><b>"Credits expiring within 24 hours"</b>" are used first."</li>
                    <li>"Otherwise the account whose "<b>"natural weekly reset is furthest away"</b>"."</li>
                    <li>"A refused credit "<b>"waits"</b>" while other accounts stay eligible."</li>
                </ol></div>
            </div>
            <div class="card span-6" id={ConfigId}>
                <div class="card-h"><h2>"Configuration"</h2></div>
                <div class="card-b"><dl>
                    <div class="setting first"><dt>"Plan prices"</dt><dd>"Read from each provider twice a day. Codex: the subscription and ChatGPT's own price table (billed price). Claude: Anthropic reports the tier (Max 20x) but not the price to Claude Code sign-ins, so the claude.com list price for that tier is used. To override, set USD per month under "<code>"usageLedger.planPrices"</code>" in "<code>"state/install.json"</code>"; an override beats a list price, not a provider-billed one."</dd></div>
                    <div class="setting"><dt>"Credit automation"</dt><dd><code>"resetPolicy"</code>" in "<code>"state/install.json"</code>"; master switch "<code>"resetAutomation"</code>" in "<code>"state/pool-state.json"</code>" ("{p.automation}")."</dd></div>
                    <div class="setting"><dt>"Cadence"</dt><dd>{&p.cadence}</dd></div>
                </dl></div>
            </div>
            <div class="card span-12">
                <div class="card-h"><h2>"Agent access"</h2><span class="right mut fs12">"read-only, no tokens"</span></div>
                <div class="card-b">
                    <p class="mut fs13 agents-intro">"Agents and scripts on this machine read the same data this page shows. From WSL use the Windows host address on port 8318."</p>
                    <div class="tw"><table class="t">
                        <thead><tr><th class="w34">"Endpoint"</th><th>"What it returns"</th></tr></thead>
                        <tbody>
                            {endpoint("GET http://127.0.0.1:8318/api/summary", "One compact document: per provider cost, credits, forecast with three scenarios (resets only, banked credits, +1 credit/week) and per-account plan, price, windows and credit expiries. Start here.", None)}
                            {endpoint("GET /api/status", "Full live pool state: accounts, windows, routing, reset journal, policies.", None)}
                            {endpoint("GET /api/capacity", "capacity-forecast/v1 (also ", Some(("state/capacity-forecast.json", "): states, balance signal, projections, scenarios.")))}
                            {endpoint("GET /api/value", "usage-value/v1 (also ", Some(("state/usage-value.json", "): API-equivalent dollars, quota rates, value vs plan.")))}
                        </tbody>
                    </table></div>
                </div>
            </div>
        </div>
    }
}
