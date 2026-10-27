//! One refresh cycle (refresh.mjs `createRefresh`): read every account through its provider
//! adapter, rank and prioritize, record samples, raise alerts, run each provider's reset
//! lifecycle and save. All I/O goes through [`Io`], so a cycle runs against recorded provider
//! payloads and a fake proxy as easily as against the real one; nothing here opens a socket or
//! a file.
//!
//! State is the pool state as JSON (`pool-state.json`), changed in place with JavaScript's
//! property order, so a saved file reads as Agent Pools writes it.
//!
//! # Where the port knowingly differs
//!
//! 1. Accounts are read concurrently, as in Agent Pools, but the order in which their proxy
//!    calls interleave differs; each account's own calls are in the same order.
//! 2. A body that is not JSON fails as a `SyntaxError` would (not transient), with V8's message
//!    for an empty body and a shorter one otherwise.
//! 3. A shared price-table read that fails is retried by an account that was waiting on it; in
//!    Agent Pools the waiters share the failure. Plan reads run twice a day.
//! 4. A toast is awaited (its failure logged as `notify-error`), where Agent Pools fires it
//!    and moves on; an [`Io`] that shows toasts in the background keeps the cycle as fast.

pub mod account;
pub mod adapters;
pub mod lifecycle;
pub mod notify;
pub mod obj;

use crate::js;
use crate::providers::{self, PROVIDERS};
use crate::resets::ResetPolicy;
use crate::routing::{rank_accounts, Account, Policy};
use crate::summary::current_account;
use obj::{at, del, put, str_at};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::future::Future;

/// A failed call, as Agent Pools' errors carry it: the message, an HTTP status when there was
/// one, and the error's kind where it decides whether the read is retried.
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub message: String,
    pub status: Option<u16>,
    /// `error.name` when it matters (`TimeoutError`, `AbortError`).
    pub name: Option<String>,
    /// A body that was not JSON (`SyntaxError`): never transient.
    pub syntax: bool,
}

impl Failure {
    pub fn new(message: impl Into<String>) -> Failure {
        Failure { message: message.into(), status: None, name: None, syntax: false }
    }

    pub fn with_status(message: impl Into<String>, status: u16) -> Failure {
        Failure { status: Some(status), ..Failure::new(message) }
    }
}

/// What a cycle needs from the outside: the proxy's management API, the logs, persistence and
/// the clock, plus the capacity forecast's hooks (each `None` when not wired).
pub trait Io: Sync {
    /// Milliseconds since the epoch.
    fn now(&self) -> f64;
    /// `management(endpoint, body?, method?)`: the proxy's management API, which injects an
    /// account's token into an `api-call`.
    fn management(&self, endpoint: &str, body: Option<&Value>, method: Option<&str>) -> impl Future<Output = Result<Value, Failure>> + Send;
    /// The event log (`logs/pool-events.jsonl`).
    fn event(&self, kind: &str, details: Value) -> impl Future<Output = Result<(), Failure>> + Send;
    /// The usage-sample log (`logs/usage-samples.jsonl`).
    fn sample(&self, kind: &str, details: Value) -> impl Future<Output = Result<(), Failure>> + Send;
    /// Persist the pool state.
    fn save(&self, state: &Value) -> impl Future<Output = Result<(), Failure>> + Send;
    /// A desktop alert.
    fn toast(&self, title: &str, body: &str) -> impl Future<Output = Result<(), Failure>> + Send;
    /// The reset policies, re-read each cycle.
    fn load_policies(&self) -> impl Future<Output = Vec<ResetPolicy>> + Send;
    /// The account's credential file, or `None`; never fails.
    fn auth_file(&self, file: &Value) -> impl Future<Output = Option<Value>> + Send;
    /// The version of the provider's client that the proxy presents upstream.
    fn client_version(&self, provider: &str) -> impl Future<Output = Option<String>> + Send;
    /// The pause before a read is retried.
    fn sleep(&self, ms: f64) -> impl Future<Output = ()> + Send;
    /// A deadline that elapses in real time (a plan read is abandoned after it).
    fn timer(&self, ms: f64) -> impl Future<Output = ()> + Send;
    /// A fresh random UUID (a Claude claim's request id).
    fn random_uuid(&self) -> String;
    /// Minutes east of UTC at `ms`, for times written in alerts.
    fn local_offset_minutes(&self, ms: f64) -> f64;
    /// `forecast({ accounts, newSamples, resetPolicies, resetAutomation })`: the capacity
    /// forecast's document.
    fn forecast(&self, _input: ForecastInput<'_>) -> impl Future<Output = Option<Result<Option<Value>, Failure>>> + Send {
        async { None }
    }
    /// `creditPlan({ accounts, policy, now })`: accounts that may redeem before a lapse.
    fn credit_plan(&self, _accounts: &[Value], _policy: &CyclePolicy, _now: f64) -> Option<Result<Option<HashSet<String>>, Failure>> {
        None
    }
    /// `refillDemand({ policy, now })`: `{ unitsPerHour, weights }` for a refill provider.
    fn refill_demand(&self, _policy: &ResetPolicy, _now: f64) -> Option<Result<Option<Value>, Failure>> {
        None
    }
    /// `drainFirst({ provider, now })`: account id to credit expiry (ms) for planned holders.
    fn drain_first(&self, _provider: &str, _now: f64) -> Option<Result<Option<HashMap<String, f64>>, Failure>> {
        None
    }
}

