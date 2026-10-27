// Pool-wide usage overlay for the Codex desktop's /wham/usage poll.
//
// The desktop app-server fetches {chatgpt_base_url}/wham/usage for the signed-in
// account and renders rate_limit / additional_rate_limits verbatim (verified
// 2026-09-15: used_percent is not clamped, custom limit_name / metered_feature
// buckets surface in rateLimitsByLimitId, rate_limit_upsell is passed through).
// This module rewrites that payload so the banner and usage panel reflect the
// pool instead of one account. The upstream body stays the base so account_id,
// credits, plan_type and reset credits remain those of the signed-in account.

// How the pool's remaining capacity maps onto the desktop's 0-100 meter:
//   capped  - remaining capacity in account-equivalents, capped to the last
//             one: 0% used until less than one account's worth is left, then an
//             honest countdown to the banner (default).
//   sum     - the same without the cap; goes negative above one account's worth.
//   average - mean used percentage across accounts.
import { normalizeUsage } from "./routing.mjs";
import { isUsableUsage } from "./usage.mjs";

export const USED_PERCENT = {
  capped: (used, totalRemaining) => Math.max(0, 100 - totalRemaining),
  sum: (used, totalRemaining) => 100 - totalRemaining,
  average: (used) => used.reduce((s, v) => s + v, 0) / used.length,
};
export const POOL_MODES = Object.keys(USED_PERCENT);

const finite = (v) => typeof v === "number" && Number.isFinite(v);

// How long a failed poll may lean on the account's last successful read.
// Within this window the account is "unverified": its last-known rows stay on
// the meter and it blocks any claim that the pool is exhausted, so one failed
// provider read cannot turn the desktop banner on (2026-09-16: a 10 s
// management timeout on the only account with capacity produced "Codex pool
// exhausted (2 accounts)" while inference kept succeeding). Past the window
// the account is dropped as before, so a token that has died for good cannot
// hide a real exhaustion indefinitely.
export const UNVERIFIED_GRACE_MS = 30 * 60 * 1000;

function pooledAccounts(accounts, provider, now) {
  const pool = accounts.filter((a) => a.provider === provider && !a.disabled);
  const verified = pool.filter((a) => !a.usageStale && a.status !== "unknown" && Array.isArray(a.quotaWindows) && a.quotaWindows.length > 0);
  const unverified = pool.filter((a) => !verified.includes(a) && withinGrace(a, now));
  const withRows = [...verified, ...unverified].filter((a) => Array.isArray(a.quotaWindows) && a.quotaWindows.length > 0);
  return { pool, withRows, verified, unverified: unverified.length };
}

function withinGrace(account, now) {
  const last = Date.parse(account.usageUpdatedAt);
  return Number.isFinite(last) && last <= now && now - last <= UNVERIFIED_GRACE_MS;
}

// One window (e.g. "Codex / Weekly") aggregated across accounts.
// `rows` are { w: window, verdict } pairs. The provider's explicit verdict
// ("exhausted" / "available", from normalizeUsage) overrides the percentage
// for common windows: allowed:false at 0% is no capacity, allowed:true at
// 100% is capacity. Scoped windows carry no verdict and use the percentage.
function aggregateWindow(rows, mode, now, unverified = 0) {
  const metered = rows.filter((r) => finite(r.w.used));
  if (metered.length === 0) return null;
  const remaining = metered.map((r) => r.verdict === "exhausted" ? 0 : r.verdict === "available" ? Math.max(1, 100 - r.w.used) : Math.max(0, 100 - r.w.used));
  // Effective used percentages (verdict-adjusted) feed every mode alike.
  const used = remaining.map((v) => 100 - v);
  const totalRemaining = remaining.reduce((s, v) => s + v, 0);
  const usedPercent = USED_PERCENT[mode](used, totalRemaining);
  // Next to a capacity meter, "resets on" means the next time capacity comes
  // back, so use the soonest future reset of any account.
  const resets = metered.map((r) => r.w.resetAt).filter((t) => finite(t) && t > now);
  const resetAt = resets.length ? Math.min(...resets) : null;
  return {
    // The app-server decodes used_percent as an integer; a fractional value
    // fails the whole /wham/usage decode (verified 2026-09-15).
    used_percent: Math.round(usedPercent),
    limit_window_seconds: metered.find((r) => finite(r.w.durationSeconds))?.w.durationSeconds ?? null,
    reset_after_seconds: resetAt ? Math.max(0, Math.round((resetAt - now) / 1000)) : null,
    reset_at: resetAt ? Math.round(resetAt / 1000) : null,
    accounts: used.length,
    available: remaining.filter((v) => v > 0).length,
    unverified,
    total_remaining_percent: Math.round(totalRemaining),
    // Exhaustion needs evidence from every account; unverified ones abstain.
    exhausted: totalRemaining <= 0 && unverified === 0,
  };
}

