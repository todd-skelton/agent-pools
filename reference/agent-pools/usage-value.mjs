import { createReadStream } from "node:fs";
import { readFile, readdir, writeFile, rename } from "node:fs/promises";
import { createInterface } from "node:readline";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { capacityWeight, planOf } from "./capacity-forecast.mjs";
import { LEDGER_DIR, LEDGER_FILE } from "./usage-ledger.mjs";

// Usage value report (state/usage-value.json, served at GET /api/value).
//
// Inputs: the usage ledger (one row per proxied request, usage-ledger.mjs),
// logs/usage-samples.jsonl(.1) and the API price table (model-prices.json).
// Each request is priced at API list price ("API-equivalent dollars"). That is
// what the same tokens would cost on the API, not money saved and not the
// subscription's own accounting. How much weekly quota a dollar of each model
// actually uses is measured, not assumed: per account and time bucket, the
// rise in the account's weekly used percentage is fitted against its requests'
// API-equivalent dollars, three ways (one rate per provider, one per model,
// one per token type), with non-negative least squares. The fit's leftover
// (usage the requests do not explain, such as use outside this pool) is
// reported per account instead of being forced into the rates.
//
// Quota units are fractions of a full (weight 1) account-week: 1% of a Pro
// account's week is 0.01, 1% of a ProLite (weight 0.5) account's week 0.005.

export const VALUE_FORMAT = "usage-value/v1";
export const HOUR = 3600000;
const DAY = 24 * HOUR;
export const DEFAULTS = Object.freeze({ analysisDays: 14, valueDays: 7, wasteDays: 30, bucketHours: 12, reportIntervalMinutes: 60, minFitObservations: 6, singleModelShare: 0.95 });
// Reset times jitter between reads; a move larger than this is a new window.
const RESET_JITTER_MS = HOUR;
// Smallest fall in used percent read as a refill rather than out-of-order reads.
const REFILL_MIN_DROP = 5;

export function valueConfig(input) {
  const config = { ...DEFAULTS, planPrices: {}, modelLimits: Object.create(null) };
  const problems = [];
  if (!input || typeof input !== "object") return { config, problems };
  for (const [key, min, max] of [["analysisDays", 1, 35], ["valueDays", 1, 35], ["wasteDays", 1, 90], ["bucketHours", 1, 48], ["reportIntervalMinutes", 5, 1440]]) {
    if (!(key in input)) continue;
    const v = input[key];
    if (Number.isInteger(v) && v >= min && v <= max) config[key] = v;
    else problems.push(`usageLedger.${key} must be an integer from ${min} to ${max}`);
  }
  if ("planPrices" in input) {
    const p = input.planPrices;
    if (p && typeof p === "object" && !Array.isArray(p)) {
      for (const [k, v] of Object.entries(p)) {
        if (Number.isFinite(v) && v > 0) config.planPrices[k] = v;
        else problems.push(`usageLedger.planPrices.${k} must be a positive number (USD per month)`);
      }
    } else problems.push("usageLedger.planPrices must be an object of USD per month");
  }
  if ("modelLimits" in input) {
    const object = (v) => v && typeof v === "object" && !Array.isArray(v);
    if (!object(input.modelLimits)) problems.push("usageLedger.modelLimits must be an object");
    else for (const [provider, scopes] of Object.entries(input.modelLimits)) {
      if (!object(scopes)) { problems.push(`usageLedger.modelLimits.${provider} must be an object`); continue; }
      for (const [scope, override] of Object.entries(scopes)) {
        if (!object(override) || (!Object.hasOwn(override, "kind") && !Object.hasOwn(override, "shareOfCommon")) ||
            (Object.hasOwn(override, "kind") && !["cap", "reserve", "unknown"].includes(override.kind)) ||
            (Object.hasOwn(override, "shareOfCommon") && !(Number.isFinite(override.shareOfCommon) && override.shareOfCommon > 0))) {
          problems.push(`usageLedger.modelLimits.${provider}.${scope} needs kind (cap, reserve or unknown) and/or positive shareOfCommon`);
          continue;
        }
        (config.modelLimits[provider] ??= Object.create(null))[scope] = {
          ...(Object.hasOwn(override, "kind") ? { kind: override.kind } : {}),
          ...(Object.hasOwn(override, "shareOfCommon") ? { shareOfCommon: override.shareOfCommon } : {}),
        };
      }
    }
  }
  if (config.valueDays > config.analysisDays) {
    problems.push("usageLedger.valueDays cannot exceed analysisDays; using analysisDays");
    config.valueDays = config.analysisDays;
  }
  return { config, problems };
}

// ---- pricing ---------------------------------------------------------------

