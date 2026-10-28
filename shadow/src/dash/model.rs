//! The live dashboard's rendering logic (dashboard.html, the pure functions of `S` (/api/status),
//! `V` (/api/value) and `NOW`), as data for the views: what each card, row and item says.
//! Function names follow the JavaScript's so the two can be read side by side.

use pool_core::js;
use serde_json::Value;

use super::fmt::{self, D, H, Zone, ago, day, dt, in_t, n, num, num_v, ok_time, plural, text, ts, usd, usd_v};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Warn,
    Bad,
    Info,
    Neutral,
}

/// A page of the dashboard an action goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Overview,
    Policy,
}

/// An action button. What changes the pool works on a copy (`run-copy`); the read-only shadow
/// only names it.
#[derive(Clone, Debug)]
pub enum Act {
    /// The account's row, open, on the accounts page.
    Account { text: String, id: String },
    /// The accounts page filtered to banked credits, the soonest-expiring holder open.
    Credits { text: String, open: Option<String> },
    /// Another page, at an anchor.
    Go { text: String, view: View, anchor: Option<&'static str> },
    /// Sign-in, refresh: a form or the sign-in page where the app may write, else a disabled
    /// button (done on the live dashboard).
    Live { text: String, primary: bool, does: Does, writable: bool },
}

/// What a `Live` action does.
#[derive(Clone, Debug, PartialEq)]
pub enum Does {
    /// A refresh cycle (`refreshNow`), then back to the account's row when there is one.
    Refresh { open: Option<String> },
    /// The sign-in steps (`openSignin(p, id)`).
    Signin { provider: Option<String>, account: Option<String> },
}

fn get<'a>(v: Option<&'a Value>, k: &str) -> Option<&'a Value> {
    js::get(v, k)
}

fn truthy(v: Option<&Value>) -> bool {
    js::truthy(v)
}

fn s_eq(v: Option<&Value>, s: &str) -> bool {
    v.and_then(Value::as_str) == Some(s)
}

fn arr(v: Option<&Value>) -> &[Value] {
    match v {
        Some(Value::Array(a)) => a,
        _ => &[],
    }
}

/// `x ?? d` for a number.
fn or(v: Option<&Value>, d: f64) -> f64 {
    if js::nullish(v) { d } else { n(v) }
}

/// JavaScript `String(x)` in concatenation (`undefined` included).
fn cat(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(v) => js::to_string(Some(v)),
    }
}

/// What every view reads: the status, the value report, the time and the zone times are shown in.
pub struct Ctx<'a> {
    pub s: &'a Value,
    pub v: Option<&'a Value>,
    pub now: f64,
    pub z: Zone,
    /// Whether the app may change the pool (a copy): its actions work.
    pub writable: bool,
}

pub struct Price {
    pub usd: f64,
    pub source: String,
    pub detail: String,
}

pub struct PlanCost {
    pub usd: f64,
    pub count: usize,
    pub missing: usize,
    pub sources: Vec<String>,
}

pub fn source_text(source: &str) -> String {
    match source {
        "provider" => "billed price reported by the provider".into(),
        "list" => "list price for the plan tier the provider reports".into(),
        "config" => "price set in install.json".into(),
        other => other.into(),
    }
}

/// `STATE[f.state] ?? STATE.unknown`
pub fn state_of(state: Option<&Value>) -> (Tone, &'static str) {
    match state.and_then(Value::as_str) {
        Some("conserve") => (Tone::Bad, "Conserve"),
        Some("spend") => (Tone::Warn, "Spend"),
        Some("normal") => (Tone::Ok, "Normal"),
        _ => (Tone::Neutral, "Unknown"),
    }
}

/// `level(u)`
pub fn level(u: f64) -> Option<Tone> {
    if u >= 100.0 {
        Some(Tone::Bad)
    } else if u >= 85.0 {
        Some(Tone::Warn)
    } else {
        None
    }
}

impl<'a> Ctx<'a> {
    fn sv(&self) -> Option<&'a Value> {
        Some(self.s)
    }

    pub fn day(&self, v: Option<&Value>) -> String {
        day(self.z, v)
    }
    pub fn dt(&self, v: Option<&Value>) -> String {
        dt(self.z, v)
    }
    pub fn clock(&self, v: Option<&Value>) -> String {
        fmt::clock(self.z, v)
    }
    pub fn in_t(&self, v: Option<&Value>) -> String {
        in_t(ts(v), self.now)
    }

    /// `act(text, "refresh")`
    pub fn refresh(&self, text: impl Into<String>, open: Option<String>) -> Act {
        Act::Live { text: text.into(), primary: false, does: Does::Refresh { open }, writable: self.writable }
    }

    /// `act(text, "signin", { p, id })`
    pub fn signin(&self, text: impl Into<String>, primary: bool, p: Option<&str>, id: Option<&str>) -> Act {
        let does = Does::Signin { provider: p.map(str::to_string), account: id.map(str::to_string) };
        Act::Live { text: text.into(), primary, does, writable: self.writable }
    }

    pub fn pname(&self, p: &str) -> String {
        get(get(get(self.sv(), "providers"), p), "name").filter(|v| !v.is_null()).map_or_else(|| p.to_string(), |v| text(Some(v)))
    }

    pub fn term(&self, p: &str) -> &'static str {
        let pol = get(get(self.sv(), "resetPolicy"), p);
        if s_eq(get(pol, "redemption"), "refill") || (!truthy(pol) && p == "claude") { "refill" } else { "reset" }
    }

    pub fn all_accounts(&self) -> &'a [Value] {
        arr(get(self.sv(), "accounts"))
    }

    pub fn accounts(&self, p: &str) -> Vec<&'a Value> {
        self.all_accounts().iter().filter(|a| s_eq(a.get("provider"), p)).collect()
    }

    pub fn find_account(&self, id: Option<&Value>) -> Option<&'a Value> {
        self.all_accounts().iter().find(|a| a.get("id").is_some() && a.get("id") == id)
    }

    pub fn value_row(&self, a: &Value) -> Option<&'a Value> {
        arr(get(self.v, "accounts")).iter().find(|x| x.get("id") == a.get("id"))
    }

    pub fn weight(&self, a: &Value) -> f64 {
        or(get(self.value_row(a), "weight"), 1.0)
    }

    pub fn cap(&self) -> Option<&'a Value> {
        get(get(self.sv(), "capacity"), "providers")
    }

    pub fn f(&self, p: &str) -> Option<&'a Value> {
        get(self.cap(), p)
    }

    pub fn sc(&self, p: &str) -> Option<&'a Value> {
        get(self.f(p), "scenarios").filter(|v| !v.is_null())
    }

    /// `providerIds()`: Claude, Codex, then any other provider named or seen.
    pub fn provider_ids(&self) -> Vec<String> {
        let known: Vec<String> = js::entries(get(self.sv(), "providers")).into_iter().map(|(k, _)| k).collect();
        let mut seen: Vec<String> = Vec::new();
        for a in self.all_accounts() {
            let p = text(a.get("provider"));
            if !seen.contains(&p) {
                seen.push(p);
            }
        }
        let mut out: Vec<String> = Vec::new();
        for p in ["claude".to_string(), "codex".to_string()].into_iter().chain(known.iter().cloned()).chain(seen.iter().cloned()) {
            if !out.contains(&p) && (known.contains(&p) || seen.contains(&p)) {
                out.push(p);
            }
        }
        out
    }

    /// `modelLimit(p, w)`: the limit's name and what it means.
    pub fn model_limit(&self, p: &str, w: &Value) -> (String, &'static str) {
        let m = arr(get(self.v, "modelLimits")).iter().find(|m| {
            s_eq(m.get("provider"), p) && m.get("window") == w.get("id") && m.get("scope") == w.get("scope")
        });
        let kind = get(m, "kind").and_then(Value::as_str).filter(|k| matches!(*k, "cap" | "reserve")).unwrap_or("model limit");
        let share = get(m, "shareOfCommon").and_then(Value::as_f64).filter(|s| kind != "model limit" && s.is_finite() && *s > 0.0);
        let share = share.map_or_else(String::new, |s| format!(" ({}% of all-models)", num(s * 100.0, 1)));
        let detail = match kind {
            "cap" => "The cap sits inside the all-models quota.",
            "reserve" => "The reserve adds capacity outside the all-models quota.",
            _ => "Whether this limit is a cap or a reserve is unknown.",
        };
        let scope = get(Some(w), "scope").filter(|v| !v.is_null()).map_or_else(|| "Model".to_string(), |v| text(Some(v)));
        (format!("{scope} {kind}{share}"), detail)
    }

    /// `priceOf(a)`
    pub fn price_of(&self, a: &Value) -> Option<Price> {
        let p = get(get(Some(a), "plan"), "price");
        if truthy(p)
            && let Some(u) = get(p, "usdPerMonth").and_then(Value::as_f64).filter(|u| u.is_finite())
        {
            return Some(Price { usd: u, source: text(get(p, "source")), detail: text(get(p, "detail")) });
        }
        let v = self.value_row(a)?;
        let u = v.get("planUsdPerMonth").and_then(Value::as_f64).filter(|u| u.is_finite())?;
        let source = get(Some(v), "planPriceSource").filter(|v| !v.is_null()).map_or_else(|| "config".to_string(), |s| text(Some(s)));
        Some(Price { usd: u, source, detail: "usageLedger.planPrices".into() })
    }

    /// `planCost(p)`
    pub fn plan_cost(&self, p: &str) -> PlanCost {
        let list = self.accounts(p);
        let priced: Vec<Price> = list.iter().filter_map(|a| self.price_of(a)).collect();
        let mut sources: Vec<String> = Vec::new();
        for x in &priced {
            if !sources.contains(&x.source) {
                sources.push(x.source.clone());
            }
        }
        PlanCost { usd: priced.iter().map(|x| x.usd).sum(), count: priced.len(), missing: list.len() - priced.len(), sources }
    }

    /// `nextUp(p)`: the router's own order (priority, highest first), then the soonest weekly reset.
    pub fn next_up(&self, p: &str) -> Option<&'a Value> {
        let mut list: Vec<&Value> = self
            .accounts(p)
            .into_iter()
            .filter(|a| !truthy(a.get("disabled")) && s_eq(a.get("status"), "available") && s_eq(a.get("routingStatus"), "ready"))
            .collect();
        js::sort_by(&mut list, |x, y| {
            let pr = or(y.get("priority"), f64::NEG_INFINITY) - or(x.get("priority"), f64::NEG_INFINITY);
            if pr != 0.0 && !pr.is_nan() {
                return pr;
            }
            or(get(weekly(x), "resetAt"), f64::INFINITY) - or(get(weekly(y), "resetAt"), f64::INFINITY)
        });
        list.first().copied()
    }

    /// `lapseReason(p, l, shortAt)`
    pub fn lapse_reason(&self, p: &str, l: &Value, short_at: Option<&Value>) -> String {
        let a = self.find_account(l.get("id"));
        let w = a.and_then(weekly);
        let t = self.term(p);
        let who = a.map_or_else(|| format!("One {t}"), |a| format!("{}'s {t}", label(a)));
        let at = ts(l.get("at"));
        let before = if ok_time(short_at) && at <= ts(short_at) { ", before the shortfall starts" } else { "" };
        if let Some(w) = w {
            let reset = ts(w.get("resetAt"));
            if reset < at {
                let duration = n(w.get("durationSeconds"));
                let tail = if duration > 0.0 && reset + duration * 1000.0 < at {
                    format!("its own weekly resets (next {}) keep refilling it", self.day(w.get("resetAt")))
                } else {
                    format!("its own weekly reset on {} refills it first", self.day(w.get("resetAt")))
                };
                return format!("{who} expires {}{before}, and at this pace that account will not be empty while it is valid: {tail}", self.day(l.get("at")));
            }
        }
        format!("{who} expires {}{before}, before that account runs out at this pace", self.day(l.get("at")))
    }

    fn who_used(&self, b: Option<&Value>) -> Vec<String> {
        arr(get(b, "redemptions"))
            .iter()
            .map(|r| {
                let who = self.find_account(r.get("id")).map_or_else(|| "one account".to_string(), label);
                format!("{who} on {}", self.day(r.get("at")))
            })
            .collect()
    }

    fn short_text(&self, r: Option<&Value>) -> String {
        if !truthy(r) {
            "?".into()
        } else if truthy(get(r, "firstOutageAt")) {
            format!("~{} h short from {}", cat(get(r, "outageHours")), self.day(get(r, "firstOutageAt")))
        } else {
            "no shortfall".into()
        }
    }

    /// The forecast's credit plan, when it is the counterfactual planner's and was planned.
    fn plan(&self, f: Option<&'a Value>) -> Option<&'a Value> {
        let cp = get(f, "creditPlan");
        (s_eq(get(cp, "method"), "counterfactual") && truthy(get(cp, "planned")) && get(cp, "credits").is_some_and(Value::is_array)).then_some(cp?)
    }
}

