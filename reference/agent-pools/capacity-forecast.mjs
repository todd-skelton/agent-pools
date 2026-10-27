import { createHash } from "node:crypto";
import { PROVIDERS } from "./providers.mjs";

// Capacity forecast: per-provider quota supply against observed demand.
//
// One point of Codex weekly and one of Claude weekly are different token
// quantities, so nothing here compares raw numbers across providers. Each
// provider is judged against its own supply and reduced to an ordinal state:
//   conserve  projected to run out within the horizon (or out now)
//   spend     projected to let capacity expire unused (weekly windows that
//             reset with headroom, reset credits that lapse unredeemed)
//   normal    neither
//   unknown   not enough data to say
// Each state has separate enter/exit tests and a minimum dwell, so it does not
// chatter. A balance block turns the pair of states into a direction and a
// step count (0..maxSteps) that moves one step per stepHours; a consumer maps
// steps to an ordered, Todd-ruled list of rows. AgentPools only publishes;
// it never routes a dispatch.
//
// Units: one account-unit is one account's full window scaled by its capacity
// weight (plan size relative to the provider's reference plan).

export const FORECAST_FORMAT = "capacity-forecast/v1";
export const HOUR = 3600000;
export const DEFAULTS = Object.freeze({
  halfLifeHours: 12,
  lookbackHours: 168,
  horizonDays: 14,
  maxHorizonDays: 35,
  minObservedHours: 6,
  margin: 0.25,
  spendEnterUnits: 0.5,
  spendExitUnits: 0.25,
  minDwellHours: 2,
  stepHours: 6,
  maxSteps: 4,
  staleAfterMinutes: 15,
  // A provider whose accounts all failed to read keeps its last state this long.
  staleHoldMinutes: 30,
  // Credit planner: plan at the highest of the EWMA and the 24 h / 72 h means
  // times (1 + planMargin); publish holders only with planMinObservedHours of
  // demand evidence and only for credits expiring within planWindowDays.
  planMargin: 0.5,
  planMinObservedHours: 72,
  planWindowDays: 14,
  weights: {},
  // "shadow": published for review only; "active": consumers may act on the
  // balance block. Todd flips this in install.json capacityForecast.mode.
  mode: "shadow",
});
export const FORECAST_MODES = Object.freeze(["shadow", "active"]);
// Upper bounds keep a mistyped setting from stalling the supervisor.
const MAX = Object.freeze({ halfLifeHours: 168, lookbackHours: 720, horizonDays: 35, maxHorizonDays: 60, minObservedHours: 168,
  margin: 4, spendEnterUnits: 100, spendExitUnits: 100, minDwellHours: 48, stepHours: 168, maxSteps: 20, staleAfterMinutes: 1440, staleHoldMinutes: 1440, planMargin: 4, planMinObservedHours: 168, planWindowDays: 35 });
// Reset estimates jitter (Claude by milliseconds, Codex by minutes); only a
// larger move is a new window.
export const RESET_JITTER_MS = HOUR;

export function forecastConfig(input) {
  const raw = input && typeof input === "object" && !Array.isArray(input) ? input : {};
  const problems = [];
  if (input !== undefined && input !== null && raw !== input) problems.push("capacityForecast must be an object; using defaults");
  const out = { ...DEFAULTS };
  for (const [key, def] of Object.entries(DEFAULTS)) {
    if (raw[key] === undefined) continue;
    const v = raw[key];
    const ok = key === "weights"
      ? v && typeof v === "object" && !Array.isArray(v) && Object.values(v).every((n) => Number.isFinite(n) && n > 0)
      : key === "mode" ? FORECAST_MODES.includes(v)
      : Number.isFinite(v) && v >= 0 && v <= (MAX[key] ?? Infinity) && (key !== "maxSteps" || Number.isInteger(v));
    if (ok) out[key] = v;
    else problems.push("capacityForecast." + key + " is invalid; using " + JSON.stringify(def));
  }
  out.weights = { ...out.weights };
  return { config: out, problems };
}

export function planOf(accountId) {
  const m = /-([a-z]+)\.json$/i.exec(accountId ?? "");
  return m ? m[1].toLowerCase() : null;
}

// Override order: exact account id, "<provider>:<plan>", the adapter's plan
// default, then 1.
export function capacityWeight(provider, accountId, weights = {}) {
  const plan = planOf(accountId);
  const candidates = [weights[accountId], plan ? weights[provider + ":" + plan] : undefined, plan ? PROVIDERS[provider]?.capacity?.planWeights?.[plan] : undefined];
  for (const v of candidates) if (Number.isFinite(v) && v > 0) return v;
  return 1;
}

export const weeklyWindowId = (provider) => PROVIDERS[provider]?.capacity?.weeklyWindowId ?? null;
const startsOnFirstUse = (provider) => !!PROVIDERS[provider]?.capacity?.windowStartsOnFirstUse;
// "refill" (Claude): a redeemed credit clears usage but keeps the reset date.
const refills = (provider) => PROVIDERS[provider]?.redemption === "refill";
export const defaultMaxRunwayPercent = (provider) => PROVIDERS[provider]?.resetDefaults?.maxRunwayPercent ?? 50;

// Consumption increments in account-units, plus per-hour exhaustion flags,
// from an ordered sample stream. A window that reset (used fell, or resetAt
// moved by more than the jitter tolerance) contributes its new used value.
export function replaySamples(samples, weights = {}) {
  const last = new Map();
  const events = [];
  const marks = [];
  for (const s of samples) {
    if (!s || !Number.isFinite(s.used) || typeof s.window !== "string") continue;
    const at = Date.parse(s.at);
    if (!Number.isFinite(at)) continue;
    const key = s.provider + "|" + s.window + "|" + s.accountId;
    const prev = last.get(key);
    last.set(key, { used: s.used, resetAt: s.resetAt });
    marks.push({ at, provider: s.provider, window: s.window, accountId: s.accountId, used: s.used });
    if (!prev) continue;
    const moved = Number.isFinite(s.resetAt) && Number.isFinite(prev.resetAt) && Math.abs(s.resetAt - prev.resetAt) > RESET_JITTER_MS;
    const delta = moved || s.used < prev.used ? s.used : s.used - prev.used;
    if (delta > 0) events.push({ at, provider: s.provider, window: s.window, units: (capacityWeight(s.provider, s.accountId, weights) * delta) / 100 });
  }
  return { events, marks };
}

