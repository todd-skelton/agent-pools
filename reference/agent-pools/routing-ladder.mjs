import { createHash } from "node:crypto";
import { arenaScore, scores } from "./model-calibrate.mjs";

const HOUR = 3600000;
const EFFORTS = ["low", "medium", "high", "xhigh", "max"];
const positive = (v) => Number.isFinite(v) && v > 0;
const nonnegative = (v) => Number.isFinite(v) && v >= 0;
const hash = (v) => createHash("sha256").update(JSON.stringify(v ?? null)).digest("hex");
const median = (values) => {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b), mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
};
const key = (row, model, effort) => JSON.stringify([String(row), model, effort]);
const taskCost = (inputs, model, effort) => {
  const v = inputs.snapshot.rows.find((r) => r.model === model && r.effort === effort)?.usdPerTask;
  return positive(v) ? v : null;
};

function quotaPrice(forecast, provider, floor, now) {
  const p = forecast?.providers?.[provider], pace = p?.pace;
  const demand = nonnegative(pace?.atDemand?.unitsPerHour) ? pace.atDemand.unitsPerHour : null;
  const neutral = (priceSource) => ({ quotaPrice: 1, priceSource, demand, target: null });
  if (!p || p.state === "unknown" || p.stale || !(Date.parse(forecast.staleAfter) > now)) return neutral("forecast-unavailable");
  const computed = Date.parse(pace?.computedAt);
  if (!Number.isFinite(computed) || computed > now || now - computed > 15 * 60000) return neutral("pace-stale");
  if (!nonnegative(pace.atDemand?.lostUnits) || pace.atDemand.lostUnits <= .05) return neutral("no-loss");
  if (demand === null) return neutral("demand-unavailable");
  let target = pace.fullUse?.unitsPerHour, priceSource = "full-use";
  if (!positive(target)) {
    const steps = (pace.steps ?? []).filter((s) => s.reachable === true && positive(s.unitsPerHour));
    target = steps.length ? Math.max(...steps.map((s) => s.unitsPerHour)) : null;
    priceSource = "reachable-step";
  }
  if (!positive(target)) {
    const curve = (pace.curve ?? []).filter((s) => nonnegative(s.lostUnits) && s.lostUnits <= .05 && positive(s.unitsPerHour));
    target = curve.length ? Math.min(...curve.map((s) => s.unitsPerHour)) : null;
    priceSource = "no-loss-curve";
  }
  if (Date.parse(pace.then?.from) <= now && positive(pace.then?.unitsPerHour)) {
    target = pace.then.unitsPerHour; priceSource = "then";
  }
  if (!positive(target)) return neutral("target-unavailable");
  return { quotaPrice: Math.min(1, Math.max(floor, demand / target)), priceSource, demand, target };
}

function measuredConfigs(inputs, now, costBasis) {
  const groups = new Map(), days = inputs.value?.config?.analysisDays ?? 14;
  if (!positive(days)) throw new Error("routing ladder needs positive analysisDays");
  for (const a of inputs.attempts?.attempts ?? []) {
    // Strict controller review rows duplicate labelled dispatches but have no
    // configuration identity. Never let them supply cost, scale, time or n.
    if (typeof a.label !== "string" || !a.label.trim() || !a.model || !EFFORTS.includes(a.effort) || !/^([1-9]|1[0-5])$/.test(String(a.row))) continue;
    const at = Date.parse(a.at), cost = costBasis === "usd" ? a.apiEquivalentUsd : a.quotaUnits;
    if (!(at >= now - days * 24 * HOUR && at <= now) || !nonnegative(cost) || !nonnegative(a.apiEquivalentUsd) || !nonnegative(a.hours)) continue;
    const k = key(a.row, a.model, a.effort);
    if (!groups.has(k)) groups.set(k, { row: String(a.row), model: a.model, effort: a.effort, attempts: [] });
    groups.get(k).attempts.push(a);
  }
  return [...groups.values()].map((g) => ({ ...g, n: g.attempts.length,
    cost: median(g.attempts.map((a) => costBasis === "usd" ? a.apiEquivalentUsd : a.quotaUnits)),
    usd: median(g.attempts.map((a) => a.apiEquivalentUsd)), hours: median(g.attempts.map((a) => a.hours)) }));
}