/// `label(a)`: the email (or id) without its top-level domain.
pub fn label(a: &Value) -> String {
    let s = js::coalesce([a.get("email"), a.get("id")]).filter(|v| truthy(Some(v))).map_or_else(String::new, |v| text(Some(v)));
    for tld in [".com", ".net", ".org", ".io"] {
        if let Some(stem) = s.strip_suffix(tld) {
            return stem.to_string();
        }
    }
    s
}

pub fn windows(a: &Value) -> &[Value] {
    arr(a.get("quotaWindows"))
}

/// `weekly(a)`: the all-models weekly window.
pub fn weekly(a: &Value) -> Option<&Value> {
    let ws = windows(a);
    ws.iter()
        .find(|w| truthy(w.get("common")) && s_eq(w.get("label"), "Weekly"))
        .or_else(|| ws.iter().find(|w| truthy(w.get("common")) && n(w.get("durationSeconds")) >= 6.0 * 86400.0))
}

pub fn model_wins(a: &Value) -> Vec<&Value> {
    windows(a).iter().filter(|w| !truthy(w.get("common"))).collect()
}

pub fn reauth_needed(a: &Value) -> bool {
    s_eq(get(a.get("refreshError"), "code"), "reauth_required")
}

/// `planName(a)`
pub fn plan_name(a: &Value) -> String {
    let label = get(a.get("plan"), "label");
    if truthy(label) {
        let l = text(label);
        return l.strip_prefix("ChatGPT ").or_else(|| l.strip_prefix("Claude ")).unwrap_or(&l).to_string();
    }
    let id = text(a.get("id"));
    for (suffix, name) in [("-prolite.json", "Pro Lite"), ("-pro.json", "Pro"), ("-plus.json", "Plus"), ("-max.json", "Max")] {
        if id.ends_with(suffix) {
            return name.into();
        }
    }
    String::new()
}

// ---------------------------------------------------------------- Attention

pub struct Item {
    pub tone: Tone,
    pub provider: Option<String>,
    pub title: String,
    pub detail: String,
    pub why: Option<String>,
    pub acts: Vec<Act>,
}

impl Item {
    fn new(tone: Tone, provider: Option<&str>, title: String, detail: String, acts: Vec<Act>) -> Item {
        Item { tone, provider: provider.map(str::to_string), title, detail, why: None, acts }
    }
    fn why(mut self, why: String) -> Item {
        self.why = Some(why);
        self
    }
}

impl<'a> Ctx<'a> {
    /// `planItems(p, plan, f)`
    fn plan_items(&self, p: &str, plan: &Value, f: Option<&Value>) -> Vec<Item> {
        let mut out = Vec::new();
        let t = self.term(p);
        let pace = |x: f64| format!("{} acct-weeks/week", num(x * 168.0, 1));
        for c in arr(plan.get("credits")) {
            let c = Some(c);
            let who = self.find_account(get(c, "accountId")).map_or_else(|| "One account".to_string(), label);
            let need = if !js::nullish(get(c, "needUnitsPerHour")) {
                format!(
                    "It needs about {} until then; the current pace is {}. ",
                    pace(n(get(c, "needUnitsPerHour"))),
                    pace(or(get(get(f, "demand"), "unitsPerHour"), 0.0))
                )
            } else {
                "No steady pace saves it. ".into()
            };
            let action = text(get(c, "action"));
            let how = match action.as_str() {
                "redeem-now" => format!("Automation redeems it now: {who} is out, and using it now beats waiting for every account to run out. "),
                "drain-first" => format!(
                    "New sessions go to {who} first so it is empty (a refill only applies at the limit) before {}. ",
                    self.dt(get(c, "drainBy"))
                ),
                _ => "Automation waits: redeeming early would not add usage. ".into(),
            };
            let acts = vec![Act::Credits { text: "Review credits".into(), open: self.credit_holder(p) }];
            if s_eq(get(c, "outcome"), "lapses") {
                out.push(
                    Item::new(
                        Tone::Warn,
                        Some(p),
                        format!("{who}'s {t} projected to expire unused"),
                        format!("Expires {}. {how}{need}", self.dt(get(c, "expiresAt"))),
                        acts,
                    )
                    .why(format!(
                        "A refill clears its own account's usage and keeps that account's reset date, so it only applies once that account is at its limit and must be used before that date. More {} work is the only thing that saves it.",
                        self.pname(p)
                    )),
                );
            } else if action != "wait" {
                let title = format!(
                    "{who}'s {t}: {}",
                    if action == "redeem-now" { "redeeming before it lapses" } else { "drained first so it gets used" }
                );
                let used = if s_eq(get(c, "outcome"), "used") { "At the current pace it gets used. " } else { "" };
                let tail = if action == "drain-first" { need.as_str() } else { "" };
                out.push(Item::new(Tone::Info, Some(p), title, format!("Expires {}. {how}{used}{tail}", self.dt(get(c, "expiresAt"))), acts));
            }
        }
        out
    }

    /// The account the "Review credits" action opens: the soonest-expiring holder.
    pub fn credit_holder(&self, p: &str) -> Option<String> {
        let mut list: Vec<&Value> = self
            .accounts(p)
            .into_iter()
            .filter(|a| n(a.get("creditCount")) > 0.0 && truthy(a.get("nextCreditExpiry")))
            .collect();
        js::sort_by(&mut list, |x, y| ts(x.get("nextCreditExpiry")) - ts(y.get("nextCreditExpiry")));
        list.first().map(|a| text(a.get("id")))
    }