// Hours (epoch hour numbers) at whose end every roster account of the class
// was at 100% (or the provider's weekly class was). Demand cannot be observed
// while a class is out, so those hours are skipped rather than read as zero.
export function censoredHours(marks, provider, window, roster, startHour, endHour) {
  const weekly = weeklyWindowId(provider);
  const ids = new Set(roster);
  const latest = { cls: new Map(), weekly: new Map() };
  const out = new Set();
  const rel = marks.filter((m) => m.provider === provider && ids.has(m.accountId) && (m.window === window || m.window === weekly)).sort((a, b) => a.at - b.at);
  let i = 0;
  const allOut = (map) => ids.size > 0 && [...ids].every((id) => (map.get(id) ?? 0) >= 100);
  for (let h = startHour; h <= endHour; h++) {
    const end = (h + 1) * HOUR;
    while (i < rel.length && rel[i].at < end) {
      const m = rel[i++];
      if (m.window === window) latest.cls.set(m.accountId, m.used);
      if (m.window === weekly) latest.weekly.set(m.accountId, m.used);
    }
    if (allOut(latest.cls) || allOut(latest.weekly)) out.add(h);
  }
  return out;
}

// Demand in account-units/hour: EWMA of hourly consumption over completed,
// uncensored hours (idle hours count as zero), plus trailing means. The
// current hour is partial, so it is reported separately and never scaled up.
// firstHour is the first hour the class was sampled at all; from there idle
// hours count as zero, so an unused provider reads as zero demand, not unknown.
export function demandRate(events, censored, { now = Date.now(), halfLifeHours = 12, lookbackHours = 168, firstHour = null } = {}) {
  const current = Math.floor(now / HOUR);
  const end = current - 1;
  const start = end - lookbackHours + 1;
  let currentHourUnits = 0;
  for (const e of events) if (Math.floor(e.at / HOUR) === current) currentHourUnits += e.units;
  const buckets = new Map();
  for (const e of events) {
    const h = Math.floor(e.at / HOUR);
    if (h < start || h > end) continue;
    buckets.set(h, (buckets.get(h) ?? 0) + e.units);
  }
  const firstSeen = Math.min(Number.isFinite(firstHour) ? firstHour : Infinity, ...buckets.keys());
  if (!Number.isFinite(firstSeen) || firstSeen > end) return { ewma: 0, last24h: 0, last72h: 0, last168h: 0, observedHours: 0, censoredHours: censored.size, currentHourUnits };
  const first = Math.max(start, firstSeen);
  const decay = Math.pow(0.5, 1 / halfLifeHours);
  let ewma = null, observed = 0;
  for (let h = first; h <= end; h++) {
    if (censored.has(h)) continue;
    const v = buckets.get(h) ?? 0;
    ewma = ewma === null ? v : decay * ewma + (1 - decay) * v;
    observed++;
  }
  const trailing = (hours) => {
    let sum = 0, n = 0;
    for (let h = end - hours + 1; h <= end; h++) { if (censored.has(h)) continue; n++; sum += buckets.get(h) ?? 0; }
    return n ? sum / n : 0;
  };
  return { ewma: ewma ?? 0, last24h: trailing(24), last72h: trailing(72), last168h: trailing(Math.min(168, lookbackHours)), observedHours: observed, censoredHours: censored.size, currentHourUnits };
}

// Supply for one window class. Claude lists only the next claimable grant
// while creditCount counts every remaining reset; unlisted ones take the
// last listed expiry. A bank with a count but nothing listed (organization
// unknown, cooling down, or a Claude grant withheld until the server reports
// the account at its limit) cannot be claimed automatically, so it is
// reported as unmodeled rather than counted as supply.
export function supplyFor(accounts, provider, window, weights = {}) {
  return accounts
    .filter((a) => a.provider === provider && !a.disabled)
    .map((a) => {
      const w = (a.quotaWindows ?? []).find((x) => x.id === window && Number.isFinite(x.used) && Number.isFinite(x.durationSeconds) && x.durationSeconds > 0);
      if (!w) return null;
      const listed = (a.credits ?? []).map((c) => Date.parse(c.expires_at ?? c.expiresAt ?? "")).map((t) => (Number.isFinite(t) ? t : Infinity)).sort((x, y) => x - y);
      const count = Math.max(listed.length, Number.isInteger(a.creditCount) ? a.creditCount : 0);
      const credits = listed.length ? Array.from({ length: count }, (_, i) => listed[Math.min(i, listed.length - 1)]) : [];
      const unmodeledCredits = count - credits.length;
      const used = Math.min(100, Math.max(0, w.used));
      return {
        id: a.id,
        weight: capacityWeight(provider, a.id, weights),
        used,
        resetAt: Number.isFinite(w.resetAt) ? w.resetAt : null,
        periodMs: w.durationSeconds * 1000,
        firstUse: startsOnFirstUse(provider),
        floating: startsOnFirstUse(provider) && used === 0,
        refill: refills(provider),
        credits,
        unmodeledCredits,
        stale: !!a.usageStale || a.status === "unknown",
        scope: w.scope ?? null,
        label: w.label ?? null,
      };
    })
    .filter(Boolean);
}

