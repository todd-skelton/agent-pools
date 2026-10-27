import { randomUUID } from "node:crypto";
import { expiryMs } from "./routing.mjs";

// Claude Code reads its reset bank as the `cedar_ember` block of the OAuth
// usage endpoint, and the block is only filled in for requests that identify as
// Claude Code (otherwise eligible:false, ineligible_reason:"surface"). The API
// is undocumented: anything unexpected leaves the bank unknown, which blocks
// automation instead of guessing.
export const CLAUDE_USAGE_URL = "https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1";
// The version is the one the proxy presents for the installed Claude Code
// (see providers.mjs clientVersion). Without one the read goes out without
// Claude Code identity: usage still reads, the bank reports "surface".
export const claudeClientHeaders = (version) => version ? {
  "User-Agent": `claude-cli/${version} (external, cli)`,
  "x-app": "cli",
} : {};
const GRANT_ID = /^[a-z0-9_-]{1,40}$/;
const ORG_UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

// Returns null when the block is missing or unreadable (bank unknown).
export function claudeBank(block, now = Date.now()) {
  if (!block || typeof block !== "object" || typeof block.eligible !== "boolean") return null;
  const grants = Array.isArray(block.grants) ? block.grants : [];
  const usable = grants.filter((g) =>
    g && typeof g === "object" && typeof g.id === "string" && GRANT_ID.test(g.id) &&
    g.usable_now === true && g.paused !== true &&
    Number.isInteger(g.resets_left) && g.resets_left > 0 &&
    expiryMs(g.ends_at) > now && !(Date.parse(g.starts_at) > now));
  const coolingUntil = expiryMs(block.cooldown_until);
  const cooling = coolingUntil !== Infinity && coolingUntil > now;
  // The server redeems only the grant it names next.
  const named = block.eligible && !cooling ? usable.find((g) => g.id === block.next_grant_id) : null;
  // A grant the server will only spend at its own limit is withheld until the
  // server agrees the account is there, so local and server views cannot loop.
  const next = named && !(named.use_requires_limit === true && block.at_limit !== true) ? named : null;
  return {
    credits: next
      ? [{ id: next.id, expires_at: next.ends_at ?? null, label: typeof next.label === "string" ? next.label : "" }]
      : [],
    count: block.eligible ? usable.reduce((sum, g) => sum + g.resets_left, 0) : 0,
    note: !block.eligible
      ? `Resets not offered to this client (${block.ineligible_reason ?? "unknown"})`
      : cooling ? `Resets cooling down until ${block.cooldown_until}` : null,
  };
}

// Anthropic reports the subscription tier to a Claude Code sign-in
// (/api/oauth/profile) but not the billed price (subscription_details answers
// 403), so the price is the public list price for that tier.
export const CLAUDE_PROFILE_URL = "https://api.anthropic.com/api/oauth/profile";
export const CLAUDE_LIST_PRICES = Object.freeze({
  source: "claude.com/pricing, read 2026-10-15",
  usdPerMonth: Object.freeze({ pro: 20, max_5x: 100, max_20x: 200 }),
});
const CLAUDE_PLAN_LABELS = Object.freeze({ pro: "Claude Pro", max_5x: "Claude Max 5x", max_20x: "Claude Max 20x", max: "Claude Max" });
export function claudePlan(profile) {
  const org = profile?.organization && typeof profile.organization === "object" ? profile.organization : {};
  const tier = typeof org.rate_limit_tier === "string" ? org.rate_limit_tier : "";
  const type = typeof org.organization_type === "string" ? org.organization_type : "";
  const plan = /max_20x/.test(tier) ? "max_20x" : /max_5x/.test(tier) ? "max_5x"
    : type === "claude_max" || profile?.account?.has_claude_max === true ? "max"
    : type === "claude_pro" || profile?.account?.has_claude_pro === true ? "pro" : null;
  if (!plan) return null;
  const amount = CLAUDE_LIST_PRICES.usdPerMonth[plan];
  return {
    plan,
    label: CLAUDE_PLAN_LABELS[plan],
    status: typeof org.subscription_status === "string" ? org.subscription_status : null,
    billingPeriod: null,
    renewsAt: null,
    price: Number.isFinite(amount) ? { usdPerMonth: amount, amount, currency: "USD", source: "list", detail: CLAUDE_LIST_PRICES.source + "; Anthropic does not report the billed price to Claude Code sign-ins" } : null,
  };
}