/// What the forecast hook is given.
pub struct ForecastInput<'a> {
    pub accounts: &'a [Value],
    pub new_samples: &'a [Value],
    pub reset_policies: &'a [CyclePolicy],
    pub reset_automation: bool,
}

/// A reset policy as a cycle applies it: the published policy plus the forecast's refill
/// demand and the credit plan's drain-first holders.
#[derive(Clone, Debug)]
pub struct CyclePolicy {
    pub reset: ResetPolicy,
    pub refill_demand: Option<Value>,
    pub drain_first: Option<HashMap<String, f64>>,
}

impl CyclePolicy {
    /// The policy object refresh.mjs passes on (a drain-first `Map` writes as `{}`).
    pub fn to_value(&self) -> Value {
        let mut v = self.reset.to_value();
        if let Some(d) = &self.refill_demand {
            put(&mut v, "refillDemand", d.clone());
        }
        if self.drain_first.is_some() {
            put(&mut v, "drainFirst", json!({}));
        }
        v
    }

    /// As routing reads it.
    pub fn routing(&self) -> Policy {
        let mut p = Policy::from_value(&self.to_value());
        p.drain_first = self.drain_first.clone();
        p
    }
}

/// `createRefresh`'s options.
#[derive(Clone, Debug)]
pub struct Options {
    /// Read each account's plan and price (billing calls); off unless asked for.
    pub read_plans: bool,
    pub plan_ttl_ms: f64,
    pub plan_retry_ms: f64,
    pub memo_ttl_ms: f64,
    pub plan_timeout_ms: f64,
}

impl Default for Options {
    fn default() -> Options {
        Options { read_plans: false, plan_ttl_ms: 12.0 * 3_600_000.0, plan_retry_ms: 3_600_000.0, memo_ttl_ms: 12.0 * 3_600_000.0, plan_timeout_ms: 10_000.0 }
    }
}

/// What a cycle asks of the refresh loop: `degraded` when an account could not be verified
/// for a transient reason, `follow_up` when a reset outcome should be re-read soon.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub degraded: bool,
    pub follow_up: bool,
}

/// The refresh, with what it keeps between cycles: each account's last known usage (so a
/// failed read after a gap still shows the last rows) and shared reads such as a price table.
pub struct Refresh {
    pub options: Options,
    last_known: HashMap<String, Value>,
    memo: futures_util::lock::Mutex<HashMap<String, (f64, Value)>>,
}

fn history_key(a: &Value) -> String {
    format!("{}:{}", js::to_string(at(a, "provider")), js::to_string(at(a, "id")))
}

fn has_history(a: &Value) -> bool {
    js::truthy(at(a, "usageUpdatedAt")) && matches!(at(a, "quotaWindows"), Some(Value::Array(w)) if !w.is_empty())
}

/// One cycle's view of the I/O, lent to the per-account work.
pub struct Cycle<'a, I: Io> {
    pub io: &'a I,
    refresh: &'a Refresh,
}