// Group windows by scope, then by window id suffix (primary/secondary) or duration.
function scopedWindows(accounts, mode, now, unverified = 0) {
  const byScope = new Map();
  for (const a of accounts)
    for (const w of a.quotaWindows) {
      const key = w.scope ?? "";
      const slot = w.id.endsWith(":secondary_window") ? "secondary" : w.id.endsWith(":primary_window") ? "primary" : w.durationSeconds === 604800 ? "weekly" : w.durationSeconds === 18000 ? "session" : w.label ?? w.id;
      if (!byScope.has(key)) byScope.set(key, { common: w.common, slots: new Map() });
      const scope = byScope.get(key);
      if (!scope.slots.has(slot)) scope.slots.set(slot, []);
      scope.slots.get(slot).push({ w, verdict: w.common && ["exhausted", "available"].includes(a.status) ? a.status : null });
    }
  const out = [];
  for (const [scope, { common, slots }] of byScope) {
    const windows = [...slots].map(([slot, rows]) => [slot, aggregateWindow(rows, mode, now, unverified)]).filter(([, w]) => w);
    if (windows.length) out.push({ scope, common, windows });
  }
  return out;
}

// Capacity is decided per account, not per window slot: an account with a
// verdict of "available", or with every common window below 100%, has
// capacity. A slot that only some accounts report (e.g. a secondary window)
// cannot exhaust the pool on its own.
function accountHasCapacity(a) {
  if (a.status === "exhausted") return false;
  if (a.status === "available") return true;
  const common = (a.quotaWindows ?? []).filter((w) => w.common && finite(w.used));
  return common.length > 0 && common.every((w) => w.used < 100);
}

// When capacity returns: each account recovers once its last blocking common
// window (used >= 100) resets; the pool recovers with the earliest account.
// An exhausted account with no window at 100% (verdict-only) recovers at its
// soonest future common reset, the only estimate available.
function soonestCommonReset(accounts, now) {
  const perAccount = accounts.map((a) => {
    const future = (a.quotaWindows ?? []).filter((w) => w.common && finite(w.resetAt) && w.resetAt > now);
    const blocking = future.filter((w) => finite(w.used) && w.used >= 100).map((w) => w.resetAt);
    if (blocking.length) return Math.max(...blocking);
    return future.length ? Math.min(...future.map((w) => w.resetAt)) : null;
  }).filter(finite);
  return perAccount.length ? Math.min(...perAccount) : null;
}

function rateLimitBlock(windows, poolExhausted = null) {
  // Codex payload uses primary_window / secondary_window; longest-lived window
  // is the one that decides "out of usage", so it becomes primary when the
  // upstream naming is unavailable (Claude-derived buckets).
  const ordered = [...windows].sort(([, a], [, b]) => (b.limit_window_seconds ?? 0) - (a.limit_window_seconds ?? 0));
  const named = Object.fromEntries(windows);
  const primary = named.primary ?? ordered[0]?.[1] ?? null;
  const secondary = named.secondary ?? (ordered.length > 1 && !named.primary ? ordered[1][1] : null);
  const exhausted = poolExhausted ?? [primary, secondary].some((w) => w?.exhausted);
  const strip = (w) => w && { used_percent: w.used_percent, limit_window_seconds: w.limit_window_seconds, reset_after_seconds: w.reset_after_seconds, reset_at: w.reset_at };
  return {
    block: { allowed: !exhausted, limit_reached: exhausted, primary_window: strip(primary), secondary_window: strip(secondary) },
    primary,
  };
}

const slug = (s) => String(s).toLowerCase().replace(/[^a-z0-9]+/g, "_").replace(/^_|_$/g, "");

export function poolLabel(scope, { accounts, available, unverified = 0, total_remaining_percent }) {
  const note = unverified > 0 ? `, ${unverified} unverified` : "";
  return `${scope} pool: ${available} of ${accounts} available${note}, ${total_remaining_percent}% of ${accounts * 100}% left`;
}

