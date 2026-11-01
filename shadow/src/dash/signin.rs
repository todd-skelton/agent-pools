//! The sign-in steps on a copy: dashboard.html's drawer (choose a provider, open its sign-in,
//! finish here or from another device) as a page and three forms, with no script of the app's own.
//!
//! What the live page keeps in `localStorage` (`pendingSignin`: the provider, when it started,
//! its accounts and the ones needing sign-in then) the copy keeps in memory, so the "Finish
//! sign-in" banner is on every open page, and its `waitForSignin` (a refresh now and every four
//! checks, up to 24 checks over three minutes) runs on the server, whether a page is open or not.
//! A sign-in is confirmed (`checkPendingSignin`) when the status shows a new account of that
//! provider or one that needed sign-in no longer does. On the copy the stand-in proxy answers
//! both calls and signs nothing in, so a sign-in there is submitted and never confirmed.
//!
//! Differences: the provider's page is a link (`target="_blank"`), not opened by script, which is
//! what the live page shows when the browser blocks its new tab; there is no "Paste from
//! clipboard" button and a pasted link is submitted with "Submit link", not as it is pasted.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use corten::axum::http::StatusCode;
use corten::prelude::*;
use serde_json::{Value, json};

use super::model::{Ctx, Tone};
use super::views::{Dash, Header};
use crate::app::url;
use crate::design::{DotSize, ProviderDot};
use crate::ui::button::{Button, ButtonSize, ButtonVariant};
use crate::ui::card::{Card, CardAction, CardContent, CardHeader};
use crate::ui::input::Input;
use crate::tailnet::Owner;
use crate::watch::{Shared, now_ms};

/// How long a sign-in stays pending (`pendingSignin`: 15 minutes).
const PENDING_MS: f64 = 15.0 * 60e3;
/// How long "Signed in: ..." stays on the banner (the live page toasts it).
const CONFIRMED_MS: f64 = 2.0 * 60e3;

/// `signinName(p)`
pub fn signin_name(p: &str) -> &'static str {
    if p == "codex" { "ChatGPT (Codex)" } else { "Claude" }
}

/// `callbackProvider`: the proxy's login id for a provider.
fn login_id(p: &str) -> Option<&'static str> {
    pool_core::providers::provider(p).map(|a| a.login_id)
}

/// A sign-in that was started and not yet confirmed or cancelled (`pendingSignin`).
#[derive(Clone, Debug)]
pub struct Pending {
    pub provider: String,
    pub started_at: f64,
    /// The provider's accounts when it started.
    pub accounts: Vec<String>,
    /// Its accounts that needed sign-in then.
    pub reauth: Vec<String>,
    /// The provider's sign-in page.
    pub url: Option<String>,
    /// What the steps say about it last (`setMsg`).
    pub note: Option<(Tone, String)>,
}

/// What the banner above every page shows (`renderFinish`, and the toast on confirmation).
#[derive(Clone, Debug, PartialEq)]
pub enum Banner {
    Pending(String),
    Signed(String),
}

/// The copy's sign-in, shared by its pages.
#[derive(Default)]
pub struct Signins {
    pending: Mutex<Option<Pending>>,
    confirmed: Mutex<Option<(f64, String)>>,
    waiting: AtomicBool,
}

impl Signins {
    /// The sign-in in progress, if it started under 15 minutes ago.
    pub fn pending(&self, now: f64) -> Option<Pending> {
        let mut p = self.pending.lock().unwrap();
        if p.as_ref().is_some_and(|p| now - p.started_at >= PENDING_MS) {
            *p = None;
        }
        p.clone()
    }

    fn start(&self, p: Pending) {
        *self.pending.lock().unwrap() = Some(p);
    }

    fn cancel(&self) {
        *self.pending.lock().unwrap() = None;
    }

