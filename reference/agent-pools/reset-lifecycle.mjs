import { chooseReset, creditKey, accountOut } from "./routing.mjs";
import { currentAccount } from "./freshness.mjs";

// Outcomes that used nothing. That credit is passed over until the backoff
// passes, so a refusal cannot turn into a request every refresh, while the
// provider's other credits stay eligible.
export const RESET_BACKOFF_MS = { no_credit: 30 * 60000, nothing_to_reset: 30 * 60000, rejected: 30 * 60000, deferred: 5 * 60000 };
const TERMINAL = ["reset", "already_redeemed", ...Object.keys(RESET_BACKOFF_MS)];

// Refusals still inside their backoff, keyed by creditKey(). Expired entries
// are dropped, which keeps the map bounded by the credits refused recently.
export function activeRefusals(slot, now) {
  return Object.fromEntries(Object.entries(slot?.refused ?? {})
    .filter(([, r]) => now - Date.parse(r?.at) < (RESET_BACKOFF_MS[r?.outcome] ?? 0)));
}

// Credits redeemed outside AgentPools (for example on the provider's web page)
// leave no claim in the journal. One shows up between two reads of the same
// account as: a known bank on both reads whose count fell, and an all-models
// weekly window whose use fell by at least 20 points while its reset was not
// yet due (a natural reset also clears usage). A claim of our own is excluded:
// one pending for that account, or one finished since the previous read
// (`since`; a claim is sent after the read that chose it and seen on the next
// one) or within the last OWN_CLAIM_MS, in case the provider shows it late.
// Unverified or stale reads are never evidence, so the first read after a
// restart (saved accounts start unverified) detects nothing. Pure; returns
// { provider, accountId, creditsBefore, creditsAfter, weeklyUsedBefore,
// weeklyUsedAfter } per detected redemption.
const WEEK_SECONDS = 604800;
const OWN_CLAIM_MS = 6 * 3600000;
export function externalRedemptions({ previous = [], accounts = [], resets = {}, since = null, now = Date.now() }) {
  const before = new Map(previous.map((a) => [a.provider + ":" + a.id, a]));
  const weekly = (a) => (a.windows ?? []).find((w) => w.common && w.durationSeconds === WEEK_SECONDS);
  const verified = (a) => !a.disabled && a.creditsKnown === true && Number.isInteger(a.creditCount) && a.status !== "unknown" && !a.usageStale;
  const out = [];
  for (const a of accounts) {
    const p = before.get(a.provider + ":" + a.id);
    if (!p || !verified(p) || !verified(a) || a.creditCount >= p.creditCount) continue;
    const w0 = weekly(p), w1 = weekly(a);
    if (!w0 || !w1 || !Number.isFinite(w0.used) || !Number.isFinite(w1.used) || w0.used - w1.used < 20) continue;
    if (!(Number.isFinite(w0.resetAt) && w0.resetAt > now)) continue;
    const slot = resets?.[a.provider];
    if (slot?.pending?.accountId === a.id) continue;
    const lastAt = Date.parse(slot?.last?.at);
    if (slot?.last?.accountId === a.id && Number.isFinite(lastAt) && (!Number.isFinite(since) || lastAt >= Math.min(since, now - OWN_CLAIM_MS))) continue;
    out.push({ provider: a.provider, accountId: a.id, creditsBefore: p.creditCount, creditsAfter: a.creditCount, weeklyUsedBefore: w0.used, weeklyUsedAfter: w1.used });
  }
  return out;
}

// Whether automation is about to act for this provider: a claim is in flight
// (unsent or uncertain; "attention" needs a person) or one would be sent now.
// Used to decide whether an exhausted pool is worth an alert.
export function resetExpected(state, policy, accounts, now = Date.now()) {
  if (!state.resetAutomation || !policy?.enabled) return false;
  const slot = state.resets?.[policy.provider];
  if (slot?.pending) return slot.pending.phase !== "attention";
  return !!chooseReset(accounts, policy, now, new Set(Object.keys(activeRefusals(slot, now))));
}