    /// `attentionItems()`
    pub fn attention_items(&self) -> Vec<Item> {
        let mut items: Vec<Item> = Vec::new();
        for a in self.all_accounts() {
            if truthy(a.get("disabled")) {
                continue;
            }
            let p = text(a.get("provider"));
            let e = a.get("refreshError");
            if reauth_needed(a) {
                let since = if truthy(get(e, "since")) { format!(" (since {})", self.dt(get(e, "since"))) } else { String::new() };
                items.push(Item::new(
                    Tone::Bad,
                    Some(&p),
                    format!("Sign-in expired: {}", label(a)),
                    format!("The provider rejected the saved login{since}. The account stops serving requests when its current token lapses."),
                    vec![self.signin("Sign in again", true, Some(&p), a.get("id").and_then(Value::as_str))],
                ));
            } else if truthy(e) {
                let attempts = if n(get(e, "attempts")) > 1.0 { format!(" ({} attempts)", cat(get(e, "attempts"))) } else { String::new() };
                items.push(Item::new(
                    Tone::Warn,
                    Some(&p),
                    format!("Token refresh failing: {}", label(a)),
                    format!("The proxy keeps retrying{attempts}."),
                    vec![Act::Account { text: "View account".into(), id: text(a.get("id")) }],
                ));
            }
            let ml = a.get("modelList");
            if truthy(ml) && !s_eq(get(ml, "source"), "live") {
                let err = if truthy(get(ml, "error")) { format!(" (last read failed: {})", cat(get(ml, "error"))) } else { String::new() };
                items.push(Item::new(
                    Tone::Warn,
                    Some(&p),
                    format!("Not routed yet: {}", label(a)),
                    format!("Its live model list has not been read{err}; nothing is routed to it until it is."),
                    vec![self.refresh("Retry now", None)],
                ));
            }
            if truthy(a.get("usageStale")) || s_eq(a.get("status"), "unknown") {
                let err = if truthy(a.get("error")) { format!("{}. ", cat(a.get("error"))) } else { String::new() };
                let last = if truthy(a.get("usageUpdatedAt")) {
                    format!("Showing last known usage from {}.", self.dt(a.get("usageUpdatedAt")))
                } else {
                    "No usage read yet.".into()
                };
                items.push(Item::new(Tone::Warn, Some(&p), format!("Usage not verified: {}", label(a)), format!("{err}{last}"), vec![self.refresh("Refresh", None)]));
            }
        }
        for (p, since) in js::entries(get(self.sv(), "exhausted")) {
            let t = self.term(&p);
            items.push(Item::new(
                Tone::Bad,
                Some(&p),
                format!("Every {} account is at its limit", self.pname(&p)),
                format!(
                    "Since {}, and no banked {t} is about to be used. New {} turns fail until a window resets.",
                    self.dt(Some(since)),
                    self.pname(&p)
                ),
                vec![self.signin("Connect another account", true, Some(&p), None)],
            ));
        }
        for (p, f) in js::entries(self.cap()) {
            let f = Some(f);
            let t = self.term(&p);
            let pr = get(f, "projection");
            let s = self.sc(&p);
            let g = get(s, "weeklyGrant").filter(|v| truthy(Some(v)));
            let plan = self.plan(f);
            let saved_by = |l: &Value| arr(get(plan, "credits")).iter().any(|c| c.get("accountId") == l.get("id") && s_eq(c.get("outcome"), "used"));
            let b0 = get(s, "bankedCredits").filter(|v| truthy(Some(v)));
            let lapses: Vec<&Value> = match (b0, plan) {
                (Some(b0), Some(_)) => arr(b0.get("lapses")).iter().filter(|l| !saved_by(l)).collect(),
                (Some(b0), None) => arr(b0.get("lapses")).iter().collect(),
                _ => Vec::new(),
            };
            let conserve_short = s_eq(get(f, "state"), "conserve") && truthy(get(pr, "firstOutageAt"));
            if conserve_short {
                let bal = get(get(self.sv(), "capacity"), "balance");
                let mut detail = format!("Demand is {}x weekly supply. ", num_v(get(pr, "demandToNaturalSupply"), 2));
                let n0 = get(s, "naturalOnly");
                if truthy(get(n0, "firstOutageAt")) {
                    detail += &format!(
                        "With window resets alone the gap would start {} (~{} h). ",
                        self.day(get(n0, "firstOutageAt")),
                        cat(get(n0, "outageHours"))
                    );
                }
                if let Some(b) = b0 {
                    let used = n(b.get("creditsUsed"));
                    let lapsing = n(b.get("creditsLapsing"));
                    match plan {
                        None => {
                            if used + lapsing != 0.0 && !(used + lapsing).is_nan() {
                                let who = if used != 0.0 { format!(" ({})", self.who_used(Some(b)).join(", ")) } else { String::new() };
                                detail += &format!(
                                    "{} of {} banked {t}s get{} used{who}. ",
                                    js::number_to_string(used),
                                    js::number_to_string(used + lapsing),
                                    if used == 1.0 { "s" } else { "" }
                                );
                            } else {
                                detail += &format!("No banked {t}s. ");
                            }
                        }
                        Some(plan) => {
                            let credits = arr(plan.get("credits"));
                            if !credits.is_empty() {
                                let got = credits.iter().filter(|c| s_eq(c.get("outcome"), "used")).count();
                                detail += &format!("{got} of {} banked {t}s get used under the credit plan. ", credits.len());
                            }
                        }
                    }
                    if !lapses.is_empty() {
                        let reasons: Vec<String> = lapses.iter().map(|l| self.lapse_reason(&p, l, get(pr, "firstOutageAt"))).collect();
                        detail += &format!(
                            "{}{} cover the gap. ",
                            reasons.join(". "),
                            if lapses.len() > 1 { ", so they do not" } else { ", so it does not" }
                        );
                    }
                }
                detail += &self.pace_sentence(&p);
                if g.is_some() {
                    detail += &format!("If one new {t} arrives each week: {}. ", self.short_text(g));
                }
                let toward = get(bal, "toward");
                if truthy(toward) && !s_eq(toward, &p) {
                    detail += &format!(
                        "Published signal: toward {}, step {} of {}.",
                        self.pname(&text(toward)),
                        cat(get(bal, "steps")),
                        cat(get(bal, "maxSteps"))
                    );
                }
                let title = format!(
                    "{} short ~{} h from {}{}",
                    self.pname(&p),
                    cat(get(pr, "outageHours")),
                    self.day(get(pr, "firstOutageAt")),
                    if truthy(get(pr, "creditsModeled")) { format!(", after banked {t}s") } else { String::new() }
                );
                let why = format!(
                    "The pool only routes among accounts of the provider a request names; it cannot move work between providers. The balance on /api/capacity is a signal the model-routing skill and dispatch-lane.ps1 read when they start new lanes. The pace plan is the same kind of signal: how fast to use {0} to get the most out of it. Running short loses nothing; quota is lost only when a window resets unused or a {t} lapses. Inside the pool, the only lever is more {0} supply.",
                    self.pname(&p)
                );
                items.push(
                    Item::new(
                        Tone::Bad,
                        Some(&p),
                        title,
                        detail,
                        vec![
                            Act::Go { text: "Timeline".into(), view: View::Overview, anchor: Some("tl-anchor") },
                            self.signin(format!("Connect {} account", self.pname(&p)), false, Some(&p), None),
                        ],
                    )
                    .why(why),
                );
            }
            let lapsing = get(get(f, "creditOutlook"), "creditsLapsing");
            let lapsing_n = n(lapsing);
            let explained = conserve_short
                && !lapses.is_empty()
                && lapses.iter().all(|l| ts(l.get("at")) <= ts(get(pr, "firstOutageAt")))
                && lapsing_n <= lapses.len() as f64;
            let review = || vec![Act::Credits { text: "Review credits".into(), open: self.credit_holder(&p) }];
            let redeem_before = if truthy(get(get(get(self.sv(), "resetPolicy"), &p), "redeemBeforeLapse")) { "on" } else { "off" };
            if let Some(plan) = plan {
                items.extend(self.plan_items(&p, plan, f));
            } else if truthy(lapsing) && !lapses.is_empty() && !explained {
                let reasons: Vec<String> = lapses.iter().map(|l| self.lapse_reason(&p, l, get(pr, "firstOutageAt"))).collect();
                let why = if t == "refill" {
                    format!("A refill clears its own account's usage and keeps that account's reset date, so it only helps once that account is empty. Redeem-before-lapse is {redeem_before}: automation uses it early whenever the account is out before it expires.")
                } else {
                    "A reset restarts its own account's weekly window, and automation uses it once every account is near its limit, or earlier when it would otherwise lapse.".into()
                };
                items.push(
                    Item::new(
                        Tone::Warn,
                        Some(&p),
                        format!("{} projected to expire unused", plural(lapses.len() as f64, &format!("{} {t}", self.pname(&p)))),
                        format!("{}. {}", reasons.join(". "), self.pace_sentence(&p)),
                        review(),
                    )
                    .why(why),
                );
            } else if truthy(lapsing) && !explained {
                let mut holders: Vec<&Value> = self
                    .accounts(&p)
                    .into_iter()
                    .filter(|a| n(a.get("creditCount")) > 0.0 && truthy(a.get("nextCreditExpiry")))
                    .collect();
                js::sort_by(&mut holders, |x, y| ts(x.get("nextCreditExpiry")) - ts(y.get("nextCreditExpiry")));
                let expiries = arr(get(get(f, "supply"), "creditExpiries"));
                let banked_on = if holders.is_empty() {
                    String::new()
                } else {
                    format!(" Banked on: {}.", holders.iter().map(|a| format!("{} ({})", label(a), cat(a.get("creditCount")))).collect::<Vec<_>>().join(", "))
                };
                let why = if t == "refill" {
                    format!("A refill clears usage but keeps the reset date, so automation only uses one where there is time to burn it before that date. Redeem-before-lapse is {redeem_before}.")
                } else {
                    "A reset restarts the weekly window from now. Automation fires once every account is near its limit, or earlier when a credit would lapse.".into()
                };
                items.push(
                    Item::new(
                        Tone::Info,
                        Some(&p),
                        format!("{} likely to expire unused", plural(lapsing_n, &format!("{} {t}", self.pname(&p)))),
                        format!("Projected by {} at current demand. {}{banked_on}", self.day(expiries.last()), self.pace_sentence(&p)),
                        review(),
                    )
                    .why(why),
                );
            }
        }
        for a in self.all_accounts() {
            if truthy(a.get("disabled")) {
                continue;
            }
            let p = text(a.get("provider"));
            if let Some(w) = weekly(a) {
                let used = n(w.get("used"));
                if (80.0..100.0).contains(&used) {
                    items.push(Item::new(
                        Tone::Info,
                        Some(&p),
                        format!("{} at {}% of its weekly limit", label(a), js::round(used)),
                        format!(
                            "Expected: it resets soonest, so it drains first. Resets {} (in {}); new sessions spill to the next account at the limit.",
                            self.dt(w.get("resetAt")),
                            self.in_t(w.get("resetAt"))
                        ),
                        vec![Act::Account { text: "View".into(), id: text(a.get("id")) }],
                    ));
                }
            }
            for m in model_wins(a) {
                let used = n(m.get("used"));
                if used >= 85.0 {
                    let (name, detail) = self.model_limit(&p, m);
                    let scope = m.get("scope").filter(|v| !v.is_null()).map_or_else(|| "Those".to_string(), |v| text(Some(v)));
                    items.push(Item::new(
                        if used >= 100.0 { Tone::Warn } else { Tone::Info },
                        Some(&p),
                        format!("{} {} its {name} ({}%)", label(a), if used >= 100.0 { "hit" } else { "near" }, js::round(used)),
                        format!("{scope} requests on this account spill to the next account until {}. {detail}", self.dt(m.get("resetAt"))),
                        vec![Act::Account { text: "View".into(), id: text(a.get("id")) }],
                    ));
                }
            }
        }
        for (p, r) in js::entries(get(self.sv(), "resets")) {
            let r = Some(r);
            let t = self.term(&p);
            if s_eq(get(get(r, "pending"), "phase"), "attention") {
                items.push(Item::new(
                    Tone::Bad,
                    Some(&p),
                    format!("A {} {t} needs attention", self.pname(&p)),
                    "Reconnect its account so the outcome can be reconciled.".into(),
                    vec![self.signin("Reconnect", true, Some(&p), None)],
                ));
            }
            let e = get(r, "error");
            if truthy(e) {
                items.push(Item::new(
                    Tone::Warn,
                    Some(&p),
                    format!("{} {t} automation hit an error", self.pname(&p)),
                    format!("{} ({}). It retries on the next refresh.", cat(get(e, "message")), self.dt(get(e, "at"))),
                    vec![],
                ));
            }
        }
        for (p, c) in js::entries(get(self.sv(), "resetPolicy")) {
            let problems = arr(c.get("problems"));
            if !problems.is_empty() {
                let list: Vec<String> = problems.iter().map(|x| text(Some(x))).collect();
                items.push(Item::new(
                    Tone::Warn,
                    Some(&p),
                    format!("{} {} policy has a configuration problem", self.pname(&p), self.term(&p)),
                    format!("Defaults used: {}.", list.join("; ")),
                    vec![Act::Go { text: "Automation".into(), view: View::Policy, anchor: None }],
                ));
            }
        }
        let le = get(get(self.sv(), "usageLedger"), "lastError");
        if truthy(le) {
            items.push(Item::new(Tone::Warn, None, "Usage ledger is not writing".into(), text(le), vec![]));
        }
        let unpriced: Vec<&Value> = self.all_accounts().iter().filter(|a| self.price_of(a).is_none()).collect();
        if !unpriced.is_empty() {
            let names: Vec<String> = unpriced.iter().map(|a| label(a)).collect();
            let failed = if unpriced.iter().any(|a| truthy(a.get("planError"))) { " (last read failed)" } else { "" };
            items.push(Item::new(
                Tone::Info,
                None,
                format!("Plan price unknown for {}", plural(unpriced.len() as f64, "account")),
                format!("Read from each provider twice a day; {} not read yet{failed}.", names.join(", ")),
                vec![Act::Go { text: "Configuration".into(), view: View::Policy, anchor: Some("config") }],
            ));
        }
        let order = |t: Tone| match t {
            Tone::Bad => 0,
            Tone::Warn => 1,
            _ => 2,
        };
        items.sort_by_key(|i| order(i.tone));
        items
    }

    /// `healthy()`
    pub fn healthy(&self) -> Vec<String> {
        let live: Vec<&Value> = self.all_accounts().iter().filter(|a| !truthy(a.get("disabled"))).collect();
        let mut out = Vec::new();
        if !live.is_empty() && live.iter().all(|a| !truthy(a.get("refreshError"))) {
            out.push(format!("{} accounts signed in", live.len()));
        }
        if !live.is_empty() && live.iter().all(|a| s_eq(a.get("routingStatus"), "ready")) {
            out.push("all routing ready".into());
        }
        if !live.is_empty() && live.iter().all(|a| s_eq(get(a.get("modelList"), "source"), "live")) {
            out.push("live model lists on all".into());
        }
        if truthy(get(self.sv(), "resetAutomation")) {
            out.push("credit automation on".into());
        }
        let l = get(self.sv(), "usageLedger");
        if truthy(get(l, "enabled")) && !truthy(get(l, "lastError")) {
            out.push(format!("ledger recording ({} rows)", fmt::locale(or(get(l, "rows"), 0.0), 0, 3)));
        }
        out
    }
}

// ---------------------------------------------------------------- Pace and scenarios

/// One row of a ladder: label, value, note, and whether it is the main row or an assumption.
pub struct LRow {
    pub label: String,
    pub value: String,
    pub value_tone: Option<Tone>,
    pub note: String,
    pub main: bool,
    pub assume: bool,
    pub title: Option<String>,
}

pub struct Ladder {
    pub heading: String,
    pub title: Option<String>,
    pub rows: Vec<LRow>,
    pub note: Option<String>,
}

fn wk(u: f64) -> String {
    format!("{} acct-weeks/week", num(u * 168.0, 1))
}

fn aw(x: f64) -> String {
    let s = num(x, 1);
    if s == "1" { format!("{s} acct-week") } else { format!("{s} acct-weeks") }
}

impl<'a> Ctx<'a> {
    /// `ladderHtml(p)`: the next days at current demand, per scenario.
    pub fn ladder(&self, p: &str) -> Option<Ladder> {
        let s = self.sc(p);
        let b = get(s, "bankedCredits").filter(|v| truthy(Some(v)))?;
        let t = self.term(p);
        let g = get(s, "weeklyGrant");
        let tone = |r: Option<&Value>| if truthy(get(r, "firstOutageAt")) { Tone::Bad } else { Tone::Ok };
        let used = n(b.get("creditsUsed"));
        let lapsing = n(b.get("creditsLapsing"));
        let b_note = format!(
            "{} used{}",
            js::number_to_string(used),
            if lapsing != 0.0 && !lapsing.is_nan() { format!(", {} lapse", js::number_to_string(lapsing)) } else { String::new() }
        );
        let off = if get(s, "creditsInStateRun") == Some(&Value::Bool(false)) { " (automation off)" } else { "" };
        let assumption = get(g, "assumption").filter(|a| truthy(Some(a)));
        let rows = vec![
            LRow { label: "Window resets only".into(), value: self.short_text(get(s, "naturalOnly")), value_tone: Some(tone(get(s, "naturalOnly"))), note: String::new(), main: false, assume: false, title: None },
            LRow { label: format!("+ banked {t}s{off}"), value: self.short_text(Some(b)), value_tone: Some(tone(Some(b))), note: b_note, main: true, assume: false, title: None },
            LRow {
                label: format!("+ 1 new {t} per week"),
                value: self.short_text(g),
                value_tone: Some(tone(g)),
                note: "assumption".into(),
                main: false,
                assume: true,
                title: assumption.map(|a| format!("Assumption: {}. Providers do not promise new credits.", text(Some(a)))),
            },
        ];
        let lapses = arr(b.get("lapses"));
        let note = (!lapses.is_empty()).then(|| {
            let r: Vec<String> = lapses.iter().map(|l| self.lapse_reason(p, l, b.get("firstOutageAt"))).collect();
            format!("Lapses: {}.", r.join("; "))
        });
        Some(Ladder { heading: format!("Next {} days at current demand", js::round(or(get(s, "horizonHours"), 336.0) / 24.0)), title: None, rows, note })
    }

