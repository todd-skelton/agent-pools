// Periodic refresh. `check` may resolve to { degraded: true } (some account
// could not be verified for a transient reason) or { followUp: true } (state
// just changed, e.g. a banked reset landed, and should be re-read); the next
// automatic check then runs after `degradedDelay` instead of `delay`, so a
// single failed provider read leaves an account unverified for seconds rather
// than a full interval.
// At most `degradedBudget` consecutive fast cycles run before the loop falls
// back to the full interval, so a persistent fault cannot keep the whole pool
// polling four times as often. A rejected check waits the full interval.
export function createRefreshLoop(check, delay = 120000, clock = globalThis, degradedDelay = 30000, degradedBudget = 3) {
  let timer;
  let running;
  let stopped = true;
  let fastCycles = 0;

  function schedule(wait = delay) {
    clock.clearTimeout(timer);
    if (!stopped)
      timer = clock.setTimeout(() => void refresh().catch(() => {}), wait);
  }

  function refresh() {
    if (running) return running;
    clock.clearTimeout(timer);
    let wait = delay;
    running = Promise.resolve()
      .then(check)
      .then((result) => {
        const soon = result?.degraded || result?.followUp;
        if (soon && fastCycles < degradedBudget) {
          fastCycles++;
          wait = Math.min(degradedDelay, delay);
        } else if (!soon) fastCycles = 0;
        return result;
      })
      .finally(() => {
        running = null;
        schedule(wait);
      });
    return running;
  }

  return {
    start() {
      stopped = false;
      schedule();
    },
    refresh,
    stop() {
      stopped = true;
      clock.clearTimeout(timer);
    },
  };
}
