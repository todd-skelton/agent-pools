import { isFresh } from "./freshness.mjs";
import { PROVIDERS } from "./providers.mjs";

export function applyRouting(account, snapshot) {
  const routing = snapshot?.routing;
  account.routingStatus = routing?.status ?? "unknown";
  account.routingCheckedAt = routing?.checked_at ?? null;
  account.routingModels = routing?.models ?? {};
  // Where the routed model list came from: the account's own live list
  // ("live"), or none yet ("pending"): the proxy routes nothing to an account
  // before its first successful read. Only auth-files snapshots carry it; a
  // recover-quota reply leaves it alone.
  const list = snapshot?.live_models;
  if (list && typeof list === "object") {
    account.modelList = {
      source: list.source === "live" ? "live" : "pending",
      fetchedAt: list.fetched_at ?? null,
      checkedAt: list.checked_at ?? null,
      models: Number.isInteger(list.models) ? list.models : null,
      error: typeof list.error === "string" && list.error ? list.error : null,
    };
  }
}

// Blocks shorter than the adapter's minRecoverableBlockMs are left to lapse.
function longBlock(snapshot, now, minMs) {
  if (minMs <= 0) return true;
  // Unlisted: blocked states for names the credential no longer routes (a
  // model that left its live list, an image tool call); their quota block is
  // still the account's.
  return [...Object.values(snapshot.routing?.models ?? {}), ...Object.values(snapshot.routing?.unlisted ?? {})].some((m) =>
    m?.status === "blocked" && Date.parse(m.next_retry_after) - now > minMs);
}
export function canRecoverQuota(account, snapshot, now = Date.now()) {
  const started = Date.parse(account.usageCheckStartedAt);
  const adapter = PROVIDERS[account.provider];
  return !!adapter && account.status === "available" && !account.disabled &&
    !account.error && !account.usageStale && isFresh(account.usageUpdatedAt, now) &&
    Number.isFinite(started) && started <= now && now - started <= 60000 &&
    !account.modelQuotaRecoveryBlocked && snapshot?.id === account.id &&
    snapshot.auth_index === account.auth_index && snapshot.provider === account.provider &&
    !snapshot.disabled && snapshot.routing?.quota_blocked === true &&
    typeof snapshot.routing?.generation === "string" &&
    longBlock(snapshot, now, adapter.minRecoverableBlockMs);
}

export async function reconcileQuota(account, { readSnapshot, clearQuota, now = Date.now }) {
  let snapshot = await readSnapshot();
  applyRouting(account, snapshot);
  if (!canRecoverQuota(account, snapshot, now())) return false;
  let result;
  try {
    result = await clearQuota({ auth_index: account.auth_index,
      generation: snapshot.routing.generation, observed_after: account.usageCheckStartedAt });
  } catch (error) {
    if (error.status !== 409) throw error;
    applyRouting(account, await readSnapshot());
    return false;
  }
  applyRouting(account, { routing: result.routing });
  if (result.changed) account.quotaRecoveredAt = new Date(now()).toISOString();
  return !!result.changed;
}
