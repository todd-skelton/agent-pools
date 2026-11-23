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

// The models a quota clear must leave blocked: those a spent (or unread)
// model-specific window covers, matched by the window's scope against the
// models the proxy routes for the account. A spent model limit blocks only its
// own models, never the account. null when a spent window names no routed
// model, or the adapter flagged a model limit the windows do not show; the
// clear then waits. Codex flags a model limit by its status as well as its
// windows, and its usage-limit 429 blocks the whole credential, so a Codex
// model limit keeps the account-wide wait.
export function recoveryKeep(account, snapshot) {
  if (account.provider !== "claude") return account.modelQuotaRecoveryBlocked ? null : [];
  const scopes = [];
  for (const w of account.quotaWindows ?? []) {
    if (w?.common === true || (Number.isFinite(w?.used) && w.used < 100)) continue;
    const scope = String(w?.scope).trim().toLowerCase().replaceAll(" ", "-");
    if (!scope || scope === "undefined" || scope === "null") return null;
    scopes.push(scope);
  }
  if (!scopes.length) return account.modelQuotaRecoveryBlocked ? null : [];
  const routed = [...Object.keys(snapshot?.routing?.models ?? {}), ...Object.keys(snapshot?.routing?.unlisted ?? {})];
  const keep = [];
  for (const scope of scopes) {
    const covered = routed.filter((m) => modelInScope(m, scope));
    if (!covered.length) return null;
    keep.push(...covered);
  }
  return [...new Set(keep)].sort();
}

function modelInScope(model, scope) {
  const m = model.toLowerCase();
  return m === scope || m.startsWith(scope + "-") || m.split(/[-_.]/).includes(scope);
}

// Blocks shorter than the adapter's minRecoverableBlockMs are left to lapse.
// Only a model the clear would free counts: not one in keep, and not one the
// proxy says is blocked for another reason than quota (quota: false).
function longBlock(snapshot, now, minMs, keep) {
  // Unlisted: blocked states for names the credential no longer routes (a
  // model that left its live list, an image tool call); their quota block is
  // still the account's.
  const freed = [...Object.entries(snapshot.routing?.models ?? {}), ...Object.entries(snapshot.routing?.unlisted ?? {})]
    .filter(([model, m]) => !keep.includes(model) && m?.quota !== false).map(([, m]) => m);
  if (keep.length && !freed.some((m) => m?.status === "blocked")) return false;
  if (minMs <= 0) return true;
  return freed.some((m) => m?.status === "blocked" && Date.parse(m.next_retry_after) - now > minMs);
}
export function canRecoverQuota(account, snapshot, now = Date.now()) {
  const started = Date.parse(account.usageCheckStartedAt);
  const adapter = PROVIDERS[account.provider];
  if (!adapter || !snapshot) return false;
  const keep = recoveryKeep(account, snapshot);
  return keep !== null && account.status === "available" && !account.disabled &&
    !account.error && !account.usageStale && isFresh(account.usageUpdatedAt, now) &&
    Number.isFinite(started) && started <= now && now - started <= 60000 &&
    snapshot.id === account.id &&
    snapshot.auth_index === account.auth_index && snapshot.provider === account.provider &&
    !snapshot.disabled && snapshot.routing?.quota_blocked === true &&
    typeof snapshot.routing?.generation === "string" &&
    longBlock(snapshot, now, adapter.minRecoverableBlockMs, keep);
}

export async function reconcileQuota(account, { readSnapshot, clearQuota, now = Date.now }) {
  let snapshot = await readSnapshot();
  applyRouting(account, snapshot);
  if (!canRecoverQuota(account, snapshot, now())) return false;
  let result;
  try {
    const keep = recoveryKeep(account, snapshot) ?? [];
    result = await clearQuota({ auth_index: account.auth_index,
      generation: snapshot.routing.generation, observed_after: account.usageCheckStartedAt,
      ...(keep.length ? { keep_models: keep } : {}) });
  } catch (error) {
    if (error.status !== 409) throw error;
    applyRouting(account, await readSnapshot());
    return false;
  }
  applyRouting(account, { routing: result.routing });
  if (result.changed) account.quotaRecoveredAt = new Date(now()).toISOString();
  return !!result.changed;
}