// Exact names only, after removing a provider prefix (openai/gpt-6.1-sol) and
// a dated or effort suffix (claude-opus-5-5-20261001, gpt-6.1-sol-high). Any
// other variant (a "-mini" or "-fast" model) stays unpriced rather than being
// priced as its base model.
export function priceEntry(prices, model) {
  const models = prices?.models ?? {};
  const raw = String(model ?? "").toLowerCase();
  const names = [raw, raw.replace(/^[a-z0-9_.-]+\//, "")];
  for (const n of [...names]) names.push(n.replace(/-(\d{8}|\d{4}-\d{2}-\d{2}|minimal|none|low|medium|high|xhigh|max|ultra)$/, ""));
  for (const name of names) if (Object.hasOwn(models, name)) return { name, entry: models[name] };
  return null;
}

// API-equivalent USD by token type for one ledger row; null when unpriced.
export function costOf(row, prices) {
  const found = priceEntry(prices, row.model);
  if (!found) return null;
  const e = found.entry;
  const prompt = (row.in || 0) + (row.cr || 0) + (row.cw || 0);
  const r = e.longContext && prompt > e.longContext.threshold ? { ...e, ...e.longContext } : e;
  const m = row.tier === "priority" && Number.isFinite(e.priorityMultiplier) ? e.priorityMultiplier : 1;
  const per = (rate, n) => ((Number(rate) || 0) * (n || 0) * m) / 1e6;
  const byType = { in: per(r.input, row.in), cr: per(r.cacheRead ?? r.input * 0.1, row.cr), cw: per(r.cacheWrite ?? r.input * 1.25, row.cw), out: per(r.output, row.out) };
  return { model: found.name, usd: byType.in + byType.cr + byType.cw + byType.out, byType };
}

// ---- quota observations ----------------------------------------------------

// Increments of one account's weekly used percentage from an ordered stream of
// { at, used, resetAt }. A drop with the same reset date is a Claude refill
// (or a Codex reset seen before its new date); a moved reset date is a new
// window. Neither counts as usage. Natural window ends are returned with the
// percentage left unused.
//
// Readings are compared with the window's high-water mark, not the previous
// reading: concurrent requests report their headers slightly out of order
// (77, 76, 77), and counting each climb back would add phantom usage. Only a
// drop of REFILL_MIN_DROP points or more is a refill; smaller dips are noise.
export function quotaIncrements(stream) {
  const out = { increments: [], windowEnds: [], refills: [] };
  let prev = null;
  let high = null;
  for (const o of stream) {
    if (!Number.isFinite(o.at) || !Number.isFinite(o.used)) continue;
    if (prev) {
      const moved = Number.isFinite(o.resetAt) && Number.isFinite(prev.resetAt) && Math.abs(o.resetAt - prev.resetAt) > RESET_JITTER_MS;
      if (moved) {
        out.windowEnds.push({ at: o.at, lastUsed: high, endedAt: prev.resetAt, early: Number.isFinite(prev.resetAt) && o.at < prev.resetAt - RESET_JITTER_MS });
        high = o.used;
      } else if (o.used <= high - REFILL_MIN_DROP) {
        out.refills.push({ at: o.at, from: high, to: o.used });
        high = o.used;
      } else if (o.used > high) {
        out.increments.push({ at: o.at, percent: o.used - high });
        high = o.used;
      }
    } else high = o.used;
    prev = o;
  }
  return out;
}

// ---- fitting ---------------------------------------------------------------

// Non-negative least squares by projected coordinate descent; small column
// counts only. X rows are observations, y targets. Returns coefficients and R².
export function nnls(X, y, iterations = 5000) {
  const n = X.length, p = n ? X[0].length : 0;
  const XtX = Array.from({ length: p }, () => new Array(p).fill(0));
  const Xty = new Array(p).fill(0);
  for (let i = 0; i < n; i++) for (let a = 0; a < p; a++) {
    Xty[a] += X[i][a] * y[i];
    for (let b = 0; b < p; b++) XtX[a][b] += X[i][a] * X[i][b];
  }
  const k = new Array(p).fill(0);
  for (let it = 0; it < iterations; it++) {
    let change = 0;
    for (let j = 0; j < p; j++) {
      if (XtX[j][j] <= 0) continue;
      let g = Xty[j];
      for (let b = 0; b < p; b++) g -= XtX[j][b] * k[b];
      const next = Math.max(0, k[j] + g / XtX[j][j]);
      change = Math.max(change, Math.abs(next - k[j]));
      k[j] = next;
    }
    if (change < 1e-15) break;
  }
  const mean = n ? y.reduce((s, v) => s + v, 0) / n : 0;
  let ssr = 0, sst = 0;
  for (let i = 0; i < n; i++) {
    const pred = X[i].reduce((s, v, j) => s + v * k[j], 0);
    ssr += (y[i] - pred) ** 2;
    sst += (y[i] - mean) ** 2;
  }
  return { k, r2: sst > 0 ? 1 - ssr / sst : null };
}

// Least squares is pulled far off by a few account-buckets whose quota rose
// for reasons the requests do not explain (use outside the pool). Refit
// without observations whose residual is more than 4 robust standard
// deviations (1.4826 x median absolute deviation) out, up to three times;
// those set aside still count in each account's unexplained usage.
export function robustNnls(X, y) {
  let keep = X.map(() => true);
  let fit = nnls(X, y);
  for (let round = 0; round < 3; round++) {
    const residuals = X.map((row, i) => y[i] - row.reduce((s, v, j) => s + v * fit.k[j], 0));
    const kept = residuals.filter((_, i) => keep[i]);
    const center = median(kept);
    const scale = 1.4826 * median(kept.map((r) => Math.abs(r - center)));
    const limit = Math.max(4 * scale, 1e-9);
    const next = residuals.map((r) => Math.abs(r - center) <= limit);
    if (next.every((v, i) => v === keep[i]) || next.filter(Boolean).length < Math.max(3, X[0]?.length ?? 1)) break;
    keep = next;
    fit = nnls(X.filter((_, i) => keep[i]), y.filter((_, i) => keep[i]));
  }
  return { ...fit, outliers: keep.filter((v) => !v).length, covariance: robustCovariance(X.filter((_, i) => keep[i]), y.filter((_, i) => keep[i]), fit.k) };
}

// Robust residual scale and inverse information matrix. Singular fits cannot
// distinguish a model's own rate from another model's rate: keep them unknown.
function robustCovariance(X, y, k) {
  const p = k.length;
  if (!p || X.length <= p) return null;
  const matrix = k.map((_, a) => [...k.map((_, b) => X.reduce((s, row) => s + row[a] * row[b], 0)), ...k.map((_, b) => +(a === b))]);
  const max = Math.max(...matrix.map((row, i) => row[i]));
  for (let j = 0; j < p; j++) {
    let pivot = j;
    for (let i = j + 1; i < p; i++) if (Math.abs(matrix[i][j]) > Math.abs(matrix[pivot][j])) pivot = i;
    if (Math.abs(matrix[pivot][j]) <= max * 1e-12) return null;
    [matrix[j], matrix[pivot]] = [matrix[pivot], matrix[j]];
    const divisor = matrix[j][j];
    matrix[j] = matrix[j].map((v) => v / divisor);
    for (let i = 0; i < p; i++) if (i !== j) {
      const factor = matrix[i][j];
      matrix[i] = matrix[i].map((v, c) => v - factor * matrix[j][c]);
    }
  }
  const residuals = X.map((row, i) => y[i] - row.reduce((s, v, j) => s + v * k[j], 0));
  const center = median(residuals);
  const scale = 1.4826 * median(residuals.map((r) => Math.abs(r - center)));
  return matrix.map((row) => row.slice(p).map((v) => v * scale ** 2 * X.length / (X.length - p)));
}

const median = (xs) => {
  if (!xs.length) return null;
  const s = [...xs].sort((a, b) => a - b), m = s.length >> 1;
  return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
};
const round = (v, d = 4) => (Number.isFinite(v) ? Math.round(v * 10 ** d) / 10 ** d : null);
// USD per full account-week from a units-per-USD rate.
const perWeek = (k) => (k > 0 ? round(1 / k, 0) : null);

// ---- report ----------------------------------------------------------------

// rows: ledger rows, any (async) iterable (deduplicated here); samples: usage-samples records;
// accounts: pool accounts (id, provider, email, auth_index, quotaWindows);
// registry: model-registry/v2, including historical identities.
// forecast: state/capacity-forecast.json (for projected credit lapses).
export async function buildValueReport(input) {
  return (await buildUsageReports(input)).valueReport;
}

export async function buildUsageReports({ rows, samples = [], accounts = [], prices, config: inputConfig = {}, weights = {}, forecast = null, registry = null, now = Date.now() }) {
  const config = { ...DEFAULTS, planPrices: {}, ...inputConfig };
  const bucketMs = config.bucketHours * HOUR;
  const analysisFrom = now - config.analysisDays * DAY;
  const valueFrom = now - config.valueDays * DAY;
  const wasteFrom = now - config.wasteDays * DAY;
  const byAuthIndex = new Map(accounts.filter((a) => a.auth_index).map((a) => [String(a.auth_index), a]));
  const byId = new Map(accounts.map((a) => [a.id, a]));
  const providerOf = (id, fallback) => byId.get(id)?.provider ?? fallback;
  const weightOf = (id, provider) => capacityWeight(provider, id, weights);

  // Per account: header quota stream, and per bucket: $ by model, $ by type.
  const headerStreams = new Map();
  const cells = new Map(); // provider|account|bucket -> { usd: {model: $}, type: {in,cr,cw,out}, units }
  const cellOf = (provider, acct, bucket) => {
    const key = provider + "|" + acct + "|" + bucket;
    if (!cells.has(key)) cells.set(key, { provider, acct, bucket, usd: {}, type: { in: 0, cr: 0, cw: 0, out: 0 }, units: 0 });
    return cells.get(key);
  };
  const accountsAgg = new Map();
  const modelsAgg = new Map();
  const sessions = new Map();
  const analysisSessions = new Map();
  const unattributedRequests = { codex: { count: 0, apiEquivalentUsd: 0 }, claude: { count: 0, apiEquivalentUsd: 0 } };
  const unpriced = new Map();
  const seen = new Set();
  let ledgerSince = null, ledgerRows = 0, duplicates = 0, unattributed = 0;

  for await (const row of rows) {
    const at = Date.parse(row?.at);
    if (!Number.isFinite(at) || at < analysisFrom || at > now + HOUR) continue;
    if (row.rid) {
      const key = row.rid + "|" + row.at;
      if (seen.has(key)) { duplicates++; continue; }
      seen.add(key);
    }
    ledgerRows++;
    if (ledgerSince === null || at < ledgerSince) ledgerSince = at;
    const acct = row.acct ?? byAuthIndex.get(String(row.ai ?? ""))?.id ?? null;
    const provider = providerOf(acct, row.prov);
    if (!acct) unattributed++;
    if (acct && row.quota?.weekly && Number.isFinite(row.quota.weekly.used)) {
      if (!headerStreams.has(acct)) headerStreams.set(acct, []);
      headerStreams.get(acct).push({ at, used: row.quota.weekly.used, resetAt: row.quota.weekly.resetAt ?? NaN });
    }
    const cost = costOf(row, prices);
    if (!cost) { unpriced.set(row.model, (unpriced.get(row.model) ?? 0) + 1); }
    const usd = cost?.usd ?? 0;
    const model = cost?.model ?? String(row.model);
    if (at <= now) {
      if (row.sid) {
        const s = analysisSessions.get(row.sid) ?? { sid: row.sid, psid: null, client: row.client ?? null, provider, firstAt: at, lastAt: at, requests: 0, failedRequests: 0, usd: 0, usdByModel: {}, efforts: new Set() };
        if (row.psid && !s.psid) s.psid = row.psid;
        s.requests++; if (row.ok === false) s.failedRequests++;
        s.usd += usd; s.usdByModel[model] = (s.usdByModel[model] ?? 0) + usd;
        if (row.eff != null) s.efforts.add(row.eff);
        s.firstAt = Math.min(s.firstAt, at); s.lastAt = Math.max(s.lastAt, at);
        analysisSessions.set(row.sid, s);
      } else {
        const a = unattributedRequests[provider] ??= { count: 0, apiEquivalentUsd: 0 };
        a.count++; a.apiEquivalentUsd += usd;
      }
    }
    if (acct && cost) {
      const cell = cellOf(provider, acct, Math.floor(at / bucketMs));
      cell.usd[model] = (cell.usd[model] ?? 0) + usd;
      for (const t of ["in", "cr", "cw", "out"]) cell.type[t] += cost.byType[t];
    }
    if (at >= valueFrom) {
      const a = accountsAgg.get(acct ?? "(unattributed)") ?? { requests: 0, failed: 0, usd: 0, usdByModel: {} };
      a.requests++; if (row.ok === false) a.failed++;
      a.usd += usd; a.usdByModel[model] = (a.usdByModel[model] ?? 0) + usd;
      accountsAgg.set(acct ?? "(unattributed)", a);
      const mk = provider + "|" + model;
      const m = modelsAgg.get(mk) ?? { provider, model, requests: 0, failed: 0, usd: 0, tokens: { in: 0, cr: 0, cw: 0, out: 0, rsn: 0 }, sessions: new Set() };
      m.requests++; if (row.ok === false) m.failed++;
      m.usd += usd;
      for (const t of ["in", "cr", "cw", "out", "rsn"]) m.tokens[t] += row[t] || 0;
      if (row.sid) m.sessions.add(row.sid);
      modelsAgg.set(mk, m);
      if (row.sid) {
        const s = sessions.get(row.sid) ?? { sid: row.sid, client: row.client, provider, firstAt: at, lastAt: at, requests: 0, usd: 0, usdByModel: {} };
        s.requests++; s.usd += usd; s.usdByModel[model] = (s.usdByModel[model] ?? 0) + usd;
        s.firstAt = Math.min(s.firstAt, at); s.lastAt = Math.max(s.lastAt, at);
        sessions.set(row.sid, s);
      }
    }
  }

  // Sample streams (weekly all-models window per provider) per account.
  const sampleStreams = new Map();
  const weeklyWindow = { claude: "limit:1", codex: "codex:primary_window" };
  for (const s of samples) {
    if (!s || weeklyWindow[s.provider] !== s.window) continue;
    const at = Date.parse(s.at);
    if (!Number.isFinite(at) || !Number.isFinite(s.used)) continue;
    if (!sampleStreams.has(s.accountId)) sampleStreams.set(s.accountId, []);
    sampleStreams.get(s.accountId).push({ at, used: s.used, resetAt: Number(s.resetAt) });
  }
  for (const list of [...sampleStreams.values(), ...headerStreams.values()]) list.sort((a, b) => a.at - b.at);

  // Two views of each account's usage. Fitting needs usage close to the
  // requests that caused it: response headers from the first header reading on
  // (one per request), samples before that. Totals need every rise, including
  // use outside the pool while it sends no requests: samples (polled every two
  // minutes), or headers for an account with no samples. The two sources round
  // differently, so they are never mixed within one stretch.
  const ids = new Set([...sampleStreams.keys(), ...headerStreams.keys()]);
  const accountUsage = new Map();
  for (const id of ids) {
    const headers = headerStreams.get(id) ?? [];
    const sampled = sampleStreams.get(id) ?? [];
    const start = headers.length ? headers[0].at : Infinity;
    const fromSamples = quotaIncrements(sampled);
    accountUsage.set(id, {
      fitIncrements: [...quotaIncrements(sampled.filter((o) => o.at < start)).increments, ...quotaIncrements(headers).increments],
      increments: sampled.length ? fromSamples.increments : quotaIncrements(headers).increments,
      windowEnds: fromSamples.windowEnds,
      refills: sampled.length ? fromSamples.refills : quotaIncrements(headers).refills,
      source: sampled.length ? "samples" : "headers",
      fitSource: headers.length ? "headers" : "samples",
    });
  }
  // Fit observations: only buckets the ledger covers (dollars known).
  const ledgerStart = ledgerSince ?? Infinity;
  for (const [id, u] of accountUsage) {
    const provider = providerOf(id, id.startsWith("claude") ? "claude" : "codex");
    for (const inc of u.fitIncrements) {
      if (inc.at < Math.max(analysisFrom, ledgerStart)) continue;
      cellOf(provider, id, Math.floor(inc.at / bucketMs)).units += (weightOf(id, provider) * inc.percent) / 100;
    }
  }
  // The newest bucket is still filling; quota reads lag requests within it.
  const currentBucket = Math.floor(now / bucketMs);
  const fitCells = [...cells.values()].filter((c) => c.bucket < currentBucket);

  const fits = {};
  const modelFits = new Map();
  const providers = [...new Set([...fitCells.map((c) => c.provider), ...[...modelsAgg.values()].map((m) => m.provider)])];
  for (const provider of providers) {
    const obs = fitCells.filter((c) => c.provider === provider);
    const totalUsd = {};
    for (const c of obs) for (const [m, v] of Object.entries(c.usd)) totalUsd[m] = (totalUsd[m] ?? 0) + v;
    const grand = Object.values(totalUsd).reduce((s, v) => s + v, 0);
    const buckets = obs.map((c) => c.bucket);
    const hours = obs.length ? ((buckets.reduce((a, b) => Math.max(a, b)) - buckets.reduce((a, b) => Math.min(a, b)) + 1) * bucketMs) / HOUR : 0;
    const fit = { observations: obs.length, hours, usd: round(grand, 2), units: round(obs.reduce((s, c) => s + c.units, 0)) };
    if (obs.length < config.minFitObservations || hours < 24 || grand <= 0) {
      fit.status = "insufficient-data";
      fit.note = `Needs at least ${config.minFitObservations} account-buckets over 24 hours with priced requests; has ${obs.length} over ${hours} h.`;
      fits[provider] = fit;
      continue;
    }
    const y = obs.map((c) => c.units);
    const pooled = robustNnls(obs.map((c) => [Object.values(c.usd).reduce((s, v) => s + v, 0)]), y);
    // Models under 1% of the provider's dollars share one column.
    const models = Object.keys(totalUsd).filter((m) => totalUsd[m] >= 0.01 * grand).sort();
    const cols = [...models, ...(models.length < Object.keys(totalUsd).length ? ["(other)"] : [])];
    const perModel = robustNnls(obs.map((c) => cols.map((m) => (m === "(other)" ? Object.entries(c.usd).filter(([k]) => !models.includes(k)).reduce((s, [, v]) => s + v, 0) : c.usd[m] ?? 0))), y);
    modelFits.set(provider, { cols, ...perModel });
    const types = ["in", "cr", "cw", "out"];
    const perType = robustNnls(obs.map((c) => types.map((t) => c.type[t])), y);
    // Single-model check: buckets where one model is nearly all of an
    // account's dollars give that model's rate directly.
    const single = {};
    for (const c of obs) {
      const total = Object.values(c.usd).reduce((s, v) => s + v, 0);
      if (total <= 0 || c.units <= 0) continue;
      const [m, v] = Object.entries(c.usd).sort((a, b) => b[1] - a[1])[0];
      if (v / total >= config.singleModelShare) (single[m] ??= []).push(total / c.units);
    }
    Object.assign(fit, {
      status: "ok",
      pooled: { usdPerAccountWeek: perWeek(pooled.k[0]), unitsPerUsd: pooled.k[0], r2: round(pooled.r2, 3), outliers: pooled.outliers },
      perModel: { r2: round(perModel.r2, 3), outliers: perModel.outliers, models: Object.fromEntries(cols.map((m, i) => [m, { usdPerAccountWeek: perWeek(perModel.k[i]), unitsPerUsd: perModel.k[i], usd: round(m === "(other)" ? grand - models.reduce((s, x) => s + totalUsd[x], 0) : totalUsd[m], 2) }])) },
      perTokenType: { r2: round(perType.r2, 3), outliers: perType.outliers, types: Object.fromEntries(types.map((t, i) => [t, { unitsPerUsd: perType.k[i], usdPerAccountWeek: perWeek(perType.k[i]) }])) },
      singleModel: Object.fromEntries(Object.entries(single).map(([m, xs]) => [m, { buckets: xs.length, usdPerAccountWeek: round(median(xs), 0) }])),
    });
    fits[provider] = fit;
  }
  const modelLimits = [];
  const limits = new Map();
  const coverageOf = (stream) => {
    const first = stream.reduce((t, s) => Math.min(t, s.at), Infinity);
    return new Set(stream.filter((s) => s.at > first && first <= Math.floor(s.at / bucketMs) * bucketMs).map((s) => Math.floor(s.at / bucketMs)));
  };
  const commonCoverage = new Map([...ids].map((id) => [id, coverageOf([...(headerStreams.get(id) ?? []), ...(sampleStreams.get(id) ?? [])].filter((s) => s.at <= now))]));
  const windowStreams = new Map();
  for (const s of samples) {
    const at = Date.parse(s.at);
    if (!Number.isFinite(at) || at > now || !Number.isFinite(s.used)) continue;
    const key = JSON.stringify([s.provider, s.accountId, s.window]);
    if (!windowStreams.has(key)) windowStreams.set(key, []);
    windowStreams.get(key).push({ at, used: s.used, resetAt: s.resetAt });
  }
  for (const a of accounts) for (const w of a.quotaWindows ?? []) {
    if (w.common !== false || !Number.isFinite(w.durationSeconds) || w.durationSeconds < 86400) continue;
    const key = JSON.stringify([a.provider, w.scope, w.id]);
    if (!limits.has(key)) limits.set(key, { provider: a.provider, scope: w.scope, window: w.id, accounts: new Set() });
    limits.get(key).accounts.add(a.id);
  }
  for (const limit of limits.values()) {
    const { provider, scope, window } = limit;
    const models = Object.entries(registry?.identities ?? {}).filter(([, m]) => m.provider === provider && String(m.family).toLowerCase() === String(scope).toLowerCase()).map(([id]) => id).sort();
    const modelSet = new Set(models);
    const units = new Map(), coverage = new Map();
    for (const id of limit.accounts) {
      const stream = (windowStreams.get(JSON.stringify([provider, id, window])) ?? []).sort((a, b) => a.at - b.at);
      coverage.set(id, coverageOf(stream));
      for (const inc of quotaIncrements(stream).increments) {
        if (inc.at < Math.max(analysisFrom, ledgerStart)) continue;
        const key = id + "|" + Math.floor(inc.at / bucketMs);
        units.set(key, (units.get(key) ?? 0) + weightOf(id, provider) * inc.percent / 100);
      }
    }
    const scopeUsd = (c) => Object.entries(c.usd).reduce((s, [m, usd]) => s + (modelSet.has(m) ? usd : 0), 0);
    const obs = fitCells.filter((c) => c.provider === provider && limit.accounts.has(c.acct) && scopeUsd(c) > 0 &&
      coverage.get(c.acct)?.has(c.bucket) && commonCoverage.get(c.acct)?.has(c.bucket));
    const hours = obs.length ? (obs.reduce((v, c) => Math.max(v, c.bucket), -Infinity) - obs.reduce((v, c) => Math.min(v, c.bucket), Infinity) + 1) * bucketMs / HOUR : 0;
    const result = { provider, scope, window, models, kind: "unknown", shareOfCommon: null, shareBasis: null, source: "unknown", observations: obs.length, note: null };
    const own = modelFits.get(provider);
    let windowRate = null, modelRate = null;
    if (obs.length < config.minFitObservations || hours < 24) result.note = `Needs at least ${config.minFitObservations} account-buckets over 24 hours with this scope's priced requests and quota readings; has ${obs.length} over ${hours} h.`;
    else if (!own?.covariance) result.note = "The all-models fit cannot distinguish the scope's own rate.";
    else {
      const total = obs.reduce((s, c) => s + scopeUsd(c), 0);
      const shares = own.cols.map((m) => modelSet.has(m) ? obs.reduce((s, c) => s + (c.usd[m] ?? 0), 0) / total : 0);
      const mapped = shares.reduce((s, v) => s + v, 0);
      const k = shares.reduce((s, v, j) => s + v * own.k[j], 0);
      const se = Math.sqrt(Math.max(0, shares.reduce((s, a, i) => s + shares.reduce((v, b, j) => v + a * b * own.covariance[i][j], 0), 0)));
      const w = robustNnls(obs.map((c) => [scopeUsd(c)]), obs.map((c) => units.get(c.acct + "|" + c.bucket) ?? 0)).k[0];
      if (mapped > 1 - 1e-9 && w > 1e-12) {
        windowRate = w; modelRate = k;
        result.kind = k > se + 1e-12 ? "cap" : "reserve";
        result.shareBasis = result.kind === "cap" ? "model" : "pooled";
        const kRef = result.kind === "cap" ? k : fits[provider]?.pooled?.unitsPerUsd;
        if (kRef > 1e-12) { result.shareOfCommon = round(kRef / w); result.source = "measured"; }
        else { result.kind = "unknown"; result.shareBasis = null; }
      }
      if (result.source === "unknown") result.note = "No positive, identifiable model-window rate and common reference rate.";
    }
    const override = Object.entries(config.modelLimits?.[provider] ?? {}).find(([name]) => name.toLowerCase() === String(scope).toLowerCase())?.[1];
    if (override) {
      Object.assign(result, override, { source: "config", note: "Configured in usageLedger.modelLimits." });
      result.shareBasis = result.kind === "cap" ? "model" : result.kind === "reserve" ? "pooled" : null;
      if (override.kind && !Object.hasOwn(override, "shareOfCommon")) {
        const reference = override.kind === "cap" ? modelRate : fits[provider]?.pooled?.unitsPerUsd;
        result.shareOfCommon = reference > 1e-12 && windowRate > 1e-12 ? round(reference / windowRate) : null;
      }
      if (result.kind === "unknown") result.shareOfCommon = null;
    }
    modelLimits.push(result);
  }
  // Units per USD for a model: its own fitted rate when it has one (and the
  // per-model fit explains at least as much as the pooled one), else pooled.
  const rateFor = (provider, model) => {
    const f = fits[provider];
    if (f?.status !== "ok") return null;
    const own = f.perModel.models[model] ?? f.perModel.models["(other)"];
    if (own && own.unitsPerUsd > 0 && (f.perModel.r2 ?? -1) >= (f.pooled.r2 ?? -1)) return own.unitsPerUsd;
    return f.pooled.unitsPerUsd > 0 ? f.pooled.unitsPerUsd : null;
  };
  const estUnits = (provider, usdByModel) => {
    let total = 0;
    for (const [m, v] of Object.entries(usdByModel)) {
      const k = rateFor(provider, m);
      if (k === null) return null;
      total += k * v;
    }
    return total;
  };

  // Accounts over the value window, or since the ledger's first row when it is
  // younger, so dollars and quota always cover the same stretch. Rates and the
  // plan comparison wait for a full day of it.
  const windowStart = ledgerSince === null ? valueFrom : Math.max(valueFrom, ledgerSince);
  const windowDays = (now - windowStart) / DAY;
  const enough = windowDays >= 1;
  const weeks = windowDays / 7;
  // Monthly plan price: the provider's own billing data first, then
  // usageLedger.planPrices, then a list price for the tier the provider
  // reports (refresh.mjs account.plan).
  const planPrice = (id, provider) => {
    const reported = byId.get(id)?.plan?.price;
    const usd = Number.isFinite(reported?.usdPerMonth) && reported.usdPerMonth > 0 ? reported.usdPerMonth : null;
    if (usd !== null && reported.source === "provider") return { usd, source: "provider" };
    const plan = byId.get(id)?.plan?.plan ?? planOf(id);
    const v = config.planPrices[id] ?? (plan ? config.planPrices[provider + ":" + plan] : undefined) ?? config.planPrices[provider];
    if (Number.isFinite(v)) return { usd: v, source: "config" };
    return usd !== null ? { usd, source: reported.source ?? "list" } : { usd: null, source: null };
  };
  const accountRows = accounts.map((a) => {
    const agg = accountsAgg.get(a.id) ?? { requests: 0, failed: 0, usd: 0, usdByModel: {} };
    const usage = accountUsage.get(a.id);
    const weight = weightOf(a.id, a.provider);
    const usedPercent = (usage?.increments ?? []).filter((i) => i.at >= windowStart).reduce((s, i) => s + i.percent, 0);
    const usedUnits = (weight * usedPercent) / 100;
    const predicted = estUnits(a.provider, agg.usdByModel);
    const { usd: monthly, source: planPriceSource } = planPrice(a.id, a.provider);
    const planPerWindow = monthly === null ? null : (monthly * 12 / 52) * weeks;
    return {
      id: a.id, email: a.email, provider: a.provider, weight,
      requests: agg.requests, failedRequests: agg.failed,
      apiEquivalentUsd: round(agg.usd, 2),
      usdByModel: Object.fromEntries(Object.entries(agg.usdByModel).map(([m, v]) => [m, round(v, 2)])),
      weeklyPercentUsed: round(usedPercent, 1),
      quotaUnitsUsed: round(usedUnits),
      quotaSource: usage?.source ?? "none",
      fitSource: usage?.fitSource ?? "none",
      usdPerAccountWeek: enough && usedUnits > 0 && agg.usd > 0 ? round(agg.usd / usedUnits, 0) : null,
      // Positive: quota used that this pool's requests do not explain at the
      // fitted rates (other clients, the web or phone apps, or fit error).
      unexplainedUnits: predicted !== null && enough ? round(usedUnits - predicted) : null,
      planUsdPerMonth: monthly,
      planPriceSource,
      valueMultiple: planPerWindow && enough ? round(agg.usd / planPerWindow, 2) : null,
    };
  });
  const unattributedAgg = accountsAgg.get("(unattributed)");

  // Models over the value window, with their estimated share of quota.
  const providerUnits = {};
  const modelRows = [...modelsAgg.values()].map((m) => {
    const k = rateFor(m.provider, m.model);
    const units = k === null ? null : k * m.usd;
    if (units !== null) providerUnits[m.provider] = (providerUnits[m.provider] ?? 0) + units;
    return { provider: m.provider, model: m.model, requests: m.requests, failedRequests: m.failed, sessions: m.sessions.size, apiEquivalentUsd: round(m.usd, 2), usdPerRequest: m.requests ? round(m.usd / m.requests, 3) : null, tokens: m.tokens, estimatedQuotaUnits: round(units) };
  }).map((m) => ({ ...m, shareOfProviderQuota: m.estimatedQuotaUnits !== null && providerUnits[m.provider] > 0 ? round(m.estimatedQuotaUnits / providerUnits[m.provider], 3) : null }))
    .sort((a, b) => b.apiEquivalentUsd - a.apiEquivalentUsd);

  const sessionRows = [...sessions.values()].map((s) => ({
    sid: s.sid, client: s.client, provider: s.provider, firstAt: new Date(s.firstAt).toISOString(), lastAt: new Date(s.lastAt).toISOString(),
    requests: s.requests, apiEquivalentUsd: round(s.usd, 2), models: Object.keys(s.usdByModel), estimatedQuotaUnits: round(estUnits(s.provider, s.usdByModel)),
  })).sort((a, b) => b.apiEquivalentUsd - a.apiEquivalentUsd).slice(0, 15);

  const costBasis = Object.fromEntries([...new Set(["codex", "claude", ...[...analysisSessions.values()].map((s) => s.provider), ...Object.keys(unattributedRequests)])].sort()
    .map((p) => [p, fits[p]?.status === "ok" ? "units" : "usd"]));
  const rootOf = (session) => {
    const seen = new Set([session.sid]);
    let current = session;
    while (current.psid) {
      const parent = analysisSessions.get(current.psid);
      if (!parent || parent.provider !== session.provider || seen.has(parent.sid)) break;
      seen.add(parent.sid);
      current = parent;
    }
    return current.sid;
  };
  const sessionUsage = {
    format: "session-usage/v1", generatedAt: new Date(now).toISOString(), from: new Date(analysisFrom).toISOString(), costBasis,
    sessions: [...analysisSessions.values()].sort((a, b) => a.firstAt - b.firstAt || (a.sid < b.sid ? -1 : a.sid > b.sid ? 1 : 0)).map((s) => ({
      sid: s.sid, psid: s.psid, rootSid: rootOf(s), provider: s.provider, client: s.client,
      firstAt: new Date(s.firstAt).toISOString(), lastAt: new Date(s.lastAt).toISOString(), requests: s.requests, failedRequests: s.failedRequests,
      apiEquivalentUsd: s.usd, usdByModel: Object.fromEntries(Object.entries(s.usdByModel).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0)),
      efforts: [...s.efforts].sort(), quotaUnits: costBasis[s.provider] === "units" ? estUnits(s.provider, s.usdByModel) : null,
    })),
    unattributedRequests,
  };

  // Waste: weekly quota left when a window ended naturally (samples), over the
  // waste window. Codex windows start on first use, so one that ended at 0%
  // never ran and lost nothing.
  const waste = accounts.map((a) => {
    const ends = (accountUsage.get(a.id)?.windowEnds ?? []).filter((e) => e.at >= wasteFrom && !e.early && !(a.provider === "codex" && e.lastUsed <= 0));
    const weight = weightOf(a.id, a.provider);
    const refills = (accountUsage.get(a.id)?.refills ?? []).filter((r) => r.at >= wasteFrom && r.from - r.to >= 20);
    return {
      id: a.id, provider: a.provider, windowsEnded: ends.length,
      unusedUnits: round(ends.reduce((s, e) => s + (weight * Math.max(0, 100 - e.lastUsed)) / 100, 0)),
      ends: ends.map((e) => ({ at: new Date(e.at).toISOString(), lastUsedPercent: e.lastUsed })),
      refillsSeen: refills.length,
    };
  });
  const projected = {};
  for (const [p, f] of Object.entries(forecast?.providers ?? {})) {
    projected[p] = { state: f.state ?? null, creditsExpiringUnused: f.projection?.creditsExpiringUnused ?? null, wastedUnits: f.projection?.wastedUnits ?? null, creditsLapsingOutlook: f.creditOutlook?.creditsLapsing ?? null, creditExpiries: f.supply?.creditExpiries ?? [] };
  }
  const samplesSince = samples.reduce((min, s) => { const t = Date.parse(s?.at); return Number.isFinite(t) && t < min ? t : min; }, Infinity);

  const valueReport = {
    format: VALUE_FORMAT,
    generatedAt: new Date(now).toISOString(),
    config: { analysisDays: config.analysisDays, valueDays: config.valueDays, wasteDays: config.wasteDays, bucketHours: config.bucketHours },
    // Start of the per-account, per-model and session tables.
    valueFrom: new Date(windowStart).toISOString(),
    valueWindowDays: round(windowDays, 2),
    coverage: {
      ledgerSince: ledgerSince === null ? null : new Date(ledgerSince).toISOString(),
      ledgerRows, duplicates, unattributedRows: unattributed,
      unattributedUsd: round(unattributedAgg?.usd ?? 0, 2),
      samplesSince: Number.isFinite(samplesSince) ? new Date(samplesSince).toISOString() : null,
      unpricedModels: Object.fromEntries(unpriced),
      prices: { source: prices?.source ?? null, effectiveFrom: prices?.effectiveFrom ?? null },
    },
    fits,
    modelLimits,
    accounts: accountRows,
    models: modelRows,
    sessions: sessionRows,
    waste: { measured: waste, projected },
  };
  return { valueReport, sessionUsage };
}

// ---- CLI: node usage-value.mjs [--root DIR] --------------------------------

async function* jsonLines(file) {
  let stream;
  try { stream = createReadStream(file, { encoding: "utf8" }); } catch { return; }
  const lines = createInterface({ input: stream, crlfDelay: Infinity });
  try {
    for await (const line of lines) {
      if (!line) continue;
      try { const v = JSON.parse(line); if (v && typeof v === "object") yield v; } catch {}
    }
  } catch (e) { if (e.code !== "ENOENT") throw e; }
}
const readJSON = async (file) => JSON.parse((await readFile(file, "utf8")).replace(/^\uFEFF/, ""));

async function transcriptSession(file) {
  const stream = createReadStream(file, { encoding: "utf8" });
  const lines = createInterface({ input: stream, crlfDelay: Infinity });
  try {
    for await (const line of lines) {
      const event = JSON.parse(line.replace(/^\uFEFF/, ""));
      return event?.type === "thread.started" ? event.thread_id : event?.session_id;
    }
  } catch (e) {
    if (e.code !== "ENOENT" && !(e instanceof SyntaxError)) throw e;
  } finally {
    lines.close();
    stream.destroy();
  }
  return null;
}

export async function buildRoutingAttempts(sessionUsage, orchestratorRoot) {
  const report = { format: "routing-attempts/v1", generatedAt: sessionUsage.generatedAt, orchestratorRoot: orchestratorRoot || null, attempts: [] };
  if (!orchestratorRoot) return { ...report, note: "state/install.json has no orchestratorRoot; dispatch join is disabled." };
  const sessions = new Map(sessionUsage.sessions.map((s) => [s.sid, s]));
  const roots = new Map();
  for (const s of sessionUsage.sessions) {
    const key = s.provider + "|" + s.rootSid;
    const group = roots.get(key) ?? { first: Infinity, last: -Infinity, quotaUnits: 0, apiEquivalentUsd: 0 };
    group.first = Math.min(group.first, Date.parse(s.firstAt)); group.last = Math.max(group.last, Date.parse(s.lastAt));
    group.apiEquivalentUsd += s.apiEquivalentUsd;
    group.quotaUnits = group.quotaUnits === null || s.quotaUnits === null ? null : group.quotaUnits + s.quotaUnits;
    roots.set(key, group);
  }
  const transcripts = new Map();
  for await (const row of jsonLines(path.join(orchestratorRoot, "dispatch-log.jsonl"))) {
    const at = Date.parse(row.ts ?? row.at);
    if (row.kind !== "dispatch" || !Number.isFinite(at) || at < Date.parse(sessionUsage.from) || at > Date.parse(sessionUsage.generatedAt) || typeof row.transcript !== "string" || !row.transcript) continue;
    const file = path.resolve(orchestratorRoot, row.transcript);
    if (!transcripts.has(file)) transcripts.set(file, await transcriptSession(file));
    const session = sessions.get(transcripts.get(file));
    if (!session) continue;
    const group = roots.get(session.provider + "|" + session.rootSid);
    report.attempts.push({ label: row.label ?? null, row: row.row ?? null, model: row.model ?? null, effort: row.effort ?? null, rootSid: session.rootSid,
      at: new Date(at).toISOString(), quotaUnits: group.quotaUnits, apiEquivalentUsd: group.apiEquivalentUsd, hours: (group.last - group.first) / HOUR });
  }
  return report;
}

async function writeAtomic(file, report) {
  const temporary = `${file}.${process.pid}.tmp`;
  // A reader holding the file can make the rename fail on Windows: retry once.
  for (let attempt = 0; ; attempt++) {
    try {
      await writeFile(temporary, JSON.stringify(report, null, 2));
      await rename(temporary, file);
      return;
    } catch (e) {
      if (attempt >= 1) throw e;
      await new Promise((r) => setTimeout(r, 250));
    }
  }
}

export async function writeValueReport(root, now = Date.now()) {
  let install = {};
  try { install = await readJSON(path.join(root, "state/install.json")); } catch {}
  const { config, problems } = valueConfig(install.usageLedger);
  const prices = await readJSON(path.join(root, "model-prices.json"));
  let accounts = [];
  try { accounts = (await readJSON(path.join(root, "state/pool-state.json"))).accounts ?? []; } catch {}
  let forecast = null;
  try { forecast = await readJSON(path.join(root, "state/capacity-forecast.json")); } catch {}
  let registry = null;
  try { registry = await readJSON(path.join(root, "state/model-registry.json")); } catch {}
  const firstDay = new Date(now - (config.analysisDays + 1) * DAY).toISOString().slice(0, 10);
  let names = [];
  try { names = (await readdir(path.join(root, LEDGER_DIR))).filter((n) => { const m = LEDGER_FILE.exec(n); return m && m[1] >= firstDay; }).sort(); } catch {}
  // Streamed: weeks of rows are never held in memory at once.
  async function* rows() { for (const name of names) yield* jsonLines(path.join(root, LEDGER_DIR, name)); }
  const samples = [];
  const keepFrom = now - Math.max(config.analysisDays, config.wasteDays) * DAY - 8 * DAY;
  for (const name of ["logs/usage-samples.jsonl.1", "logs/usage-samples.jsonl"]) {
    for await (const s of jsonLines(path.join(root, name))) if (Date.parse(s.at) >= keepFrom) samples.push(s);
  }
  samples.sort((a, b) => Date.parse(a.at) - Date.parse(b.at));
  const { valueReport: report, sessionUsage } = await buildUsageReports({
    rows: rows(), samples, prices, now, forecast, registry,
    accounts: accounts.map((a) => ({ id: a.id, provider: a.provider, email: a.email, auth_index: a.auth_index, plan: a.plan ?? null, quotaWindows: a.quotaWindows ?? [] })),
    config, weights: install.capacityForecast?.weights ?? {},
  });
  report.problems = problems;
  const attempts = await buildRoutingAttempts(sessionUsage, typeof install.orchestratorRoot === "string" ? install.orchestratorRoot : null);
  await writeAtomic(path.join(root, "state/session-usage.json"), sessionUsage);
  await writeAtomic(path.join(root, "state/routing-attempts.json"), attempts);
  await writeAtomic(path.join(root, "state/usage-value.json"), report);
  return report;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const i = process.argv.indexOf("--root");
  const root = i > 0 ? path.resolve(process.argv[i + 1]) : path.dirname(fileURLToPath(import.meta.url));
  writeValueReport(root).then(
    (r) => { console.log(`usage-value: ${r.coverage.ledgerRows} ledger rows, fits: ${Object.entries(r.fits).map(([p, f]) => p + "=" + f.status).join(", ") || "none"}`); },
    (e) => { console.error("usage-value failed: " + e.message); process.exit(1); },
  );
}
