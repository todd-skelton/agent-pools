import { rankAccounts, liveDrainFirst } from "./routing.mjs";
import { refreshUsage, isTransientReadError } from "./usage.mjs";
import { reconcileQuota } from "./quota-recovery.mjs";
import { currentAccount, invalidateAccount } from "./freshness.mjs";
import { processReset, resetExpected, externalRedemptions } from "./reset-lifecycle.mjs";
import { exhaustionTransition, reauthTransitions, creditRiskTransitions } from "./notifications.mjs";
import { usageSamples } from "./projection.mjs";
import { PROVIDERS, PROVIDER_IDS, createAccount } from "./providers.mjs";

// One refresh cycle: read every account through its provider adapter, rank
// and prioritize, record samples, raise exhaustion alerts, run each provider's
// reset lifecycle and save. All I/O is passed in so the cycle can be tested
// against recorded provider payloads:
//   state        the live pool state (mutated in place)
//   management   (endpoint, body?, method?) => proxy management API
//   event        (type, details) => event log;  sample  => usage-sample log
//   save         () => persist state;  toast  (title, body) => desktop alert
//   loadPolicies () => { [provider]: resetPolicy }
//   authFile     (file) => the account's credential file, or null (never throws)
//   clientVersion (provider) => version of that provider's client the proxy presents
//   forecast     ({ accounts, newSamples, resetPolicies, resetAutomation }) => capacity forecast
//   creditPlan   ({ accounts, policy, now }) => Set of account ids that may redeem before lapse, or null
//                publisher (capacity-publisher.mjs); a failure is logged, never fails the cycle
//   refillDemand ({ policy, now }) => { unitsPerHour, weights } or null: refill timing for
//                refill providers (routing.mjs refillNeedMs); null or a failure falls back
//                to the policy's maxRunwayPercent
//   drainFirst   ({ provider, now }) => Map account id -> credit expiry (ms) or null: the
//                credit plan's drain-first holders (routing.mjs rankAccounts), applied only
//                while the account still lists that credit; a failure ranks as before
//   readPlans    read each account's subscription plan and price (adapter.readPlan)
//                every planTtlMs; off by default so the cycle makes no billing calls
//   now, sleep   clock and delay (tests)
// refresh() resolves to { degraded, followUp }: degraded when an account could
// not be verified for a transient reason, followUp when a reset outcome should
// be re-read soon. Either asks the refresh loop for its short interval.
export function createRefresh({ state, management, event, sample = async () => {}, save, toast = async () => {},
  loadPolicies, authFile = async () => null, clientVersion = async () => undefined, forecast = null, creditPlan = null, refillDemand = null, drainFirst = null,
  readPlans = false, planTtlMs = 12 * 3600000, planRetryMs = 3600000, memoTtlMs = 12 * 3600000, planTimeoutMs = 10000,
  now = Date.now, sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms)) }) {
  const iso = () => new Date(now()).toISOString();
  // Same-account history (last object with a usageUpdatedAt) survives a cycle
  // in which the proxy listing omitted the account or it was disabled, so a
  // failed read on reappearance still has last-known rows and a timestamp for
  // the overlay's grace window. Seeded from saved state; bounded by account count.
  const lastKnown = new Map();
  const historyKey = (a) => `${a.provider}:${a.id}`;
  const rememberHistory = (a) => { if (a.usageUpdatedAt && Array.isArray(a.quotaWindows) && a.quotaWindows.length > 0) lastKnown.set(historyKey(a), a); };
  state.accounts.forEach(rememberHistory);

  // Provider call through the proxy, which injects the account's token.
  // extraHeaders (for example a billing read's user agent) go on top of the
  // provider's own; the token placeholder cannot be replaced.
  async function read(file, url, data, extraHeaders = {}) {
    const r = await management("api-call", {
      auth_index: file.auth_index,
      method: data ? "POST" : "GET",
      url,
      header: { "Content-Type": "application/json", ...(await PROVIDERS[file.provider].headers(file, io)), ...extraHeaders, Authorization: "Bearer $TOKEN$" },
      ...(data ? { data: JSON.stringify(data) } : {}),
    });
    if (r.status_code < 200 || r.status_code >= 300)
      throw Object.assign(new Error(`Account provider returned HTTP ${r.status_code}`), { status: r.status_code });
    return typeof r.body === "string" ? JSON.parse(r.body) : r.body;
  }
  // Shared reads that do not depend on the account (a price table): one
  // provider call per key per memoTtlMs. A failure is not cached.
  const memoCache = new Map();
  function memo(key, fn) {
    const hit = memoCache.get(key);
    if (hit && now() - hit.at < memoTtlMs) return hit.value;
    const value = Promise.resolve().then(fn);
    memoCache.set(key, { at: now(), value });
    value.catch(() => { if (memoCache.get(key)?.value === value) memoCache.delete(key); });
    return value;
  }
  const io = { read, authFile, clientVersion, memo };

  // Subscription plan and price (adapter.readPlan), read every planTtlMs, or
  // planRetryMs after a failed read; between reads and after a failure the
  // last known plan is kept. Never fails the account.
  async function refreshPlan(a, file, previous) {
    if (previous?.plan) a.plan = previous.plan;
    if (previous?.planCheckedAt) a.planCheckedAt = previous.planCheckedAt;
    if (previous?.planError) a.planError = previous.planError;
    const adapter = PROVIDERS[file.provider];
    if (!readPlans || typeof adapter.readPlan !== "function") return;
    const last = Date.parse(a.planCheckedAt ?? "");
    if (Number.isFinite(last) && now() - last < (a.planError ? planRetryMs : planTtlMs)) return;
    a.planCheckedAt = iso();
    // Bounded: a slow billing endpoint must not hold the cycle (and its reset
    // lifecycle) behind it; a late answer is dropped and read next time.
    let timer;
    try {
      const plan = await Promise.race([
        adapter.readPlan(io, file),
        new Promise((_, reject) => { timer = setTimeout(() => reject(new Error("Plan read timed out")), planTimeoutMs); }),
      ]);
      a.plan = { ...plan, readAt: iso() };
      delete a.planError;
    } catch (e) {
      // Logged when the problem changes, not on every hourly retry.
      if (a.planError !== e.message) await event("plan-read-error", { accountId: a.id, provider: a.provider, message: e.message }).catch(() => {});
      a.planError = e.message;
    } finally { clearTimeout(timer); }
  }
  async function proxySnapshot(account) {
    return (await management("auth-files")).files.find(
      (f) => f.id === account.id && f.auth_index === account.auth_index,
    ) ?? null;
  }

  // Never throws: every failure is recorded on the account.
  async function refreshAccount(file) {
    const adapter = PROVIDERS[file.provider];
    const a = createAccount(file);
    const previousPlan = [state.accounts.find((p) => p.id === a.id && p.provider === a.provider), lastKnown.get(historyKey(a))].find((p) => p?.plan || p?.planCheckedAt) ?? null;
    if (a.disabled) {
      a.status = "disabled";
      for (const k of ["plan", "planCheckedAt", "planError"]) if (previousPlan?.[k]) a[k] = previousPlan[k];
      return a;
    }
    try {
      const usage = await refreshUsage(
        a,
        [state.accounts.find((previous) => previous.id === a.id && previous.provider === a.provider), lastKnown.get(historyKey(a))]
          .find((previous) => previous?.usageUpdatedAt && previous.quotaWindows?.length) ?? null,
        () => read(file, adapter.usageUrl),
        iso,
        sleep,
      );
      try {
        if (await reconcileQuota(a, {
          readSnapshot: () => proxySnapshot(a),
          clearQuota: (request) => management("recover-quota", request),
          now,
        })) {
          await event("quota-recovered", { accountId: a.id, usageUpdatedAt: a.usageUpdatedAt });
        }
      } catch (e) {
        a.routingStatus = "unknown";
        a.routingError = e.message;
        await event("quota-recovery-error", { accountId: a.id, message: e.message });
      }
      if (usage) {
        const bank = await adapter.readBank(io, file, usage, now());
        a.creditsKnown = bank.known;
        a.credits = bank.credits;
        a.creditCount = bank.count;
        if (bank.note) a.creditNote = bank.note;
        if (bank.known) a.creditsUpdatedAt = iso();
      }
    } catch (e) {
      // Routing/credit reads failed after the usage read (e.g. the proxy was
      // restarting): classify like a usage failure so a transient one gets
      // the short reschedule.
      a.error = e.message;
      a.errorTransient = isTransientReadError(e);
      invalidateAccount(a);
    }
    await refreshPlan(a, file, previousPlan);
    return a;
  }

  async function refresh() {
    try {
      const files = (await management("auth-files")).files.filter((f) => f.provider in PROVIDERS);
      // Accounts refresh concurrently: one slow provider read does not hold
      // the others behind its timeout.
      const accounts = await Promise.all(files.map(refreshAccount));
      accounts.forEach((a, i) => { accounts[i] = currentAccount(a, now()); });
      const boosts = {};
      for (const provider of PROVIDER_IDS) {
        let plan = null;
        if (drainFirst) {
          try { plan = drainFirst({ provider, now: now() }); } catch (e) { await event("drain-first-error", { provider, message: e.message }); }
        }
        boosts[provider] = liveDrainFirst(plan, accounts.filter((a) => a.provider === provider), now());
      }
      for (const provider of PROVIDER_IDS) {
        const ranked = rankAccounts(accounts.filter((a) => a.provider === provider && !a.disabled), boosts[provider]);
        for (let i = 0; i < ranked.length; i++) {
          const a = ranked[i], priority = 100 - i;
          if (a.priority !== priority) {
            await management("auth-files/fields", { name: a.name, priority }, "PATCH");
            a.priority = priority;
          }
        }
      }
      const samples = usageSamples(state.accounts, accounts, now());
      for (const s of samples) await sample("usage-sample", s);
      // Credits redeemed outside AgentPools, so the redemption history is complete.
      for (const x of externalRedemptions({ previous: state.accounts, accounts, resets: state.resets, since: Date.parse(state.checkedAt), now: now() }))
        await event("reset-result", { ...x, outcome: "reset", source: "external" });
      accounts.forEach(rememberHistory);
      state.accounts = accounts;
      state.checkedAt = iso();
      state.error = null;
      const loaded = await loadPolicies();
      // Refill providers judge refill timing on the forecast's low demand; the
      // published policies themselves stay unchanged.
      const policies = {};
      for (const provider of PROVIDER_IDS) {
        const p = loaded[provider];
        let demand = null;
        if (p?.redemption === "refill" && refillDemand) {
          try { demand = refillDemand({ policy: p, now: now() }); } catch (e) { await event("refill-demand-error", { provider, message: e.message }); }
        }
        policies[provider] = demand || boosts[provider] ? { ...p, ...(demand ? { refillDemand: demand } : {}), ...(boosts[provider] ? { drainFirst: boosts[provider] } : {}) } : p;
      }
      for (const provider of PROVIDER_IDS) {
        const transition = exhaustionTransition(state, accounts, {
          provider, resetExpected: resetExpected(state, policies[provider], accounts, now()), now: new Date(now()),
        });
        if (!transition) continue;
        await event(transition, { provider, exhaustedSince: state.exhausted[provider] ?? null });
        const { name } = PROVIDERS[provider];
        if (transition === "pool-exhausted")
          toast(`Agent Pools: ${name} accounts exhausted`, `Every ${name} account is at its limit and no banked reset is about to be used. New ${name} turns will fail until a window resets.`)
            .catch((e) => event("notify-error", { message: e.message }).catch(() => {}));
      }
      // A rejected login shows up at the usage endpoint as whatever the
      // provider answers a dead token with (HTTP 401, then 429 on 2026-09-29),
      // so it is named from the proxy's refresh verdict instead.
      for (const t of reauthTransitions(state, accounts)) {
        if (t.type === "reauth-cleared") {
          await event("reauth-cleared", { accountId: t.accountId, provider: t.provider, reason: t.reason });
          continue;
        }
        const a = t.account, { name } = PROVIDERS[a.provider];
        await event("reauth-required", { accountId: a.id, provider: a.provider, since: a.refreshError.since, message: a.refreshError.message });
        toast(`Agent Pools: ${name} sign-in expired`, `${a.email || a.id}: ${name} rejected the saved login. Sign in again with Connect account on the Agent Pools dashboard; the account stops serving requests when its current token lapses, if it has not already.`)
          .catch((e) => event("notify-error", { message: e.message }).catch(() => {}));
      }
      // Providers are independent: an uncertain outcome on one must not stall
      // the other or discard this refresh.
      let followUp = false;
      for (const provider of PROVIDER_IDS) {
        const adapter = PROVIDERS[provider];
        try {
          const outcome = await processReset(state, {
            policy: policies[provider], requestId: adapter.requestId, save, log: event, now, plan: creditPlan,
            consume: (account, pending) => {
              const file = files.find((f) => f.id === account.id && !f.disabled);
              if (!file) throw new Error("Reset account unavailable; original identity retained");
              return adapter.redeem(io, file, pending);
            },
          });
          delete state.resets[provider].error;
          // A claim was sent. Re-read soon: after a reset so the refilled
          // account is verified and its proxy quota block cleared within
          // seconds; after a refusal so the next account's credit is tried
          // without waiting a full interval (the loop caps fast cycles).
          if (outcome) followUp = true;
        } catch (e) {
          followUp = true;
          state.resets ??= {};
          (state.resets[provider] ??= { pending: null, last: null }).error = { at: iso(), message: e.message };
          await event("reset-error", { provider, message: e.message });
        }
      }
      await save();
      // After the reset lifecycle and save, so it never delays a redemption.
      if (forecast) {
        let doc = null;
        try {
          doc = await forecast({ accounts, newSamples: samples, resetPolicies: policies, resetAutomation: state.resetAutomation !== false });
        } catch (e) {
          await event("capacity-forecast-error", { message: e.message }).catch(() => {});
        }
        if (doc) await creditRiskAlerts(doc, accounts);
      }
      // An account whose read failed transiently (after one retry) stays
      // unverified until the next cycle; ask the loop to come back sooner than
      // the full interval. Verdicts (401/403/429, unusable body) wait the full
      // interval so one broken account does not speed up polling for the rest.
      const unverified = accounts.filter((a) => !a.disabled && (a.status === "unknown" || a.usageStale));
      const degraded = unverified.some((a) => a.errorTransient === true);
      if (unverified.length)
        await event("refresh-degraded", { degraded, accounts: unverified.map((a) => ({ id: a.id, error: a.error ?? null, transient: a.errorTransient === true,
          ...(a.refreshError ? { refreshError: a.refreshError.code } : {}) })) });
      return { degraded, followUp };
    } catch (e) {
      state.error = e.message;
      state.accounts.forEach(invalidateAccount);
      await save().catch(() => {});
      await event("refresh-error", { message: e.message });
      return { degraded: true, followUp: false };
    }
  }
  // A banked credit the plan still loses at the measured pace, inside 72 h:
  // one event and toast per credit (notifications.mjs creditRiskTransitions).
  async function creditRiskAlerts(doc, accounts) {
    try {
      const transitions = creditRiskTransitions(state, doc, { now: now() });
      for (const t of transitions) {
        if (t.type === "credit-risk-cleared") { await event("credit-risk-cleared", { provider: t.provider, accountId: t.accountId, expiresAt: t.expiresAt, reason: t.reason }); continue; }
        const c = t.credit, adapter = PROVIDERS[t.provider], f = doc.providers[t.provider];
        const term = adapter?.redemption === "refill" ? "refill" : "reset";
        const a = accounts.find((x) => x.id === c.accountId);
        const perWeek = (x) => (Number.isFinite(x) ? (x * 168).toFixed(1) : null);
        const pace = perWeek(f?.demand?.unitsPerHour), need = perWeek(c.needUnitsPerHour);
        await event("credit-at-risk", { provider: t.provider, accountId: c.accountId, expiresAt: c.expiresAt, action: c.action, drainBy: c.drainBy, unitsLeft: c.unitsLeft,
          demandUnitsPerHour: f?.demand?.unitsPerHour ?? null, needUnitsPerHour: c.needUnitsPerHour });
        const when = new Date(c.expiresAt).toLocaleString("en-US", { weekday: "short", month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
        toast(`Agent Pools: ${adapter?.name ?? t.provider} ${term} at risk`,
          `${a?.email || c.accountId}: its ${term} expires ${when} and lapses unused at the current pace (${pace} acct-weeks/week). ` +
          (need ? `It needs about ${need} acct-weeks/week until then.` : "No steady pace saves it."))
          .catch((e) => event("notify-error", { message: e.message }).catch(() => {}));
      }
      if (transitions.length) await save();
    } catch (e) {
      await event("credit-risk-error", { message: e.message }).catch(() => {});
    }
  }
  return { refresh };
}