    fn pace_parts(&self, p: &str) -> Option<(&'a Value, f64, f64)> {
        let f = self.f(p);
        let pc = get(f, "pace").filter(|v| truthy(Some(v)))?;
        if !get(Some(pc), "maxUnits").and_then(Value::as_f64).is_some_and(f64::is_finite) || s_eq(get(f, "state"), "unknown") {
            return None;
        }
        let lost = or(get(get(Some(pc), "atDemand"), "lostUnits"), 0.0);
        let days = js::round(n(pc.get("horizonHours")) / 24.0);
        Some((pc, lost, days))
    }

    /// `paceSentence(p)`
    pub fn pace_sentence(&self, p: &str) -> String {
        let Some((pc, lost, days)) = self.pace_parts(p) else { return String::new() };
        if lost < 0.05 {
            return String::new();
        }
        let t = self.term(p);
        let pc = Some(pc);
        let full = get(pc, "fullUse").filter(|v| truthy(Some(v)));
        let then = get(pc, "then").filter(|v| truthy(Some(v)));
        let how = match full {
            Some(full) => format!(
                "at least {}{}",
                wk(n(full.get("unitsPerHour"))),
                then.map_or_else(String::new, |th| format!(" until {}, then at least {}", self.dt(th.get("from")), wk(n(th.get("unitsPerHour")))))
            ),
            None => "drain each account as fast as possible and reset it".into(),
        };
        let credits = n(get(pc, "credits"));
        let max_lapsing = n(get(pc, "maxCreditsLapsing"));
        let uses = if credits != 0.0 && !credits.is_nan() {
            let what = if max_lapsing != 0.0 && !max_lapsing.is_nan() {
                format!("{} of {}", js::number_to_string(credits - max_lapsing), plural(credits, t))
            } else if credits == 1.0 {
                format!("the {t}")
            } else if credits == 2.0 {
                format!("both {t}s")
            } else {
                format!("all {} {t}s", js::number_to_string(credits))
            };
            format!(" (uses {what})")
        } else {
            String::new()
        };
        let lapsing = n(get(get(pc, "atDemand"), "creditsLapsing"));
        let lapse_note = if lapsing != 0.0 && !lapsing.is_nan() { format!(", {} lapsing", plural(lapsing, t)) } else { String::new() };
        format!(
            "To get the most out of {}: {how}{uses}. The current pace leaves {} unused over {} days{lapse_note}. ",
            self.pname(p),
            aw(lost),
            js::number_to_string(days)
        )
    }

    /// `paceHtml(p)`
    pub fn pace(&self, p: &str) -> Option<Ladder> {
        let (pc, lost, days) = self.pace_parts(p)?;
        let t = self.term(p);
        let pc = Some(pc);
        let credits = n(get(pc, "credits"));
        let has_credits = credits != 0.0 && !credits.is_nan();
        let cr = |v: Option<&Value>| -> String {
            if !has_credits {
                return String::new();
            }
            let l = n(get(v, "creditsLapsing"));
            if l != 0.0 && !l.is_nan() {
                format!("{}{}", plural(l, t), if l == 1.0 { " lapses" } else { " lapse" })
            } else {
                format!("every {t} used")
            }
        };
        let gap = |v: Option<&Value>| {
            let h = n(get(v, "shortHours"));
            if h != 0.0 && !h.is_nan() { format!("short ~{} h", js::number_to_string(h)) } else { "never short".into() }
        };
        let join = |a: String, b: String| [a, b].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(", ");
        let row = |label: String, value: String, note: String, main: bool| LRow { label, value, value_tone: None, note, main, assume: false, title: None };
        let at = get(pc, "atDemand");
        let curve = arr(get(pc, "curve"));
        let full = get(pc, "fullUse").filter(|v| truthy(Some(v)));
        let then = get(pc, "then").filter(|v| truthy(Some(v)));
        let no_gap = get(pc, "noGap");
        let mut rows = vec![
            row("Most usable".into(), aw(n(get(pc, "maxUnits"))), "draining as fast as possible".into(), true),
            row(
                format!("At current pace {}", num(n(get(pc, "demandUnitsPerHour")) * 168.0, 1)),
                format!("{}{}", num_v(get(at, "usedUnits"), 1), if lost >= 0.05 { format!(" ({} unused)", num(lost, 1)) } else { String::new() }),
                join(cr(at), gap(at)),
                false,
            ),
        ];
        rows.push(match full {
            Some(full) => row(
                "Use it all".into(),
                format!(
                    "at least {}{}",
                    wk(n(full.get("unitsPerHour"))),
                    then.map_or_else(String::new, |th| format!(", then {} from {}", num(n(th.get("unitsPerHour")) * 168.0, 1), self.day(th.get("from"))))
                ),
                join(cr(Some(full)), gap(Some(full))),
                false,
            ),
            None => row(
                "Use it all".into(),
                "as fast as possible".into(),
                format!("no steady pace up to {}/week gets there", num(n(get(curve.last(), "unitsPerHour")) * 168.0, 1)),
                false,
            ),
        });
        let ng_lost = n(get(no_gap, "lostUnits"));
        rows.push(row(
            "Never run out".into(),
            format!("at most {}", wk(n(get(no_gap, "unitsPerHour")))),
            format!("{} used{}", num_v(get(no_gap, "usedUnits"), 1), if ng_lost >= 0.05 { format!(", {} unused", num(ng_lost, 1)) } else { String::new() }),
            false,
        ));
        let mut notes: Vec<String> = Vec::new();
        if !curve.is_empty() {
            let pts: Vec<String> = curve.iter().map(|v| format!("{}: {}", num(n(v.get("unitsPerHour")) * 168.0, 1), num_v(v.get("usedUnits"), 1))).collect();
            notes.push(format!("Usage by steady pace (acct-weeks/week: used): {}", pts.join(", ")));
        }
        let steps: Vec<&Value> = arr(get(pc, "steps")).iter().filter(|s| truthy(s.get("reachable"))).collect();
        if !steps.is_empty() && credits > 1.0 {
            let pts: Vec<String> = steps.iter().map(|s| format!("{}: {}", cat(s.get("credits")), num(n(s.get("unitsPerHour")) * 168.0, 1))).collect();
            notes.push(format!("Pace that uses {}", pts.join(", ")));
        }
        notes.push(format!(
            "Running short loses nothing, it only means waiting; quota is lost when a window resets unused or a {t} lapses{}",
            if p == "codex" {
                ". A reset re-dates the window, so it is worth most right after a window opens"
            } else {
                ". A refill lasts only until that account's own reset date"
            }
        ));
        let d = js::number_to_string(days);
        Some(Ladder {
            heading: format!("Getting the most out of it (next {d} days)"),
            title: Some(format!("Next {d} days, from the same simulation as the scenarios above. A signal for dispatchers; the pool does not enforce it.")),
            rows,
            note: Some(format!("{}.", notes.join(". "))),
        })
    }
}

// ---------------------------------------------------------------- Provider cards

pub struct Stat {
    pub tone: Option<Tone>,
    pub label: String,
    pub value: String,
    pub sub: String,
}

pub struct SupplySlice {
    pub weight: f64,
    pub left: f64,
    pub title: String,
}

pub struct AcctLine {
    pub name: String,
    pub email: String,
    pub next: bool,
    pub used: Option<f64>,
    pub reset_title: String,
    pub resets: String,
    pub credits: Option<(String, String)>,
    pub no_credits: String,
}

pub struct ProviderCard {
    pub id: String,
    pub name: String,
    pub accounts: String,
    pub cost: Option<(String, String)>,
    pub state: (Tone, &'static str),
    pub state_title: String,
    pub remaining: String,
    pub total: String,
    pub supply: Vec<SupplySlice>,
    pub stats: Vec<Stat>,
    pub ladder: Option<Ladder>,
    pub pace: Option<Ladder>,
    pub lines: Vec<AcctLine>,
    pub empty: bool,
}

impl<'a> Ctx<'a> {
    /// `providerHtml(p)`
    pub fn provider_card(&self, p: &str) -> ProviderCard {
        let f = self.f(p);
        let su = get(f, "supply");
        let pr = get(f, "projection");
        let s = self.sc(p);
        let list = self.accounts(p);
        let live: Vec<&Value> = list.iter().copied().filter(|a| !truthy(a.get("disabled"))).collect();
        let total = if js::nullish(get(su, "weightedAccounts")) { live.iter().map(|a| self.weight(a)).sum() } else { n(get(su, "weightedAccounts")) };
        let remaining = if js::nullish(get(su, "remainingUnits")) {
            live.iter().map(|a| self.weight(a) * (100.0 - or(get(weekly(a), "used"), 100.0)) / 100.0).sum()
        } else {
            n(get(su, "remainingUnits"))
        };
        let up = self.next_up(p);
        let cost = self.plan_cost(p);
        let credits: f64 = live.iter().map(|a| if truthy(a.get("creditsKnown")) { or(a.get("creditCount"), 0.0) } else { 0.0 }).sum();
        let planned = self.plan(f).map(|cp| arr(cp.get("credits")));
        let lapsing = match planned {
            Some(c) => c.iter().filter(|c| s_eq(c.get("outcome"), "lapses")).count() as f64,
            None => or(get(get(f, "creditOutlook"), "creditsLapsing"), 0.0),
        };
        let saving = planned.map_or(0, |c| c.iter().filter(|c| !s_eq(c.get("action"), "wait") && s_eq(c.get("outcome"), "used")).count());
        let b = get(s, "bankedCredits").filter(|v| truthy(Some(v)));
        let g = get(s, "weeklyGrant").filter(|v| truthy(Some(v)));
        let n0 = get(s, "naturalOnly");
        let t = self.term(p);
        let ratio = get(pr, "demandToNaturalSupply");
        let state = get(f, "state");
        let runway = if !truthy(state) {
            Stat { tone: None, label: "Runway".into(), value: "?".into(), sub: "no forecast yet".into() }
        } else if truthy(get(pr, "firstOutageAt")) {
            let after = if truthy(get(pr, "creditsModeled")) {
                let k = if js::nullish(get(b, "creditsUsed")) { or(get(pr, "creditsUsed"), 0.0) } else { n(get(b, "creditsUsed")) };
                format!(", after {}", plural(k, t))
            } else {
                String::new()
            };
            let grant = if g.is_some() && !truthy(get(g, "firstOutageAt")) { format!("; none with 1 new {t}/week") } else { String::new() };
            Stat { tone: Some(Tone::Bad), label: "Short from".into(), value: self.day(get(pr, "firstOutageAt")), sub: format!("~{} h{after}{grant}", cat(get(pr, "outageHours"))) }
        } else {
            let used = n(get(b, "creditsUsed"));
            let sub = if truthy(get(n0, "firstOutageAt")) && used != 0.0 && !used.is_nan() {
                format!("covered by {}", plural(used, &format!("banked {t}")))
            } else {
                format!("next {} days", js::round(or(get(pr, "horizonHours"), 336.0) / 24.0))
            };
            Stat { tone: Some(Tone::Ok), label: "Runway".into(), value: "No shortfall".into(), sub }
        };
        let ratio_n = n(ratio);
        let stats = vec![
            Stat {
                tone: (ratio_n > 1.2).then_some(Tone::Warn),
                label: "Demand".into(),
                value: if js::nullish(ratio) { "?".into() } else { format!("{}x supply", num(ratio_n, 2)) },
                sub: format!("{} acct-weeks per week", num(or(get(get(f, "demand"), "unitsPerHour"), 0.0) * 168.0, 1)),
            },
            runway,
            Stat {
                tone: (lapsing != 0.0 && !lapsing.is_nan()).then_some(Tone::Warn),
                label: format!("Banked {t}s"),
                value: js::number_to_string(credits),
                sub: if lapsing != 0.0 && !lapsing.is_nan() {
                    format!("{} may lapse unused", js::number_to_string(lapsing))
                } else if saving > 0 {
                    format!("{saving} planned for use")
                } else {
                    "none at risk".into()
                },
            },
        ];
        let supply = live
            .iter()
            .map(|a| {
                let left = weekly(a).map_or(0.0, |w| (100.0 - n(w.get("used"))).max(0.0));
                SupplySlice { weight: self.weight(a), left, title: format!("{}: {}% left", label(a), js::round(left)) }
            })
            .collect();
        let mut sorted = live.clone();
        js::sort_by(&mut sorted, |x, y| or(get(weekly(x), "resetAt"), 0.0) - or(get(weekly(y), "resetAt"), 0.0));
        let lines = sorted
            .iter()
            .map(|a| {
                let w = weekly(a);
                let count = n(a.get("creditCount"));
                AcctLine {
                    name: label(a),
                    email: text(a.get("email")),
                    next: up.is_some_and(|u| u.get("id") == a.get("id")),
                    used: w.and_then(|w| fmt::number(w.get("used"))),
                    reset_title: format!("Weekly reset {}", w.map_or_else(String::new, |w| self.dt(w.get("resetAt")))),
                    resets: w.map_or_else(String::new, |w| format!("resets {}", self.in_t(w.get("resetAt")))),
                    credits: (count != 0.0 && !count.is_nan()).then(|| (cat(a.get("creditCount")), format!("{t}{}", if count == 1.0 { "" } else { "s" }))),
                    no_credits: format!("no {t}s"),
                }
            })
            .collect();
        let cost_line = (cost.count > 0).then(|| {
            let mut tip: Vec<String> = cost.sources.iter().map(|x| source_text(x)).collect::<Vec<_>>();
            let mut tip_text = tip.join("; ");
            if cost.missing > 0 {
                tip_text += &format!("; {} account(s) unpriced", cost.missing);
            }
            tip.clear();
            (format!("{}/mo{}", usd(cost.usd, 0), if cost.missing > 0 { "+" } else { "" }), tip_text)
        });
        ProviderCard {
            id: p.to_string(),
            name: self.pname(p),
            accounts: plural(list.len() as f64, "account"),
            cost: cost_line,
            state: state_of(state),
            state_title: format!("Capacity forecast state{}", if truthy(get(f, "since")) { format!(" since {}", self.dt(get(f, "since"))) } else { String::new() }),
            remaining: num(remaining, 1),
            total: num(total, 1),
            supply,
            stats,
            ladder: self.ladder(p),
            pace: self.pace(p),
            lines,
            empty: list.is_empty(),
        }
    }
}

// ---------------------------------------------------------------- Timeline

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Reset,
    ResetNext,
    Used,
    Lapse,
    Credit,
}

