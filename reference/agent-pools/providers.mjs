import { codex } from "./provider-codex.mjs";
import { claude } from "./provider-claude.mjs";

// Provider adapters. Everything that differs between subscription providers
// (request identity, endpoints, reset banks, redemption, recovery rules) lives
// in one adapter per provider; routing, reset policy, the reset lifecycle,
// freshness and projection stay provider-neutral and work on the normalized
// account below. Quota parsing (quota-windows.mjs, normalizeUsage) is still
// keyed by provider id: it is pure, shared by display and routing, and gains
// nothing from moving.
//
// @typedef {object} ProviderIO  I/O the supervisor lends an adapter
// @property {(file, url, body?) => Promise<any>} read  provider call through the proxy (token injected there)
// @property {(file) => Promise<object|null>} authFile  the account's credential file, read-only, or null
// @property {(provider: string) => Promise<string|undefined>} clientVersion
//   version of the provider's client (adapter.clientId) that the proxy presents
//   upstream (installed-clients in proxy.yaml, read from the installed binary);
//   undefined when the proxy cannot say
//
// @typedef {object} Bank  { known, credits: [{ id, expires_at }], count, note }
//   known false means "not reported": it blocks automation, it never means zero.
//
// @typedef {object} ProviderAdapter
// @property {string} id                  auth-file provider id ("codex", "claude")
// @property {string} name                display name
// @property {string} loginId             the proxy's OAuth provider id
// @property {string} clientId            the proxy's installed-client id (GET /v0/management/client-versions)
// @property {string} usageUrl
// @property {"reset"|"refill"} redemption  what spending a banked credit does:
//   "reset" restarts the weekly window from now (Codex); "refill" clears usage
//   but keeps the window's fixed reset date (Claude), so it is only worth
//   spending with enough of the window left to use it up
// @property {{ enabled: boolean, trigger: "any"|"weekly", headroomPercent: number, redeemBeforeLapse: boolean, maxRunwayPercent?: number }} resetDefaults
// @property {number} maxHeadroomPercent  above this the provider refuses redemptions
// @property {number} minRecoverableBlockMs  proxy quota blocks shorter than this are left to lapse
// @property {(file) => string} accountId stable provider account id
// @property {(file, io: ProviderIO) => Promise<object>} headers
// @property {(usage, normalized) => boolean} modelRecoveryBlocked  a model-specific limit is out or unknown
// @property {(io: ProviderIO, file, usage, now: number) => Promise<Bank>} readBank
// @property {(io: ProviderIO, file, pending) => Promise<{ code: string }>} redeem
//   codes: reset | already_redeemed | no_credit | nothing_to_reset | rejected | deferred;
//   anything else, or a throw, means the outcome is unknown
// @property {(account, creditId) => string} requestId
// @property {{ weeklyWindowId: string, windowStartsOnFirstUse: boolean, planWeights: object }} capacity
//   facts for capacity-forecast.mjs: the provider's all-models weekly window id, whether that
//   window starts on first use (so an unused account loses nothing), and plan capacity weights
// Null prototype: a provider id from the proxy or a request body ("constructor",
// "__proto__") must never resolve to an inherited property.
export const PROVIDERS = Object.freeze(Object.assign(Object.create(null), { codex, claude }));
export const PROVIDER_IDS = Object.freeze(Object.keys(PROVIDERS));
export const providerByLogin = (loginId) => Object.values(PROVIDERS).find((p) => p.loginId === loginId) ?? null;

// The normalized account every provider-neutral module reads. The refresh
// creates one per auth file each cycle; later modules fill in the rest.
//
// @typedef {object} Account
// @property {string} id, name, email, auth_index, provider   from the auth file
// @property {string} accountId          provider account id (Codex request ids hash it)
// @property {boolean} disabled
// @property {number} priority           proxy routing priority (100 = first)
// @property {"available"|"exhausted"|"unknown"|"disabled"} status
//   usage (usage.mjs refreshUsage, routing.mjs normalizeUsage):
// @property {object[]} [quotaWindows] [windows]   display rows / common windows
// @property {number} [remaining] [rankReset] [rankRemaining] [nextReset]
// @property {string|null} [usageUpdatedAt] [usageCheckStartedAt]
// @property {boolean} [usageStale] [modelQuotaRecoveryBlocked]
// @property {string} [error]  @property {boolean} [errorTransient]
// @property {RefreshError} [refreshError]  the proxy's refresh verdict (refreshErrorOf); absent when refreshes succeed
//   or the proxy predates pool.19
//   routing (quota-recovery.mjs applyRouting):
// @property {string} [routingStatus] [routingCheckedAt] [routingError] [quotaRecoveredAt]
// @property {object} [routingModels]   { [model]: { status, next_retry_after } }, read by the orchestrator;
//   the models are the account's own live list once the proxy has read it
// @property {{ source: "live"|"pending", fetchedAt, checkedAt, models, error }} [modelList]
//   where routingModels came from and when the proxy last read the live list;
//   "pending" = never read, so the proxy routes nothing to the account
//   reset bank (adapter readBank; chooseReset and processReset rely on these):
// @property {boolean} creditsKnown      false = not reported; blocks automation, never means zero
// @property {{ id: string, expires_at: string|null }[]} credits  claimable now, soonest expiry first
// @property {number} creditCount        bank size for display and tie-breaks
// @property {string} [creditNote]       why the bank is empty or unclaimable
// @property {string} [creditsUpdatedAt] a bank older than five minutes is unknown (freshness.mjs)
// The proxy's current streak of failed credential refreshes (auth-files
// refresh_error, proxy pool.19+). Code "reauth_required": the provider
// rejected the refresh grant, so the account has to sign in again; any other
// code is a failure the proxy keeps retrying. Returns null when absent or
// unreadable.
// @typedef {{ code: string, httpStatus: number|null, message: string, since: string|null,
//   lastFailedAt: string|null, attempts: number|null }} RefreshError
/** @returns {RefreshError|null} */
export function refreshErrorOf(file) {
  const e = file?.refresh_error;
  if (!e || typeof e !== "object" || typeof e.code !== "string" || !e.code) return null;
  return {
    code: e.code,
    httpStatus: Number.isInteger(e.http_status) && e.http_status > 0 ? e.http_status : null,
    message: typeof e.message === "string" ? e.message : "",
    since: typeof e.since === "string" ? e.since : null,
    lastFailedAt: typeof e.last_failed_at === "string" ? e.last_failed_at : null,
    attempts: Number.isInteger(e.attempts) ? e.attempts : null,
  };
}

/** @returns {Account} */
export function createAccount(file) {
  const adapter = PROVIDERS[file.provider];
  const refreshError = refreshErrorOf(file);
  return {
    id: file.id,
    name: file.name ?? file.id,
    auth_index: file.auth_index,
    accountId: adapter.accountId(file),
    email: file.email ?? "",
    provider: file.provider,
    disabled: !!file.disabled,
    priority: file.priority ?? 0,
    status: "unknown",
    creditsKnown: false,
    creditCount: 0,
    credits: [],
    ...(refreshError ? { refreshError } : {}),
  };
}
