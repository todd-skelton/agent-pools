//! A confirmation is for one observed Claude grant, never for the next available credit.
use crate::{js, routing::credit_key, summary::is_fresh};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Scope { Full, Model }

impl Scope {
    pub fn key(self) -> &'static str {
        match self { Self::Full => "full", Self::Model => "model" }
    }
}

pub struct Refill {
    pub account: String,
    pub grant: String,
    pub scope: Scope,
    pub observed: String,
    pub request_id: String,
}

/// Only the provider's claimable next grant is present in these lists, not its whole inventory.
pub fn offer<'a>(state: &'a Value, account: &str, now: f64) -> Option<(&'a Value, Scope, &'a Value)> {
    let a = state["accounts"].as_array()?.iter().find(|a| a["provider"] == "claude" && a["id"] == account)?;
    if a["disabled"] == true || a["creditsKnown"] != true || a["usageStale"] == true
        || !is_fresh(a.get("creditsUpdatedAt"), now) || !is_fresh(a.get("usageUpdatedAt"), now) {
        return None;
    }
    let slot = &state["resets"]["claude"];
    if !slot["pending"].is_null() { return None; }
    // Following a claim the saved account can still hold its pre-claim bank until the next read.
    if slot["last"]["accountId"] == account && js::date_parse_value(slot["last"].get("at")) >= js::date_parse_value(a.get("creditsUpdatedAt")) {
        return None;
    }
    let refused = super::lifecycle::active_refusals(Some(slot), now);
    for (key, scope) in [("credits", Scope::Full), ("modelCredits", Scope::Model)] {
        if let Some(g) = a[key].as_array().and_then(|l| l.first()) {
            if g["id"].as_str().is_some_and(|id| !id.is_empty()) && js::expiry_ms(g.get("expires_at")) > now
                && !refused.contains_key(&credit_key(account, g.get("id"))) {
                return Some((a, scope, g));
            }
        }
    }
    None
}

impl Refill {
    pub fn confirmed_in(&self, state: &Value, now: f64) -> bool {
        offer(state, &self.account, now).is_some_and(|(a, s, g)|
            a["creditsUpdatedAt"] == self.observed && s == self.scope && g["id"] == self.grant)
    }
    pub fn offered_in(&self, state: &Value, now: f64) -> bool {
        offer(state, &self.account, now).is_some_and(|(_, s, g)| s == self.scope && g["id"] == self.grant)
    }
    pub fn result(&self, state: &Value) -> String {
        let slot = &state["resets"]["claude"];
        if slot["last"]["requestId"] == self.request_id {
            return match slot["last"]["outcome"].as_str() {
                Some("reset") => "Provider confirmed the refill.",
                Some("already_redeemed") => "The provider says this refill was already used.",
                Some("deferred") => "The provider deferred the refill. Wait before trying again.",
                Some("nothing_to_reset") => "The provider found no usage to reset.",
                Some("no_credit") => "The provider no longer offers this refill.",
                _ => "The refill was refused or cancelled.",
            }.into();
        }
        if slot["pending"]["requestId"] == self.request_id {
            return "Outcome not confirmed. The original request is journaled for reconciliation; do not submit another.".into();
        }
        "Not submitted. The account, grant, or bank reading changed or could not be verified. Review the latest offer.".into()
    }
}