function candidates(inputs) {
  return Object.entries(inputs.registry.families).sort(([a], [b]) => a.localeCompare(b)).flatMap(([family, f]) => {
    if (!f.current) return [];
    const provider = f.provider ?? inputs.registry.models?.[f.current]?.provider;
    if (!provider) throw new Error(`routing ladder family ${family} needs a provider`);
    return EFFORTS.map((effort) => ({ model: f.current, family, provider, effort }));
  });
}

function probabilities(inputs, rule, configs) {
  if (rule.source === "arena" || rule.source === "manual") {
    const rated = configs.flatMap((c) => {
      const rating = arenaScore(rule.source === "arena" ? inputs.arena : inputs.manual, c.model, c.effort, rule.board, inputs.rules.arenaMinVotes ?? 300)?.values[rule.board];
      return Number.isFinite(rating) ? [{ ...c, rating }] : [];
    });
    const top = Math.max(...rated.map((r) => r.rating));
    return rated.map(({ rating, ...c }) => ({ ...c, p: 1 / (1 + 10 ** ((top - rating) / 400)) }));
  }
  if (rule.source && rule.source !== "aa") throw new Error(`unknown routing ladder source ${rule.source}`);
  if (!rule.metrics?.length) return [];
  return configs.flatMap((c) => {
    const values = scores(inputs.snapshot, c.model, c.effort, rule.metrics)?.values;
    if (!values || !rule.metrics.every((m) => Number.isFinite(values[m]) && values[m] >= 0 && values[m] <= 100)) return [];
    const p = rule.metrics.reduce((sum, m) => sum + values[m], 0) / rule.metrics.length / 100;
    return [{ ...c, p }];
  });
}