    fn note(&self, provider: &str, note: (Tone, String)) {
        if let Some(p) = self.pending.lock().unwrap().as_mut().filter(|p| p.provider == provider) {
            p.note = Some(note);
        }
    }

    /// The banner: the sign-in to finish, or the one just confirmed.
    pub fn banner(&self, now: f64) -> Option<Banner> {
        if let Some(p) = self.pending(now) {
            return Some(Banner::Pending(p.provider));
        }
        let c = self.confirmed.lock().unwrap();
        c.as_ref().filter(|(at, _)| now - at < CONFIRMED_MS).map(|(_, m)| Banner::Signed(m.clone()))
    }

    /// `checkPendingSignin(s)`: done once one of the accounts that needed sign-in no longer
    /// does, or a new account of that provider appears. Returns what to say when it is.
    pub fn observe(&self, s: &Value, now: f64) -> Option<String> {
        let p = self.pending(now)?;
        let list: Vec<&Value> = pool_core::js::values(s.get("accounts")).into_iter().filter(|a| a.get("provider").and_then(Value::as_str) == Some(p.provider.as_str())).collect();
        let id_of = |a: &Value| a.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let added = list.iter().find(|a| !p.accounts.contains(&id_of(a)));
        let cleared: Vec<&String> = p.reauth.iter().filter(|id| !pool_core::js::truthy(s.get("reauth").and_then(|r| r.get(id.as_str())))).collect();
        if added.is_none() && cleared.is_empty() {
            return None;
        }
        let who_of = |a: &Value| {
            let email = a.get("email");
            if pool_core::js::truthy(email) { email.and_then(Value::as_str).unwrap_or_default().to_string() } else { id_of(a) }
        };
        let who = match added {
            Some(a) => who_of(a),
            None => cleared.iter().map(|id| list.iter().find(|a| &id_of(a) == *id).map_or_else(|| id.to_string(), |a| who_of(a))).collect::<Vec<_>>().join(", "),
        };
        let msg = format!("Signed in: {who} is connected{}.", if added.is_some() { "" } else { " again" });
        *self.pending.lock().unwrap() = None;
        *self.confirmed.lock().unwrap() = Some((now, msg.clone()));
        Some(msg)
    }
}

/// `waitForSignin(provider)`: a refresh now and every fourth check, a check after 3 s and then
/// every 7.5 s, 24 at most, while the sign-in is pending; then says it is not confirmed yet.
fn wait(shadow: Shared, provider: String) {
    if shadow.signin.waiting.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        for i in 0..24 {
            if shadow.signin.pending(now_ms()).is_none() {
                break;
            }
            if i % 4 == 0
                && let Some(t) = shadow.writer.get()
            {
                t.request();
            }
            tokio::time::sleep(std::time::Duration::from_millis(if i == 0 { 3000 } else { 7500 })).await;
            let _ = super::load(&shadow).await;
        }
        if shadow.signin.pending(now_ms()).is_some_and(|p| p.provider == provider) {
            let msg = format!("{} sign-in not confirmed yet. If you signed in on another device, paste the localhost link above; otherwise start again.", signin_name(&provider));
            shadow.signin.note(&provider, (Tone::Neutral, msg));
            shadow.changed_inputs();
        }
        shadow.signin.waiting.store(false, Ordering::SeqCst);
    });
}

/// The steps as drawn (`renderSignin`).
pub struct Steps {
    pub step: u8,
    pub provider: Option<String>,
    /// The account to sign in again: its id and email.
    pub account: Option<(String, String)>,
    pub url: Option<String>,
    pub note: Option<(Tone, String)>,
    /// Each provider, its name and how many accounts it has.
    pub choices: Vec<(String, String, usize)>,
}

fn steps_of(c: &Ctx, step: u8, provider: Option<String>, account: Option<&str>, url: Option<String>, note: Option<(Tone, String)>) -> Steps {
    let account = account.and_then(|id| c.all_accounts().iter().find(|a| a.get("id").and_then(Value::as_str) == Some(id))).map(|a| {
        let s = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        (s("id"), s("email"))
    });
    let choices = ["claude", "codex"].map(|p| (p.to_string(), c.pname(p), c.accounts(p).len())).to_vec();
    Steps { step, provider, account, url, note, choices }
}

