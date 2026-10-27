// Daily Artificial Analysis snapshot keyed by exact model id and effort.
// Rows are matched only when the AA slug maps unambiguously to a registry
// identity and an effort; anything else stays unmatched (never guessed).
// Snapshots are dated files under state/benchmarks/ and are never rewritten.
// Scores come from the data endpoint; weighted USD per Intelligence Index task
// comes from the free language endpoint, joined on the AA model id.
// Attribution: data from https://artificialanalysis.ai/ (free API terms).
import path from "node:path";
import { mkdir, writeFile, link, rm } from "node:fs/promises";
import { randomBytes } from "node:crypto";
import { EFFORTS } from "./model-registry.mjs";
import { readJSON, writeJSONAtomic } from "./model-store.mjs";

const API = "https://artificialanalysis.ai/api/v2/data/llms/models";
const COST_API = "https://artificialanalysis.ai/api/v2/language/models/free";
const MAX_COST_PAGES = 25;

export function effortFromName(name) {
  const n = String(name ?? "").toLowerCase();
  if (/non-reasoning/.test(n)) return null;
  for (const e of [...EFFORTS].reverse()) {
    if (new RegExp(`\\(${e}[),]|[(,] ?${e} effort`).test(n)) return e;
  }
  return null;
}

// AA writes "gpt-6-1-sol" for gpt-6.1-sol and appends "-<effort>" except for the
// maximum-effort row. The name carries the effort, so both must agree.
export function matchRow(row, identities) {
  const effort = effortFromName(row.name);
  if (!effort) return null;
  const base = effort === "max" ? row.slug : row.slug.endsWith(`-${effort}`) ? row.slug.slice(0, -effort.length - 1) : null;
  if (!base) return null;
  const hits = Object.keys(identities).filter((id) => id.replace(/\./g, "-") === base);
  return hits.length === 1 ? { model: hits[0], effort } : null;
}

// costRows: rows from the free endpoint. A cost is attached only when that
// endpoint has the same AA id with the same slug and name, so a re-labelled
// row can never lend its cost to another configuration.
// An id listed twice with any difference is ambiguous and gets no cost.
export function taskCosts(costRows) {
  const seen = new Map(), conflicted = new Set();
  for (const r of costRows ?? []) {
    if (!r?.id) continue;
    const v = r.artificial_analysis_intelligence_index_cost?.cost_per_task?.total_cost;
    const sig = JSON.stringify([r.slug, r.name, v ?? null]);
    if (seen.has(r.id) && seen.get(r.id).sig !== sig) conflicted.add(r.id);
    seen.set(r.id, { sig, slug: r.slug, name: r.name, usd: v });
  }
  const byId = new Map();
  for (const [id, e] of seen) {
    if (conflicted.has(id) || typeof e.usd !== "number" || !Number.isFinite(e.usd) || e.usd <= 0) continue;
    byId.set(id, { slug: e.slug, name: e.name, usd: e.usd });
  }
  return byId;
}

export function snapshot(apiData, identities, fetchedAt, { costRows = null, indexVersion = null } = {}) {
  const costs = taskCosts(costRows);
  const keyed = new Map(), dup = new Set();
  for (const r of apiData) {
    const m = matchRow(r, identities);
    if (!m) continue;
    const c = costs.get(r.id);
    const usdPerTask = c && c.slug === r.slug && c.name === r.name ? c.usd : null;
    const key = `${m.model}|${m.effort}`;
    if (keyed.has(key)) { dup.add(key); continue; }
    keyed.set(key, { model: m.model, effort: m.effort, aaId: r.id, aaSlug: r.slug, aaName: r.name,
      evaluations: r.evaluations ?? {}, pricing: r.pricing ?? {}, usdPerTask,
      outputTokensPerSecond: r.median_output_tokens_per_second ?? null,
      firstAnswerTokenSeconds: r.median_time_to_first_answer_token ?? null });
  }
  // Two AA rows for one (model, effort) are ambiguous; neither is used.
  const rows = [...keyed].filter(([k]) => !dup.has(k)).map(([, v]) => v);
  rows.sort((a, b) => a.model.localeCompare(b.model) || EFFORTS.indexOf(a.effort) - EFFORTS.indexOf(b.effort));
  return { format: "aa-snapshot/v2", source: API, costSource: COST_API, intelligenceIndexVersion: indexVersion,
    attribution: "https://artificialanalysis.ai/", fetchedAt, rows, ambiguous: [...dup].sort() };
}

