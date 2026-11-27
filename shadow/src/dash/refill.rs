//! Owner-only review and confirmation of the specific refill the provider offers.
use corten::{axum::http::StatusCode, prelude::*};
use pool_core::refresh::manual::{Refill, Scope, offer};
use crate::{app::url, tailnet::Owner, watch::{Shared, now_ms}};
use crate::ui::button::{Button, ButtonSize, ButtonVariant};
use crate::ui::card::{Card, CardContent, CardHeader};
use super::views::{Dash, Header};

fn fail(status: StatusCode, message: &str) -> corten::Error {
    corten::Error::new(message.to_owned()).with_status(status)
}

fn writer(shadow: &Shared) -> corten::Result<&crate::writer::Copy> {
    let copy = shadow.copy.get().ok_or_else(|| fail(StatusCode::NOT_FOUND, "This dashboard is read-only"))?;
    if copy.closing() { return Err(fail(StatusCode::SERVICE_UNAVAILABLE, "Service stopping")); }
    Ok(copy)
}

pub async fn review(req: Req, State(shadow): State<Shared>, _owner: Caller<Owner>, Params(q): Params<url::Refill<'_>>) -> HtmlResult {
    let copy = writer(&shadow)?;
    let account = q.account.as_deref().unwrap_or_default();
    let pool = copy.io.memory.pool();
    let offered = offer(&pool, account, now_ms());
    let slot = &pool["resets"]["claude"];
    let last_manual = copy.trigger.last_refill(account);
    show(req, &shadow, account, html! {
        if let Some(note) = &last_manual {
            <p role="status">"Last manual request: "{note}</p>
        }
        if slot["pending"]["accountId"] == account {
            <p role="status">"A claim is pending for this account. Do not submit another."</p>
        } else if slot["last"]["accountId"] == account {
            <p>"Last claim: "{slot["last"]["outcome"].as_str().unwrap_or("unknown")}" at "{slot["last"]["at"].as_str().unwrap_or_default()}</p>
        }
        if let Some((a, scope, grant)) = offered {
            <p>"Account: "<strong>{a["email"].as_str().unwrap_or(account)}</strong></p>
            <p>{match scope { Scope::Full => "Full refill", Scope::Model => "Model-only refill" }}": "
                <strong>{grant["label"].as_str().filter(|l| !l.is_empty()).unwrap_or_else(|| grant["id"].as_str().unwrap_or_default())}</strong></p>
            <p>"This spends one grant now. The weekly renewal date does not change."</p>
            <form method="post" action={url::Refill { account: None }}>
                <input type="hidden" name="account" value={account}>
                <input type="hidden" name="grant" value={grant["id"].as_str().unwrap_or_default()}>
                <input type="hidden" name="scope" value={scope.key()}>
                <input type="hidden" name="observed" value={a["creditsUpdatedAt"].as_str().unwrap_or_default()}>
                <Button variant={ButtonVariant::Primary} size={ButtonSize::Md}><button type="submit">"Confirm and spend one refill"</button></Button>
            </form>
        } else {
            <p>"No refill can be claimed for this account right now. Check the bank after its next refresh."</p>
        }
    }).await
}

#[derive(serde::Deserialize)]
pub struct Confirmation {
    account: String,
    grant: String,
    scope: Scope,
    observed: String,
}

pub async fn submit(req: Req, State(shadow): State<Shared>, _owner: Caller<Owner>, Form(f): Form<Confirmation>) -> HtmlResult {
    let copy = writer(&shadow)?;
    let account = f.account;
    let request = Refill { account: account.clone(), grant: f.grant, scope: f.scope, observed: f.observed, request_id: uuid::Uuid::new_v4().to_string() };
    let result = if !request.confirmed_in(&copy.io.memory.pool(), now_ms()) {
        "Not submitted. This offer is no longer current. Review the account again.".into()
    } else {
        let rx = copy.trigger.refill(request).ok_or_else(|| fail(StatusCode::CONFLICT, "A refill is already queued"))?;
        match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => return Err(fail(StatusCode::SERVICE_UNAVAILABLE, "Service stopped. Review the account before trying again.")),
            Err(_) => "The refill was queued, not yet confirmed. Review this account after its next refresh; do not resubmit.".into(),
        }
    };
    let account = account.as_str();
    show(req, &shadow, account, html! {
        <p role="status">{result}</p>
        <p><a href={url::Refill { account: Some(account) }}>"Check refill status"</a></p>
    }).await
}

async fn show(req: Req, shadow: &Shared, account: &str, content: impl Render) -> HtmlResult {
    let d = super::load(shadow).await?;
    let c = d.ctx();
    let (fresh, bad) = c.freshness();
    let header = Header { count: c.attention_count(&c.attention_items()), fresh, bad, finish: d.signin.clone() };
    req.page(html! {
        <Dash req={&req} title="Claude refill" header={&header} writable={true} service={d.service}>
            <Card>
                <CardHeader><h2>"Claude refill"</h2></CardHeader>
                <CardContent>{content}<p><a href={url::Accounts { filter: None, open: Some(account) }}>"Back to account"</a></p></CardContent>
            </Card>
        </Dash>
    })
}
