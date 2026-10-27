import { normalizeUsage, expiryMs, redemptionId } from "./routing.mjs";

const BACKEND = "https://chatgpt.com/backend-api";
const CREDITS_URL = BACKEND + "/wham/rate-limit-reset-credits";
// Billing reads (subscription, checkout pricing) answer 403 to the proxy's
// default user agent; they answer the Codex CLI's, as the desktop's own reads do.
const billingHeaders = async (io) => ({ "User-Agent": `codex_cli_rs/${(await io.clientVersion("codex")) ?? "0.0.0"}` });
export const CODEX_PLAN_LABELS = Object.freeze({ free: "Free", go: "Go", plus: "Plus", prolite: "Pro 100", pro: "Pro 200", promax: "Pro 500" });
export const PRICING_COUNTRY = "US";

// Plan and monthly price as the provider reports them: the account's
// subscription (plan_type, billing period and currency) priced from ChatGPT's
// own checkout pricing table, which is what the desktop's "Your plan" card
// shows. A price is only given when the table's currency is the account's.
export function codexPlan(sub, pricing) {
  const plan = typeof sub?.plan_type === "string" && sub.plan_type ? sub.plan_type : null;
  if (!plan) return null;
  const currency = sub.billing_currency ?? sub.entitlement?.billing_currency ?? null;
  const period = sub.billing_period ?? sub.entitlement?.billing_period ?? null;
  const cfg = pricing?.currency_config;
  const row = cfg && typeof cfg === "object" ? cfg[plan] : null;
  // year.amount is the per-month price of annual billing (Plus: 16.67).
  const amount = period === "yearly" ? row?.year?.amount : row?.month?.amount;
  const priced = Number.isFinite(amount) && amount >= 0 && typeof currency === "string" && cfg?.symbol_code === currency;
  return {
    plan,
    label: "ChatGPT " + (CODEX_PLAN_LABELS[plan] ?? plan),
    status: sub.will_renew === false ? "ends" : "active",
    billingPeriod: period,
    renewsAt: sub.active_until ?? sub.entitlement?.renews_at ?? null,
    price: priced ? { usdPerMonth: currency === "USD" ? amount : null, amount, currency, source: "provider", detail: `ChatGPT checkout pricing (${PRICING_COUNTRY})` } : null,
  };
}

export function availableCredits(data, now = Date.now()) {
  return (data.credits ?? [])
    .filter(
      (c) =>
        c.status === "available" &&
        ["codex_rate_limits", "codexRateLimits"].includes(
          c.reset_type ?? c.resetType,
        ) &&
        expiryMs(c.expires_at ?? c.expiresAt) > now,
    )
    .sort(
      (a, b) =>
        expiryMs(a.expires_at ?? a.expiresAt) -
        expiryMs(b.expires_at ?? b.expiresAt),
    );
}

export const codex = Object.freeze({
  id: "codex",
  name: "Codex",
  loginId: "codex",
  clientId: "codex",
  usageUrl: "https://chatgpt.com/backend-api/wham/usage",
  // A Codex credit is a reset: it restarts the weekly window from now.
  redemption: "reset",
  resetDefaults: Object.freeze({ enabled: true, trigger: "any", headroomPercent: 5, redeemBeforeLapse: true }),
  // Capacity forecast facts (capacity-forecast.mjs). The weekly window starts
  // on first use: an unused account's resetAt slides and nothing expires.
  // Plan weights are relative to Pro; ProLite measured about half of a Pro
  // (points/hour as the only active account, 2026-09-18..10-02). Override
  // in install.json capacityForecast.weights.
  capacity: Object.freeze({ weeklyWindowId: "codex:primary_window", windowStartsOnFirstUse: true, planWeights: Object.freeze({ prolite: 0.5 }) }),
  // OpenAI refuses a redemption unless the account has 10% or less remaining.
  maxHeadroomPercent: 10,
  minRecoverableBlockMs: 0,
  accountId: (file) => file.id_token?.chatgpt_account_id ?? file.id,
  headers: async (file) => ({
    "OpenAI-Beta": "codex-1",
    Originator: "Codex Desktop",
    ...(file.id_token?.chatgpt_account_id ? { "Chatgpt-Account-Id": file.id_token.chatgpt_account_id } : {}),
  }),
  modelRecoveryBlocked: (usage) => [
    ...(Array.isArray(usage.additional_rate_limits) ? usage.additional_rate_limits.map((r) => r.rate_limit) : []),
    ...Object.entries(usage).filter(([key]) => key !== "rate_limit" && key.endsWith("_rate_limit")).map(([, value]) => value),
  ].filter((rate_limit) => rate_limit != null)
    .some((rate_limit) => normalizeUsage("codex", { rate_limit }).status !== "available"),
  // Plan read (refresh.mjs, a few times a day): throws when unreadable, and
  // the refresh keeps the last known plan.
  async readPlan(io, file) {
    const accountId = file.id_token?.chatgpt_account_id;
    if (!accountId) throw new Error("ChatGPT account id unknown");
    const headers = await billingHeaders(io);
    const sub = await io.read(file, `${BACKEND}/subscriptions?account_id=${encodeURIComponent(accountId)}`, undefined, headers);
    const pricing = await io.memo(`codex-pricing:${PRICING_COUNTRY}`, () => io.read(file, `${BACKEND}/checkout_pricing_config/configs/${PRICING_COUNTRY}`, undefined, headers)).catch(() => null);
    const plan = codexPlan(sub, pricing);
    if (!plan) throw new Error("Subscription plan not reported");
    return plan;
  },
  // A failed or malformed bank read leaves the bank unknown; it never costs
  // the account its (already verified) usage.
  async readBank(io, file, usage, now) {
    try {
      const data = await io.read(file, CREDITS_URL);
      const credits = availableCredits(data, now);
      return {
        known: Array.isArray(data.credits),
        credits,
        count: Number(data.available_count ?? usage.rate_limit_reset_credits?.available_count ?? credits.length),
        note: null,
      };
    } catch {
      return { known: false, credits: [], count: 0, note: null };
    }
  },
  redeem: (io, file, pending) =>
    io.read(file, `${CREDITS_URL}/consume`, { redeem_request_id: pending.requestId, credit_id: pending.creditId }),
  // Deterministic per credit, compatible with earlier redemptions.
  requestId: (account, creditId) => redemptionId(account.accountId, creditId),
});