// Hour-by-hour simulation at a constant demand rate. Accounts drain
// earliest-reset-first (routing.mjs rankAccounts), floating (unused,
// first-use) windows last. A credit is redeemed only when demand is unmet,
// on the holder whose natural reset is furthest away (routing.mjs
// chooseReset). A reset (Codex) restarts that window; a refill (Claude) clears
// its usage but keeps its reset date, and is spent only when the time to that
// date covers the refill at the simulated demand (at most maxRunwayPercent of
// the window), or when the credit expires before that date (routing.mjs
// refillRunwayOk). Credits are only spent once every account is drained, so no
// sooner-resetting quota competes with the refill here.
// rate is units per hour, or a function of the hour's start time (a pace plan).
// grants (scenario only): times at which one new credit is assumed to arrive,
// each valid grantLifetimeHours, given to the account whose natural reset is
// furthest away then (where a refill has the most room to be used).
// Credit plan only (refillCreditPlan):
//   drainFirst   Map id -> credit expiry: while that account still banks that
//                credit and it expires before the account resets, the account
//                drains as if it reset at the expiry (routing.mjs rankAccounts)
//   earlyRefill  Set of ids that redeem their refill as soon as they are out,
//                when it can be used before their reset counting the quota
//                drained ahead of them (routing.mjs refillRunwayOk)
export function simulate(supply, rate, { now = Date.now(), horizonHours = 336, useCredits = true, maxRunwayPercent = 50, grants = [], grantLifetimeHours = 168, drainFirst = null, earlyRefill = null } = {}) {
  const A = supply.map((a) => ({ ...a, credits: [...a.credits], resetAt: a.floating ? null : a.resetAt ?? now + a.periodMs }));
  const pendingGrants = [...grants].sort((x, y) => x - y);
  let creditsGranted = 0;
  const rateAt = typeof rate === "function" ? rate : () => rate;
  const key = (a) => drainKey(a, drainFirst);
  const runwayOk = (a, t, rate) => {
    if (!a.refill) return true;
    if (a.resetAt === null || a.resetAt <= t) return false;
    if (a.credits[0] <= a.resetAt) return true;
    const need = rate > 0 ? (a.weight / rate) * HOUR : Infinity;
    return a.resetAt - t >= Math.min(need, (maxRunwayPercent / 100) * a.periodMs);
  };
  let outageHours = 0, firstOutageAt = null, expiringUnits = 0, creditsUsed = 0, creditsExpired = 0, creditUnitsExpired = 0;
  const redemptions = [], lapses = [], outages = [];
  let unmetUnits = 0;
  for (let h = 0; h < horizonHours; h++) {
    const t = now + h * HOUR;
    for (const a of A) {
      while (a.resetAt !== null && a.resetAt <= t) {
        expiringUnits += (a.weight * (100 - a.used)) / 100;
        a.used = 0;
        if (a.firstUse) { a.resetAt = null; a.floating = true; }
        else a.resetAt += a.periodMs;
      }
      while (a.credits.length && a.credits[0] <= t) { a.credits.shift(); creditsExpired++; creditUnitsExpired += a.weight; lapses.push({ id: a.id, at: t }); }
    }
    while (pendingGrants.length && pendingGrants[0] <= t && A.length) {
      pendingGrants.shift();
      const to = [...A].sort((x, y) => (y.resetAt ?? Infinity) - (x.resetAt ?? Infinity))[0];
      to.credits.push(t + grantLifetimeHours * HOUR);
      to.credits.sort((x, y) => x - y);
      creditsGranted++;
    }
    const r = rateAt(t);
    let need = r;
    if (useCredits && earlyRefill?.size) {
      for (const a of A) {
        if (!earlyRefill.has(a.id) || !a.refill || !a.credits.length || a.used < 100 - 1e-9) continue;
        if (!refillRunway(a, A, t, r, maxRunwayPercent, key)) continue;
        a.credits.shift();
        creditsUsed++;
        redemptions.push({ id: a.id, at: t, early: true });
        a.used = 0;
      }
    }
    const drain = () => {
      const order = [...A].sort((x, y) => key(x) - key(y));
      for (const a of order) {
        if (need <= 1e-12) break;
        const room = (a.weight * (100 - a.used)) / 100;
        if (room <= 1e-12) continue;
        const take = Math.min(room, need);
        if (a.resetAt === null) { a.resetAt = t + a.periodMs; a.floating = false; }
        a.used += (take / a.weight) * 100;
        need -= take;
      }
    };
    drain();
    while (need > 1e-12 && useCredits) {
      // routing.mjs chooseReset: a credit expiring within 24 h first,
      // otherwise the holder whose natural refill is furthest away.
      const holders = A.filter((a) => a.credits.length && runwayOk(a, t, r));
      const urgent = holders.filter((a) => a.credits[0] <= t + 24 * HOUR).sort((x, y) => x.credits[0] - y.credits[0]);
      const holder = urgent[0] ?? holders.sort((x, y) => (y.resetAt ?? Infinity) - (x.resetAt ?? Infinity))[0];
      if (!holder) break;
      holder.credits.shift();
      creditsUsed++;
      redemptions.push({ id: holder.id, at: t });
      holder.used = 0;
      if (!holder.refill) holder.resetAt = null;
      drain();
    }
    if (need > 1e-12) { unmetUnits += need; outageHours++; firstOutageAt ??= t; outages.push(t); }
  }
  const remainingUnits = A.reduce((s, a) => s + (a.weight * (100 - a.used)) / 100, 0);
  const creditUnitsLeft = A.reduce((s, a) => s + a.weight * a.credits.length, 0);
  return { unmetUnits, remainingUnits, creditUnitsLeft, outageHours, firstOutageAt, expiringUnits, creditsUsed, creditsExpired, creditUnitsExpired, wastedUnits: expiringUnits + creditUnitsExpired, redemptions, lapses, creditsGranted, outages };
}

// Drain order key in simulate(): the weekly reset, or for a drain-first account
// the expiry of the credit it was boosted for, while it still banks that
// credit and that comes first (routing.mjs rankAccounts).
function drainKey(a, drainFirst) {
  const reset = a.resetAt ?? Infinity;
  const until = drainFirst?.get(a.id);
  return Number.isFinite(until) && a.credits[0] === until && until < reset ? until : reset;
}

// Whether account a, out now at time t, can use a refill before its own reset
// at the given rate (routing.mjs refillRunwayOk with refillNeedMs): the refill
// plus the quota drained ahead of it (each account only up to what the rate
// can use before that account resets), capped at maxRunwayPercent of the
// window. A credit expiring before the reset has no later chance there.
export function refillRunway(a, all, t, rate, maxRunwayPercent, key = (x) => x.resetAt ?? Infinity) {
  if (a.resetAt === null || a.resetAt <= t) return false;
  if (a.credits[0] <= a.resetAt) return true;
  const cap = (maxRunwayPercent / 100) * a.periodMs;
  if (!(rate > 0)) return a.resetAt - t >= cap;
  let units = a.weight;
  for (const b of all) {
    if (b === a || b.id === a.id || b.resetAt === null || b.resetAt <= t || !(key(b) < a.resetAt)) continue;
    units += Math.min((b.weight * (100 - b.used)) / 100, (rate * (b.resetAt - t)) / HOUR);
  }
  return a.resetAt - t >= Math.min((units / rate) * HOUR, cap);
}