pub struct TlRow {
    pub provider: Option<String>,
    pub label: String,
    pub title: String,
    /// The shortfall band: start and width in percent of the span, with its text.
    pub band: Option<(f64, f64, String, String)>,
    pub marks: Vec<(Mark, f64, String)>,
}

pub struct Timeline {
    pub ticks: Vec<(f64, String, bool)>,
    pub rows: Vec<TlRow>,
}

impl<'a> Ctx<'a> {
    /// `timelineHtml()`: the next 14 days.
    pub fn timeline(&self) -> Timeline {
        let start = self.now;
        let span = 14.0 * D;
        let end = start + span;
        let x = |t: f64| (t - start) / span * 100.0;
        let inside = |v: Option<&Value>| ok_time(v) && ts(v) >= start && ts(v) <= end;
        let mut ticks = Vec::new();
        // As the browser steps: local midnight today, then 24 hours at a time.
        let mut d = self.z.day_start(start) + D;
        while d < end {
            let (text, weekend) = fmt::tick(self.z, d);
            ticks.push((x(d), text, weekend));
            d += D;
        }
        let mut rows = Vec::new();
        for p in self.provider_ids() {
            let pr = get(self.f(&p), "projection");
            let b = get(self.sc(&p), "bankedCredits");
            let t = self.term(&p);
            let band = (truthy(get(pr, "firstOutageAt")) && inside(get(pr, "firstOutageAt"))).then(|| {
                let l = x(ts(get(pr, "firstOutageAt")));
                let w = (100.0 - l).min(n(get(pr, "outageHours")) * H / span * 100.0);
                let title = format!(
                    "Projected shortfall{}: about {} h with every account at its limit, starting {}",
                    if truthy(get(pr, "creditsModeled")) { " after banked credits" } else { "" },
                    cat(get(pr, "outageHours")),
                    self.dt(get(pr, "firstOutageAt"))
                );
                (l, w, format!("~{} h short", cat(get(pr, "outageHours"))), title)
            });
            rows.push(TlRow { provider: Some(p.clone()), label: self.pname(&p), title: String::new(), band, marks: Vec::new() });
            for a in self.accounts(&p).into_iter().filter(|a| !truthy(a.get("disabled"))) {
                let mut marks = Vec::new();
                if let Some(w) = weekly(a).filter(|w| ok_time(w.get("resetAt"))) {
                    let mut r = ts(w.get("resetAt"));
                    let mut k = 0;
                    while r <= end && k < 4 {
                        if r >= start {
                            let title = format!("{}{}", if k > 0 { "Following weekly reset " } else { "Weekly reset " }, fmt::dt_ms(self.z, r));
                            marks.push((if k > 0 { Mark::ResetNext } else { Mark::Reset }, x(r), title));
                        }
                        r += 7.0 * D;
                        k += 1;
                    }
                }
                for r in arr(get(b, "redemptions")) {
                    if r.get("id") == a.get("id") && inside(r.get("at")) {
                        marks.push((Mark::Used, x(ts(r.get("at"))), format!("Projected: {t} used {}", self.dt(r.get("at")))));
                    }
                }
                let lapses = arr(get(b, "lapses"));
                for l in lapses {
                    if l.get("id") == a.get("id") && inside(l.get("at")) {
                        marks.push((Mark::Lapse, x(ts(l.get("at"))), format!("Projected: {t} lapses unused {}", self.dt(l.get("at")))));
                    }
                }
                let mut spent: Vec<f64> = arr(get(b, "redemptions")).iter().filter(|r| r.get("id") == a.get("id")).map(|r| ts(r.get("at"))).collect();
                spent.sort_by(f64::total_cmp);
                let mut expiries: Vec<Value> = match a.get("creditExpiries") {
                    Some(Value::Array(e)) => e.clone(),
                    _ if truthy(a.get("nextCreditExpiry")) => vec![a.get("nextCreditExpiry").cloned().unwrap_or(Value::Null)],
                    _ => Vec::new(),
                };
                js::sort_by(&mut expiries, |p1, p2| ts(Some(p1)) - ts(Some(p2)));
                for e in &expiries {
                    let et = ts(Some(e));
                    if let Some(k) = spent.iter().position(|r| *r <= et) {
                        spent.remove(k);
                        continue;
                    }
                    let near_lapse = lapses.iter().any(|l| l.get("id") == a.get("id") && (ts(l.get("at")) - et).abs() < 2.0 * H);
                    if inside(Some(e)) && !near_lapse {
                        marks.push((Mark::Credit, x(et), format!("{t} credit expires {}", self.dt(Some(e)))));
                    }
                }
                rows.push(TlRow { provider: None, label: label(a), title: text(a.get("email")), band: None, marks });
            }
        }
        Timeline { ticks, rows }
    }
}

// ---------------------------------------------------------------- Accounts

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    All,
    Credits,
    Attention,
}

impl Filter {
    pub const ALL: [(Filter, &'static str, &'static str); 3] =
        [(Filter::All, "all", "All"), (Filter::Credits, "credits", "Has credits"), (Filter::Attention, "attention", "Needs attention")];

    pub fn parse(s: Option<&str>) -> Filter {
        match s {
            Some("credits") => Filter::Credits,
            Some("attention") => Filter::Attention,
            _ => Filter::All,
        }
    }

    pub fn key(self) -> &'static str {
        Filter::ALL.iter().find(|(f, _, _)| *f == self).map_or("all", |(_, k, _)| k)
    }
}

pub struct Col {
    pub head: String,
    /// Width in percent of the table (its `<col>`).
    pub w: u8,
    pub tip: Option<String>,
    /// Hidden on narrow screens.
    pub sm: bool,
}

pub enum Cell {
    Who { tone: Tone, status: &'static str, name: String, email: String, next: bool, notes: Vec<(Tone, String)>, reauth: Option<Act> },
    Plan { name: String, weight: Option<String>, sub: Option<String>, tip: String },
    Meter { used: f64, tick: Option<f64>, sub: Option<String> },
    Text { text: String, title: Option<String>, sub: Option<String>, tone: Option<Tone>, dim: bool, mono: bool },
    Credits { pips: usize, count: String, title: String, next: Option<(String, String, bool)> },
}

impl Cell {
    fn dim(text: impl Into<String>) -> Cell {
        Cell::Text { text: text.into(), title: None, sub: None, tone: None, dim: true, mono: false }
    }
}

pub struct KV {
    pub key: String,
    pub value: String,
    pub note: Option<String>,
}

pub struct Detail {
    pub windows: Vec<KV>,
    pub models_ready: String,
    pub models: Vec<(String, bool)>,
    pub list_read: String,
    pub plan: Vec<KV>,
    pub notes: Vec<String>,
    pub id: String,
    /// "Refresh usage", then "Sign in again" (primary) or "Reconnect".
    pub acts: Vec<Act>,
}

pub struct Row {
    pub id: String,
    pub cells: Vec<Cell>,
    pub open: bool,
    pub detail: Option<Detail>,
}

pub struct AcctTable {
    pub provider: String,
    /// "Connect {name}"
    pub connect: Act,
    pub name: String,
    pub summary: String,
    pub state: (Tone, &'static str),
    pub cols: Vec<Col>,
    pub rows: Vec<Row>,
}

pub struct AccountsView {
    pub filter: Filter,
    pub shown: String,
    pub tables: Vec<AcctTable>,
}

fn elapsed_pct(w: &Value, now: f64) -> Option<f64> {
    let d = n(w.get("durationSeconds"));
    (d != 0.0 && !d.is_nan() && ok_time(w.get("resetAt"))).then(|| (100.0 - (ts(w.get("resetAt")) - now) / (d * 1000.0) * 100.0).clamp(0.0, 100.0))
}

fn find_window<'b>(a: &'b Value, k: &str) -> Option<&'b Value> {
    windows(a).iter().find(|x| window_key(x) == k)
}

fn window_key(w: &Value) -> String {
    format!("{}|{}", text(w.get("scope")), cat(w.get("label")))
}

impl<'a> Ctx<'a> {
    fn needs_attention(&self, a: &Value) -> bool {
        !truthy(a.get("disabled"))
            && (truthy(a.get("refreshError"))
                || truthy(a.get("usageStale"))
                || s_eq(a.get("status"), "unknown")
                || !s_eq(a.get("routingStatus"), "ready")
                || !s_eq(get(a.get("modelList"), "source"), "live")
                || or(get(weekly(a), "used"), 0.0) >= 80.0
                || model_wins(a).iter().any(|w| n(w.get("used")) >= 85.0))
    }

    fn not_started(&self, a: &Value, w: &Value) -> bool {
        s_eq(a.get("provider"), "codex") && n(w.get("used")) == 0.0 && ts(w.get("resetAt")) - self.now > n(w.get("durationSeconds")) * 1000.0 - 2.0 * H
    }

