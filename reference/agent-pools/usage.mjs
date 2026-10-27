import { normalizeUsage } from "./routing.mjs";
import { invalidateAccount } from "./freshness.mjs";
import { PROVIDERS } from "./providers.mjs";

// A provider read that fails at the transport (proxy 502, timeout, socket
// error) is retried once after a short pause before the account is invalidated.
// Provider verdicts (401, 403, 429) are final for this cycle and are not retried.
export const READ_RETRY_DELAY_MS = 2000;
export function isTransientReadError(error) {
  if (error instanceof SyntaxError) return false;
  const status = error?.status;
  if (Number.isFinite(status)) return status >= 500 && status <= 599;
  return ["TimeoutError", "AbortError"].includes(error?.name) || /returned HTTP 5\d\d|fetch failed|ECONN|ETIMEDOUT|EAI_AGAIN|socket/i.test(error?.message ?? "");
}
// Verified evidence: a known verdict plus a finite common window to meter.
export function isUsableUsage(normalized) {
  return normalized?.status !== "unknown" &&
    (normalized?.quotaWindows ?? []).some((w) => w.common && Number.isFinite(w.used));
}
async function readWithRetry(read, sleep) {
  try {
    return await read();
  } catch (error) {
    if (!isTransientReadError(error)) throw error;
    await sleep(READ_RETRY_DELAY_MS);
    return read();
  }
}

// Retain display telemetry on failure; never reuse stale routing or credit state.
export async function refreshUsage(
  account,
  previous,
  read,
  now = () => new Date().toISOString(),
  sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
) {
  account.usageCheckStartedAt = now();
  try {
    const usage = await readWithRetry(read, sleep);
    const normalized = normalizeUsage(account.provider, usage, Date.parse(now()));
    // A 2xx body without a readable verdict and at least one common quota
    // window is a failed read: it must not stamp a fresh usageUpdatedAt (which
    // would renew the overlay's grace period) or replace retained rows with
    // empty ones.
    if (!isUsableUsage(normalized)) throw new Error("Account provider returned an unusable usage payload");
    delete account.remaining;
    delete account.nextReset;
    delete account.rankReset;
    delete account.rankRemaining;
    Object.assign(account, normalized);
    // The proxy's quota-clear operation is credential-wide. A common-pool
    // recovery must not clear a still-exhausted or unknown model-specific limit.
    // An unknown provider never qualifies for recovery.
    account.modelQuotaRecoveryBlocked = PROVIDERS[account.provider]?.modelRecoveryBlocked(usage, normalized) ?? true;
    account.usageUpdatedAt = now();
    account.usageStale = false;
    delete account.error;
    delete account.errorTransient;
    return usage;
  } catch (error) {
    const sameAccount =
      previous?.id === account.id && previous?.provider === account.provider;
    account.quotaWindows = sameAccount ? (previous.quotaWindows ?? []) : [];
    account.usageUpdatedAt = sameAccount
      ? (previous.usageUpdatedAt ?? null)
      : null;
    account.usageStale = account.quotaWindows.length > 0;
    account.error = error.message;
    // Transient failures are worth re-polling sooner; verdicts (401/403/429,
    // unusable body) are not.
    account.errorTransient = isTransientReadError(error);
    invalidateAccount(account);
    account.status = "unknown";
    delete account.windows;
    delete account.remaining;
    delete account.nextReset;
    delete account.rankReset;
    delete account.rankRemaining;
    account.creditsKnown = false;
    account.creditCount = 0;
    account.credits = [];
    return null;
  }
}