// The signed-in desktop account is usually one of the pooled accounts, and the
// upstream body the desktop just fetched is the freshest reading of it. Use it
// as verified evidence for that account so a failed supervisor poll (or an
// expired grace window) cannot contradict what the provider just said.
function withUpstreamEvidence(accounts, upstream, now) {
  const id = typeof upstream?.account_id === "string" ? upstream.account_id : null;
  if (!id) return accounts;
  return accounts.map((a) => {
    if (a.provider !== "codex" || a.disabled || a.accountId !== id) return a;
    const fresh = normalizeUsage("codex", upstream, now);
    if (!isUsableUsage(fresh)) return a;
    return { ...a, ...fresh, status: fresh.status, usageStale: false, usageUpdatedAt: new Date(now).toISOString(), error: undefined };
  });
}

// Build the overlay. `upstream` is the signed-in account's real /wham/usage
// body (already parsed). Returns the merged object, or upstream unchanged
// when the pool has no fresh Codex data.
export function poolUsage(upstream, accounts, { mode = "capped", now = Date.now(), includeClaude = true } = {}) {
  if (!POOL_MODES.includes(mode)) throw new Error(`Unknown pool mode: ${mode}`);
  accounts = withUpstreamEvidence(accounts, upstream, now);
  const codex = pooledAccounts(accounts, "codex", now);
  if (codex.verified.length === 0) return upstream;
  const scopes = scopedWindows(codex.withRows, mode, now, codex.unverified);
  const common = scopes.find((s) => s.common);
  if (!common) return upstream;
  // The common pool is exhausted only with fresh evidence from every account
  // and no account left with capacity.
  const poolExhausted = codex.unverified === 0 && !codex.verified.some(accountHasCapacity);
  const { block: rate_limit, primary } = rateLimitBlock(common.windows, poolExhausted);
  // When exhausted, capacity returns at the soonest reset of any common window.
  const recoverAt = poolExhausted ? soonestCommonReset(codex.verified, now) : null;
  if (poolExhausted && recoverAt) {
    for (const w of [rate_limit.primary_window, rate_limit.secondary_window].filter(Boolean)) {
      w.reset_at = Math.round(recoverAt / 1000);
      w.reset_after_seconds = Math.max(0, Math.round((recoverAt - now) / 1000));
    }
  }
  const additional = [];
  additional.push({
    limit_name: poolLabel("Codex", primary),
    metered_feature: "codex_pool",
    rate_limit: { ...rate_limit },
    normal_model_slug: null,
  });
  for (const s of scopes.filter((s) => !s.common)) {
    const { block, primary: p } = rateLimitBlock(s.windows);
    const upstreamEntry = (upstream.additional_rate_limits ?? []).find((e) => e.limit_name === s.scope);
    additional.push({
      limit_name: poolLabel(s.scope, p),
      metered_feature: upstreamEntry?.metered_feature ?? `pool_${slug(s.scope)}`,
      rate_limit: block,
      normal_model_slug: upstreamEntry?.normal_model_slug ?? null,
    });
  }
  if (includeClaude) {
    const claude = pooledAccounts(accounts, "claude", now);
    for (const s of claude.verified.length ? scopedWindows(claude.withRows, mode, now, claude.unverified) : []) {
      const claudeExhausted = s.common ? claude.unverified === 0 && !claude.verified.some(accountHasCapacity) : null;
      const { block, primary: p } = rateLimitBlock(s.windows, claudeExhausted);
      if (claudeExhausted) {
        const at = soonestCommonReset(claude.verified, now);
        if (at) for (const w of [block.primary_window, block.secondary_window].filter(Boolean)) { w.reset_at = Math.round(at / 1000); w.reset_after_seconds = Math.max(0, Math.round((at - now) / 1000)); }
      }
      const name = s.scope === "All models" ? "Claude" : `Claude ${s.scope}`;
      additional.push({ limit_name: poolLabel(name, p), metered_feature: `pool_${slug(name)}`, rate_limit: block, normal_model_slug: null });
    }
  }
  const exhausted = rate_limit.limit_reached;
  return {
    ...upstream,
    rate_limit,
    additional_rate_limits: additional,
    rate_limit_reached_type: exhausted ? { type: "rate_limit_reached", details: "pool" } : null,
    rate_limit_upsell: exhausted
      ? {
          banner_type: "pro_rate_limit_reached",
          title: `Codex pool exhausted (${primary.accounts} accounts)`,
          description: "Every pooled account is at its limit. The soonest window resets on {time}.",
          ctas: [],
          reset_at: rate_limit.primary_window?.reset_at ?? null,
          referral: null,
          request_url: null,
        }
      : null,
    model_usage: exhausted ? upstream.model_usage ?? {} : {},
    pool: { mode, accounts: primary.accounts, available: primary.available, unverified: primary.unverified, total_remaining_percent: primary.total_remaining_percent },
  };
}