    /// `statusOf(a)`
    pub fn status_of(&self, a: &Value) -> (Tone, &'static str) {
        if truthy(a.get("disabled")) {
            (Tone::Neutral, "Disabled")
        } else if reauth_needed(a) {
            (Tone::Bad, "Sign-in expired")
        } else if s_eq(a.get("status"), "exhausted") && !truthy(a.get("usageStale")) {
            (Tone::Bad, "At limit")
        } else if truthy(a.get("usageStale")) {
            (Tone::Warn, "Last known")
        } else if s_eq(a.get("status"), "unknown") {
            (Tone::Warn, "Not verified")
        } else if s_eq(a.get("routingStatus"), "blocked") {
            (Tone::Warn, "Routing paused")
        } else if !s_eq(a.get("routingStatus"), "ready") {
            (Tone::Warn, "Not routed")
        } else {
            (Tone::Ok, "Ready")
        }
    }

    /// `accountNotes(a)`: what is wrong, cause first.
    pub fn account_notes(&self, a: &Value) -> Vec<(Tone, String)> {
        let mut out = Vec::new();
        let e = a.get("refreshError").filter(|e| truthy(Some(e)));
        let since = |e: Option<&Value>| if truthy(get(e, "since")) { format!(" since {}", self.dt(get(e, "since"))) } else { String::new() };
        if s_eq(get(e, "code"), "reauth_required") {
            let msg = if truthy(get(e, "message")) { format!(" ({})", cat(get(e, "message"))) } else { String::new() };
            let who = js::coalesce([a.get("email"), a.get("id")]).filter(|v| truthy(Some(v))).map_or_else(String::new, |v| text(Some(v)));
            out.push((Tone::Bad, format!("Sign-in expired{}: the provider rejected this account's saved login. Sign in again as {who}.{msg}", since(e))));
        } else if e.is_some() {
            let attempts = if n(get(e, "attempts")) > 1.0 { format!(" ({} attempts)", cat(get(e, "attempts"))) } else { String::new() };
            out.push((Tone::Warn, format!("Token refresh failing{}{attempts}; the proxy keeps retrying.", since(e))));
        }
        if truthy(a.get("error")) {
            out.push((Tone::Bad, cat(a.get("error"))));
        }
        if truthy(a.get("usageStale")) {
            let from = if truthy(a.get("usageUpdatedAt")) { format!(" from {}", self.dt(a.get("usageUpdatedAt"))) } else { String::new() };
            out.push((Tone::Warn, format!("Showing last known usage{from}. Refresh retries automatically.")));
        }
        let rs = a.get("routingStatus");
        if !truthy(a.get("disabled")) && s_eq(a.get("status"), "available") && !truthy(a.get("usageStale")) && truthy(rs) && !s_eq(rs, "ready") {
            let what = match rs.and_then(Value::as_str) {
                Some("ready") => "ready",
                Some("blocked") => "temporarily unavailable",
                Some("disabled") => "disabled",
                _ => "not verified",
            };
            out.push((Tone::Warn, format!("Quota available; routing: {what}.")));
        }
        if truthy(a.get("routingError")) {
            out.push((Tone::Bad, cat(a.get("routingError"))));
        }
        let ml = a.get("modelList");
        if truthy(ml) && !s_eq(get(ml, "source"), "live") {
            let err = if truthy(get(ml, "error")) { format!(" (last read failed: {})", cat(get(ml, "error"))) } else { String::new() };
            out.push((Tone::Warn, format!("Live model list not read yet{err}; no requests are routed to this account until it is.")));
        } else if truthy(get(ml, "error")) {
            out.push((Tone::Warn, format!("Live model list: last read failed ({}); routing keeps the last good list.", cat(get(ml, "error")))));
        }
        out
    }

    /// `wmeter(w, opts)`
    fn wmeter(&self, w: Option<&Value>, tick: bool, sub: Option<String>) -> Cell {
        match w.filter(|w| !js::nullish(w.get("used"))) {
            None => Cell::dim("not reported"),
            Some(w) => Cell::Meter { used: n(w.get("used")), tick: if tick { elapsed_pct(w, self.now) } else { None }, sub: sub.filter(|s| !s.is_empty()) },
        }
    }

    /// `creditCell(a)`
    fn credit_cell(&self, a: &Value) -> Cell {
        if !truthy(a.get("creditsKnown")) {
            return Cell::Text { text: "unknown".into(), title: Some(text(a.get("creditNote"))), sub: None, tone: None, dim: true, mono: false };
        }
        let count = n(a.get("creditCount"));
        if count == 0.0 || count.is_nan() {
            return Cell::dim("none");
        }
        let t = self.term(&text(a.get("provider")));
        let days = if truthy(a.get("nextCreditExpiry")) { (ts(a.get("nextCreditExpiry")) - self.now) / D } else { f64::INFINITY };
        let all: Vec<String> = arr(a.get("creditExpiries")).iter().map(|e| self.dt(Some(e))).collect();
        let title = format!("{}{}", plural(count, &format!("banked {t}")), if all.is_empty() { String::new() } else { format!(": expires {}", all.join(", ")) });
        let next = truthy(a.get("nextCreditExpiry")).then(|| {
            (format!("next expires in {}", self.in_t(a.get("nextCreditExpiry"))), self.dt(a.get("nextCreditExpiry")), days < 8.0)
        });
        Cell::Credits { pips: count.clamp(0.0, 6.0) as usize, count: cat(a.get("creditCount")), title, next }
    }

    /// `paceCell(a)`
    fn pace_cell(&self, a: &Value) -> Cell {
        let Some(w) = weekly(a) else { return Cell::dim("") };
        if self.not_started(a, w) {
            return Cell::Text { text: "not started".into(), title: Some("Codex starts the weekly window on first use".into()), sub: None, tone: None, dim: true, mono: false };
        }
        let Some(e) = elapsed_pct(w, self.now) else { return Cell::dim("") };
        let used = n(w.get("used"));
        let diff = js::round(used - e);
        let text = if diff.abs() < 5.0 {
            "on pace".to_string()
        } else if diff > 0.0 {
            format!("{} pts ahead", js::number_to_string(diff))
        } else {
            format!("{} pts under", js::number_to_string(-diff))
        };
        Cell::Text {
            text,
            title: Some(format!("{}% used, {}% of the week elapsed", js::round(used), js::round(e))),
            sub: None,
            tone: (diff >= 15.0).then_some(Tone::Warn),
            dim: false,
            mono: false,
        }
    }

