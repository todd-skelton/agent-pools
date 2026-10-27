//! The pool's write side on Corten, run only against a copy of the install: the refresh loop
//! (refresh-loop.mjs) driving `pool_core::refresh` with file and proxy I/O ([`io`]), the
//! capacity publisher ([`publisher`]), and a stand-in proxy ([`fake_proxy`]) so no request
//! reaches a provider. [`copy::guard`] refuses the live install.

pub mod copy;
pub mod fake_proxy;
pub mod io;
pub mod publisher;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pool_core::js;
use pool_core::refresh::{Options, Refresh, account::invalidate, migrate_state};
use serde_json::{Value, json};
use tokio::sync::Notify;

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

/// The refresh loop: the first cycle after `delays.every` (startup serves the saved state), then
/// on the interval, sooner after a degraded or follow-up cycle within the budget, and at once on
/// a request. `changed` is called after each cycle.
pub async fn run(io: Arc<io::FileIo>, trigger: Arc<Trigger>, delays: Delays, options: Options, changed: impl Fn() + Send, signal: corten::Signal) {
    let mut state = load_state(&io.root);
    let mut refresh = Refresh::new(options, &state);
    let mut wait = delays.every;
    let mut fast = 0;
    loop {
        let notified = trigger.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !trigger.requested.swap(false, Ordering::SeqCst) {
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = &mut notified => {}
                _ = signal.aborted() => return,
            }
            trigger.requested.store(false, Ordering::SeqCst);
        }
        trigger.running.store(true, Ordering::SeqCst);
        // A cycle in progress finishes, so a journaled claim is saved before the process ends.
        let outcome = refresh.refresh(&mut state, &*io).await;
        trigger.running.store(false, Ordering::SeqCst);
        changed();
        println!(
            "writer: cycle at {} degraded={} followUp={}",
            js::iso(io::now_ms()),
            outcome.degraded,
            outcome.follow_up
        );
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
