//! pool-service.mjs's proxy watch on the copy: `checkProxy` every 10 s (healthy when the
//! stand-in's `auth-files` answers with a `files` list), then `readHolds` (the proxy's provider
//! outage hold counters, `outage-holds`), whose transitions (notifications.mjs `holdTransition`)
//! are logged and toasted. A first healthy answer starts a refresh when the writer's own start did
//! not find the proxy (`bootstrapped`). Restarting the proxy is not ported: the copy's proxy is
//! the stand-in, which this process owns.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pool_core::js;
use pool_core::refresh::Io;
use serde_json::{Value, json};

use super::Trigger;
use super::io::FileIo;

pub const EVERY: Duration = Duration::from_secs(10);

/// What `/api/health` and `/api/status` read, and the `holdSince` the refresh cycle saves.
pub struct Health {
    pub proxy_healthy: AtomicBool,
    /// The last `holds` reading, `None` until the proxy answers (or when it stops).
    pub holds: Mutex<Option<Value>>,
    /// `state.holdSince`: set when a hold starts, removed when it clears. Shared with the
    /// refresh cycle, which writes it into the state it saves.
    pub hold_since: Mutex<Option<String>>,
    /// The writer's first cycle has run (or been asked for).
    pub bootstrapped: AtomicBool,
}

impl Health {
    /// Starting from the saved state's `holdSince`.
    pub fn new(state: &Value) -> Health {
        Health {
            proxy_healthy: AtomicBool::new(false),
            holds: Mutex::new(None),
            hold_since: Mutex::new(state.get("holdSince").and_then(Value::as_str).map(str::to_string)),
            bootstrapped: AtomicBool::new(false),
        }
    }

    pub fn healthy(&self) -> bool {
        self.proxy_healthy.load(Ordering::SeqCst)
    }

    pub fn holds(&self) -> Option<Value> {
        self.holds.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Writes `holdSince` into `state` as the shared state would hold it.
    pub fn apply(&self, state: &mut Value) {
        let since = self.hold_since.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Value::Object(m) = state {
            match since {
                Some(s) => {
                    m.insert("holdSince".into(), json!(s));
                }
                None => {
                    m.shift_remove("holdSince");
                }
            }
        }
    }
}

/// notifications.mjs `holdTransition(state, holds, now)`: a hold starts when requests are held
/// and none was, and clears when none are held and one was.
pub fn hold_transition(hold_since: &mut Option<String>, holds: Option<&Value>, now: f64) -> Option<&'static str> {
    let active = holds?.get("active")?.as_f64()?;
    if active > 0.0 && hold_since.as_deref().is_none_or(str::is_empty) {
        *hold_since = Some(js::iso(now));
        return Some("outage-hold-started");
    }
    if active == 0.0 && hold_since.as_deref().is_some_and(|s| !s.is_empty()) {
        *hold_since = None;
        return Some("outage-hold-cleared");
    }
    None
}

/// `Number(v) || 0`.
fn count(v: Option<&Value>) -> f64 {
    let n = js::to_number(v);
    if n.is_nan() { 0.0 } else { n }
}

/// `readHolds()`: the counters, or `None` when the proxy does not give them.
fn reading(v: &Value, now: f64) -> Value {
    json!({
        "enabled": js::truthy(v.get("enabled")),
        "active": js::number_value(count(v.get("active"))),
        "turns": js::number_value(count(v.get("turns"))),
        "checkedAt": js::iso(now),
    })
}

/// One `checkProxy()`.
pub async fn check(io: &FileIo, health: &Health, trigger: &Trigger) {
    let healthy = io.call("auth-files", None, None).await.is_ok_and(|v| v.get("files").is_some_and(Value::is_array));
    health.proxy_healthy.store(healthy, Ordering::SeqCst);
    if !healthy {
        *health.holds.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return;
    }
    let now = super::io::now_ms();
    let holds = io.call("outage-holds", None, None).await.ok().map(|v| reading(&v, now));
    *health.holds.lock().unwrap_or_else(|e| e.into_inner()) = holds.clone();
    if let Some(holds) = &holds {
        let (transition, since) = {
            let mut since = health.hold_since.lock().unwrap_or_else(|e| e.into_inner());
            (hold_transition(&mut since, Some(holds), now), since.clone())
        };
        if let Some(kind) = transition {
            let _ = io.event(kind, json!({ "active": holds["active"], "turns": holds["turns"], "holdSince": since })).await;
            if kind == "outage-hold-started" {
                let body = format!(
                    "{} request(s) are waiting on the proxy for a provider to recover. Turns stay alive up to the hold budget; do not redeploy the proxy until this clears.",
                    js::to_string(holds.get("active"))
                );
                let _ = io.toast("Agent Pools: provider outage, requests held", &body).await;
            }
        }
    }
    // Saved status is unverified after a start: pull as soon as the proxy answers.
    if !health.bootstrapped.swap(true, Ordering::SeqCst) {
        trigger.request();
    }
}

/// `setInterval(checkProxy, 10000)` until the app stops.
pub async fn run(io: Arc<FileIo>, health: Arc<Health>, trigger: Arc<Trigger>, every: Duration, signal: corten::Signal) {
    loop {
        check(&io, &health, &trigger).await;
        tokio::select! {
            _ = tokio::time::sleep(every) => {}
            _ = signal.aborted() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hold_starts_once_and_clears_once() {
        let mut since = None;
        let held = json!({ "active": 2, "turns": 5 });
        assert_eq!(hold_transition(&mut since, Some(&held), 1_000.0), Some("outage-hold-started"));
        assert_eq!(since.as_deref(), Some("1970-01-01T00:00:01.000Z"));
        assert_eq!(hold_transition(&mut since, Some(&held), 2_000.0), None, "still held: no second start");
        let clear = json!({ "active": 0 });
        assert_eq!(hold_transition(&mut since, Some(&clear), 3_000.0), Some("outage-hold-cleared"));
        assert_eq!(since, None);
        assert_eq!(hold_transition(&mut since, Some(&clear), 4_000.0), None);
        // No reading, or a count that is not a number: nothing changes.
        assert_eq!(hold_transition(&mut since, None, 5_000.0), None);
        assert_eq!(hold_transition(&mut since, Some(&json!({ "active": "2" })), 5_000.0), None);
    }

    #[test]
    fn a_reading_counts_loosely() {
        let r = reading(&json!({ "enabled": 1, "active": "3", "turns": "x" }), 0.0);
        assert_eq!(r, json!({ "enabled": true, "active": 3.0, "turns": 0.0, "checkedAt": "1970-01-01T00:00:00.000Z" }));
    }
}