fn known(p: &str) -> bool {
    matches!(p, "claude" | "codex")
}

#[derive(serde::Deserialize)]
pub struct OpenInput {
    provider: String,
    account: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct FinishInput {
    link: String,
    provider: Option<String>,
}

/// The copy, or 404 on the read-only shadow (sign-in changes the pool); 503 while it stops.
fn copy_of(shadow: &Shared) -> corten::Result<std::sync::Arc<crate::writer::Copy>> {
    let copy = shadow.copy.get().cloned().ok_or_else(|| corten::Error::new("This shadow only reads; sign in on the live dashboard").with_status(StatusCode::NOT_FOUND))?;
    if copy.closing() {
        return Err(corten::Error::new("Service stopping").with_status(StatusCode::SERVICE_UNAVAILABLE));
    }
    Ok(copy)
}

/// `GET /signin?provider=&account=` (`openSignin(provider, accountId)`): the provider's step
/// when one is named, else the pending sign-in's last step, else the choice of provider.
pub async fn page(req: Req, State(shadow): State<Shared>, Params(q): Params<url::Signin<'_>>) -> HtmlResult {
    copy_of(&shadow)?;
    let d = super::load(&shadow).await?;
    let c = d.ctx();
    let steps = match (q.provider.filter(|p| known(p)), shadow.signin.pending(d.now)) {
        (Some(p), _) => steps_of(&c, 2, Some(p), q.account.as_deref(), None, None),
        (None, Some(p)) => steps_of(&c, 3, Some(p.provider), None, p.url, p.note),
        (None, None) => steps_of(&c, 1, None, None, None, None),
    };
    draw(req, &shadow, &d, &steps)
}

/// `POST /signin` (`login()`): the stand-in's sign-in address for the provider, the sign-in
/// noted as pending, and the wait for it started.
pub async fn open(req: Req, State(shadow): State<Shared>, _owner: Caller<Owner>, Form(input): Form<OpenInput>) -> HtmlResult {
    let copy = copy_of(&shadow)?;
    let d = super::load(&shadow).await?;
    let c = d.ctx();
    let p = input.provider;
    let account = input.account.as_deref();
    let Some(login) = login_id(&p).filter(|_| known(&p)) else {
        return draw(req, &shadow, &d, &steps_of(&c, 1, None, None, None, Some((Tone::Bad, "Unknown provider".into()))));
    };
    let url = match copy.io.call(&format!("{login}-auth-url?is_webui=true"), None, None).await {
        Ok(r) => r.get("url").or_else(|| r.get("auth_url")).and_then(Value::as_str).map(str::to_string).ok_or_else(|| "Sign-in URL unavailable".to_string()),
        Err(e) => Err(e.message),
    };
    let url = match url {
        Ok(u) => u,
        Err(e) => return draw(req, &shadow, &d, &steps_of(&c, 2, Some(p), account, None, Some((Tone::Bad, e)))),
    };
    let ids = |list: Vec<&Value>| list.iter().filter_map(|a| a.get("id").and_then(Value::as_str).map(str::to_string)).collect::<Vec<_>>();
    let reauth = pool_core::js::entries(d.s.get("reauth"))
        .into_iter()
        .filter(|(_, v)| v.get("provider").and_then(Value::as_str) == Some(&p))
        .map(|(id, _)| id)
        .collect();
    let note = (Tone::Neutral, format!("Open the sign-in page and complete {} sign-in in the new tab.", signin_name(&p)));
    shadow.signin.start(Pending { provider: p.clone(), started_at: d.now, accounts: ids(c.accounts(&p)), reauth, url: Some(url.clone()), note: Some(note.clone()) });
    shadow.changed_inputs();
    wait(shadow.clone(), p.clone());
    draw(req, &shadow, &d, &steps_of(&c, 3, Some(p), account, Some(url), Some(note)))
}

/// `POST /signin/finish` (`finishSignin(link)`): the localhost address the provider ended on,
/// checked as the live page checks it and passed to the proxy's `oauth-callback`.
pub async fn finish(req: Req, State(shadow): State<Shared>, _owner: Caller<Owner>, Form(input): Form<FinishInput>) -> HtmlResult {
    let copy = copy_of(&shadow)?;
    let d = super::load(&shadow).await?;
    let c = d.ctx();
    let pending = shadow.signin.pending(d.now);
    let chosen = input.provider.filter(|p| known(p));
    let shown = chosen.clone().or_else(|| pending.as_ref().map(|p| p.provider.clone()));
    let url = pending.as_ref().and_then(|p| p.url.clone());
    let say = |tone: Tone, text: String, provider: Option<String>| steps_of(&c, 3, provider, None, url.clone(), Some((tone, text)));
    let link = match reqwest::Url::parse(input.link.trim()) {
        Ok(u) => u,
        Err(_) => return draw(req, &shadow, &d, &say(Tone::Bad, "That is not the sign-in link. Copy the full address of the localhost page the provider opened.".into(), shown)),
    };
    if !matches!(link.host_str(), Some("localhost" | "127.0.0.1")) {
        return draw(req, &shadow, &d, &say(Tone::Bad, "That is not the localhost sign-in link.".into(), shown));
    }
    let param = |k: &str| link.query_pairs().find(|(key, _)| key == k).map(|(_, v)| v.into_owned()).filter(|v| !v.is_empty());
    if let Some(error) = param("error") {
        let why = param("error_description").unwrap_or(error);
        return draw(req, &shadow, &d, &say(Tone::Bad, format!("The provider refused the sign-in: {why}. Start again."), shown));
    }
    if param("code").is_none() {
        return draw(req, &shadow, &d, &say(Tone::Bad, "That link has no sign-in code. Copy the full address of the localhost page the provider opened.".into(), shown));
    }
    let by_port = match link.port() {
        Some(54545) => Some("claude".to_string()),
        Some(1455) => Some("codex".to_string()),
        _ => None,
    };
    let Some(provider) = by_port.or_else(|| pending.as_ref().map(|p| p.provider.clone())).or(chosen) else {
        return draw(req, &shadow, &d, &say(Tone::Bad, "Choose a provider and open its sign-in first.".into(), None));
    };
    let name = signin_name(&provider);
    let body = json!({ "provider": login_id(&provider), "redirect_url": link.as_str() });
    if let Err(e) = copy.io.call("oauth-callback", Some(body), None).await {
        return draw(req, &shadow, &d, &say(Tone::Bad, e.message, Some(provider)));
    }
    let steps = if pending.as_ref().is_some_and(|p| p.provider == provider) {
        let note = (Tone::Neutral, format!("{name} sign-in submitted; waiting for the account to reconnect..."));
        shadow.signin.note(&provider, note.clone());
        wait(shadow.clone(), provider.clone());
        steps_of(&c, 3, Some(provider), None, url.clone(), Some(note))
    } else {
        say(Tone::Ok, format!("{name} sign-in submitted. The account shows up after the next refresh."), Some(provider))
    };
    draw(req, &shadow, &d, &steps)
}

/// `POST /signin/cancel` (`si-cancel`): the pending sign-in forgotten.
pub async fn cancel(req: Req, State(shadow): State<Shared>, _owner: Caller<Owner>) -> HtmlResult {
    copy_of(&shadow)?;
    shadow.signin.cancel();
    shadow.changed_inputs();
    let d = super::load(&shadow).await?;
    draw(req, &shadow, &d, &steps_of(&d.ctx(), 1, None, None, None, Some((Tone::Neutral, "Sign-in cancelled.".into()))))
}

fn draw(req: Req, _shadow: &Shared, d: &super::Data, steps: &Steps) -> HtmlResult {
    let c = d.ctx();
    let items = c.attention_items();
    let (fresh, bad) = c.freshness();
    // The banner is not shown over the steps (`renderFinish` hides it while the drawer is open).
    let header = Header { count: c.attention_count(&items), fresh, bad, finish: None };
    let title = super::views::titled(if steps.account.is_some() { "Sign in again · Agent pools" } else { "Connect an account · Agent pools" }, d.service);
    req.page(html! {
        <Dash req={&req} title={&title} header={&header} writable={true} service={d.service}>
            {view(steps)}
        </Dash>
    })
}

/// `renderSignin()`: the three steps and the message under them.
#[component(css)]
pub fn SigninSteps<'a>(s: &'a Steps) -> impl Render {
    let cls = |n: u8| (Step, (s.step == n).then_some(Active), (s.step > n).then_some(Done));
    let n = |k: u8| if s.step > k { "\u{2713}".to_string() } else { k.to_string() };
    let p = s.provider.as_deref();
    let name = signin_name(p.unwrap_or_default());
    let site = if p == Some("codex") { "ChatGPT" } else { "Claude" };
    let port = if p == Some("codex") { 1455 } else { 54545 };
    let hint = match &s.note {
        Some((Tone::Bad, _)) => Some(Bad),
        Some((Tone::Ok, _)) => Some(Good),
        _ => None,
    };
    html! {
        <div class={SigninSteps}>
            <Card>
                <CardHeader>
                    <h2>if s.account.is_some() { "Sign in again" } else { "Connect an account" }</h2>
                    <CardAction><Button variant={ButtonVariant::Ghost} size={ButtonSize::Sm}><a href={url::Overview}>"Close"</a></Button></CardAction>
                </CardHeader>
                <CardContent>
                    <ol class={Steps}>
                        <li class={cls(1)}>
                            <span class={N}>{n(1)}</span>
                            <div>
                                <h3>"Choose provider" if p.is_some() { " "<span class={Quiet}>{name}</span> }</h3>
                                <div class={Body}>
                                    <div class={Choice}>
                                        for (id, pname, count) in &s.choices {
                                            <a href={url::Signin { provider: Some(id), account: None }} aria-current={(p == Some(id.as_str())).then_some("true")}>
                                                <ProviderDot provider={id} size={DotSize::Sm} title={None} />" "{pname}<small>{count}" connected"</small>
                                            </a>
                                        }
                                    </div>
                                </div>
                            </div>
                        </li>
                        <li class={cls(2)}>
                            <span class={N}>{n(2)}</span>
                            <div>
                                <h3>"Sign in with "{name} if let Some((_, email)) = &s.account { " "<span class={Quiet}>"as "{email}</span> }</h3>
                                <div class={Body}>
                                    if let Some((_, email)) = &s.account { <p>"Use "<b class={Strong}>{email}</b>" on the provider page."</p> }
                                    <p>"Opens the provider's page in a new tab. Finish one sign-in before starting another."</p>
                                    match &s.url {
                                        Some(u) => <Button variant={ButtonVariant::Primary} size={ButtonSize::Md}><a target="_blank" rel="noopener" href={u.as_str()}>"Continue to "{site}" sign-in"</a></Button>,
                                        None => {
                                            <form method="post" action={url::Signin { provider: None, account: None }}>
                                                <input type="hidden" name="provider" value={p.unwrap_or_default()}>
                                                if let Some((id, _)) = &s.account { <input type="hidden" name="account" value={id}> }
                                                <Button variant={ButtonVariant::Primary} size={ButtonSize::Md}><button type="submit">"Open "{name}" sign-in"</button></Button>
                                            </form>
                                        }
                                    }
                                </div>
                            </div>
                        </li>
                        <li class={cls(3)}>
                            <span class={N}>{n(3)}</span>
                            <div>
                                <h3>"Finish"</h3>
                                <div class={Body}>
                                    if let Some(u) = &s.url { <p><Button variant={ButtonVariant::Outline} size={ButtonSize::Sm}><a target="_blank" rel="noopener" href={u.as_str()}>"Open the sign-in page"</a></Button></p> }
                                    <p>"On this PC the sign-in finishes by itself; keep this page open to see it confirmed."</p>
                                    <p>"On a phone or another device, the browser ends on a localhost page that will not load. That is expected: copy its address and paste it here."</p>
                                    <form method="post" action={url::SigninFinish}>
                                        if let Some(p) = p { <input type="hidden" name="provider" value={p}> }
                                        <Input><input name="link" autocomplete="off" aria-label="The localhost sign-in link" placeholder={format!("http://localhost:{port}/callback?code=...")}></Input>
                                        <div class={Acts}><Button variant={ButtonVariant::Outline} size={ButtonSize::Sm}><button type="submit">"Submit link"</button></Button></div>
                                    </form>
                                    <form method="post" action={url::SigninCancel} class={Acts}><Button variant={ButtonVariant::Outline} size={ButtonSize::Sm}><button type="submit">"Cancel sign-in"</button></Button></form>
                                </div>
                            </div>
                        </li>
                    </ol>
                    // The live drawer draws this as a fourth step with an empty marker; here it
                    // sits under the steps, so the line ends at Finish.
                    <p class={(Hint, hint)} role="status" aria-live="polite">
                        if let Some((_, text)) = &s.note { {text} }
                    </p>
                </CardContent>
            </Card>
        </div>
    }
}

