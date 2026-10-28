//! What the JSON routes and the dashboard are built from ([`Inputs`]), and the answers built
//! from it, kept until something they depend on changes ([`Answers`]).
//!
//! On a copy (`run-copy`) the inputs are the write side's memory, as pool-service.mjs answers
//! from its `state` and `capacity.current()`: the pool state as last saved, the forecast and
//! ladders as last published, and the value report as last written. Nothing is read from disk
//! to answer a request. On the read-only shadow they are the live install's files, parsed once
//! per save ([`crate::sources::Install::doc`]).
//!
//! `/api/status` and `/api/summary` depend on the moment only through each account's three
//! readings ([`summary::READINGS`]): whether each is fresh. An answer is kept while the inputs
//! are the same documents and every reading is as fresh as when it was built, so it is built
//! once per save, publication, ledger drain or proxy check, and when a reading turns stale, not
//! once per request. `/api/summary`'s `generatedAt` is the request's moment, set into the kept
//! bytes.

use std::sync::{Arc, Mutex, OnceLock};

use corten::axum::body::Bytes;
use pool_core::{js, resets::ResetPolicy, status, summary};
use serde_json::{Value, json};

/// The inputs of `/api/status`, `/api/summary` and the dashboard.
#[derive(Clone, Debug)]
pub struct Inputs {
    /// The pool state as saved (accounts as saved, not yet judged by the moment).
    pub pool: Arc<Value>,
    /// The forecast last published, when it is one (`capacity-forecast/v1`).
    pub forecast: Option<Arc<Value>>,
    /// The usage value report last written.
    pub value: Option<Arc<Value>>,
    /// install.json `expectedAccounts`.
    pub expected: Option<Value>,
    pub policies: Vec<ResetPolicy>,
    /// What only the running service knows (`processes`, `holds`, `usageLedger`).
    pub live: status::Live,
}

