import { createHash } from "node:crypto";
import { quotaWindows } from "./quota-windows.mjs";
export function expiryMs(value) {
  if (value == null) return Infinity;
  const n = typeof value === "number" ? value * 1000 : Date.parse(value);
  return Number.isFinite(n) ? n : Infinity;
}
export function normalizeUsage(provider, data, now = Date.now()) {
  const rows = quotaWindows(provider, data);
  const windows = rows.filter((w) => w.common);
  const validWindows = windows.length > 0 &&
    windows.every((w) => Number.isFinite(w.used) && w.used >= 0 && w.used <= 100);
  const limit = provider === "codex" ? data.rate_limit : null;
  const malformedFlags = limit && ["allowed", "limit_reached"].some(
    (key) => limit[key] != null && typeof limit[key] !== "boolean",
  );
  let status = "unknown";
  if (!malformedFlags) {
    if (limit?.allowed === false || limit?.limit_reached === true) status = "exhausted";
    else if (limit?.allowed === true) status = "available";
    else if (validWindows) status = windows.some((w) => w.used >= 100) ? "exhausted" : "available";
  }
  // Reset times are estimates. Only the current pull determines availability;
  // an expired estimate must neither unlock quota nor invalidate fresh usage.
  const future = windows.filter((w) => Number.isFinite(w.resetAt) && w.resetAt > now);
  // Routing drains the account whose long (weekly) window refills soonest.
  // Short windows are rate caps the proxy enforces by cooldown; ranking on
  // them rotates new sessions across accounts every few hours instead. The
  // weekly estimate is used as reported: one already in the past is the
  // soonest refill there is, not a reason to fall back to a short window.
  const longest = [...windows].sort(
    (a, b) =>
      (b.durationSeconds ?? 0) - (a.durationSeconds ?? 0) ||
      (Number.isFinite(b.resetAt) ? b.resetAt : -Infinity) - (Number.isFinite(a.resetAt) ? a.resetAt : -Infinity),
  )[0];
  return {
    status,
    windows,
    quotaWindows: rows,
    ...(validWindows && status !== "unknown"
      ? { remaining: Math.max(0, Math.min(...windows.map((w) => 100 - w.used))) }
      : {}),
    nextReset: future.length ? Math.min(...future.map((w) => w.resetAt)) : null,
    rankReset: Number.isFinite(longest?.resetAt) ? longest.resetAt : null,
    rankRemaining: longest && Number.isFinite(longest.used) ? Math.max(0, 100 - longest.used) : null,
  };
}
// Drain-first: the account whose weekly window resets soonest takes every new
// session until it is exhausted; ties go to the account with less left so one
// finishes before the next starts. Accounts without a weekly estimate rank last
// within their status. `drainFirst` (Map id -> ms, from the credit plan via
// liveDrainFirst) moves a refill holder up to rank as if it reset when its
// credit expires: Claude only refills an account at its limit, so the holder
// has to be empty before then.
export const drainKey = (a, drainFirst = null) => {
  const reset = Number.isFinite(a.rankReset) ? a.rankReset : Infinity;
  const until = drainFirst?.get(a.id);
  return Number.isFinite(until) && until < reset ? until : reset;
};
export function rankAccounts(accounts, drainFirst = null) {
  const rank = { available: 0, unknown: 1, exhausted: 2, disabled: 3 };
  return [...accounts].sort(
    (a, b) =>
      rank[a.status] - rank[b.status] ||
      drainKey(a, drainFirst) - drainKey(b, drainFirst) ||
      (a.rankRemaining ?? Infinity) - (b.rankRemaining ?? Infinity) ||
      a.id.localeCompare(b.id),
  );
}
// The credit plan's drain-first entries that still apply: the account is
// enabled, its bank is known and its next listed credit still expires at the
// planned time (within a minute), in the future. A redeemed, lapsed or
// replaced credit drops its entry at once, whatever the forecast still says.
export function liveDrainFirst(plan, accounts, now = Date.now()) {
  if (!(plan instanceof Map) || !plan.size) return null;
  const out = new Map();
  for (const a of accounts) {
    const until = plan.get(a.id);
    if (!Number.isFinite(until) || until <= now || a.disabled || !a.creditsKnown) continue;
    const next = (a.credits ?? [])[0];
    if (next && Math.abs(expiryMs(next.expires_at ?? next.expiresAt) - until) <= 60000) out.set(a.id, until);
  }
  return out.size ? out : null;
}
const WEEK_SECONDS = 604800;
// Whether one account counts as out of quota under a reset policy (see
// resets.mjs). "any": the provider's verdict or any common window at the
// threshold. "weekly": only the all-models weekly window counts, so a 5-hour
// or model-specific limit never spends a reset. Unverified status never counts.
export function accountOut(a, { trigger, headroomPercent }) {
  if (trigger === "weekly") {
    if (!["available", "exhausted"].includes(a.status)) return false;
    const weekly = (a.windows ?? []).filter((w) => w.common && w.durationSeconds === WEEK_SECONDS);
    return weekly.length > 0 && weekly.every((w) => Number.isFinite(w.used)) &&
      weekly.some((w) => w.used >= 100 - headroomPercent);
  }
  return a.status === "exhausted" ||
    (a.status === "available" && Number.isFinite(a.remaining) && a.remaining <= headroomPercent);
}
export const creditKey = (accountId, creditId) => `${accountId}|${creditId}`;
// Codex resets only: a reset restarts the weekly window, so spending one on an
// account whose own refill is under a day away mostly duplicates that refill.
export const EARLY_MIN_REFILL_MS = 24 * 3600 * 1000;
const HOUR_MS = 3600000;
// Claude refills: the credit clears usage but the weekly reset date does not
// move, so the refill must be used up before that fixed date. Time it needs,
// at policy.refillDemand.unitsPerHour (a deliberately low recent demand from
// the capacity forecast, in weighted account-units): the refill itself plus
// the quota still left on accounts that reset sooner, because routing drains
// those first (each counted only up to what that demand could use before its
// own reset; an unread remainder counts as full). Null when demand is unknown.
// "Sooner" uses the routing order (drainKey), so a drain-first holder counts.
export function refillNeedMs(a, pool, demand, now, drainFirst = null) {
  const rate = demand?.unitsPerHour;
  if (!(Number.isFinite(rate) && rate > 0)) return null;
  const weight = (id) => (Number.isFinite(demand.weights?.[id]) && demand.weights[id] > 0 ? demand.weights[id] : 1);
  let units = weight(a.id);
  for (const b of pool) {
    if (b.id === a.id || b.disabled || b.provider !== a.provider) continue;
    if (!Number.isFinite(b.rankReset) || b.rankReset <= now || drainKey(b, drainFirst) >= a.rankReset) continue;
    const left = ["available", "exhausted"].includes(b.status) && Number.isFinite(b.rankRemaining) ? b.rankRemaining / 100 : 1;
    units += Math.min(weight(b.id) * left, (rate * (b.rankReset - now)) / HOUR_MS);
  }
  return (units / rate) * HOUR_MS;
}
// A refill is spent only on an account whose time to its reset covers
// refillNeedMs, capped at policy.maxRunwayPercent of the week (default 50%,
// 3.5 days: never stricter than that, and the rule as is when demand is
// unknown). A credit that expires before the account resets has no later
// chance there, so it is always allowed.
export function refillRunwayOk(a, credit, policy, now, pool = []) {
  if (policy.redemption !== "refill") return true;
  if (!credit || !Number.isFinite(a.rankReset) || a.rankReset <= now) return false;
  if (expiryMs(credit.expires_at ?? credit.expiresAt) <= a.rankReset) return true;
  const weekly = (a.windows ?? []).find((w) => w.common && w.durationSeconds === WEEK_SECONDS);
  const periodMs = (weekly?.durationSeconds ?? WEEK_SECONDS) * 1000;
  const cap = (Number.isFinite(policy.maxRunwayPercent) ? policy.maxRunwayPercent / 100 : 0.5) * periodMs;
  const need = refillNeedMs(a, pool, policy.refillDemand, now, policy.drainFirst);
  return a.rankReset - now >= (need === null ? cap : Math.min(need, cap));
}
// A reset fires only once every enrolled account of the policy's provider is
// out and every bank is known (and, for refills, the account has the runway
// above). `skip` holds creditKey()s that were recently
// refused: they are passed over, and the next best credit is chosen instead.
// `beforeLapse` (credit planner, capacity-forecast.mjs lapsingHolders) holds
// ids of accounts whose own next credit is projected to lapse unused: such an
// account may spend that credit as soon as it is out itself, while other
// accounts still have quota. Every bank must still be known.
export function chooseReset(accounts, policy, now = Date.now(), skip = new Set(), beforeLapse = null) {
  const pool = accounts.filter((a) => a.provider === policy.provider && !a.disabled);
  if (pool.length < policy.expectedCount || pool.some((a) => !a.creditsKnown)) return null;
  if (pool.some((a) => !accountOut(a, policy))) {
    if (!beforeLapse?.size) return null;
    // Out means the weekly all-models window, whatever the trigger: a short
    // window must never spend a weekly credit early. A Codex reset restarts
    // the weekly window, so an account whose own refill is under a day away is
    // left alone unless its credit expires first; a Claude refill needs the
    // runway in refillRunwayOk.
    const naturalReset = (a) => (Number.isFinite(a.rankReset) ? a.rankReset : -Infinity);
    const exp = (c) => expiryMs(c.expires_at ?? c.expiresAt);
    const early = pool
      .filter((a) => beforeLapse.has(a.id) && accountOut(a, { ...policy, trigger: "weekly" }))
      .map((a) => ({ a, credit: (a.credits ?? [])[0] }))
      // Only the credit the projection was about, and only if not backing off.
      .filter(({ a, credit }) => credit && !skip.has(creditKey(a.id, credit.id)))
      .filter(({ a, credit }) => Number.isFinite(a.rankReset) && (policy.redemption === "refill"
        ? refillRunwayOk(a, credit, policy, now, pool)
        : a.rankReset - now >= EARLY_MIN_REFILL_MS || exp(credit) < a.rankReset))
      // As on the all-out path: a credit expiring within 24 h first, otherwise
      // the account whose own refill is furthest away.
      .sort((x, y) => {
        const ux = exp(x.credit) <= now + 24 * 3600 * 1000, uy = exp(y.credit) <= now + 24 * 3600 * 1000;
        if (ux !== uy) return ux ? -1 : 1;
        return (ux ? exp(x.credit) - exp(y.credit) : 0) || naturalReset(y.a) - naturalReset(x.a) || exp(x.credit) - exp(y.credit) || x.a.id.localeCompare(y.a.id);
      });
    return early.length ? { accountId: early[0].a.id, creditId: early[0].credit.id, trigger: "before-lapse" } : null;
  }
  // Each account offers its soonest-expiring credit that was not refused (and,
  // for a refill, that it has the runway to use up).
  const offered = new Map();
  for (const a of pool) {
    const credit = (a.credits ?? []).find((c) => !skip.has(creditKey(a.id, c.id)) && refillRunwayOk(a, c, policy, now, pool));
    if (credit) offered.set(a, credit);
  }
  const choices = [...offered.keys()];
  const expiry = (a) =>
    expiryMs(offered.get(a).expires_at ?? offered.get(a).expiresAt);
  const urgent = choices.filter((a) => expiry(a) <= now + 24 * 3600 * 1000);
  // Spend where the natural reset is furthest away: a Codex reset restarts the
  // window from now and forfeits that refill; a Claude refill lasts until that
  // fixed date, so the furthest one gives it the most time to be used.
  // An unknown reset estimate is never preferred.
  const naturalReset = (a) => (Number.isFinite(a.rankReset) ? a.rankReset : -Infinity);
  const chosen = urgent.length
    ? urgent.sort((a, b) => expiry(a) - expiry(b) || naturalReset(b) - naturalReset(a) || a.id.localeCompare(b.id))[0]
    : choices.sort(
        (a, b) =>
          naturalReset(b) - naturalReset(a) ||
          b.creditCount - a.creditCount ||
          expiry(a) - expiry(b) ||
          a.id.localeCompare(b.id),
      )[0];
  return chosen
    ? { accountId: chosen.id, creditId: offered.get(chosen).id }
    : null;
}
// Stable per-account/per-credit UUID, retained for compatibility with prior redemptions.
export function redemptionId(accountId, creditId) {
  const b = createHash("sha1")
    .update(Buffer.from("6f1c2a9e2d4b4c1e9a7f3b8d5e0c1a42", "hex"))
    .update(`${accountId}:${creditId}`)
    .digest()
    .subarray(0, 16);
  b[6] = (b[6] & 15) | 80;
  b[8] = (b[8] & 63) | 128;
  const h = b.toString("hex");
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}
