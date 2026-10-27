// Usage samples for the capacity forecast (capacity-forecast.mjs) and the
// usage value report. The old per-account pace projection and pool burn rate
// were removed 2026-10-15: the capacity forecast replaces them and, unlike
// them, counts banked resets and refills.
// Provider reset estimates jitter (Claude by milliseconds, Codex by minutes),
// so only a resetAt move beyond RESET_JITTER_MS counts as a reset.
export const RESET_JITTER_MS = 3600000;

// Usage samples: one line per window whose used percentage changed (Codex
// reports integers, so most refreshes produce no line). Model-specific
// windows (common: false, for example Claude's Fable weekly) are recorded for
// the capacity forecast. A window whose resetAt
// moved by more than the jitter tolerance, or whose used percentage fell, has
// reset.
export function usageSamples(previousAccounts, accounts, at = Date.now()) {
  const samples = [];
  const previous = new Map(previousAccounts.map((a) => [`${a.provider}:${a.id}`, a]));
  for (const a of accounts) {
    if (a.disabled || a.status === "unknown" || a.usageStale) continue;
    const before = previous.get(`${a.provider}:${a.id}`);
    for (const w of a.quotaWindows ?? []) {
      if (!Number.isFinite(w.used)) continue;
      const prior = before?.quotaWindows?.find((p) => p.id === w.id);
      // Reset estimates jitter between reads; an unchanged window is not a sample.
      if (prior && prior.used === w.used && (prior.resetAt === w.resetAt ||
        (Number.isFinite(prior.resetAt) && Number.isFinite(w.resetAt) && Math.abs(prior.resetAt - w.resetAt) <= RESET_JITTER_MS))) continue;
      samples.push({ at: new Date(at).toISOString(), provider: a.provider, accountId: a.id, window: w.id, used: w.used, resetAt: w.resetAt, common: !!w.common });
    }
  }
  return samples;
}