// Credit plan for refill providers (Claude). The question for each banked
// refill is a counterfactual, not "does it lapse at some demand": does acting
// now give more total use than not acting? Total use is work done plus quota
// still available at the end plus credits still banked at the end. Each
// action is judged at three paces (low recent demand, measured demand, the
// planning demand) and taken only when it is never worse there (within
// tieUnits) and better at one of them. Actions:
//   holders     accounts that are out now and should redeem now rather than
//               wait for the all-accounts-out rule; also checked against the
//               refill runway at the low pace, as the router will check it
//   drainFirst  accounts not yet out whose credit expires before their reset:
//               routing drains them as if they reset at the expiry, so they
//               are empty (Claude only refills an account at its limit) in time
// credits lists every account's next credit with the chosen action, its
// outcome at the measured pace under the plan, and the lowest steady pace at
// which the plan uses it (needUnitsPerHour; null when no pace up to
// PACE.maxFactor x natural supply does). In the simulations every account
// with a drain deadline redeems as soon as it is out (earlyRefill), since the
// planner will judge it again then.
export const CREDIT_PLAN = Object.freeze({ tieUnits: 0.02, atRiskHours: 72 });
// keep (Map id -> expiry, the previous plan's drainFirst): a boost already in
// place stays while it is not worse at any pace, so routing does not flip
// between accounts as the demand estimates move.
export function refillCreditPlan(supply, paces, { now, horizonHours, maxRunwayPercent, keep = null }) {
  const rates = [paces.low, paces.measured, paces.high].filter((r) => Number.isFinite(r) && r > 0);
  if (!supply.length || !rates.length) return null;
  const runHours = horizonHours + Math.ceil(Math.max(...supply.map((a) => a.periodMs)) / HOUR);
  const opts = { now, horizonHours: runHours, maxRunwayPercent, useCredits: true };
  const total = (sup, rate, extra) => {
    const x = simulate(sup, rate, { ...opts, ...extra });
    return x.remainingUnits + x.creditUnitsLeft - x.unmetUnits;
  };
  // a dominates b: never worse at any pace, better at one (or, with
  // notWorse, only never worse).
  const better = (a, b, notWorse = false) => {
    let gain = notWorse;
    for (const r of rates) {
      const d = a(r) - b(r);
      if (d < -CREDIT_PLAN.tieUnits) return false;
      if (d > CREDIT_PLAN.tieUnits) gain = true;
    }
    return gain;
  };
  const resetOf = (a) => a.resetAt ?? Infinity;
  const banked = supply.filter((a) => a.credits.length && Number.isFinite(a.credits[0]) && a.credits[0] > now);
  const usable = banked.filter((a) => !a.stale);
  const deadline = usable.filter((a) => a.used < 100 && a.credits[0] < resetOf(a));
  const early = new Set(deadline.map((a) => a.id));
  const redeemed = (sup, id) => sup.map((b) => (b.id === id ? { ...b, used: 0, credits: b.credits.slice(1) } : b));
  const low = rates[0];
  const holders = [];
  for (const a of usable.filter((x) => x.used >= 100)) {
    if (!refillRunway(a, supply, now, paces.low > 0 ? paces.low : 0, maxRunwayPercent)) continue;
    const after = redeemed(supply, a.id);
    if (better((r) => total(after, r, { earlyRefill: early }), (r) => total(supply, r, { earlyRefill: early }))) holders.push(a.id);
  }
  let planned = supply;
  for (const id of holders) planned = redeemed(planned, id);
  const drainFirst = new Map();
  for (const a of [...deadline].sort((x, y) => x.credits[0] - y.credits[0] || (x.id < y.id ? -1 : 1))) {
    const withA = new Map([...drainFirst, [a.id, a.credits[0]]]);
    const kept = keep?.get(a.id) === a.credits[0];
    if (better((r) => total(planned, r, { drainFirst: withA, earlyRefill: early }), (r) => total(planned, r, { drainFirst, earlyRefill: early }), kept)) drainFirst.set(a.id, a.credits[0]);
  }
  // First event for an account's next credit in a run: used, lapsed, or still banked.
  const fate = (x, id) => {
    const used = x.redemptions.find((e) => e.id === id), lapsed = x.lapses.find((e) => e.id === id);
    if (used && (!lapsed || used.at <= lapsed.at)) return "used";
    return lapsed ? "lapses" : "banked";
  };
  const natural = supply.reduce((s, a) => s + a.weight / (a.periodMs / HOUR), 0);
  const credits = banked.map((a) => {
    const action = holders.includes(a.id) ? "redeem-now" : drainFirst.has(a.id) ? "drain-first" : "wait";
    const drainBy = a.credits[0] < resetOf(a) ? a.credits[0] : null;
    let outcome = null, needUnitsPerHour = null;
    if (action === "redeem-now") { outcome = "used"; }
    else {
      const plan = { drainFirst, earlyRefill: early };
      if (paces.measured > 0) outcome = fate(simulate(planned, paces.measured, { ...opts, ...plan }), a.id);
      // The lowest steady pace that uses it, with this account drained first
      // where that applies and redeeming once it is out (more pace is what
      // turns those actions on).
      const forced = { drainFirst: drainBy !== null && a.used < 100 ? new Map([...drainFirst, [a.id, a.credits[0]]]) : drainFirst, earlyRefill: new Set([...early, a.id]) };
      const saved = (r) => fate(simulate(planned, r, { ...opts, ...forced }), a.id) === "used";
      const top = PACE.maxFactor * natural, eps = PACE.precision * natural;
      if (natural > 0 && saved(top)) {
        let lo = 0, hi = top;
        while (hi - lo > eps) { const mid = (lo + hi) / 2; if (saved(mid)) hi = mid; else lo = mid; }
        needUnitsPerHour = hi;
      }
    }
    return { accountId: a.id, expiresAt: a.credits[0], resetAt: a.resetAt, out: a.used >= 100, unitsLeft: (a.weight * (100 - a.used)) / 100, stale: !!a.stale, action, drainBy, outcome, needUnitsPerHour };
  });
  return { holders: holders.sort(), drainFirst, credits };
}

// What-if runs over the same horizon and demand as the state decision, for
// people and agents reading the forecast. They never set the state:
//   naturalOnly    window resets alone, no credit spent
//   bankedCredits  the banked credits spent as the pool spends them (the
//                  projection the state is judged on, when credits are modeled)
//   weeklyGrant    bankedCredits plus an assumed new credit every week
//                  (first one a week from now). An assumption: providers do
//                  not promise one.
export const SCENARIO_GRANT = Object.freeze({ everyHours: 168, lifetimeHours: 168 });
export function scenarioRuns(supply, rate, opts) {
  const iso = (t) => new Date(t).toISOString();
  const events = (list) => list.slice(0, 20).map((e) => ({ id: e.id, at: iso(e.at) }));
  const view = (r) => ({ outageHours: r.outageHours, firstOutageAt: r.firstOutageAt ? iso(r.firstOutageAt) : null, creditsUsed: r.creditsUsed, creditsLapsing: r.creditsExpired, redemptions: events(r.redemptions), lapses: events(r.lapses) });
  const grants = [];
  for (let t = opts.now + SCENARIO_GRANT.everyHours * HOUR; t < opts.now + opts.horizonHours * HOUR; t += SCENARIO_GRANT.everyHours * HOUR) grants.push(t);
  const natural = simulate(supply.map((a) => ({ ...a, credits: [] })), rate, { ...opts, useCredits: false });
  const banked = simulate(supply, rate, { ...opts, useCredits: true });
  const granted = simulate(supply, rate, { ...opts, useCredits: true, grants, grantLifetimeHours: SCENARIO_GRANT.lifetimeHours });
  return {
    horizonHours: opts.horizonHours,
    unitsPerHour: Math.round(rate * 1e5) / 1e5,
    naturalOnly: view(natural),
    bankedCredits: view(banked),
    weeklyGrant: { ...view(granted), assumption: `one new credit every ${SCENARIO_GRANT.everyHours / 24} days, each valid ${SCENARIO_GRANT.lifetimeHours / 24} days`, creditsGranted: granted.creditsGranted },
  };
}

