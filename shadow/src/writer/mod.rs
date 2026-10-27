//! The pool's write side on Corten, run only against a copy of the install: the refresh loop
//! (refresh-loop.mjs) driving `pool_core::refresh` with file and proxy I/O ([`io`]), the
//! capacity publisher ([`publisher`]), the desktop relay ([`desktop`]), and a stand-in proxy
//! ([`fake_proxy`]) so no request reaches a provider. [`copy::guard`] refuses the live install.

pub mod copy;
pub mod desktop;
pub mod fake_proxy;
pub mod health;
pub mod io;
pub mod publisher;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pool_core::js;
use pool_core::refresh::{Options, Outcome, Refresh, account::invalidate, migrate_state};
use serde_json::{Value, json};
use tokio::sync::Notify;

/// The write side of a copy, as its routes and tasks reach it.
pub struct Copy {
    pub io: Arc<io::FileIo>,
    pub health: Arc<health::Health>,
    pub trigger: Arc<Trigger>,
}

/// A manual refresh request (`POST /refresh`). While a cycle runs, a request joins it, as
/// refresh-loop.mjs returns the running check.
#[derive(Default)]
pub struct Trigger {
    requested: AtomicBool,
    running: AtomicBool,
    notify: Notify,
}

impl Trigger {
    /// Asks for a cycle now. False when one is already running (the request joins it).
    pub fn request(&self) -> bool {
        if self.running.load(Ordering::SeqCst) {
            return false;
        }
        self.requested.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
        true
    }
}

/// refresh-loop.mjs's timing: the interval, the short one after a degraded or follow-up cycle,
/// and how many short ones may run in a row.
#[derive(Clone, Copy, Debug)]
pub struct Delays {
    pub every: Duration,
    pub degraded: Duration,
    pub budget: u32,
}

impl Default for Delays {
    fn default() -> Delays {
        Delays { every: Duration::from_secs(120), degraded: Duration::from_secs(30), budget: 3 }
    }
}

/// The pool state as pool-service.mjs starts with it: the saved file over the defaults, the
/// retired `burn` dropped, every account unverified until it is read again, and migrated.
pub fn load_state(root: &Path) -> Value {
    let mut state = json!({ "accounts": [], "checkedAt": null, "error": null, "resetAutomation": true, "resets": {} });
    let saved = std::fs::read_to_string(root.join("state/pool-state.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(t.trim_start_matches('\u{feff}')).ok());
    if let Some(Value::Object(saved)) = saved {
        for (k, v) in saved {
            state[k] = v;
        }
        if let Value::Object(m) = &mut state {
            m.shift_remove("burn");
        }
        if let Some(Value::Array(accounts)) = state.get_mut("accounts") {
            accounts.iter_mut().for_each(invalidate);
        }
    }
    migrate_state(&mut state);
    state
}

/// One refresh: what [`run_loop`] calls each time.
pub trait Check {
    fn check(&mut self) -> impl Future<Output = Outcome> + Send;
}

/// `pool_core::refresh` over the copy's files and the stand-in proxy.
struct Cycle<F> {
    io: Arc<io::FileIo>,
    health: Arc<health::Health>,
    state: Value,
    refresh: Refresh,
    changed: F,
}

impl<F: Fn() + Send> Check for Cycle<F> {
    async fn check(&mut self) -> Outcome {
        // The proxy watch's holdSince, as the one state object pool-service.mjs shares holds it.
        self.health.apply(&mut self.state);
        let outcome = self.refresh.refresh(&mut self.state, &*self.io).await;
        (self.changed)();
        println!(
            "writer: cycle at {} degraded={} followUp={}",
            js::iso(io::now_ms()),
            outcome.degraded,
            outcome.follow_up
        );
        outcome
    }
}

/// The write side's refresh: the saved state, a first cycle as soon as the proxy answers
/// (pool-service.mjs's `bootstrapped`: saved status is unverified after a start), then
/// [`run_loop`]. `changed` is called after each cycle.
pub async fn run(copy: Arc<Copy>, delays: Delays, options: Options, changed: impl Fn() + Send, signal: corten::Signal) {
    let state = load_state(&copy.io.root);
    let refresh = Refresh::new(options, &state);
    // checkProxy: healthy when `auth-files` answers with a `files` list.
    let healthy = copy.io.call("auth-files", None, None).await.is_ok_and(|v| v.get("files").is_some_and(Value::is_array));
    // The proxy watch starts the first cycle instead when the proxy is not up yet.
    let bootstrap = healthy && !copy.health.bootstrapped.swap(true, Ordering::SeqCst);
    let trigger = copy.trigger.clone();
    let mut cycle = Cycle { io: copy.io.clone(), health: copy.health.clone(), state, refresh, changed };
    run_loop(&trigger, delays, bootstrap, &mut cycle, &signal).await;
}

/// refresh-loop.mjs: the next cycle `delays.every` after the last one ends (at once when
/// `bootstrap`), sooner after a degraded or follow-up cycle within the budget, and at once on a
/// request. A request while a cycle runs joins it, and the delay starts when it ends. A cycle in
/// progress finishes before the loop stops, so a journaled claim is saved first.
pub async fn run_loop<C: Check>(trigger: &Trigger, delays: Delays, bootstrap: bool, check: &mut C, signal: &corten::Signal) {
    let mut wait = if bootstrap { Duration::ZERO } else { delays.every };
    let mut fast = 0;
    loop {
        let notified = trigger.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !trigger.requested.load(Ordering::SeqCst) {
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = &mut notified => {}
                _ = signal.aborted() => return,
            }
        }
        // Running before the request is cleared: one that arrives from here on joins this cycle.
        trigger.running.store(true, Ordering::SeqCst);
        trigger.requested.store(false, Ordering::SeqCst);
        let outcome = check.check().await;
        trigger.running.store(false, Ordering::SeqCst);
        let soon = outcome.degraded || outcome.follow_up;
        if soon && fast < delays.budget {
            fast += 1;
            wait = delays.degraded.min(delays.every);
        } else {
            if !soon {
                fast = 0;
            }
            wait = delays.every;
        }
    }
}

#[cfg(test)]
mod tests;
