import { PROVIDERS, PROVIDER_IDS } from "./providers.mjs";

// Banked-reset configuration: state/install.json resetPolicy.<provider>,
// re-read on every refresh. Missing keys take the adapter's defaults; invalid
// ones fall back and are reported.
//   enabled          automatic redemption for that provider
//   trigger          "any":    an account is out when the provider says it is
//                              exhausted or any common window (5-hour or
//                              weekly) reaches the threshold
//                    "weekly": only the all-models weekly window counts; the
//                              5-hour and model-specific (Fable) limits never
//                              trigger a reset
//   headroomPercent  an account is out at (100 - headroomPercent)% used
//   redeemBeforeLapse  credit planner: an account that is out may spend its
//                    own credit early when the capacity forecast projects
//                    that credit to lapse unused (see routing.mjs chooseReset)
//   maxRunwayPercent refill providers only (Claude): a refill keeps the
//                    account's reset date, so it is spent only on an account
//                    with time to use it up before that date at a low recent
//                    demand; this is the most that is ever required (and the
//                    rule as is while demand is unknown). A credit expiring
//                    before the account resets is always allowed
//                    (routing.mjs refillRunwayOk)
// Otherwise a reset fires only when every enrolled account of the provider is out.
// resetAutomation:false in pool-state.json remains the master off switch.
export const RESET_TRIGGERS = ["any", "weekly"];
export const RESET_POLICY_DEFAULTS = Object.fromEntries(PROVIDER_IDS.map((id) => [id, PROVIDERS[id].resetDefaults]));

export function resetPolicy(provider, config, expectedCount) {
  const adapter = PROVIDERS[provider];
  const defaults = adapter.resetDefaults;
  const input = config && typeof config === "object" && !Array.isArray(config) ? config : {};
  const problems = [];
  if (config != null && input !== config) problems.push(`resetPolicy.${provider} must be an object; using defaults`);
  const pick = (key, valid) => {
    if (input[key] === undefined) return defaults[key];
    if (valid(input[key])) return input[key];
    problems.push(`resetPolicy.${provider}.${key} is invalid; using ${JSON.stringify(defaults[key])}`);
    return defaults[key];
  };
  return {
    provider,
    redemption: adapter.redemption,
    enabled: pick("enabled", (v) => typeof v === "boolean"),
    trigger: pick("trigger", (v) => RESET_TRIGGERS.includes(v)),
    headroomPercent: pick("headroomPercent", (v) =>
      typeof v === "number" && Number.isFinite(v) && v >= 0 && v <= adapter.maxHeadroomPercent),
    redeemBeforeLapse: pick("redeemBeforeLapse", (v) => typeof v === "boolean"),
    ...(adapter.redemption === "refill" ? { maxRunwayPercent: pick("maxRunwayPercent", (v) =>
      typeof v === "number" && Number.isFinite(v) && v >= 0 && v <= 100) } : {}),
    // Without a known roster size a partial listing could look like a fully
    // exhausted pool, so an invalid count disables automation.
    expectedCount: Number.isInteger(expectedCount) && expectedCount > 0 ? expectedCount : Infinity,
    problems,
  };
}

export function resetPolicies(config) {
  return Object.fromEntries(PROVIDER_IDS.map((provider) =>
    [provider, resetPolicy(provider, config?.resetPolicy?.[provider], config?.expectedAccounts?.[provider])]));
}

// Saved-state upgrades, applied once at startup and idempotent:
//   pendingReset/lastReset (Codex only)  -> resets.codex.{pending,last}
//   exhaustedSince (Codex only)          -> exhausted.codex
export function migrateState(state) {
  if (!state.resets || typeof state.resets !== "object") state.resets = {};
  if ("pendingReset" in state || "lastReset" in state) {
    state.resets.codex ??= { pending: state.pendingReset ?? null, last: state.lastReset ?? null };
    delete state.pendingReset;
    delete state.lastReset;
  }
  for (const provider of PROVIDER_IDS) {
    state.resets[provider] ??= { pending: null, last: null };
    state.resets[provider].refused ??= {};
  }
  if (!state.exhausted || typeof state.exhausted !== "object") state.exhausted = {};
  if ("exhaustedSince" in state) {
    if (state.exhaustedSince) state.exhausted.codex ??= state.exhaustedSince;
    delete state.exhaustedSince;
  }
  return state;
}