// Pace plan: how fast to use a provider to get the most out of it. Quota is
// lost when a window resets with quota unused or a credit lapses; running out
// (a shortfall) loses nothing, it only means waiting. So the objective is
// total usage over the run: work done plus quota still available at the end.
// Timing matters for credits: a Codex reset re-dates the window, so it adds
// about (time left on the window it replaces) / (window length); a Claude
// refill lasts only until the account's own reset date. Both are worth most
// when a window is drained soon after it opens, which a faster pace does.
// Everything comes from simulate(), so drain order and the redemption rules
// match the pool. The run is one window longer than the horizon or the last
// credit expiry, whichever is later.
//   maxUnits   usage when quota is used as fast as possible (PACE.burst x
//              natural supply: an account drains within the hour)
//   atDemand   usage at the observed pace, and what it leaves unused
//   fullUse    lowest steady pace within PACE.tie of maxUnits, or null when
//              only a pace above PACE.maxFactor x natural supply gets there
//   then       when fullUse spends credits: from its last redemption on, the
//              lowest steady pace that still reaches fullUse's usage, and
//              never below natural supply
//   steps[k-1] lowest steady pace at which at most credits-k credits lapse
//   curve      usage at a ladder of steady paces
//   noGap      highest steady pace that never runs out, and its usage: the
//              price of continuity
// Units are weighted account-weeks per hour, as elsewhere in this file.
// precision is a share of natural supply: 0.002 is about 0.006 account-weeks
// per week for three accounts, well under what anyone reads off it.
// reuseMinutes: a plan costs up to a few hundred ms on the service's event
// loop, so it is reused until it is this old or the credits or accounts change.
export const PACE = Object.freeze({ maxFactor: 10, burst: 100, gridSteps: 40, precision: 0.002, tie: 0.01, reuseMinutes: 10, ladder: [0.5, 1, 1.5, 2, 3, 5, 10] });
export function pacePlan(supply, demand, { now, horizonHours, maxRunwayPercent }) {
  const natural = supply.reduce((s, a) => s + a.weight / (a.periodMs / HOUR), 0);
  if (!supply.length || !(natural > 0)) return null;
  const runHours = horizonHours + Math.ceil(Math.max(...supply.map((a) => a.periodMs)) / HOUR);
  const end = now + runHours * HOUR;
  const expiring = supply.flatMap((a) => a.credits.filter((c) => c <= end));
  const credits = expiring.length;
  const opts = { now, horizonHours: runHours, maxRunwayPercent, useCredits: true };
  const eps = PACE.precision * natural, maxRate = PACE.maxFactor * natural, step = maxRate / PACE.gridSteps;
  const view = (rate) => {
    const x = simulate(supply, rate, opts);
    let asked = 0;
    for (let h = 0; h < runHours; h++) asked += typeof rate === "function" ? rate(now + h * HOUR) : rate;
    const done = asked - x.unmetUnits;
    return { unitsPerHour: rate, usedUnits: done + x.remainingUnits, doneUnits: done, creditsUsed: x.creditsUsed, creditsLapsing: x.creditsExpired, shortHours: x.outageHours, firstShortAt: x.firstOutageAt, run: x };
  };
  const max = view(PACE.burst * natural);
  const full = (v) => v.usedUnits >= max.usedUnits * (1 - PACE.tie);
  // Lowest rate on a grid where ok holds, refined by bisection. The result
  // always satisfies ok; it is the lowest only where ok is monotonic.
  const lowest = (ok, from = 0) => {
    let lo = from, hi = null;
    for (let r = Math.max(from, step); r <= maxRate + 1e-12; r += step) { if (ok(view(r))) { hi = r; break; } lo = r; }
    if (hi === null) return null;
    while (hi - lo > eps) { const mid = (lo + hi) / 2; if (ok(view(mid))) hi = mid; else lo = mid; }
    return view(hi);
  };
  const fullUse = lowest(full);
  const steps = [];
  let from = 0;
  for (let k = 1; k <= credits; k++) {
    const v = lowest((x) => x.creditsLapsing <= credits - k, from);
    if (!v) { for (let j = k; j <= credits; j++) steps.push({ credits: j, reachable: false, unitsPerHour: null, spentBy: null }); break; }
    steps.push({ credits: k, reachable: true, unitsPerHour: v.unitsPerHour, spentBy: v.run.redemptions.at(-1)?.at ?? null });
    from = Math.max(0, v.unitsPerHour - step);
  }
  // Highest steady pace that never runs out (0 never does).
  let a = 0, b = maxRate;
  if (view(b).shortHours === 0) a = b;
  else while (b - a > eps) { const mid = (a + b) / 2; if (view(mid).shortHours === 0) a = mid; else b = mid; }
  const strip = ({ run, ...v }) => v;
  let then = null;
  if (fullUse && fullUse.run.redemptions.length) {
    const from = fullUse.run.redemptions.at(-1).at + HOUR;
    const plan = (r) => view((t) => (t < from ? fullUse.unitsPerHour : r));
    let lo = 0, hi = fullUse.unitsPerHour;
    while (hi - lo > eps) { const mid = (lo + hi) / 2; if (plan(mid).usedUnits >= fullUse.usedUnits - 1e-6) hi = mid; else lo = mid; }
    // Never below natural supply: less than that leaves windows resetting with
    // quota unused once the run's leftover is gone.
    then = { from, unitsPerHour: Math.max(hi, natural) };
  }
  return {
    horizonHours: runHours, naturalUnitsPerHour: natural, demandUnitsPerHour: demand || 0, credits,
    maxUnits: max.usedUnits, maxCreditsLapsing: max.creditsLapsing,
    atDemand: strip(view(demand || 0)),
    fullUse: fullUse ? strip(fullUse) : null, then,
    steps,
    curve: PACE.ladder.map((m) => strip(view(m * natural))),
    noGap: strip(view(a)),
  };
}

// States are judged on a fixed near window so both providers are measured
// over the same span. Credits that would lapse beyond it are reported by a
// separate long run out to the last credit expiry (capped), as a flag only.
export function creditHorizonHours(supply, now, config) {
  const latestCredit = Math.max(0, ...supply.flatMap((a) => a.credits.filter(Number.isFinite)));
  const creditHours = latestCredit > now ? Math.ceil((latestCredit - now) / HOUR) + 1 : 0;
  return Math.min(config.maxHorizonDays * 24, creditHours);
}