    /// `planCell(a)`
    fn plan_cell(&self, a: &Value) -> Cell {
        let pr = self.price_of(a);
        let name = plan_name(a);
        if name.is_empty() && pr.is_none() {
            return Cell::dim(if truthy(a.get("planError")) { "read failed" } else { "not read yet" });
        }
        let plan = a.get("plan");
        let mut subs = Vec::new();
        if let Some(pr) = &pr {
            subs.push(format!("{}/mo{}", usd(pr.usd, 0), if pr.source == "list" { " list" } else { "" }));
        }
        if truthy(get(plan, "renewsAt")) {
            subs.push(format!("{} {}", if s_eq(get(plan, "status"), "ends") { "ends" } else { "renews" }, self.day(get(plan, "renewsAt"))));
        }
        let tip: Vec<String> = [
            pr.as_ref().map_or_else(String::new, |p| source_text(&p.source)),
            pr.as_ref().map_or_else(String::new, |p| p.detail.clone()),
            if truthy(get(plan, "readAt")) { format!("read {}", ago(get(plan, "readAt"), self.now)) } else { String::new() },
            if truthy(a.get("planError")) { format!("last read failed: {}", cat(a.get("planError"))) } else { String::new() },
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
        let w = self.weight(a);
        Cell::Plan {
            name: if name.is_empty() { "?".into() } else { name },
            weight: (w != 1.0).then(|| format!("{}x", js::number_to_string(w))),
            sub: (!subs.is_empty()).then(|| subs.join(" · ")),
            tip: tip.join("; "),
        }
    }

    fn reset_cell(&self, a: &Value) -> Cell {
        match weekly(a).filter(|w| ok_time(w.get("resetAt"))) {
            None => Cell::dim("not reported"),
            Some(w) if ts(w.get("resetAt")) <= self.now => {
                Cell::Text { text: "Past estimate".into(), title: None, sub: Some("latest usage applies".into()), tone: None, dim: true, mono: false }
            }
            Some(w) => Cell::Text {
                text: self.in_t(w.get("resetAt")),
                title: None,
                sub: Some(format!("{} {}", self.day(w.get("resetAt")), self.clock(w.get("resetAt")))),
                tone: None,
                dim: false,
                mono: true,
            },
        }
    }

    /// `detailHtml(a)`
    fn detail(&self, a: &Value) -> Detail {
        let models: Vec<(String, bool)> =
            js::entries(a.get("routingModels")).into_iter().map(|(m, r)| (m, s_eq(r.get("status"), "ready"))).collect();
        let down = models.iter().filter(|(_, ok)| !ok).count();
        let v = self.value_row(a);
        let waste = arr(get(get(self.v, "waste"), "measured")).iter().find(|x| x.get("id") == a.get("id"));
        let pr = self.price_of(a);
        let plan = a.get("plan");
        let t = self.term(&text(a.get("provider")));
        let windows = windows(a)
            .iter()
            .map(|w| KV {
                key: format!("{} · {}", text(w.get("scope")), text(w.get("label"))),
                value: format!(
                    "{}, resets {}",
                    if js::nullish(w.get("used")) { "not reported".to_string() } else { format!("{}% used", js::round(n(w.get("used")))) },
                    self.dt(w.get("resetAt"))
                ),
                note: None,
            })
            .collect();
        let kv = |key: &str, value: String| KV { key: key.into(), value, note: None };
        let mut rows = vec![KV {
            key: "Plan".into(),
            value: format!(
                "{}{}",
                get(plan, "label").filter(|v| !v.is_null()).map_or_else(
                    || {
                        let n = plan_name(a);
                        if n.is_empty() { "not read yet".to_string() } else { n }
                    },
                    |l| text(Some(l))
                ),
                if truthy(get(plan, "status")) && !s_eq(get(plan, "status"), "active") { format!(" ({})", cat(get(plan, "status"))) } else { String::new() }
            ),
            note: None,
        }];
        rows.push(match &pr {
            Some(pr) => KV { key: "Price".into(), value: format!("{}/mo", usd(pr.usd, 0)), note: Some(format!("({})", source_text(&pr.source))) },
            None => kv("Price", "unknown".into()),
        });
        if truthy(get(plan, "renewsAt")) {
            rows.push(kv(if s_eq(get(plan, "status"), "ends") { "Ends" } else { "Renews" }, self.dt(get(plan, "renewsAt"))));
        }
        rows.push(kv(&format!("Banked {t}s"), if truthy(a.get("creditsKnown")) { cat(a.get("creditCount")) } else { "unknown".into() }));
        let expiries = arr(a.get("creditExpiries"));
        rows.push(kv(
            "Expiries",
            if !expiries.is_empty() {
                expiries.iter().map(|e| self.dt(Some(e))).collect::<Vec<_>>().join(", ")
            } else if truthy(a.get("nextCreditExpiry")) {
                self.dt(a.get("nextCreditExpiry"))
            } else {
                "none".into()
            },
        ));
        if let Some(v) = v {
            let failed = n(v.get("failedRequests"));
            rows.push(KV {
                key: "Requests".into(),
                value: fmt::locale(or(v.get("requests"), 0.0), 0, 3),
                note: (failed != 0.0 && !failed.is_nan()).then(|| format!("({} failed)", cat(v.get("failedRequests")))),
            });
            rows.push(kv("API-equivalent", usd_v(v.get("apiEquivalentUsd"), 2)));
            rows.push(kv("Plan weight", format!("{} account-week", cat(v.get("weight")))));
        }
        if let Some(w) = waste.filter(|w| truthy(w.get("windowsEnded"))) {
            rows.push(kv("Unused at window end", format!("{} acct-weeks over {} windows", num_v(w.get("unusedUnits"), 2), cat(w.get("windowsEnded")))));
        }
        let notes = [
            text(a.get("creditNote")),
            if truthy(a.get("planError")) { format!("Plan read failed: {}", cat(a.get("planError"))) } else { String::new() },
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
        let ml = a.get("modelList");
        Detail {
            windows,
            models_ready: format!("Models ({} of {} ready)", models.len() - down, models.len()),
            models,
            list_read: format!("Live list read {}", if truthy(get(ml, "fetchedAt")) { ago(get(ml, "fetchedAt"), self.now) } else { "never".into() }),
            plan: rows,
            notes,
            id: text(a.get("id")),
            acts: vec![
                self.refresh("Refresh usage", a.get("id").and_then(Value::as_str).map(str::to_string)),
                self.signin(
                    if reauth_needed(a) { "Sign in again" } else { "Reconnect" },
                    reauth_needed(a),
                    a.get("provider").and_then(Value::as_str),
                    a.get("id").and_then(Value::as_str),
                ),
            ],
        }
    }

    /// `accountsHtml(filter, openRow)`
    pub fn accounts_view(&self, filter: Filter, open: Option<&str>) -> AccountsView {
        let keep = |a: &Value| match filter {
            Filter::All => true,
            Filter::Credits => n(a.get("creditCount")) > 0.0,
            Filter::Attention => self.needs_attention(a),
        };
        let shown = self.all_accounts().iter().filter(|a| keep(a)).count();
        let tables = self
            .provider_ids()
            .into_iter()
            .map(|p| {
                let all = self.accounts(&p);
                let mut list: Vec<&Value> = all.iter().copied().filter(|a| keep(a)).collect();
                js::sort_by(&mut list, |x, y| or(get(weekly(x), "resetAt"), 0.0) - or(get(weekly(y), "resetAt"), 0.0));
                let (cols, cells) = self.columns(&p, &all);
                let up = self.next_up(&p);
                let rows = list
                    .iter()
                    .map(|a| {
                        let id = text(a.get("id"));
                        let is_open = open == Some(id.as_str());
                        Row { cells: cells.iter().map(|c| c(a, up)).collect(), open: is_open, detail: is_open.then(|| self.detail(a)), id }
                    })
                    .collect();
                let f = self.f(&p);
                let cost = self.plan_cost(&p);
                let su = get(f, "supply");
                let mut summary = plural(all.len() as f64, "account");
                if cost.count > 0 {
                    summary += &format!(" · {}/mo", usd(cost.usd, 0));
                }
                if truthy(su) {
                    summary += &format!(" · {} of {} account-weeks left", num_v(get(su, "remainingUnits"), 1), num_v(get(su, "weightedAccounts"), 1));
                }
                AcctTable { name: self.pname(&p), connect: self.signin(format!("Connect {}", self.pname(&p)), false, Some(&p), None), provider: p, summary, state: state_of(get(f, "state")), cols, rows }
            })
            .collect();
        AccountsView { filter, shown: format!("{shown} of {} accounts", self.all_accounts().len()), tables }
    }

    /// `columnsFor(p, list)`: columns from what the provider reports, and how each fills a cell.
    #[allow(clippy::type_complexity)]
    fn columns(&self, p: &str, list: &[&Value]) -> (Vec<Col>, Vec<Box<dyn Fn(&Value, Option<&Value>) -> Cell + '_>>) {
        let mut cols = vec![Col { head: "Account".into(), w: 24, tip: None, sm: false }];
        let mut cells: Vec<Box<dyn Fn(&Value, Option<&Value>) -> Cell + '_>> = vec![Box::new(move |a: &Value, up: Option<&Value>| {
            let (tone, status) = self.status_of(a);
            let who = js::coalesce([a.get("email"), a.get("id")]).filter(|v| truthy(Some(v))).map_or_else(String::new, |v| text(Some(v)));
            Cell::Who {
                tone,
                status,
                name: label(a),
                email: text(a.get("email")),
                next: up.is_some_and(|u| u.get("id") == a.get("id")),
                notes: self.account_notes(a),
                reauth: (reauth_needed(a) && !truthy(a.get("disabled"))).then(|| {
                    self.signin(format!("Sign in again as {who}"), true, a.get("provider").and_then(Value::as_str), a.get("id").and_then(Value::as_str))
                }),
            }
        })];
        if list.iter().any(|a| !plan_name(a).is_empty() || self.price_of(a).is_some()) {
            cols.push(Col { head: "Plan".into(), w: 13, tip: None, sm: false });
            cells.push(Box::new(move |a: &Value, _| self.plan_cell(a)));
        }
        let mut seen: Vec<(String, &Value)> = Vec::new();
        for a in list {
            for w in windows(a) {
                if !js::nullish(w.get("used")) {
                    let k = window_key(w);
                    match seen.iter_mut().find(|(key, _)| *key == k) {
                        Some(slot) => slot.1 = w,
                        None => seen.push((k, w)),
                    }
                }
            }
        }
        let mut wins: Vec<&Value> = seen.into_iter().map(|(_, w)| w).collect();
        js::sort_by(&mut wins, |x, y| {
            let (cx, cy) = (truthy(x.get("common")), truthy(y.get("common")));
            if cx == cy {
                n(x.get("durationSeconds")) - n(y.get("durationSeconds"))
            } else if cx {
                -1.0
            } else {
                1.0
            }
        });
        let count = wins.len();
        for w in wins {
            let k = window_key(w);
            let common = truthy(w.get("common"));
            let is_weekly = common && s_eq(w.get("label"), "Weekly");
            if !common {
                let (name, detail) = self.model_limit(p, w);
                cols.push(Col { head: name, w: 14, tip: Some(format!("{detail} At the limit those requests spill to the next account.")), sm: true });
                cells.push(Box::new(move |a: &Value, _| {
                    let m = find_window(a, &k);
                    let wk = weekly(a);
                    let sub = match (m, wk) {
                        (Some(m), Some(wk)) if (ts(m.get("resetAt")) - ts(wk.get("resetAt"))).abs() < H => "resets with weekly".to_string(),
                        (Some(m), _) => format!("resets in {}", self.in_t(m.get("resetAt"))),
                        _ => String::new(),
                    };
                    self.wmeter(m, false, Some(sub))
                }));
            } else if is_weekly {
                cols.push(Col {
                    head: if count > 1 { "Weekly, all models".into() } else { "Weekly".into() },
                    w: if count > 1 { 16 } else { 20 },
                    tip: Some("Bar: share used. Tick: share of the week elapsed.".into()),
                    sm: false,
                });
                cells.push(Box::new(move |a: &Value, _| {
                    self.wmeter(find_window(a, &k), true, None)
                }));
            } else {
                let label = text(w.get("label"));
                cols.push(Col { head: label.clone(), w: 13, tip: Some(format!("{label} rolling window")), sm: true });
                cells.push(Box::new(move |a: &Value, _| {
                    let m = find_window(a, &k);
                    let sub = m.map(|m| {
                        let left = self.in_t(m.get("resetAt"));
                        if left == "due" { "reset due".to_string() } else { format!("resets in {left}") }
                    });
                    self.wmeter(m, false, sub)
                }));
            }
        }
        if count == 1 {
            cols.push(Col { head: "Pace".into(), w: 11, tip: Some("Weekly usage against time elapsed in the window".into()), sm: true });
            cells.push(Box::new(move |a: &Value, _| self.pace_cell(a)));
        }
        cols.push(Col { head: "Weekly reset".into(), w: 12, tip: None, sm: false });
        cells.push(Box::new(move |a: &Value, _| self.reset_cell(a)));
        if list.iter().any(|a| truthy(a.get("creditsKnown"))) {
            cols.push(Col { head: format!("Banked {}s", self.term(p)), w: 14, tip: None, sm: false });
            cells.push(Box::new(move |a: &Value, _| self.credit_cell(a)));
        }
        (cols, cells)
    }
}

// ---------------------------------------------------------------- Usage & value

pub struct Kpi {
    pub label: &'static str,
    pub value: String,
    pub sub: String,
}

pub struct Bar {
    pub provider: String,
    pub label: String,
    pub title: String,
    /// Percent of the longest bar.
    pub width: f64,
    pub inside: String,
    pub right: String,
}

pub struct AccountValue {
    pub provider: String,
    pub name: String,
    pub email: String,
    pub requests: String,
    pub usd: String,
    pub per_week: String,
    pub plan: String,
    pub plan_title: String,
    pub multiple: String,
}

pub struct Ends {
    pub provider: String,
    pub name: String,
    pub title: String,
    /// Used percent at each window's end, and whether it is low.
    pub bars: Vec<(f64, bool)>,
    pub wasted: String,
}

pub struct UsageView {
    pub kpis: Vec<Kpi>,
    pub fits: Vec<(String, String)>,
    pub models: Vec<Bar>,
    pub sessions: Vec<Bar>,
    pub since: String,
    pub accounts: Vec<AccountValue>,
    pub ends: Vec<Ends>,
    pub projected: String,
    pub footnote: String,
}

impl<'a> Ctx<'a> {
    fn ledger_text(&self) -> String {
        let l = get(self.sv(), "usageLedger");
        if !truthy(l) {
            return String::new();
        }
        let pending = n(get(l, "pending"));
        format!(
            "Ledger: {}, {} rows written since the service started{}{}.",
            if get(l, "enabled") == Some(&Value::Bool(false)) { "disabled" } else { "recording" },
            fmt::locale(or(get(l, "rows"), 0.0), 0, 3),
            if pending != 0.0 && !pending.is_nan() { format!(", {} waiting to be written", cat(get(l, "pending"))) } else { String::new() },
            if truthy(get(l, "lastError")) { format!("; problem: {}", cat(get(l, "lastError"))) } else { String::new() }
        )
    }

