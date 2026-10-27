import { execFile } from "node:child_process";

// Pure transition detector for one provider. The pool enters the exhausted
// state when every non-disabled account is known exhausted and the reset
// automation is not about to act (`resetExpected`, see reset-lifecycle.mjs):
// a banked credit the policy will not spend, for example on a 5-hour limit
// under the weekly trigger, is no capacity. It leaves only when an account is
// verified available again. Returns the event to record ("pool-exhausted" or
// "pool-recovered") or null; the start time lives in state.exhausted[provider].
// "unknown" is never evidence: a partial refresh neither enters nor leaves the
// exhausted state, so a single failed provider read cannot flap the toast.
export function exhaustionTransition(state, accounts, { provider, resetExpected = false, now = new Date() }) {
  state.exhausted ??= {};
  const pool = accounts.filter((a) => a.provider === provider && !a.disabled);
  const allExhausted = pool.length > 0 && pool.every((a) => a.status === "exhausted");
  if (allExhausted && !resetExpected && !state.exhausted[provider]) {
    state.exhausted[provider] = now.toISOString();
    return "pool-exhausted";
  }
  if (pool.some((a) => a.status === "available") && state.exhausted[provider]) {
    delete state.exhausted[provider];
    return "pool-recovered";
  }
  return null;
}

// Pure transition detector for accounts that must sign in again: the proxy
// reports refresh_error code "reauth_required" (providers.mjs refreshErrorOf).
// state.reauth[accountId] = { provider, email, since } marks an alerted
// episode and is saved with the pool state, so a supervisor restart does not
// alert again. An episode ends only on positive evidence: the proxy no longer
// reports the failure and the account's usage was verified this cycle (a new
// login, or a refresh that recovered). A proxy restart forgets the failure
// but a dead login still cannot read usage, so the restart neither ends the
// episode nor alerts twice. Removing or disabling the account also ends it.
// Returns [{ type: "reauth-required", account } | { type: "reauth-cleared",
// accountId, provider, reason }].
export function reauthTransitions(state, accounts) {
  if (!state.reauth || typeof state.reauth !== "object" || Array.isArray(state.reauth)) state.reauth = {};
  const out = [];
  for (const a of accounts) {
    if (a.disabled || a.refreshError?.code !== "reauth_required" || state.reauth[a.id]) continue;
    state.reauth[a.id] = { provider: a.provider, email: a.email ?? "", since: a.refreshError.since ?? null };
    out.push({ type: "reauth-required", account: a });
  }
  const byId = new Map(accounts.map((a) => [a.id, a]));
  for (const [id, entry] of Object.entries(state.reauth)) {
    const a = byId.get(id);
    const reason = !a ? "removed"
      : a.disabled ? "disabled"
      : a.refreshError?.code !== "reauth_required" && a.status !== "unknown" && !a.usageStale ? "recovered"
      : null;
    if (!reason) continue;
    delete state.reauth[id];
    out.push({ type: "reauth-cleared", accountId: id, provider: entry.provider, reason });
  }
  return out;
}

// Pure transition detector for banked credits the plan still loses. A credit
// is at risk when the latest capacity forecast (fresh, provider not unknown or
// stale) projects it to lapse at the measured pace under the credit plan
// (creditPlan.credits[].outcome "lapses") and it expires within atRiskHours.
// state.creditRisk[accountId|expiresAt] = { provider, accountId, expiresAt,
// since } marks an alerted credit and is saved, so a restart does not alert
// twice. An entry clears once a fresh forecast no longer projects that credit
// to lapse ("planned": used under the plan; "gone": redeemed or no longer
// listed) or it has expired ("expired"). Returns [{ type: "credit-at-risk",
// provider, credit } | { type: "credit-risk-cleared", provider, accountId,
// expiresAt, reason }].
export function creditRiskTransitions(state, forecast, { now = Date.now(), atRiskHours = 72 } = {}) {
  if (!state.creditRisk || typeof state.creditRisk !== "object" || Array.isArray(state.creditRisk)) state.creditRisk = {};
  const out = [];
  const fresh = forecast && Date.parse(forecast.staleAfter) > now;
  const judged = new Set();
  const listed = new Map();
  for (const [provider, p] of Object.entries((fresh && forecast.providers) || {})) {
    if (!p || p.state === "unknown" || p.stale || !Array.isArray(p.creditPlan?.credits)) continue;
    judged.add(provider);
    for (const c of p.creditPlan.credits) {
      const expires = Date.parse(c?.expiresAt);
      if (typeof c?.accountId !== "string" || !Number.isFinite(expires)) continue;
      const key = c.accountId + "|" + c.expiresAt;
      listed.set(key, c);
      if (c.outcome !== "lapses" || expires <= now || expires - now > atRiskHours * 3600000 || state.creditRisk[key]) continue;
      state.creditRisk[key] = { provider, accountId: c.accountId, expiresAt: c.expiresAt, since: new Date(now).toISOString() };
      out.push({ type: "credit-at-risk", provider, credit: c });
    }
  }
  for (const [key, entry] of Object.entries(state.creditRisk)) {
    const reason = !(Date.parse(entry?.expiresAt) > now) ? "expired"
      : !judged.has(entry?.provider) ? null
      : !listed.has(key) ? "gone"
      : listed.get(key).outcome !== "lapses" ? "planned"
      : null;
    if (!reason) continue;
    delete state.creditRisk[key];
    out.push({ type: "credit-risk-cleared", provider: entry?.provider ?? null, accountId: entry?.accountId ?? null, expiresAt: entry?.expiresAt ?? null, reason });
  }
  return out;
}

// Pure transition detector for the proxy's provider outage hold. `holds` is the
// last /v0/management/outage-holds reading ({ active, turns }) or null when the
// proxy could not be read. Returns "outage-hold-started" once when requests
// begin waiting, "outage-hold-cleared" once when none are left, else null. A
// null reading is never evidence, so a supervisor-side read failure cannot flap
// the toast.
export function holdTransition(state, holds, now = new Date()) {
  if (!holds || typeof holds.active !== "number") return null;
  if (holds.active > 0 && !state.holdSince) {
    state.holdSince = now.toISOString();
    return "outage-hold-started";
  }
  if (holds.active === 0 && state.holdSince) {
    delete state.holdSince;
    return "outage-hold-cleared";
  }
  return null;
}

// Windows toast through Windows PowerShell 5.1, which can load WinRT types
// without extra modules. The XML travels in an environment variable: with
// -Command, PowerShell would parse a trailing argument as script text.
// Best effort: callers log failures and move on.
export function toast(title, body) {
  const script = [
    "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null",
    "[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null",
    "$xml = New-Object Windows.Data.Xml.Dom.XmlDocument",
    "$xml.LoadXml($env:AGENT_POOLS_TOAST)",
    "[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Agent Pools').Show([Windows.UI.Notifications.ToastNotification]::new($xml))",
  ].join("; ");
  const escape = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  const payload = `<toast><visual><binding template="ToastGeneric"><text>${escape(title)}</text><text>${escape(body)}</text></binding></visual></toast>`;
  return new Promise((resolve, reject) => {
    execFile("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script],
      { windowsHide: true, timeout: 10000, env: { ...process.env, AGENT_POOLS_TOAST: payload } },
      (error) => (error ? reject(error) : resolve()));
  });
}