// Next state given the previous one. Hysteresis: conserve enters on an
// outage at observed demand and holds while one remains at demand x
// (1 + margin); spend enters only if waste persists even at demand x
// (1 + margin) and holds while waste at observed demand stays above the exit
// level. Moves are one step (conserve <-> normal <-> spend), except that an
// exhausted provider enters conserve at once. A dwell delays non-urgent moves.
const LADDER = ["conserve", "normal", "spend"];
export function nextState(prev, m, config, now) {
  const exhaustedNow = m.exhaustedNow;
  let want;
  if (m.unknown) want = "unknown";
  else if (exhaustedNow || m.outage > 0) want = "conserve";
  else if (prev?.state === "conserve" && m.outageAtMargin > 0) want = "conserve";
  else if (m.wastedAtMargin >= config.spendEnterUnits) want = "spend";
  else if (prev?.state === "spend" && m.wasted >= config.spendExitUnits) want = "spend";
  else want = "normal";
  // An unknown spell does not reset the ladder: moves count from the last known state.
  const basis = prev?.state === "unknown" ? prev.lastKnownState : prev?.state;
  const from = LADDER.indexOf(basis), to = LADDER.indexOf(want);
  if (from >= 0 && to >= 0 && Math.abs(to - from) > 1 && !(want === "conserve" && exhaustedNow)) want = "normal";
  const since = prev?.since ? Date.parse(prev.since) : null;
  const dwelling = prev?.state && prev.state !== "unknown" && since !== null && now - since < config.minDwellHours * HOUR;
  if (prev?.state && want !== prev.state && dwelling && !(want === "conserve" && exhaustedNow) && want !== "unknown")
    return { state: prev.state, since: prev.since, pendingState: want };
  const lastKnown = want === "unknown" ? (prev?.state && prev.state !== "unknown" ? prev.state : prev?.lastKnownState) : undefined;
  const keep = lastKnown ? { lastKnownState: lastKnown } : {};
  if (prev?.state === want) return { state: want, since: prev.since, ...keep };
  return { state: want, since: new Date(now).toISOString(), ...keep };
}

export function forecastClass({ provider, window, accounts, replay, config, now, prev, creditsEnabled = true, maxRunwayPercent = defaultMaxRunwayPercent(provider) }) {
  const supply = supplyFor(accounts, provider, window, config.weights);
  const roster = supply.map((a) => a.id);
  const end = Math.floor(now / HOUR);
  const censored = censoredHours(replay.marks, provider, window, roster, end - config.lookbackHours + 1, end);
  const events = replay.events.filter((e) => e.provider === provider && e.window === window);
  let firstMark = Infinity;
  for (const m of replay.marks) if (m.provider === provider && m.window === window && m.at < firstMark) firstMark = m.at;
  const demand = demandRate(events, censored, { now, halfLifeHours: config.halfLifeHours, lookbackHours: config.lookbackHours, firstHour: Number.isFinite(firstMark) ? Math.floor(firstMark / HOUR) : null });
  const horizonHours = Math.round(config.horizonDays * 24);
  const opts = { now, horizonHours, useCredits: creditsEnabled, maxRunwayPercent };
  // Where credits cannot restore the window (model-specific limits, reset
  // automation off) they are neither supply nor waste.
  const simSupply = creditsEnabled ? supply : supply.map((a) => ({ ...a, credits: [] }));
  const base = simulate(simSupply, demand.ewma, opts);
  const atMargin = simulate(simSupply, demand.ewma * (1 + config.margin), opts);
  const longHours = creditHorizonHours(simSupply, now, config);
  const long = creditsEnabled && longHours > horizonHours ? simulate(simSupply, demand.ewma, { ...opts, horizonHours: longHours }) : base;
  // Credit planner: holders whose credits lapse unused even at demand +margin
  // under the all-accounts-out rule. Each may spend its soonest credit as soon
  // as it is out itself (routing.mjs chooseReset beforeLapse).
  // The 7-day mean keeps a multi-day lull from reading as lasting slack.
  const planRate = Math.max(demand.ewma, demand.last24h, demand.last72h, demand.last168h ?? 0) * (1 + config.planMargin);
  const planEvidence = demand.observedHours >= config.planMinObservedHours;
  // Refill timing (routing.mjs refillNeedMs): the lowest of the recent demand
  // measures, so a refill is only judged usable if it would be even at the
  // quietest recent pace. Published with the same evidence bar as the plan.
  const refillRate = Math.min(demand.ewma, demand.last24h, demand.last72h, demand.last168h ?? Infinity);
  // Refill providers plan by counterfactual (refillCreditPlan); a reset
  // provider keeps the lapse test at the planning demand.
  const refillPlan = creditsEnabled && refills(provider) && planEvidence
    ? refillCreditPlan(simSupply, { low: refillRate, measured: demand.ewma, high: planRate }, { now, horizonHours: Math.max(horizonHours, longHours), maxRunwayPercent,
      keep: new Map((prev?.creditPlan?.drainFirst ?? []).map((d) => [d?.accountId, Date.parse(d?.until)])) })
    : null;
  const planRun = !refills(provider) && creditsEnabled && planEvidence && longHours > 0 ? simulate(simSupply, planRate, { ...opts, horizonHours: Math.max(horizonHours, longHours) }) : null;
  const planUntil = now + config.planWindowDays * 24 * HOUR;
  const planHolders = refillPlan ? refillPlan.holders : planRun ? [...new Set(planRun.lapses.filter((l) => l.at <= planUntil + HOUR).map((l) => l.id))].sort() : [];
  const refill = creditsEnabled && refills(provider) && planEvidence && refillRate > 0
    ? { unitsPerHour: Math.round(refillRate * 1e5) / 1e5, weights: Object.fromEntries(supply.map((a) => [a.id, a.weight])) }
    : null;
  const remainingUnits = supply.reduce((s, a) => s + (a.weight * (100 - a.used)) / 100, 0);
  const exhaustedNow = supply.length > 0 && supply.every((a) => a.used >= 100);
  const allStale = supply.length > 0 && supply.every((a) => a.stale);
  const freshAt = allStale ? prev?.freshAt ?? null : new Date(now).toISOString();
  // A short read outage holds the last state instead of flipping to unknown.
  const holdStale = !!(allStale && prev?.state && prev.state !== "unknown" && freshAt && now - Date.parse(freshAt) < config.staleHoldMinutes * 60000);
  const unknown = supply.length === 0 || (allStale && !holdStale) || (demand.observedHours < config.minObservedHours && !exhaustedNow);
  const naturalRefills = supply.filter((a) => !a.floating && Number.isFinite(a.resetAt) && a.used > 0).map((a) => a.resetAt).sort((x, y) => x - y);
  const naturalSupplyPerHour = supply.reduce((s, a) => s + a.weight / (a.periodMs / HOUR), 0);
  const m = { unknown, exhaustedNow, outage: base.outageHours, outageAtMargin: atMargin.outageHours, wasted: base.wastedUnits, wastedAtMargin: atMargin.wastedUnits };
  const st = holdStale ? { state: prev.state, since: prev.since } : nextState(prev, m, config, now);
  const round = (x, d = 4) => (Number.isFinite(x) ? Math.round(x * 10 ** d) / 10 ** d : x);
  return {
    provider, window,
    scope: supply[0]?.scope ?? null,
    state: st.state, since: st.since, ...(st.pendingState ? { pendingState: st.pendingState } : {}), ...(st.lastKnownState ? { lastKnownState: st.lastKnownState } : {}),
    freshAt, ...(holdStale ? { stale: true } : {}),
    demand: { unitsPerHour: round(demand.ewma, 5), last24h: round(demand.last24h, 5), last72h: round(demand.last72h, 5), last168h: round(demand.last168h, 5), currentHourUnits: round(demand.currentHourUnits, 5), observedHours: demand.observedHours, censoredHours: demand.censoredHours },
    supply: {
      accounts: supply.length,
      weightedAccounts: round(supply.reduce((s, a) => s + a.weight, 0), 3),
      remainingUnits: round(remainingUnits, 3),
      naturalUnitsPerHour: round(naturalSupplyPerHour, 5),
      nextRefillAt: naturalRefills[0] ? new Date(naturalRefills[0]).toISOString() : null,
      credits: supply.reduce((s, a) => s + a.credits.length, 0),
      unmodeledCredits: supply.reduce((s, a) => s + a.unmodeledCredits, 0),
      creditExpiries: [...new Set(supply.flatMap((a) => a.credits.filter(Number.isFinite)).map((t) => new Date(t).toISOString()))].sort(),
    },
    projection: {
      horizonHours,
      creditsModeled: creditsEnabled,
      outageHours: base.outageHours,
      firstOutageAt: base.firstOutageAt ? new Date(base.firstOutageAt).toISOString() : null,
      expiringUnits: round(base.expiringUnits, 3),
      creditsUsed: base.creditsUsed,
      creditsExpiringUnused: base.creditsExpired,
      wastedUnits: round(base.wastedUnits, 3),
      outageHoursAtMargin: atMargin.outageHours,
      wastedUnitsAtMargin: round(atMargin.wastedUnits, 3),
      demandToNaturalSupply: naturalSupplyPerHour > 0 ? round(demand.ewma / naturalSupplyPerHour, 3) : null,
    },
    // Out to the last credit expiry at observed demand: credits projected to
    // lapse unredeemed. A flag for credit planning; it does not set the state.
    creditOutlook: { horizonHours: Math.max(horizonHours, longHours), creditsUsed: long.creditsUsed, creditsLapsing: long.creditsExpired, firstOutageAt: long.firstOutageAt ? new Date(long.firstOutageAt).toISOString() : null },
    creditPlan: { unitsPerHour: round(planRate, 5), demandFactor: round(1 + config.planMargin, 3), evidence: planEvidence, windowDays: config.planWindowDays, holders: planHolders, refill,
      ...(refills(provider) ? {
        method: "counterfactual",
        // False when there was nothing to plan with (too little evidence,
        // automation off); consumers then fall back to creditOutlook.
        planned: !!refillPlan,
        drainFirst: [...(refillPlan?.drainFirst ?? new Map())].map(([accountId, until]) => ({ accountId, until: new Date(until).toISOString() })),
        credits: (refillPlan?.credits ?? []).map((c) => ({ ...c, expiresAt: new Date(c.expiresAt).toISOString(), resetAt: Number.isFinite(c.resetAt) ? new Date(c.resetAt).toISOString() : null,
          drainBy: c.drainBy === null ? null : new Date(c.drainBy).toISOString(), unitsLeft: round(c.unitsLeft, 3), needUnitsPerHour: c.needUnitsPerHour === null ? null : round(c.needUnitsPerHour, 5) })),
      } : { method: "lapse-at-plan-rate" }) },
    // Banked credits are modeled here even when the state run leaves them
    // out (reset automation off), so the scenarios show what they are worth.
    scenarios: { ...scenarioRuns(supply, demand.ewma, { now, horizonHours, maxRunwayPercent }), creditsInStateRun: creditsEnabled },
    // A signal for dispatchers, like the balance; the pool does not enforce it.
    pace: paceFor(prev?.pace, simSupply, demand.ewma, { now, horizonHours: Math.max(horizonHours, longHours), maxRunwayPercent }),
  };
}