// Per-request timeout that never runs past the caller's overall deadline.
export function budgetSignal(deadline, maxMs) {
  const left = deadline - Date.now();
  if (!(left > 0)) throw new Error("benchmark fetch deadline reached; deferred to the next run");
  return AbortSignal.timeout(Math.max(1, Math.min(maxMs, left)));
}

async function fetchCostRows(fetchImpl, key, deadline) {
  const rows = [];
  let version = null;
  for (let page = 1; page <= MAX_COST_PAGES; page++) {
    const res = await fetchImpl(`${COST_API}?page=${page}`, { headers: { "x-api-key": key }, signal: budgetSignal(deadline, 30_000) });
    if (!res.ok) throw new Error(`AA cost API ${res.status}`);
    const body = await res.json();
    if (!Array.isArray(body.data)) throw new Error("AA cost API returned no data array");
    // Costs from different Intelligence Index versions measure different task sets.
    const v = body.intelligence_index_version;
    if (!((typeof v === "number" && Number.isFinite(v)) || (typeof v === "string" && /^\d+(\.\d+)*$/.test(v.trim())))) throw new Error(`AA cost API page ${page} has no intelligence_index_version`);
    if (version !== null && String(v) !== String(version)) throw new Error(`AA cost API index version changed mid-fetch (${version} -> ${v})`);
    version = v;
    if (typeof body.pagination?.has_more !== "boolean") throw new Error(`AA cost API page ${page} has no pagination.has_more`);
    rows.push(...body.data);
    if (!body.pagination.has_more) return { rows, version };
  }
  throw new Error(`AA cost API still has more after ${MAX_COST_PAGES} pages`);
}

// Writes the complete file under a temp name, then hard-links it to the final
// name. link() fails with EEXIST instead of replacing, so a published snapshot
// can never be overwritten; a collision just retries with a new suffix.
export async function publishExclusive(dir, stem, value) {
  const body = JSON.stringify(value, null, 2) + "\n";
  const tmp = path.join(dir, `.${stem}.${process.pid}.${randomBytes(4).toString("hex")}.tmp`);
  await writeFile(tmp, body, { flag: "wx" });
  try {
    for (let i = 0; i < 8; i++) {
      const file = path.join(dir, `${stem}-${randomBytes(3).toString("hex")}.json`);
      try { await link(tmp, file); return file; }
      catch (e) { if (e.code !== "EEXIST") throw e; }
    }
    throw new Error(`could not publish a unique snapshot for ${stem}`);
  } finally { await rm(tmp, { force: true }); }
}
export async function fetchBenchmarks(root, identities, { now = new Date(), fetchImpl = fetch, deadline = Infinity } = {}) {
  const { artificialAnalysisKey: key } = await readJSON(path.join(root, "state/secrets.json"));
  if (!key) return { skipped: "no artificialAnalysisKey in state/secrets.json" };
  const res = await fetchImpl(API, { headers: { "x-api-key": key }, signal: budgetSignal(deadline, 30_000) });
  if (!res.ok) throw new Error(`AA API ${res.status}`);
  const body = await res.json();
  if (!Array.isArray(body.data)) throw new Error("AA API returned no data array");
  const cost = await fetchCostRows(fetchImpl, key, deadline);
  const snap = snapshot(body.data, identities, now.toISOString(), { costRows: cost.rows, indexVersion: cost.version });
  const dir = path.join(root, "state/benchmarks");
  await mkdir(dir, { recursive: true });
  // One immutable file per fetch (ms timestamp + random suffix, never replaced) so a calibration preview can pin its exact input.
  const file = await publishExclusive(dir, `aa-${now.toISOString().replace(/[-:.]/g, "").slice(0, 18)}Z`, snap);
  await writeJSONAtomic(path.join(dir, "latest.json"), { file: path.basename(file), fetchedAt: snap.fetchedAt, rows: snap.rows.length });
  return { file, rows: snap.rows.length, withCost: snap.rows.filter((r) => r.usdPerTask !== null).length };
}