fn same_doc(a: &Option<Arc<Value>>, b: &Option<Arc<Value>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

impl Inputs {
    /// The same documents (by identity: a save or publication is a new one) and equal settings
    /// and memory.
    fn same(&self, other: &Inputs) -> bool {
        Arc::ptr_eq(&self.pool, &other.pool)
            && same_doc(&self.forecast, &other.forecast)
            && same_doc(&self.value, &other.value)
            && self.expected == other.expected
            && self.policies == other.policies
            && self.live == other.live
    }
}

/// Each account's readings, in milliseconds, in [`summary::READINGS`] order.
fn readings(pool: &Value) -> Vec<f64> {
    js::values(pool.get("accounts"))
        .into_iter()
        .flat_map(|a| summary::READINGS.map(|k| js::date_parse_value(js::get(Some(a), k))))
        .collect()
}

fn freshness(readings: &[f64], now: f64) -> Vec<bool> {
    readings.iter().map(|&at| summary::fresh_at(at, now)).collect()
}

/// `/api/summary` as bytes on either side of its `generatedAt`.
struct Summary {
    before: Bytes,
    after: Bytes,
}

/// The answers for one set of inputs and one freshness of the accounts' readings.
pub struct Built {
    pub inputs: Inputs,
    readings: Vec<f64>,
    fresh: Vec<bool>,
    status: OnceLock<(Arc<Value>, Bytes)>,
    summary: OnceLock<Summary>,
}

impl Built {
    fn new(inputs: Inputs, now: f64) -> Built {
        let readings = readings(&inputs.pool);
        let fresh = freshness(&readings, now);
        Built { inputs, readings, fresh, status: OnceLock::new(), summary: OnceLock::new() }
    }

    fn holds(&self, inputs: &Inputs, now: f64) -> bool {
        self.inputs.same(inputs) && self.readings.iter().zip(&self.fresh).all(|(&at, &f)| summary::fresh_at(at, now) == f)
    }

    fn status_pair(&self, now: f64) -> &(Arc<Value>, Bytes) {
        self.status.get_or_init(|| {
            let i = &self.inputs;
            let doc = status::build_status(&i.pool, i.expected.as_ref(), &i.policies, i.forecast.as_deref(), &i.live, now);
            let bytes = Bytes::from(js::stringify(&doc));
            (Arc::new(doc), bytes)
        })
    }

    /// `/api/status` (pool-service.mjs), built at a moment as fresh as `now`.
    pub fn status(&self, now: f64) -> Arc<Value> {
        self.status_pair(now).0.clone()
    }

    /// The same, as `JSON.stringify` writes it.
    pub fn status_bytes(&self, now: f64) -> Bytes {
        self.status_pair(now).1.clone()
    }

    /// `/api/summary` at `now`, as `JSON.stringify` writes it.
    pub fn summary_bytes(&self, now: f64) -> Bytes {
        let s = self.summary.get_or_init(|| {
            let i = &self.inputs;
            let doc = summary::build_summary(
                &crate::parity::current_state(&i.pool, now),
                &crate::parity::provider_names(),
                i.forecast.as_deref(),
                i.value.as_deref(),
                now,
            );
            let text = js::stringify(&doc);
            let at = js::stringify(&json!(js::iso(now)));
            // The document opens with `format` then `generatedAt` (pool-summary.mjs's order).
            let head = format!("{{\"format\":{},\"generatedAt\":", js::stringify(&json!(summary::SUMMARY_FORMAT)));
            assert!(text.starts_with(&head) && text[head.len()..].starts_with(&at), "the summary opens with format and generatedAt");
            let cut = head.len() + at.len();
            Summary { before: Bytes::from(head), after: Bytes::copy_from_slice(&text.as_bytes()[cut..]) }
        });
        let at = js::stringify(&json!(js::iso(now)));
        let mut out = Vec::with_capacity(s.before.len() + at.len() + s.after.len());
        out.extend_from_slice(&s.before);
        out.extend_from_slice(at.as_bytes());
        out.extend_from_slice(&s.after);
        Bytes::from(out)
    }
}

/// The answers last built, kept while they still hold.
#[derive(Default)]
pub struct Answers {
    last: Mutex<Option<Arc<Built>>>,
}

impl Answers {
    /// The answers for `inputs` at `now`: the kept ones when they hold, else new ones (built as
    /// they are asked for).
    pub fn at(&self, inputs: Inputs, now: f64) -> Arc<Built> {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(b) = last.as_ref().filter(|b| b.holds(&inputs, now)) {
            return b.clone();
        }
        let b = Arc::new(Built::new(inputs, now));
        *last = Some(b.clone());
        b
    }
}

/// A document's `JSON.stringify` bytes, kept while it is the same document.
#[derive(Default)]
pub struct Serialized {
    last: Mutex<Option<(Arc<Value>, Bytes)>>,
}

impl Serialized {
    pub fn bytes(&self, doc: &Arc<Value>) -> Bytes {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((d, b)) = last.as_ref().filter(|(d, _)| Arc::ptr_eq(d, doc)) {
            let _ = d;
            return b.clone();
        }
        let b = Bytes::from(js::stringify(doc));
        *last = Some((doc.clone(), b.clone()));
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: f64 = 60_000.0;

    fn inputs(pool: Value) -> Inputs {
        Inputs { pool: Arc::new(pool), forecast: None, value: None, expected: None, policies: Vec::new(), live: Default::default() }
    }

    #[test]
    fn answers_are_kept_until_a_reading_turns_stale_or_an_input_changes() {
        let t0 = 1_800_000_000_000.0;
        let pool = json!({ "checkedAt": js::iso(t0), "accounts": [
            { "id": "a", "provider": "codex", "status": "ok", "usageUpdatedAt": js::iso(t0 - MIN), "routingCheckedAt": js::iso(t0 - 3.0 * MIN), "creditsUpdatedAt": js::iso(t0) }
        ] });
        let i = inputs(pool);
        let answers = Answers::default();
        let first = answers.at(i.clone(), t0);
        let body = first.status_bytes(t0);
        // A minute later every reading is as fresh: the same answers, not built again.
        let again = answers.at(i.clone(), t0 + MIN);
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(again.status_bytes(t0 + MIN), body);
        // Built again at that moment, it is the same document.
        let rebuilt = status::build_status(&i.pool, None, &[], None, &Default::default(), t0 + MIN);
        assert_eq!(body, Bytes::from(js::stringify(&rebuilt)));
        // Past five minutes the routing reading is stale: built again, and it says so.
        let later = t0 + 2.0 * MIN + 1.0;
        let stale = answers.at(i.clone(), later);
        assert!(!Arc::ptr_eq(&first, &stale));
        assert_eq!(stale.status(later)["accounts"][0]["routingStatus"], "unknown");
        assert_eq!(first.status(t0)["accounts"][0].get("routingStatus"), None);
        // A new save is a new document, even when it is equal.
        let saved = Inputs { pool: Arc::new((*i.pool).clone()), ..i.clone() };
        assert!(!Arc::ptr_eq(&answers.at(saved, later), &stale));
        // So is a change in the service's memory.
        let held = Inputs { live: status::Live { holds: Some(json!({ "active": 1 })), ..Default::default() }, ..i };
        let moved = answers.at(held, later);
        assert_eq!(moved.status(later)["holds"]["active"], 1);
    }

    #[test]
    fn the_summary_is_kept_with_the_requests_moment_set_in() {
        let t0 = 1_800_000_000_000.0;
        let pool = json!({ "checkedAt": js::iso(t0), "accounts": [
            { "id": "a", "provider": "codex", "plan": "pro", "usageUpdatedAt": js::iso(t0) }
        ] });
        let answers = Answers::default();
        let i = inputs(pool);
        for at in [t0, t0 + 1234.0, t0 + 4.0 * MIN] {
            let body = answers.at(i.clone(), at).summary_bytes(at);
            let built = summary::build_summary(&crate::parity::current_state(&i.pool, at), &crate::parity::provider_names(), None, None, at);
            assert_eq!(body, Bytes::from(js::stringify(&built)), "at {at}");
        }
    }
}