// Bumped whenever the plan's shape or meaning changes, so a plan saved by an
// older version is never reused.
const PACE_FORMAT = 2;
export function paceKey(supply, maxRunwayPercent) {
  return digest([PACE_FORMAT, PACE, maxRunwayPercent, supply.map((a) => [a.id, a.weight, a.credits.length, a.credits[0] ?? null]).sort((x, y) => (x[0] < y[0] ? -1 : 1))]);
}
function paceFor(prev, supply, demand, opts) {
  const key = paceKey(supply, opts.maxRunwayPercent);
  const age = opts.now - Date.parse(prev?.computedAt ?? "");
  if (prev?.key === key && age >= 0 && age < PACE.reuseMinutes * 60000) return prev;
  return paceView(pacePlan(supply, demand, opts), key, opts.now);
}
function paceView(p, key, now) {
  if (!p) return null;
  const iso = (t) => (Number.isFinite(t) ? new Date(t).toISOString() : null);
  const r5 = (x) => (Number.isFinite(x) ? Math.round(x * 1e5) / 1e5 : null);
  const r3 = (x) => (Number.isFinite(x) ? Math.round(x * 1e3) / 1e3 : null);
  const v = (x) => (x ? { unitsPerHour: r5(x.unitsPerHour), usedUnits: r3(x.usedUnits), lostUnits: r3(Math.max(0, p.maxUnits - x.usedUnits)), creditsUsed: x.creditsUsed, creditsLapsing: x.creditsLapsing, shortHours: x.shortHours, firstShortAt: iso(x.firstShortAt) } : null);
  return {
    computedAt: iso(now), key, objective: "most total usage: work done plus quota still available at the end of the run; running short loses nothing",
    horizonHours: p.horizonHours, demandUnitsPerHour: r5(p.demandUnitsPerHour), naturalUnitsPerHour: r5(p.naturalUnitsPerHour), credits: p.credits,
    maxUnits: r3(p.maxUnits), maxCreditsLapsing: p.maxCreditsLapsing,
    atDemand: v(p.atDemand), fullUse: v(p.fullUse), noGap: v(p.noGap),
    then: p.then ? { from: iso(p.then.from), unitsPerHour: r5(p.then.unitsPerHour) } : null,
    steps: p.steps.map((s) => ({ credits: s.credits, reachable: s.reachable, unitsPerHour: r5(s.unitsPerHour), spentBy: iso(s.spentBy) })),
    curve: p.curve.map(v),
  };
}