pub fn view(s: &Steps) -> impl Render + '_ {
    html! { <SigninSteps s={s} /> }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(now: f64) -> Pending {
        Pending { provider: "codex".into(), started_at: now, accounts: vec!["a".into(), "b".into()], reauth: vec!["b".into()], url: None, note: None }
    }

    /// `checkPendingSignin`: a new account of the provider, or one that no longer needs sign-in,
    /// confirms it; another provider's new account does not; a sign-in 15 minutes old is gone.
    #[test]
    fn a_new_account_or_a_cleared_sign_in_confirms_the_pending_one() {
        let s = Signins::default();
        let still = json!({ "accounts": [{ "id": "a", "provider": "codex" }, { "id": "b", "provider": "codex", "email": "b@x" }, { "id": "c", "provider": "claude" }], "reauth": { "b": { "provider": "codex" } } });
        s.start(pending(0.0));
        assert_eq!(s.observe(&still, 1.0), None);
        assert_eq!(s.banner(1.0), Some(Banner::Pending("codex".into())));

        let cleared = json!({ "accounts": [{ "id": "a", "provider": "codex" }, { "id": "b", "provider": "codex", "email": "b@x" }], "reauth": {} });
        assert_eq!(s.observe(&cleared, 2.0).as_deref(), Some("Signed in: b@x is connected again."));
        assert_eq!(s.banner(3.0), Some(Banner::Signed("Signed in: b@x is connected again.".into())));
        assert_eq!(s.banner(2.0 + CONFIRMED_MS), None);

        s.start(pending(0.0));
        let added = json!({ "accounts": [{ "id": "a", "provider": "codex" }, { "id": "b", "provider": "codex" }, { "id": "d", "provider": "codex", "email": "" }], "reauth": { "b": {} } });
        assert_eq!(s.observe(&added, 2.0).as_deref(), Some("Signed in: d is connected."), "an empty email is the id");

        s.start(pending(0.0));
        assert!(s.pending(PENDING_MS - 1.0).is_some());
        assert!(s.pending(PENDING_MS).is_none());
        assert_eq!(s.observe(&cleared, PENDING_MS), None);
    }
}