// Pure scoring over cached file documents; availability belongs to the controller.
export function buildLadders(inputs, now = Date.now()) {
  now = Number(now);
  if (!Number.isFinite(now)) throw new Error("routing ladder needs a valid time");
  const configs = candidates(inputs), providerIds = [...new Set(configs.map((c) => c.provider))].sort();
  const floor = inputs.rules.quotaPriceFloor ?? .1;
  if (!positive(floor) || floor > 1) throw new Error("quotaPriceFloor must be in (0, 1]");
  const basisProviders = [...new Set(["codex", "claude", ...providerIds])];
  const costBasis = basisProviders.some((p) => (inputs.sessionUsage?.costBasis?.[p] ?? (inputs.value?.fits?.[p]?.status === "ok" ? "units" : "usd")) !== "units") ? "usd" : "units";
  const providers = Object.fromEntries(providerIds.map((p) => [p, quotaPrice(inputs.forecast, p, floor, now)]));
  const all = measuredConfigs(inputs, now, costBasis), measured = all.filter((c) => c.n >= 5);
  const groups = new Map(all.map((g) => [key(g.row, g.model, g.effort), g]));
  const scales = measured.flatMap((c) => {
    const usd = taskCost(inputs, c.model, c.effort);
    return usd !== null ? [{ row: c.row, scale: c.usd / usd }] : [];
  });
  const rows = {};
  for (let i = 1; i <= 15; i++) {
    const id = String(i), rule = inputs.rules.rows[id] ?? {};
    if (rule.fixed) {
      const rungs = rule.fixed.map((spec) => {
        const [family, effort] = spec.split("/"), c = configs.find((r) => r.family === family && r.effort === effort);
        if (!c) throw new Error(`fixed routing rung ${spec} has no current model`);
        return { ...c, p: null, cost: null, costSource: null, hours: null, hoursSource: null, n: null, score: null };
      });
      rows[id] = { basis: "fixed", failureMultiple: rule.failureMultiple ?? null, typicalCost: null, typicalHours: null, hourValue: null, failureCost: null, rungs };
      continue;
    }
    const failureMultiple = rule.failureMultiple ?? 0;
    if (!nonnegative(failureMultiple)) throw new Error(`row ${id} needs a nonnegative failureMultiple`);
    const rowMeasured = measured.filter((m) => m.row === id), rowScales = scales.filter((s) => s.row === id);
    const rowScale = median((rowScales.length ? rowScales : scales).map((s) => s.scale)) ?? 1;
    const scaleSource = rowScales.length ? "row" : scales.length ? "all-rows" : "none";
    const rungs = probabilities(inputs, rule, configs).flatMap((c) => {
      const group = groups.get(key(id, c.model, c.effort)), n = group?.n ?? 0;
      let cost = n >= 5 ? group.cost : null, costSource = "measured";
      if (n < 5) {
        const usd = taskCost(inputs, c.model, c.effort);
        if (usd === null) return [];
        const fit = inputs.value?.fits?.[c.provider];
        const own = fit?.status === "ok" ? fit.perModel?.models?.[c.model]?.unitsPerUsd : null;
        const pooled = fit?.status === "ok" ? fit.pooled?.unitsPerUsd : null;
        const units = positive(own) ? own : positive(pooled) ? pooled : null;
        if (costBasis === "units" && units === null) return [];
        cost = usd * rowScale * (costBasis === "usd" ? 1 : units);
        costSource = costBasis === "usd" ? "estimated:usd" : positive(own) ? "estimated:model-fit" : "estimated:pooled-fit";
      }
      const effortMeasured = rowMeasured.filter((m) => m.effort === c.effort);
      const hours = n >= 5 ? group.hours : median((effortMeasured.length ? effortMeasured : rowMeasured.length ? rowMeasured : measured).map((m) => m.hours));
      const hoursSource = n >= 5 ? "measured" : effortMeasured.length ? "row-effort" : rowMeasured.length ? "row" : measured.length ? "all-rows" : "none";
      return [{ ...c, cost, costSource, hours, hoursSource, n, score: null }];
    });
    const typicalCost = median(rungs.map((r) => r.cost)), typicalHours = median(rungs.map((r) => r.hours).filter(nonnegative));
    const hourValue = positive(typicalHours) ? typicalCost / typicalHours : null;
    const failureCost = typicalCost !== null ? failureMultiple * typicalCost : null;
    for (const r of rungs) if (r.p > 0 && hourValue !== null && r.hours !== null)
      r.score = (r.cost * providers[r.provider].quotaPrice + hourValue * r.hours + failureCost * (1 - r.p)) / r.p;
    rungs.sort((a, b) => (a.score ?? Infinity) - (b.score ?? Infinity) || a.cost - b.cost || a.family.localeCompare(b.family) || EFFORTS.indexOf(a.effort) - EFFORTS.indexOf(b.effort));
    rows[id] = { basis: rule.source === "arena" || rule.source === "manual" ? `${rule.source}:${rule.board}` : "metrics", failureMultiple, rowScale, scaleSource, typicalCost, typicalHours, hourValue, failureCost, rungs };
  }
  const payload = { format: "routing-ladders/v1", inputs: {
    registryAuthorityDigest: inputs.registry.authorityDigest ?? null,
    rules: hash(inputs.rules), registry: hash(inputs.registry), snapshot: inputs.snapshotFile ?? hash(inputs.snapshot),
    arena: inputs.arenaFile ?? hash(inputs.arena), manual: hash(inputs.manual),
    attempts: hash(inputs.attempts), value: hash(inputs.value), sessionUsage: hash(inputs.sessionUsage), forecast: "capacity-forecast.json",
  }, costBasis, providers, rows };
  return { ...payload, generatedAt: new Date(now).toISOString(), staleAfter: new Date(now + .5 * HOUR).toISOString(), digest: hash(payload) };
}