impl<I: Io> Cycle<'_, I> {
    pub fn now(&self) -> f64 {
        self.io.now()
    }

    pub fn iso(&self) -> String {
        js::iso(self.io.now())
    }

    /// `read(file, url, data, extraHeaders)`: a provider call through the proxy, which
    /// replaces the token placeholder with the account's token.
    pub async fn read(&self, file: &Value, url: &str, data: Option<&Value>, extra: Map<String, Value>) -> Result<Value, Failure> {
        let mut header = Map::new();
        header.insert("Content-Type".into(), json!("application/json"));
        header.extend(adapters::headers(self.io, file).await);
        header.extend(extra);
        header.insert("Authorization".into(), json!("Bearer $TOKEN$"));
        let mut body = Map::new();
        if let Some(i) = at(file, "auth_index") {
            body.insert("auth_index".into(), i.clone());
        }
        body.insert("method".into(), json!(if data.is_some() { "POST" } else { "GET" }));
        body.insert("url".into(), json!(url));
        body.insert("header".into(), Value::Object(header));
        if let Some(d) = data {
            body.insert("data".into(), json!(js::stringify(d)));
        }
        let r = self.io.management("api-call", Some(&Value::Object(body)), None).await?;
        let status = js::to_number(at(&r, "status_code"));
        if status < 200.0 || status >= 300.0 {
            let shown = js::to_string(at(&r, "status_code"));
            let mut f = Failure::new(format!("Account provider returned HTTP {shown}"));
            f.status = (status.fract() == 0.0 && (0.0..=65535.0).contains(&status)).then_some(status as u16);
            return Err(f);
        }
        match at(&r, "body") {
            Some(Value::String(s)) => parse_body(s),
            Some(v) => Ok(v.clone()),
            None => Ok(Value::Null),
        }
    }

    /// `memo(key, fn)`: a read that does not depend on the account, once per `memoTtlMs`; a
    /// failure is not kept.
    pub async fn memo<F, Fut>(&self, key: &str, f: F) -> Result<Value, Failure>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value, Failure>>,
    {
        let mut cache = self.refresh.memo.lock().await;
        if let Some((at_ms, v)) = cache.get(key) {
            if self.now() - at_ms < self.refresh.options.memo_ttl_ms {
                return Ok(v.clone());
            }
        }
        let started = self.now();
        let v = f().await?;
        cache.insert(key.to_string(), (started, v.clone()));
        Ok(v)
    }

    /// The proxy's listing of this account (`auth-files`), or `None`.
    pub async fn proxy_snapshot(&self, a: &Value) -> Result<Option<Value>, Failure> {
        let listing = self.io.management("auth-files", None, None).await?;
        let Some(files) = at(&listing, "files") else {
            return Err(Failure::new("Cannot read properties of undefined (reading 'find')"));
        };
        Ok(js::values(Some(files))
            .into_iter()
            .find(|f| obj::strict_eq(at(f, "id"), at(a, "id")) && obj::strict_eq(at(f, "auth_index"), at(a, "auth_index")))
            .cloned())
    }
}

/// `JSON.parse(body)`, failing as a `SyntaxError`.
fn parse_body(s: &str) -> Result<Value, Failure> {
    serde_json::from_str(s).map_err(|_| {
        let message = match s.trim_start().chars().next() {
            None => "Unexpected end of JSON input".to_string(),
            Some(c) => format!("Unexpected token '{c}', \"{}\" is not valid JSON", if s.chars().count() > 30 { format!("{}...", s.chars().take(10).collect::<String>()) } else { s.to_string() }),
        };
        Failure { syntax: true, name: Some("SyntaxError".into()), ..Failure::new(message) }
    })
}

