// One compact read-only document for agents and scripts on this machine
// (GET /api/summary): per provider, what the pool holds, what it costs, how
// much is left, and when it runs short under each credit assumption; per
// account, plan, price, windows and banked credits. Built from the same
// state as /api/status, /api/capacity and /api/value; no tokens or grant ids.
export const SUMMARY_FORMAT = "agent-pools-summary/v1";

const iso = (t) => (Number.isFinite(t) ? new Date(t).toISOString() : typeof t === "string" ? t : null);
const sum = (xs) => xs.reduce((s, x) => s + x, 0);
const round = (v, d = 2) => (Number.isFinite(v) ? Math.round(v * 10 ** d) / 10 ** d : null);

function scenarioView(s) {
  if (!s) return null;
  return { firstShortAt: s.firstOutageAt ?? null, shortHours: s.outageHours ?? null, creditsUsed: s.creditsUsed ?? null, creditsLapsing: s.creditsLapsing ?? null,
    ...(s.assumption ? { assumption: s.assumption } : {}) };
}

export function summaryAccount(a, valueRow) {
  const windows = (a.quotaWindows ?? []).map((w) => ({
    id: w.id, scope: w.scope ?? null, label: w.label ?? null,
    usedPercent: Number.isFinite(w.used) ? w.used : null,
    resetAt: iso(w.resetAt),
    limitsAllModels: w.common !== false,
  }));
  const expiries = (a.credits ?? []).map((c) => c?.expires_at ?? c?.expiresAt ?? null).filter((x) => typeof x === "string").sort();
  const price = a.plan?.price ?? null;
  return {
    id: a.id, provider: a.provider, email: a.email || null,
    routable: !a.disabled && a.status === "available" && a.routingStatus === "ready",
    status: a.disabled ? "disabled" : a.refreshError ? "reauth-required" : a.usageStale || a.status === "unknown" ? "unverified" : a.status ?? null,
    usageUpdatedAt: a.usageUpdatedAt ?? null,
    plan: a.plan ? { plan: a.plan.plan, label: a.plan.label ?? null, status: a.plan.status ?? null, renewsAt: a.plan.renewsAt ?? null, readAt: a.plan.readAt ?? null } : null,
    priceUsdPerMonth: price?.usdPerMonth ?? valueRow?.planUsdPerMonth ?? null,
    priceSource: price ? price.source : valueRow?.planUsdPerMonth != null ? valueRow.planPriceSource ?? "config" : null,
    windows,
    credits: { known: a.creditsKnown === true, count: Number.isInteger(a.creditCount) ? a.creditCount : expiries.length, expiries },
    value: valueRow ? { apiEquivalentUsd: valueRow.apiEquivalentUsd ?? null, valueMultiple: valueRow.valueMultiple ?? null, requests: valueRow.requests ?? null } : null,
  };
}