// Direction and step count. Direction moves load toward a provider that is not
// short when the other is (conserve), or toward a provider that is wasting
// capacity when the other is normal. Steps ramp one per stepHours and unwind
// the same way before the direction can reverse.
export function nextBalance(prev, states, config, now) {
  const [a, b] = ["codex", "claude"];
  const sa = states[a], sb = states[b];
  let target = null, reason = "balanced";
  if (sa === "unknown" || sb === "unknown" || !sa || !sb) reason = "insufficient-data";
  else if (sa === "conserve" && sb !== "conserve") { target = b; reason = a + "-conserve"; }
  else if (sb === "conserve" && sa !== "conserve") { target = a; reason = b + "-conserve"; }
  else if (sa === "conserve" && sb === "conserve") reason = "both-conserve";
  else if (sa === "spend" && sb === "normal") { target = a; reason = a + "-spend"; }
  else if (sb === "spend" && sa === "normal") { target = b; reason = b + "-spend"; }
  const p = prev ?? { toward: null, steps: 0, lastStepAt: null };
  // Missing data freezes the balance; it neither ramps nor unwinds.
  if (reason === "insufficient-data")
    return { toward: p.toward ?? null, steps: p.steps ?? 0, maxSteps: config.maxSteps, target: null, reason, lastStepAt: p.lastStepAt ?? null, conserve: [a, b].filter((x) => states[x] === "conserve") };
  const last = p.lastStepAt ? Date.parse(p.lastStepAt) : -Infinity;
  const due = now - last >= config.stepHours * HOUR;
  let toward = p.toward, steps = p.steps ?? 0, lastStepAt = p.lastStepAt ?? null;
  if (due) {
    if (target && (toward === target || steps === 0)) {
      if (steps < config.maxSteps) { toward = target; steps += 1; lastStepAt = new Date(now).toISOString(); }
    } else if (steps > 0) {
      steps -= 1; lastStepAt = new Date(now).toISOString();
      if (steps === 0) toward = null;
    }
  }
  steps = Math.min(steps, config.maxSteps);
  if (steps === 0) toward = null;
  return { toward, steps, maxSteps: config.maxSteps, target, reason, lastStepAt, conserve: [a, b].filter((x) => states[x] === "conserve") };
}

export function digest(value) {
  return createHash("sha256").update(JSON.stringify(value)).digest("hex").slice(0, 16);
}

// Full forecast. previous is the last published document (hysteresis memory).
// The credit planner's input for one provider: holder ids from a fresh,
// known forecast, or null (no early redemption) when there is none. This
// deliberately ignores capacityForecast.mode: shadow governs routing
// consumers; the planner has its own switch (resetPolicy.redeemBeforeLapse,
// Todd ruling 2026-10-02).
export function lapsingHolders(forecast, provider, now = Date.now()) {
  const p = forecast?.providers?.[provider];
  if (!p || p.state === "unknown" || p.stale || !(Date.parse(forecast.staleAfter) > now)) return null;
  const holders = p.creditPlan?.holders;
  return Array.isArray(holders) && holders.length ? new Set(holders) : null;
}
// Drain-first input for one provider (routing.mjs rankAccounts): account id ->
// credit expiry (ms) from a fresh, known forecast, or null. The router applies
// an entry only while that account still lists a credit with that expiry
// (routing.mjs liveDrainFirst).
export function drainFirstOf(forecast, provider, now = Date.now()) {
  const p = forecast?.providers?.[provider];
  if (!p || p.state === "unknown" || p.stale || !(Date.parse(forecast.staleAfter) > now)) return null;
  const list = Array.isArray(p.creditPlan?.drainFirst) ? p.creditPlan.drainFirst : [];
  const out = new Map(list.map((d) => [d?.accountId, Date.parse(d?.until)]).filter(([id, t]) => typeof id === "string" && Number.isFinite(t) && t > now));
  return out.size ? out : null;
}
// Refill timing input for one provider (routing.mjs refillNeedMs): the low
// demand rate and capacity weights from a fresh, known forecast, or null, in
// which case the refill rule falls back to maxRunwayPercent.
export function refillDemandOf(forecast, provider, now = Date.now()) {
  const p = forecast?.providers?.[provider];
  if (!p || p.state === "unknown" || p.stale || !(Date.parse(forecast.staleAfter) > now)) return null;
  const r = p.creditPlan?.refill;
  return r && Number.isFinite(r.unitsPerHour) && r.unitsPerHour > 0 ? { unitsPerHour: r.unitsPerHour, weights: r.weights ?? {} } : null;
}

export function buildForecast({ accounts, samples, config, now = Date.now(), previous = null, resetPolicies = {}, resetAutomation = true }) {
  const replay = replaySamples(samples, config.weights);
  const providers = [...new Set(accounts.map((a) => a.provider))].filter((p) => PROVIDERS[p]?.capacity).sort();
  const out = {};
  for (const provider of providers) {
    const weekly = weeklyWindowId(provider);
    const creditsEnabled = resetAutomation !== false && resetPolicies[provider]?.enabled !== false;
    const maxRunwayPercent = resetPolicies[provider]?.maxRunwayPercent ?? defaultMaxRunwayPercent(provider);
    const prevP = previous?.providers?.[provider];
    const main = forecastClass({ provider, window: weekly, accounts, replay, config, now, prev: prevP, creditsEnabled, maxRunwayPercent });
    // Model-specific limits (for example Claude's Fable weekly) are forecast
    // separately; credits are not assumed to restore them.
    const modelIds = [...new Set(accounts.filter((a) => a.provider === provider).flatMap((a) => (a.quotaWindows ?? []).filter((w) => !w.common && w.id !== weekly && Number.isFinite(w.durationSeconds) && w.durationSeconds >= 86400).map((w) => w.id)))].sort();
    const modelWindows = modelIds.map((id) => {
      const prevW = prevP?.modelWindows?.find((w) => w.window === id);
      const f = forecastClass({ provider, window: id, accounts, replay, config, now, prev: prevW, creditsEnabled: false });
      return { window: id, scope: f.scope, state: f.state, since: f.since, ...(f.pendingState ? { pendingState: f.pendingState } : {}), ...(f.lastKnownState ? { lastKnownState: f.lastKnownState } : {}), freshAt: f.freshAt, ...(f.stale ? { stale: true } : {}), demand: f.demand, supply: { remainingUnits: f.supply.remainingUnits, nextRefillAt: f.supply.nextRefillAt }, projection: { creditsModeled: f.projection.creditsModeled, creditsUsed: f.projection.creditsUsed, outageHours: f.projection.outageHours, firstOutageAt: f.projection.firstOutageAt, wastedUnits: f.projection.wastedUnits } };
    });
    out[provider] = { ...main, modelWindows };
  }
  const states = Object.fromEntries(Object.entries(out).map(([p, f]) => [p, f.state]));
  const balance = nextBalance(previous?.balance, states, config, now);
  const decision = { mode: config.mode, states, modelWindowStates: Object.fromEntries(Object.entries(out).map(([p, f]) => [p, Object.fromEntries(f.modelWindows.map((w) => [w.scope ?? w.window, w.state]))])), balance: { toward: balance.toward, steps: balance.steps, maxSteps: balance.maxSteps, conserve: balance.conserve } };
  return {
    format: FORECAST_FORMAT,
    generatedAt: new Date(now).toISOString(),
    staleAfter: new Date(now + config.staleAfterMinutes * 60000).toISOString(),
    mode: config.mode,
    config: { ...config },
    decisionDigest: digest(decision),
    decision,
    balance,
    providers: out,
  };
}