/// routing.mjs `liveDrainFirst(plan, accounts, now)`: the plan's drain-first entries that
/// still apply (the account still lists that credit, within a minute of the planned expiry).
pub fn live_drain_first(plan: Option<&HashMap<String, f64>>, accounts: &[&Value], now: f64) -> Option<HashMap<String, f64>> {
    let plan = plan.filter(|p| !p.is_empty())?;
    let mut out = HashMap::new();
    for a in accounts {
        let id = js::to_string(at(a, "id"));
        let Some(until) = plan.get(&id).copied().filter(|u| u.is_finite() && *u > now) else { continue };
        if js::truthy(at(a, "disabled")) || !js::truthy(at(a, "creditsKnown")) {
            continue;
        }
        let next = js::values(at(a, "credits")).into_iter().next().filter(|c| js::truthy(Some(c)));
        if let Some(next) = next {
            let exp = js::expiry_ms(js::coalesce([at(next, "expires_at"), at(next, "expiresAt")]));
            if (exp - until).abs() <= 60_000.0 {
                out.insert(id, until);
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

/// resets.mjs `migrateState(state)`: saved-state upgrades, applied once at startup and idempotent
/// (the old Codex-only reset fields into per-provider slots, `exhaustedSince` into `exhausted`).
pub fn migrate_state(state: &mut Value) {
    let s = obj::obj(state);
    if !matches!(s.get("resets"), Some(Value::Object(_) | Value::Array(_))) {
        s.insert("resets".into(), json!({}));
    }
    if s.contains_key("pendingReset") || s.contains_key("lastReset") {
        let pending = js::coalesce([s.get("pendingReset")]).cloned().unwrap_or(Value::Null);
        let last = js::coalesce([s.get("lastReset")]).cloned().unwrap_or(Value::Null);
        let resets = obj::obj(s.get_mut("resets").expect("set above"));
        if js::nullish(resets.get("codex")) {
            resets.insert("codex".into(), json!({ "pending": pending, "last": last }));
        }
        s.shift_remove("pendingReset");
        s.shift_remove("lastReset");
    }
    for p in PROVIDERS.iter() {
        let resets = obj::obj(s.get_mut("resets").expect("set above"));
        if js::nullish(resets.get(p.id)) {
            resets.insert(p.id.into(), json!({ "pending": null, "last": null }));
        }
        let slot = resets.get_mut(p.id).expect("set above");
        if js::nullish(at(slot, "refused")) {
            put(slot, "refused", json!({}));
        }
    }
    if !matches!(s.get("exhausted"), Some(Value::Object(_) | Value::Array(_))) {
        s.insert("exhausted".into(), json!({}));
    }
    if s.contains_key("exhaustedSince") {
        let since = s.get("exhaustedSince").cloned();
        if js::truthy(since.as_ref()) {
            let exhausted = obj::obj(s.get_mut("exhausted").expect("set above"));
            if js::nullish(exhausted.get("codex")) {
                exhausted.insert("codex".into(), since.unwrap_or(Value::Null));
            }
        }
        s.shift_remove("exhaustedSince");
    }
}

fn provider_name(id: &str) -> String {
    providers::provider(id).map_or(id.to_string(), |p| p.name.to_string())
}

impl Refresh {
    /// A refresh seeded with the saved state's history.
    pub fn new(options: Options, state: &Value) -> Refresh {
        let mut r = Refresh { options, last_known: HashMap::new(), memo: Default::default() };
        for a in js::values(at(state, "accounts")) {
            if has_history(a) {
                r.last_known.insert(history_key(a), a.clone());
            }
        }
        r
    }

    /// One cycle. Never fails: a failure is recorded on the state, which is saved.
    pub async fn refresh<I: Io>(&mut self, state: &mut Value, io: &I) -> Outcome {
        match self.cycle(state, io).await {
            Ok(outcome) => outcome,
            Err(e) => {
                put(state, "error", json!(e.message));
                if let Some(Value::Array(accounts)) = obj::obj(state).get_mut("accounts") {
                    accounts.iter_mut().for_each(account::invalidate);
                }
                let _ = io.save(state).await;
                let _ = io.event("refresh-error", json!({ "message": e.message })).await;
                Outcome { degraded: true, follow_up: false }
            }
        }
    }

    async fn toast<I: Io>(io: &I, title: &str, body: &str) {
        if let Err(e) = io.toast(title, body).await {
            let _ = io.event("notify-error", json!({ "message": e.message })).await;
        }
    }

    async fn cycle<I: Io>(&mut self, state: &mut Value, io: &I) -> Result<Outcome, Failure> {
        let listing = io.management("auth-files", None, None).await?;
        let files: Vec<Value> = match at(&listing, "files") {
            Some(Value::Array(fs)) => fs
                .iter()
                .filter(|f| str_at(f, "provider").is_some_and(|p| providers::provider(p).is_some()))
                .cloned()
                .collect(),
            Some(Value::Null) => return Err(Failure::new("Cannot read properties of null (reading 'filter')")),
            None => return Err(Failure::new("Cannot read properties of undefined (reading 'filter')")),
            Some(_) => return Err(Failure::new("(intermediate value).files.filter is not a function")),
        };
        let mut accounts = {
            let cycle = Cycle { io, refresh: self };
            let saved: &Value = state;
            futures_util::future::join_all(files.iter().map(|f| self.refresh_account(&cycle, saved, f))).await
        };
        let now = io.now();
        for a in accounts.iter_mut() {
            *a = current_account(a, now);
        }
        let mut boosts: HashMap<&str, Option<HashMap<String, f64>>> = HashMap::new();
        for p in PROVIDERS.iter() {
            let plan = match io.drain_first(p.id, now) {
                Some(Ok(plan)) => plan,
                Some(Err(e)) => {
                    io.event("drain-first-error", json!({ "provider": p.id, "message": e.message })).await?;
                    None
                }
                None => None,
            };
            let mine: Vec<&Value> = accounts.iter().filter(|a| str_at(a, "provider") == Some(p.id)).collect();
            boosts.insert(p.id, live_drain_first(plan.as_ref(), &mine, now));
        }
        for p in PROVIDERS.iter() {
            let idx: Vec<usize> = (0..accounts.len())
                .filter(|&i| str_at(&accounts[i], "provider") == Some(p.id) && !js::truthy(at(&accounts[i], "disabled")))
                .collect();
            let routing: Vec<Account> = idx.iter().map(|&i| Account::from_value(&accounts[i])).collect();
            let order: Vec<usize> = rank_accounts(&routing, boosts[p.id].as_ref())
                .into_iter()
                .map(|r| idx[routing.iter().position(|x| std::ptr::eq(x, r)).expect("ranked from this slice")])
                .collect();
            for (rank, i) in order.into_iter().enumerate() {
                let priority = 100 - rank as i64;
                if !obj::strict_eq(at(&accounts[i], "priority"), Some(&json!(priority))) {
                    let mut body = Map::new();
                    if let Some(n) = at(&accounts[i], "name") {
                        body.insert("name".into(), n.clone());
                    }
                    body.insert("priority".into(), json!(priority));
                    io.management("auth-files/fields", Some(&Value::Object(body)), Some("PATCH")).await?;
                    put(&mut accounts[i], "priority", json!(priority));
                }
            }
        }
        let previous: Vec<Value> = js::values(at(state, "accounts")).into_iter().cloned().collect();
        let samples = notify::usage_samples(&previous, &accounts, now);
        for s in &samples {
            io.sample("usage-sample", s.clone()).await?;
        }
        let since = js::date_parse_value(at(state, "checkedAt"));
        for x in lifecycle::external_redemptions(&previous, &accounts, at(state, "resets"), since, now) {
            io.event("reset-result", obj::spread(&x, vec![("outcome", Some(json!("reset"))), ("source", Some(json!("external")))])).await?;
        }
        for a in &accounts {
            if has_history(a) {
                self.last_known.insert(history_key(a), a.clone());
            }
        }
        put(state, "accounts", Value::Array(accounts.clone()));
        put(state, "checkedAt", json!(js::iso(now)));
        put(state, "error", Value::Null);
        let loaded = io.load_policies().await;
        let mut policies: Vec<CyclePolicy> = Vec::new();
        for p in PROVIDERS.iter() {
            let Some(reset) = loaded.iter().find(|r| r.provider == p.id).cloned() else { continue };
            let mut refill_demand = None;
            if reset.redemption == "refill" {
                match io.refill_demand(&reset, now) {
                    Some(Ok(d)) => refill_demand = d.filter(|d| js::truthy(Some(d))),
                    Some(Err(e)) => io.event("refill-demand-error", json!({ "provider": p.id, "message": e.message })).await?,
                    None => {}
                }
            }
            policies.push(CyclePolicy { reset, refill_demand, drain_first: boosts[p.id].clone() });
        }
        let policy_of = |id: &str| policies.iter().find(|p| p.reset.provider == id);
        for p in PROVIDERS.iter() {
            let expected = lifecycle::reset_expected(state, policy_of(p.id), &accounts, now);
            let Some(transition) = notify::exhaustion_transition(state, &accounts, p.id, expected, now) else { continue };
            let since = js::coalesce([js::get(at(state, "exhausted"), p.id)]).cloned().unwrap_or(Value::Null);
            io.event(transition, json!({ "provider": p.id, "exhaustedSince": since })).await?;
            if transition == "pool-exhausted" {
                let name = p.name;
                Self::toast(io, &format!("Agent Pools: {name} accounts exhausted"), &format!("Every {name} account is at its limit and no banked reset is about to be used. New {name} turns will fail until a window resets.")).await;
            }
        }
        for t in notify::reauth_transitions(state, &accounts) {
            match t {
                notify::Reauth::Cleared { account_id, provider, reason } => {
                    io.event("reauth-cleared", json!({ "accountId": account_id, "provider": provider, "reason": reason })).await?;
                }
                notify::Reauth::Required(a) => {
                    let e = at(&a, "refreshError");
                    io.event("reauth-required", json!({
                        "accountId": at(&a, "id"), "provider": at(&a, "provider"),
                        "since": js::get(e, "since"), "message": js::get(e, "message"),
                    })).await?;
                    let name = provider_name(str_at(&a, "provider").unwrap_or_default());
                    let who = if js::truthy(at(&a, "email")) { js::to_string(at(&a, "email")) } else { js::to_string(at(&a, "id")) };
                    Self::toast(io, &format!("Agent Pools: {name} sign-in expired"), &format!("{who}: {name} rejected the saved login. Sign in again with Connect account on the Agent Pools dashboard; the account stops serving requests when its current token lapses, if it has not already.")).await;
                }
            }
        }
        let mut follow_up = false;
        {
            let cycle = Cycle { io, refresh: self };
            let cx = Claims { cycle: &cycle, files: &files };
            for policy in &policies {
                let provider = policy.reset.provider.clone();
                match lifecycle::process_reset(state, policy, &cx).await {
                    Ok(outcome) => {
                        lifecycle::clear_error(state, &provider);
                        if outcome.is_some() {
                            follow_up = true;
                        }
                    }
                    Err(e) => {
                        follow_up = true;
                        lifecycle::record_error(state, &provider, &js::iso(io.now()), &e.message);
                        io.event("reset-error", json!({ "provider": provider, "message": e.message })).await?;
                    }
                }
            }
        }
        io.save(state).await?;
        let accounts: Vec<Value> = js::values(at(state, "accounts")).into_iter().cloned().collect();
        let input = ForecastInput {
            accounts: &accounts,
            new_samples: &samples,
            reset_policies: &policies,
            reset_automation: at(state, "resetAutomation") != Some(&Value::Bool(false)),
        };
        match io.forecast(input).await {
            Some(Ok(Some(doc))) => self.credit_risk_alerts(state, io, &doc, &accounts).await,
            Some(Err(e)) => {
                let _ = io.event("capacity-forecast-error", json!({ "message": e.message })).await;
            }
            _ => {}
        }
        let unverified: Vec<&Value> = accounts
            .iter()
            .filter(|a| !js::truthy(at(a, "disabled")) && (str_at(a, "status") == Some("unknown") || js::truthy(at(a, "usageStale"))))
            .collect();
        let degraded = unverified.iter().any(|a| at(a, "errorTransient") == Some(&Value::Bool(true)));
        if !unverified.is_empty() {
            let list: Vec<Value> = unverified
                .iter()
                .map(|a| {
                    let mut m = Map::new();
                    m.insert("id".into(), at(a, "id").cloned().unwrap_or(Value::Null));
                    m.insert("error".into(), js::coalesce([at(a, "error")]).cloned().unwrap_or(Value::Null));
                    m.insert("transient".into(), json!(at(a, "errorTransient") == Some(&Value::Bool(true))));
                    if js::truthy(at(a, "refreshError")) {
                        m.insert("refreshError".into(), js::get(at(a, "refreshError"), "code").cloned().unwrap_or(Value::Null));
                    }
                    Value::Object(m)
                })
                .collect();
            io.event("refresh-degraded", json!({ "degraded": degraded, "accounts": list })).await?;
        }
        Ok(Outcome { degraded, follow_up })
    }

    /// One account's read (`refreshAccount`). Never fails: every failure is on the account.
    async fn refresh_account<I: Io>(&self, cycle: &Cycle<'_, I>, state: &Value, file: &Value) -> Value {
        let io = cycle.io;
        let mut a = adapters::create_account(file);
        let saved = js::values(at(state, "accounts"))
            .into_iter()
            .find(|p| obj::strict_eq(at(p, "id"), at(&a, "id")) && obj::strict_eq(at(p, "provider"), at(&a, "provider")));
        let known = self.last_known.get(&history_key(&a));
        let previous_plan = [saved, known]
            .into_iter()
            .flatten()
            .find(|p| js::truthy(at(p, "plan")) || js::truthy(at(p, "planCheckedAt")))
            .cloned();
        if js::truthy(at(&a, "disabled")) {
            put(&mut a, "status", json!("disabled"));
            for k in ["plan", "planCheckedAt", "planError"] {
                if let Some(v) = previous_plan.as_ref().and_then(|p| at(p, k)).filter(|v| js::truthy(Some(v))) {
                    put(&mut a, k, v.clone());
                }
            }
            return a;
        }
        let previous = [saved, known].into_iter().flatten().find(|p| {
            js::truthy(at(p, "usageUpdatedAt"))
                && match at(p, "quotaWindows") {
                    Some(Value::Array(w)) => !w.is_empty(),
                    Some(Value::String(s)) => !s.is_empty(),
                    _ => false,
                }
        });
        let usage = account::refresh_usage(cycle, &mut a, previous, file).await;
        match account::reconcile_quota(cycle, &mut a).await {
            Ok(true) => {
                if let Err(e) = io.event("quota-recovered", json!({ "accountId": at(&a, "id"), "usageUpdatedAt": at(&a, "usageUpdatedAt") })).await {
                    self.recovery_error(io, &mut a, e).await;
                }
            }
            Ok(false) => {}
            Err(e) => self.recovery_error(io, &mut a, e).await,
        }
        if let Some(usage) = usage.filter(|u| js::truthy(Some(u))) {
            let bank = adapters::read_bank(cycle, file, &usage, io.now()).await;
            put(&mut a, "creditsKnown", json!(bank.known));
            put(&mut a, "credits", bank.credits);
            put(&mut a, "creditCount", bank.count);
            if let Some(note) = bank.note.filter(|n| !n.is_empty()) {
                put(&mut a, "creditNote", json!(note));
            }
            if bank.known {
                put(&mut a, "creditsUpdatedAt", json!(cycle.iso()));
            }
        }
        self.refresh_plan(cycle, &mut a, file, previous_plan.as_ref()).await;
        a
    }

    async fn recovery_error<I: Io>(&self, io: &I, a: &mut Value, e: Failure) {
        put(a, "routingStatus", json!("unknown"));
        put(a, "routingError", json!(e.message));
        let _ = io.event("quota-recovery-error", json!({ "accountId": at(a, "id"), "message": e.message })).await;
    }

    /// `refreshPlan(a, file, previous)`: the plan and price every `planTtlMs` (or
    /// `planRetryMs` after a failure), the last known plan kept in between and on failure.
    async fn refresh_plan<I: Io>(&self, cycle: &Cycle<'_, I>, a: &mut Value, file: &Value, previous: Option<&Value>) {
        for k in ["plan", "planCheckedAt", "planError"] {
            if let Some(v) = previous.and_then(|p| at(p, k)).filter(|v| js::truthy(Some(v))) {
                put(a, k, v.clone());
            }
        }
        let o = &self.options;
        if !o.read_plans {
            return;
        }
        let last = js::coalesce([at(a, "planCheckedAt")]).map_or(f64::NAN, |v| js::date_parse_value(Some(v)));
        let wait = if js::truthy(at(a, "planError")) { o.plan_retry_ms } else { o.plan_ttl_ms };
        if last.is_finite() && cycle.now() - last < wait {
            return;
        }
        put(a, "planCheckedAt", json!(cycle.iso()));
        let read = std::pin::pin!(adapters::read_plan(cycle, file));
        let timer = std::pin::pin!(cycle.io.timer(o.plan_timeout_ms));
        let result = match futures_util::future::select(read, timer).await {
            futures_util::future::Either::Left((r, _)) => r,
            futures_util::future::Either::Right(_) => Err(Failure::new("Plan read timed out")),
        };
        match result {
            Ok(plan) => {
                put(a, "plan", obj::spread(&plan, vec![("readAt", Some(json!(cycle.iso())))]));
                del(a, "planError");
            }
            Err(e) => {
                if str_at(a, "planError") != Some(e.message.as_str()) {
                    let _ = cycle.io.event("plan-read-error", json!({ "accountId": at(a, "id"), "provider": at(a, "provider"), "message": e.message })).await;
                }
                put(a, "planError", json!(e.message));
            }
        }
    }

    /// `creditRiskAlerts(doc, accounts)`: one event and toast per credit the plan still loses.
    async fn credit_risk_alerts<I: Io>(&self, state: &mut Value, io: &I, doc: &Value, accounts: &[Value]) {
        let now = io.now();
        let transitions = notify::credit_risk_transitions(state, doc, now);
        let any = !transitions.is_empty();
        for t in transitions {
            let result = match t {
                notify::CreditRisk::Cleared { provider, account_id, expires_at, reason } => {
                    io.event("credit-risk-cleared", json!({ "provider": provider, "accountId": account_id, "expiresAt": expires_at, "reason": reason })).await
                }
                notify::CreditRisk::AtRisk { provider, credit: c } => self.credit_at_risk(io, doc, accounts, &provider, &c).await,
            };
            if let Err(e) = result {
                let _ = io.event("credit-risk-error", json!({ "message": e.message })).await;
                return;
            }
        }
        if any {
            if let Err(e) = io.save(state).await {
                let _ = io.event("credit-risk-error", json!({ "message": e.message })).await;
            }
        }
    }

    async fn credit_at_risk<I: Io>(&self, io: &I, doc: &Value, accounts: &[Value], provider: &str, c: &Value) -> Result<(), Failure> {
        let adapter = providers::provider(provider);
        let f = js::get(at(doc, "providers"), provider);
        let term = if adapter.is_some_and(|a| a.redemption == providers::Redemption::Refill) { "refill" } else { "reset" };
        let a = accounts.iter().find(|x| obj::strict_eq(at(x, "id"), at(c, "accountId")));
        let demand = js::get(js::get(f, "demand"), "unitsPerHour");
        let pace = notify::per_week(js::number(demand));
        let need = notify::per_week(js::number(at(c, "needUnitsPerHour")));
        let mut details = Map::new();
        details.insert("provider".into(), json!(provider));
        for k in ["accountId", "expiresAt", "action", "drainBy", "unitsLeft"] {
            if let Some(v) = at(c, k) {
                details.insert(k.into(), v.clone());
            }
        }
        details.insert("demandUnitsPerHour".into(), js::coalesce([demand]).cloned().unwrap_or(Value::Null));
        if let Some(v) = at(c, "needUnitsPerHour") {
            details.insert("needUnitsPerHour".into(), v.clone());
        }
        io.event("credit-at-risk", Value::Object(details)).await?;
        let expires = js::date_parse_value(at(c, "expiresAt"));
        let when = if expires.is_finite() { notify::local_when(expires, io.local_offset_minutes(expires)) } else { "Invalid Date".into() };
        let name = adapter.map_or(provider.to_string(), |a| a.name.to_string());
        let who = match a.and_then(|a| at(a, "email")).filter(|e| js::truthy(Some(e))) {
            Some(e) => js::to_string(Some(e)),
            None => js::to_string(at(c, "accountId")),
        };
        let pace = pace.unwrap_or_else(|| "null".into());
        let tail = match need {
            Some(n) => format!("It needs about {n} acct-weeks/week until then."),
            None => "No steady pace saves it.".into(),
        };
        Self::toast(io, &format!("Agent Pools: {name} {term} at risk"), &format!("{who}: its {term} expires {when} and lapses unused at the current pace ({pace} acct-weeks/week). {tail}")).await;
        Ok(())
    }
}

/// The claim side of the lifecycle: redemption through the account's own auth file.
struct Claims<'a, 'b, I: Io> {
    cycle: &'a Cycle<'b, I>,
    files: &'a [Value],
}

impl<I: Io> lifecycle::Lifecycle for Claims<'_, '_, I> {
    type Io = I;

    fn io(&self) -> &I {
        self.cycle.io
    }

    fn plan(&self, accounts: &[Value], policy: &CyclePolicy, now: f64) -> Option<Result<Option<HashSet<String>>, Failure>> {
        self.cycle.io.credit_plan(accounts, policy, now)
    }

    fn consume(&self, account: &Value, pending: &Value) -> impl Future<Output = Result<Value, Failure>> + Send {
        let file = self
            .files
            .iter()
            .find(|f| obj::strict_eq(at(f, "id"), at(account, "id")) && !js::truthy(at(f, "disabled")))
            .cloned();
        let pending = pending.clone();
        async move {
            let Some(file) = file else {
                return Err(Failure::new("Reset account unavailable; original identity retained"));
            };
            adapters::redeem(self.cycle, &file, &pending).await
        }
    }
}