// state: pool state (accounts, checkedAt, error); providers: { id: { name } };
// forecast: capacity-forecast/v1 or null; value: usage-value/v1 or null.
export function buildSummary({ state, providers, forecast = null, value = null, now = Date.now() }) {
  const valueById = new Map((value?.accounts ?? []).map((r) => [r.id, r]));
  const accounts = (state.accounts ?? []).map((a) => summaryAccount(a, valueById.get(a.id)));
  const out = {};
  for (const [id, meta] of Object.entries(providers)) {
    const mine = accounts.filter((a) => a.provider === id);
    if (!mine.length) continue;
    const f = forecast?.providers?.[id] ?? null;
    const priced = mine.filter((a) => Number.isFinite(a.priceUsdPerMonth));
    const s = f?.scenarios ?? null;
    out[id] = {
      name: meta.name,
      accounts: mine.length,
      routable: mine.filter((a) => a.routable).length,
      monthlyCostUsd: priced.length ? round(sum(priced.map((a) => a.priceUsdPerMonth))) : null,
      unpricedAccounts: mine.length - priced.length,
      priceSources: [...new Set(priced.map((a) => a.priceSource))].sort(),
      creditsBanked: sum(mine.map((a) => a.credits.count)),
      nextCreditExpiry: mine.flatMap((a) => a.credits.expiries).sort()[0] ?? null,
      forecast: f ? {
        state: f.state, stale: !!f.stale,
        demandUnitsPerHour: f.demand?.unitsPerHour ?? null,
        remainingUnits: f.supply?.remainingUnits ?? null,
        nextResetAt: f.supply?.nextRefillAt ?? null,
        horizonHours: s?.horizonHours ?? f.projection?.horizonHours ?? null,
        naturalOnly: scenarioView(s?.naturalOnly),
        bankedCredits: scenarioView(s?.bankedCredits),
        weeklyGrant: scenarioView(s?.weeklyGrant),
        pace: f.pace ?? null,
        creditPlan: f.creditPlan?.method === "counterfactual" ? {
          planned: f.creditPlan.planned === true,
          redeemNow: f.creditPlan.holders ?? [],
          drainFirst: f.creditPlan.drainFirst ?? [],
          credits: (f.creditPlan.credits ?? []).map((c) => ({ accountId: c.accountId, expiresAt: c.expiresAt, resetAt: c.resetAt, out: c.out, unitsLeft: c.unitsLeft,
            action: c.action, drainBy: c.drainBy, outcome: c.outcome, needUnitsPerHour: c.needUnitsPerHour })),
        } : null,
        modelLimits: (f.modelWindows ?? []).map((w) => {
          const measured = (value?.modelLimits ?? []).find((m) => m.provider === id && m.window === w.window && m.scope === w.scope);
          return {
            scope: w.scope ?? w.window, state: w.state, firstShortAt: w.projection?.firstOutageAt ?? null,
            window: w.window, kind: measured?.kind ?? "unknown", shareOfCommon: measured?.shareOfCommon ?? null,
            shareBasis: measured?.shareBasis ?? null, source: measured?.source ?? "unknown", models: measured?.models ?? [],
            accounts: (state.accounts ?? []).filter((a) => a.provider === id).flatMap((a) => (a.quotaWindows ?? [])
              .filter((q) => q.common === false && q.id === w.window && q.scope === w.scope)
              .map((q) => ({ id: a.id, used: Number.isFinite(q.used) ? q.used : null, resetAt: iso(q.resetAt) }))),
          };
        }),
      } : null,
      value: value ? {
        apiEquivalentUsd: round(sum(mine.map((a) => a.value?.apiEquivalentUsd ?? 0))),
        windowDays: value.valueWindowDays ?? null,
      } : null,
    };
  }
  const costs = Object.values(out).map((p) => p.monthlyCostUsd).filter(Number.isFinite);
  return {
    format: SUMMARY_FORMAT,
    generatedAt: new Date(now).toISOString(),
    checkedAt: state.checkedAt ?? null,
    error: state.error ?? null,
    forecastGeneratedAt: forecast?.generatedAt ?? null,
    forecastStaleAfter: forecast?.staleAfter ?? null,
    valueGeneratedAt: value?.generatedAt ?? null,
    monthlyCostUsd: costs.length ? round(sum(costs)) : null,
    notes: [
      "Routing: the pool picks among its own accounts; forecast.state and the published balance are signals the model-routing skill and dispatch-lane.ps1 read, not limits the pool enforces.",
      "Scenarios: naturalOnly = window resets alone; bankedCredits = banked credits spent the way the pool spends them (the headline); weeklyGrant = bankedCredits plus an assumed new credit each week, which providers do not promise.",
      "pace: how fast to use the provider to get the most out of it. Units are weighted account-weeks per hour (x168 = account-weeks per week). Objective: total usage over the run (work done plus quota still available at the end); running short loses nothing, quota is lost only when a window resets unused or a credit lapses. maxUnits = usage when draining as fast as possible; atDemand = usage at the observed pace (lostUnits = what it leaves unused); fullUse = lowest steady pace within 1% of maxUnits (null: only as fast as possible gets there), then = the lower floor after its last credit is spent; noGap = highest pace that never runs out and what that costs; steps[k-1] = pace that uses k credits; curve = usage at a ladder of paces. Reused for up to 10 minutes (computedAt); a signal for dispatchers, not enforced.",
      "creditPlan (refill providers): per banked credit, what automation does and whether it gets used. action: redeem-now (the account is out and redeeming now beats waiting for every account to run out), drain-first (routing sends new sessions to that account before its weekly reset order so it reaches its limit before drainBy, the credit expiry; a refill only applies at the limit), wait (acting early would not add usage). Each action is taken only when it is never worse at the low, measured and planning paces and better at one. outcome = the credit's fate at the measured pace under the plan (used, lapses, banked); planned false = too little demand evidence or automation off, nothing is acted on; needUnitsPerHour = lowest steady pace that uses it (x168 = account-weeks per week), null when none does. AgentPools applies these itself; a lapsing outcome means more work on that provider is the only remedy.",
      "Model limits: a cap sits inside the all-models quota; a reserve adds capacity outside it. Kind and share are measured or configured, and stay unknown without evidence.",
      "priceSource: provider = the provider's own billing data; list = public list price for the plan tier the provider reports; config = install.json usageLedger.planPrices.",
    ],
    providers: out,
    accounts,
  };
}
