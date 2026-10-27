//! refresh-loop.test.mjs on [`run_loop`], with tokio's paused clock in place of its fake clock.
//! Its "failure waits a full interval" case has no counterpart: `Refresh::refresh` turns an
//! error into a degraded outcome (as refresh.mjs does) rather than failing.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pool_core::refresh::Outcome;
use tokio::sync::Notify;
use tokio::time::Instant;

use super::{Check, Delays, Trigger, run_loop};

const S: Duration = Duration::from_secs(1);

/// Records when each check starts (milliseconds since the test began) and answers from a script;
/// past its end every check is clean. With `hold`, a check waits for `finish` before ending.
#[derive(Clone)]
struct Scripted {
    began: Instant,
    calls: Arc<Mutex<Vec<u64>>>,
    script: Arc<Mutex<VecDeque<Outcome>>>,
    hold: Option<Arc<Notify>>,
}

impl Scripted {
    fn new(script: impl IntoIterator<Item = (bool, bool)>) -> Scripted {
        let script = script.into_iter().map(|(degraded, follow_up)| Outcome { degraded, follow_up }).collect();
        Scripted { began: Instant::now(), calls: Default::default(), script: Arc::new(Mutex::new(script)), hold: None }
    }
    fn calls(&self) -> Vec<u64> {
        self.calls.lock().unwrap().clone()
    }
}

impl Check for Scripted {
    async fn check(&mut self) -> Outcome {
        self.calls.lock().unwrap().push(self.began.elapsed().as_millis() as u64);
        if let Some(h) = &self.hold {
            h.notified().await;
        }
        self.script.lock().unwrap().pop_front().unwrap_or(Outcome { degraded: false, follow_up: false })
    }
}

fn delays(every: u64, degraded: u64, budget: u32) -> Delays {
    Delays { every: Duration::from_secs(every), degraded: Duration::from_secs(degraded), budget }
}

/// refresh-loop.test.mjs's `settle`: lets the loop run what is due at this instant.
async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

fn secs(s: &[u64]) -> Vec<u64> {
    s.iter().map(|s| s * 1000).collect()
}

/// Runs the loop on its own task, on the paused clock.
async fn spawn(trigger: Arc<Trigger>, d: Delays, bootstrap: bool, check: Scripted) -> tokio::task::JoinHandle<()> {
    let handle = tokio::spawn(async move {
        let mut check = check;
        run_loop(&trigger, d, bootstrap, &mut check, &corten::Signal::default()).await;
    });
    tokio::task::yield_now().await;
    handle
}

#[tokio::test(start_paused = true)]
async fn startup_uses_saved_values_and_a_manual_check_cancels_the_imminent_automatic_one() {
    let trigger = Arc::new(Trigger::default());
    let check = Scripted::new([]);
    let task = spawn(trigger.clone(), delays(120, 30, 3), false, check.clone()).await;
    tokio::time::sleep(Duration::from_millis(119_999)).await;
    assert!(check.calls().is_empty(), "no check at start without a bootstrap");
    assert!(trigger.request());
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(check.calls().len(), 1);
    tokio::time::sleep(Duration::from_millis(119_000)).await;
    assert_eq!(check.calls().len(), 1, "the automatic check moved to 120 s after the manual one");
    tokio::time::sleep(2 * S).await;
    assert_eq!(check.calls(), vec![119_999, 239_999]);
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn a_bootstrap_checks_at_once() {
    let check = Scripted::new([]);
    let task = spawn(Arc::new(Trigger::default()), delays(120, 30, 3), true, check.clone()).await;
    tokio::time::sleep(130 * S).await;
    assert_eq!(check.calls(), secs(&[0, 120]));
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn overlapping_requests_share_one_check_and_the_delay_starts_on_completion() {
    let trigger = Arc::new(Trigger::default());
    let mut check = Scripted::new([]);
    let finish = Arc::new(Notify::new());
    check.hold = Some(finish.clone());
    let task = spawn(trigger.clone(), delays(120, 30, 3), false, check.clone()).await;
    tokio::time::sleep(120 * S).await;
    settle().await;
    assert_eq!(check.calls().len(), 1);
    assert!(!trigger.request(), "a request while a check runs joins it");
    tokio::time::sleep(240 * S).await;
    settle().await;
    assert_eq!(check.calls().len(), 1);
    finish.notify_one();
    settle().await;
    tokio::time::sleep(119 * S).await;
    assert_eq!(check.calls().len(), 1, "the joined request did not run a second check");
    tokio::time::sleep(2 * S).await;
    assert_eq!(check.calls(), secs(&[120, 480]));
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn a_degraded_check_comes_back_after_the_short_delay_and_a_clean_one_after_the_full_one() {
    let check = Scripted::new([(true, false), (false, false), (false, true), (false, false)]);
    let task = spawn(Arc::new(Trigger::default()), delays(120, 30, 3), true, check.clone()).await;
    tokio::time::sleep(400 * S).await;
    // degraded, clean, follow-up, clean
    assert_eq!(check.calls(), secs(&[0, 30, 150, 180, 300]));
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn the_degraded_delay_never_exceeds_the_normal_one() {
    let check = Scripted::new([(true, false), (true, false)]);
    let task = spawn(Arc::new(Trigger::default()), delays(10, 30, 3), true, check.clone()).await;
    tokio::time::sleep(25 * S).await;
    assert_eq!(check.calls(), secs(&[0, 10, 20]));
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn a_persistent_degraded_result_gets_at_most_the_budget_and_a_clean_cycle_restores_it() {
    let d = (true, false);
    let check = Scripted::new([d, d, d, d, d, (false, false), d, d]);
    let task = spawn(Arc::new(Trigger::default()), delays(120, 30, 3), true, check.clone()).await;
    tokio::time::sleep(700 * S).await;
    // three fast cycles, then the full interval while still degraded; clean; degraded is fast again
    assert_eq!(check.calls(), secs(&[0, 30, 60, 90, 210, 330, 450, 480, 510, 630]));
    task.abort();
}