    /// `usageHtml()`: `None` before the first value report.
    pub fn usage(&self) -> Result<UsageView, String> {
        let Some(v) = self.v.filter(|v| truthy(Some(v))) else {
            return Err(self.ledger_text());
        };
        let v = Some(v);
        let acc = arr(get(v, "accounts"));
        let models = arr(get(v, "models"));
        let sess = arr(get(v, "sessions"));
        let reqs: f64 = acc.iter().map(|a| or(a.get("requests"), 0.0)).sum();
        let failed: f64 = acc.iter().map(|a| or(a.get("failedRequests"), 0.0)).sum();
        let spend: f64 = acc.iter().map(|a| or(a.get("apiEquivalentUsd"), 0.0)).sum();
        let days = or(get(v, "valueWindowDays"), 0.0);
        let ids = self.provider_ids();
        let costs: Vec<(String, PlanCost)> = ids.iter().map(|p| (p.clone(), self.plan_cost(p))).collect();
        let monthly: f64 = costs.iter().map(|(_, c)| c.usd).sum();
        let plan_share = monthly * 12.0 / 365.0 * days;
        let multiple = (days >= 1.0 && plan_share > 0.0).then(|| spend / plan_share);
        let max_m = models.iter().map(|m| or(m.get("apiEquivalentUsd"), 0.0)).fold(1.0, f64::max);
        let max_s = sess.iter().map(|x| or(x.get("apiEquivalentUsd"), 0.0)).fold(1.0, f64::max);
        let value_days = or(get(get(v, "config"), "valueDays"), 7.0);
        let since = if days >= value_days - 0.01 {
            format!("last {} days", js::number_to_string(value_days))
        } else {
            format!("since {}", self.dt(js::coalesce([get(v, "valueFrom"), get(get(v, "coverage"), "ledgerSince")])))
        };
        let kpis = vec![
            Kpi {
                label: "Requests",
                value: fmt::locale(reqs, 0, 3),
                sub: format!("{}{since}", if failed != 0.0 { format!("{} failed · ", js::number_to_string(failed)) } else { String::new() }),
            },
            Kpi { label: "API-equivalent", value: usd(spend, 0), sub: "list-price value, not money saved".into() },
            Kpi {
                label: "Plans",
                value: if monthly != 0.0 { format!("{}/mo", usd(monthly, 0)) } else { "?".into() },
                sub: costs.iter().filter(|(_, c)| c.count > 0).map(|(p, c)| format!("{} {}", self.pname(p), usd(c.usd, 0))).collect::<Vec<_>>().join(" · "),
            },
            Kpi {
                label: "Value vs plans",
                value: multiple.map_or_else(|| "pending".to_string(), |m| format!("{}x", num(m, 1))),
                sub: match multiple {
                    None if monthly != 0.0 => "needs a full day of ledger data".into(),
                    None => "needs plan prices".into(),
                    Some(_) => format!("{} vs {} of plan cost", usd(spend, 0), usd(plan_share, 0)),
                },
            },
        ];
        let fits = js::entries(get(v, "fits"))
            .into_iter()
            .map(|(p, f)| {
                let f = Some(f);
                let body = if s_eq(get(f, "status"), "ok") {
                    let pooled = get(f, "pooled");
                    let per: Vec<String> = js::entries(get(get(f, "perModel"), "models"))
                        .into_iter()
                        .map(|(m, r)| format!("{m} {}", usd_v(r.get("usdPerAccountWeek"), 0)))
                        .collect();
                    format!(
                        "one account-week of quota used about {} of API-equivalent usage (fit R² {} over {} account-buckets). By model: {}.",
                        usd_v(get(pooled, "usdPerAccountWeek"), 0),
                        num_v(get(pooled, "r2"), 2),
                        cat(get(f, "observations")),
                        per.join(", ")
                    )
                } else {
                    get(f, "note").filter(|v| !v.is_null()).map_or_else(|| "not enough data yet".to_string(), |v| text(Some(v)))
                };
                (self.pname(&p), body)
            })
            .collect();
        let models = models
            .iter()
            .map(|m| Bar {
                provider: text(m.get("provider")),
                label: text(m.get("model")),
                title: text(m.get("model")),
                width: or(m.get("apiEquivalentUsd"), 0.0) / max_m * 100.0,
                inside: format!("{} · {} req", usd_v(m.get("apiEquivalentUsd"), 0), cat(m.get("requests"))),
                right: usd_v(m.get("usdPerRequest"), 3),
            })
            .collect();
        let sessions = sess
            .iter()
            .take(6)
            .map(|x| {
                let sid = text(x.get("sid"));
                let models: Vec<String> = arr(x.get("models")).iter().map(|m| text(Some(m))).collect();
                Bar {
                    provider: text(x.get("provider")),
                    label: format!("{} {}", text(x.get("client")), sid.chars().take(6).collect::<String>()),
                    title: format!("{sid} · {}", models.join(", ")),
                    width: or(x.get("apiEquivalentUsd"), 0.0) / max_s * 100.0,
                    inside: String::new(),
                    right: usd_v(x.get("apiEquivalentUsd"), 0),
                }
            })
            .collect();
        let mut sorted: Vec<&Value> = acc.iter().collect();
        js::sort_by(&mut sorted, |a, b| or(b.get("apiEquivalentUsd"), 0.0) - or(a.get("apiEquivalentUsd"), 0.0));
        let accounts = sorted
            .iter()
            .map(|a| {
                let live = self.find_account(a.get("id"));
                let pr = match live {
                    Some(l) => self.price_of(l),
                    None => (!js::nullish(a.get("planUsdPerMonth"))).then(|| Price {
                        usd: n(a.get("planUsdPerMonth")),
                        source: get(Some(a), "planPriceSource").filter(|v| !v.is_null()).map_or_else(|| "config".to_string(), |s| text(Some(s))),
                        detail: String::new(),
                    }),
                };
                AccountValue {
                    provider: text(a.get("provider")),
                    name: label(a),
                    email: text(a.get("email")),
                    requests: fmt::locale(or(a.get("requests"), 0.0), 0, 3),
                    usd: usd_v(a.get("apiEquivalentUsd"), 2),
                    per_week: if js::nullish(a.get("usdPerAccountWeek")) { "pending".into() } else { usd_v(a.get("usdPerAccountWeek"), 0) },
                    plan_title: pr.as_ref().map_or_else(String::new, |p| source_text(&p.source)),
                    plan: pr.as_ref().map_or_else(|| "unknown".to_string(), |p| usd(p.usd, 0)),
                    multiple: if js::nullish(a.get("valueMultiple")) { "—".into() } else { format!("{}x", num_v(a.get("valueMultiple"), 1)) },
                }
            })
            .collect();
        let ends = arr(get(get(v, "waste"), "measured"))
            .iter()
            .filter(|w| truthy(w.get("windowsEnded")))
            .map(|w| {
                let a = self.find_account(w.get("id"));
                let list = arr(w.get("ends"));
                let unused = n(w.get("unusedUnits"));
                Ends {
                    provider: text(w.get("provider")),
                    name: a.map_or_else(|| text(w.get("id")), label),
                    title: list.iter().map(|e| format!("{}: {}%", self.day(e.get("at")), cat(e.get("lastUsedPercent")))).collect::<Vec<_>>().join(", "),
                    bars: list.iter().map(|e| (n(e.get("lastUsedPercent")), n(e.get("lastUsedPercent")) < 90.0)).collect(),
                    wasted: if unused != 0.0 && !unused.is_nan() { format!("{} wasted", num(unused, 2)) } else { "none".into() },
                }
            })
            .collect();
        let projected: Vec<String> = js::entries(get(get(v, "waste"), "projected"))
            .into_iter()
            .map(|(p, x)| format!("{} {} acct-weeks", self.pname(&p), num_v(x.get("wastedUnits"), 1)))
            .collect();
        let unpriced: Vec<String> = js::entries(get(get(v, "coverage"), "unpricedModels")).into_iter().map(|(k, _)| k).collect();
        let footnote = format!(
            "Measured from {} requests since {}. Dollars are what the same tokens would cost at API list prices, not money saved. \"$ / acct-week\" and value multiples need a full day of data.{} {} Report updated {}.",
            fmt::locale(or(get(get(v, "coverage"), "ledgerRows"), 0.0), 0, 3),
            self.dt(get(get(v, "coverage"), "ledgerSince")),
            if unpriced.is_empty() { String::new() } else { format!(" Unpriced models: {}.", unpriced.join(", ")) },
            self.ledger_text(),
            ago(get(v, "generatedAt"), self.now)
        );
        Ok(UsageView {
            kpis,
            fits,
            models,
            sessions,
            since,
            accounts,
            ends,
            projected: format!("Forecast, next 14 days: {}.", if projected.is_empty() { "unknown".to_string() } else { projected.join(", ") }),
            footnote,
        })
    }
}

// ---------------------------------------------------------------- Automation

pub struct Setting {
    pub key: String,
    pub value: String,
    pub bad: bool,
    /// A chip after the value (the last redemption's outcome).
    pub chip: Option<String>,
}

pub struct PolicyCard {
    pub provider: String,
    pub title: String,
    pub on: bool,
    pub settings: Vec<Setting>,
}

pub struct PolicyView {
    pub balance: String,
    pub cards: Vec<PolicyCard>,
    pub automation: &'static str,
    pub cadence: String,
}

impl<'a> Ctx<'a> {
    /// `policyHtml()`
    pub fn policy(&self) -> PolicyView {
        let pol = get(self.sv(), "resetPolicy");
        let automation = truthy(get(self.sv(), "resetAutomation"));
        let cards = ["claude", "codex"]
            .into_iter()
            .filter_map(|p| {
                let c = get(pol, p).filter(|c| truthy(Some(c)))?;
                let c = Some(c);
                let t = self.term(p);
                let on = automation && truthy(get(c, "enabled"));
                let reset = get(get(self.sv(), "resets"), p);
                let last = get(reset, "last").filter(|l| truthy(Some(l)));
                let headroom = n(get(c, "headroomPercent"));
                let lvl = if headroom > 0.0 { format!("{}% used or more", js::number_to_string(100.0 - headroom)) } else { "fully used".into() };
                let set = |key: &str, value: String| Setting { key: key.into(), value, bad: false, chip: None };
                let mut settings = Vec::new();
                if !on {
                    let why = if get(self.sv(), "resetAutomation") == Some(&Value::Bool(false)) {
                        "resetAutomation is false in state/pool-state.json".to_string()
                    } else {
                        format!("resetPolicy.{p}.enabled is false")
                    };
                    settings.push(set("Status", format!("Automatic {} banked {t}s: disabled ({why}).", self.pname(p))));
                }
                settings.push(set(
                    "What it does",
                    if t == "refill" { "Clears usage but keeps the account's weekly reset date." } else { "Restarts the account's weekly window from now." }.into(),
                ));
                settings.push(set(
                    "Fires when",
                    format!(
                        "Every {} account is {lvl} on {}",
                        self.pname(p),
                        if s_eq(get(c, "trigger"), "weekly") { "the all-models weekly limit" } else { "any limit" }
                    ),
                ));
                settings.push(set(
                    "Before a credit lapses",
                    if truthy(get(c, "redeemBeforeLapse")) {
                        "Redeem early on an account at the threshold when the forecast says the credit would expire unused".into()
                    } else {
                        "Off".into()
                    },
                ));
                if t == "refill" {
                    let r = get(c, "maxRunwayPercent").filter(|v| !v.is_null()).map_or_else(|| "50".to_string(), |v| cat(Some(v)));
                    settings.push(set(
                        "Runway rule",
                        format!("Used only on an account with time to use the refill up before its reset date at the lowest recent demand (never more than {r}% of the week; {r}% while demand is unknown), unless the credit expires before that date anyway"),
                    ));
                }
                let e = get(reset, "error");
                if truthy(e) {
                    settings.push(Setting { key: "Last attempt".into(), value: format!("Failed {}: {}", self.dt(get(e, "at")), cat(get(e, "message"))), bad: true, chip: None });
                }
                settings.push(match last {
                    None => set("Last used", "never".into()),
                    Some(l) => {
                        let who = self.find_account(l.get("accountId")).map_or_else(|| text(l.get("accountId")), label);
                        let trigger = if truthy(l.get("trigger")) { format!(" ({})", cat(l.get("trigger"))) } else { String::new() };
                        Setting {
                            key: "Last used".into(),
                            value: format!("{} on {who}{trigger}", self.dt(l.get("at"))),
                            bad: false,
                            chip: Some(text(l.get("outcome"))),
                        }
                    }
                });
                let problems = arr(get(c, "problems"));
                if !problems.is_empty() {
                    settings.push(Setting {
                        key: "Config problems".into(),
                        value: problems.iter().map(|x| text(Some(x))).collect::<Vec<_>>().join("; "),
                        bad: true,
                        chip: None,
                    });
                }
                Some(PolicyCard { provider: p.into(), title: format!("{} banked {t}s", self.pname(p)), on, settings })
            })
            .collect();
        let bal = get(get(self.sv(), "capacity"), "balance");
        let balance = if truthy(get(bal, "toward")) {
            format!("toward {}, step {} of {}", self.pname(&text(get(bal, "toward"))), cat(get(bal, "steps")), cat(get(bal, "maxSteps")))
        } else {
            "none".into()
        };
        PolicyView {
            balance,
            cards,
            automation: if automation { "on" } else { "off" },
            cadence: format!(
                "Usage every 2 minutes; plans twice a day; value report hourly{}.",
                if truthy(get(self.v, "generatedAt")) { format!(" (last {})", ago(get(self.v, "generatedAt"), self.now)) } else { String::new() }
            ),
        }
    }

    /// The header's freshness line (`renderAll`): text, and whether it is bad.
    pub fn freshness(&self) -> (String, bool) {
        let s = self.sv();
        let checked = get(s, "checkedAt");
        let age = if truthy(checked) { self.now - ts(checked) } else { f64::INFINITY };
        let error = get(s, "error").filter(|e| truthy(Some(e)));
        let bad = error.is_some() || age > 6.0 * 60_000.0;
        (error.map_or_else(|| format!("Checked {}", self.clock(checked)), |e| text(Some(e))), bad)
    }

    /// The overview tab's count: items that are not FYI.
    pub fn attention_count(&self, items: &[Item]) -> usize {
        items.iter().filter(|i| i.tone != Tone::Info).count()
    }
}