export function claudeOrganization(authFile) {
  const id = authFile?.organization_uuid;
  return typeof id === "string" && ORG_UUID.test(id) ? id : null;
}
export const claudeResetUrl = (organization) =>
  `https://api.anthropic.com/api/organizations/${organization}/reset_rate_limits`;
export const claudeResetBody = (pending) =>
  ({ program: "cedar_ember", grant_id: pending.creditId, request_id: pending.requestId });

// Claim response -> lifecycle outcome (see providers.mjs). A reason that says
// the server itself is unsure maps to unknown, which keeps the request id.
const UNCONFIRMED = ["reset_unconfirmed", "stamp_indeterminate"];
export function claudeClaimOutcome(response) {
  if (UNCONFIRMED.includes(response?.reason)) return { code: "unknown" };
  const code = {
    reset: "reset",
    already_used: "already_redeemed",
    not_limited: "nothing_to_reset",
    ineligible: "no_credit",
    unavailable: "no_credit",
    cooldown: "deferred",
  }[response?.result];
  return { code: code ?? "unknown" };
}
// 429 and malformed-request statuses are refused before the claim runs. Auth,
// server and transport failures leave the outcome unknown and are rethrown.
export function claudeClaimError(error) {
  if (error?.status === 429) return { code: "deferred" };
  if ([400, 404, 405, 422].includes(error?.status)) return { code: "rejected" };
  throw error;
}

export const claude = Object.freeze({
  id: "claude",
  name: "Claude",
  loginId: "anthropic",
  clientId: "claude-code",
  usageUrl: CLAUDE_USAGE_URL,
  // A Claude credit is a refill: it clears the account's usage but leaves its
  // weekly reset date where it was, so the refill has to be used up before
  // that fixed date. A refill goes only to an account with time to use it up
  // at a low recent demand, never needing more than maxRunwayPercent of the
  // week (50% = 3.5 days, also the rule while demand is unknown), unless the
  // credit expires before that account's reset anyway (routing.mjs refillRunwayOk).
  redemption: "refill",
  resetDefaults: Object.freeze({ enabled: true, trigger: "weekly", headroomPercent: 0, redeemBeforeLapse: true, maxRunwayPercent: 50 }),
  // Capacity forecast facts (capacity-forecast.mjs): fixed weekly windows;
  // the Fable weekly (limit:2) is forecast as a separate model window.
  capacity: Object.freeze({ weeklyWindowId: "limit:1", windowStartsOnFirstUse: false, planWeights: Object.freeze({}) }),
  maxHeadroomPercent: 100,
  // Claude also answers short model rate limits (Retry-After of a minute or
  // two) with a quota cooldown; those lapse on their own and clearing them
  // early only buys another 429. A weekly block, or one a banked reset lifted,
  // outlasts this.
  minRecoverableBlockMs: 10 * 60000,
  accountId: (file) => file.id,
  // Identify as the installed Claude Code: the server withholds the bank from
  // other clients and can gate on version.
  headers: async (file, io) => ({ "anthropic-beta": "oauth-2025-04-20", ...claudeClientHeaders(await io.clientVersion("claude")) }),
  modelRecoveryBlocked: (usage, normalized) =>
    normalized.quotaWindows.some((w) => !w.common && !(Number.isFinite(w.used) && w.used < 100)),
  async readPlan(io, file) {
    const plan = claudePlan(await io.read(file, CLAUDE_PROFILE_URL));
    if (!plan) throw new Error("Subscription tier not reported");
    return plan;
  },
  async readBank(io, file, usage, now) {
    const bank = claudeBank(usage.cedar_ember, now);
    if (!bank) return { known: false, credits: [], count: 0, note: "Reset bank not reported" };
    // A credit that cannot be claimed must not be offered: the lifecycle would
    // journal an attempt it can never send.
    if (bank.credits.length && !claudeOrganization(await io.authFile(file).catch(() => null)))
      return { known: true, credits: [], count: bank.count, note: "Organization unknown; resets cannot be claimed automatically" };
    return { known: true, ...bank };
  },
  async redeem(io, file, pending) {
    const organization = claudeOrganization(await io.authFile(file));
    // Credits are only offered with a readable organization, so this is a
    // retry of an earlier attempt: keep its identity for reconciliation.
    if (!organization) throw new Error("Claude organization unknown; original identity retained");
    try {
      return claudeClaimOutcome(await io.read(file, claudeResetUrl(organization), claudeResetBody(pending)));
    } catch (e) {
      return claudeClaimError(e);
    }
  },
  // A request_id may replay its first result, so a refused attempt must not
  // pin the credit forever: each new attempt gets a fresh id (journaled before
  // it is sent).
  requestId: () => randomUUID(),
});