// One provider's reset slot: state.resets[policy.provider] = { pending, last, refused }.
// Persist before sending. An uncertain redemption must retain its idempotency
// identity; it must never be replaced by a newly selected credit.
// plan({ accounts, policy, now }) returns the credit planner's beforeLapse set
// (routing.mjs chooseReset) or null; a planner failure only disables early use.
export async function processReset(state, { policy, requestId, save, consume, log = async () => {}, now = Date.now, plan = null }) {
  const provider = policy.provider;
  state.resets ??= {};
  const slot = (state.resets[provider] ??= { pending: null, last: null });
  slot.refused = activeRefusals(slot, now());
  const automation = !!state.resetAutomation && policy.enabled;
  const accounts = state.accounts.map((a) => currentAccount(a, now()));
  let pending = slot.pending;
  if (pending && !pending.phase) pending = { ...pending, phase: "uncertain" };
  let beforeLapse = null;
  if (plan && policy.redeemBeforeLapse) {
    try { beforeLapse = plan({ accounts, policy, now: now() }); } catch (e) { await log("credit-plan-error", { provider, message: e.message }); }
  }
  const selected = chooseReset(accounts, policy, now(), new Set(Object.keys(slot.refused)), beforeLapse);
  const finish = (outcome) => {
    slot.last = { ...slot.pending, provider, at: new Date(now()).toISOString(), outcome };
    if (RESET_BACKOFF_MS[outcome])
      slot.refused[creditKey(slot.pending.accountId, slot.pending.creditId)] = { outcome, at: slot.last.at };
    slot.pending = null;
  };
  if (pending?.phase === "unsent" && (!automation || !selected || selected.accountId !== pending.accountId || selected.creditId !== pending.creditId)) {
    slot.pending = pending;
    finish("cancelled-before-send");
    await save();
    pending = null;
  }
  if (!pending) {
    if (!automation || !selected) return;
    const a = accounts.find((a) => a.id === selected.accountId);
    pending = { ...selected, phase: "unsent", requestId: requestId(a, selected.creditId), ...(selected.trigger ? { creditCountAtClaim: a.creditCount } : {}) };
  }
  const account = accounts.find((a) => a.id === pending.accountId);
  slot.pending = pending;
  if (!pending.requestId) {
    slot.pending = { ...pending, phase: "attention", reason: "Missing original redemption identity; automatic retry is unsafe" };
    await save();
    return;
  }
  if (!account || account.disabled) {
    if (pending.phase === "unsent") finish("cancelled-before-send");
    else slot.pending = { ...pending, phase: "attention", reason: "Account missing or disabled; redemption outcome needs reconciliation" };
    await save();
    return;
  }
  // An early (before-lapse) claim was valid only while its account was out.
  // If a retry finds the account refilled, never resend: a credit that has
  // left the known bank was redeemed; otherwise a person reconciles it.
  const roster = accounts.filter((a) => a.provider === provider && !a.disabled);
  const allOut = roster.length >= policy.expectedCount && roster.every((a) => accountOut(a, policy));
  if (pending.trigger === "before-lapse" && pending.phase !== "unsent" && !allOut && !accountOut(account, { ...policy, trigger: "weekly" })) {
    // Only a smaller bank proves the claim landed; a missing listing alone
    // (cooling down, next grant moved, withheld) does not.
    if (account.creditsKnown && Number.isInteger(pending.creditCountAtClaim) && Number.isInteger(account.creditCount) && account.creditCount < pending.creditCountAtClaim) {
      slot.pending = pending;
      finish("already_redeemed");
      await log("reset-result", { provider, accountId: pending.accountId, creditId: pending.creditId, requestId: pending.requestId, outcome: "already_redeemed", trigger: pending.trigger, reconciled: true });
    } else if (pending.phase !== "attention") {
      slot.pending = { ...pending, phase: "attention", reason: "Early redemption outcome unknown and the account has refilled; resent with its original id only once the account is out again" };
    }
    await save();
    return;
  }
  // A reconnected account reconciles an attention item with its original identity.
  slot.pending = { ...pending, phase: "uncertain", reason: undefined,
    firstAttemptAt: pending.firstAttemptAt ?? new Date(now()).toISOString() };
  await save();
  const { accountId, creditId } = slot.pending;
  await log("reset-attempt", { provider, accountId, creditId, requestId: slot.pending.requestId, ...(slot.pending.trigger ? { trigger: slot.pending.trigger } : {}) });
  const response = await consume(account, slot.pending);
  if (!TERMINAL.includes(response?.code)) {
    throw new Error("Reset outcome unknown; original retry identity retained");
  }
  await log("reset-result", { provider, accountId, creditId, requestId: slot.pending.requestId, outcome: response.code, ...(slot.pending.trigger ? { trigger: slot.pending.trigger } : {}) });
  finish(response.code);
  // Commit the terminal result before any routing work. A subsequent refresh
  // supplies authoritative quota for selective recovery; never blanket-clear.
  await save();
  return response.code;
}
