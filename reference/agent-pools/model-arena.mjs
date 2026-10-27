// Arena (LMArena) leaderboard snapshot, keyed by exact model id and effort.
// Source: the official Hugging Face dataset lmarena-ai/leaderboard-dataset.
// The download is pinned to the dataset commit sha read first, so one snapshot
// is one exact revision. Arena names such as "claude-opus-5.5-high" map to a
// registry identity only when "<id>-<effort>" matches exactly one identity
// (dots and dashes compared alike); anything else stays unmatched, never guessed.
// Snapshots are immutable files under state/benchmarks/ like the AA ones.
import path from "node:path";
import { mkdir } from "node:fs/promises";
import { parquetReadObjects } from "hyparquet";
import { EFFORTS } from "./model-registry.mjs";
import { writeJSONAtomic } from "./model-store.mjs";
import { publishExclusive, budgetSignal } from "./model-benchmarks.mjs";

export const DATASET = "lmarena-ai/leaderboard-dataset";
// Style-controlled text (prose categories) and WebDev (UI design).
export const CONFIGS = Object.freeze(["text_style_control", "webdev"]);
const HUB = "https://huggingface.co";
const MAX_FILE_BYTES = 64 * 1024 * 1024;
const REQUIRED = ["model_name", "rating", "rating_lower", "rating_upper", "vote_count", "category", "leaderboard_publish_date"];
const NAME = new RegExp(`^([a-z0-9][a-z0-9.-]*)-(${EFFORTS.join("|")})$`);

export function matchArenaName(name, identities) {
  const m = NAME.exec(String(name ?? ""));
  if (!m) return null;
  const base = m[1].replace(/\./g, "-");
  const hits = Object.keys(identities).filter((id) => id.replace(/\./g, "-") === base);
  return hits.length === 1 ? { model: hits[0], effort: m[2] } : null;
}

const num = (v) => (typeof v === "bigint" ? Number(v) : v);
// Publish dates may decode as strings or Date objects; keep only real UTC dates.
export function isoDate(v) {
  if (v instanceof Date) return Number.isNaN(v.getTime()) ? null : v.toISOString().slice(0, 10);
  const s = String(v ?? "");
  if (!/^\d{4}-\d{2}-\d{2}$/.test(s)) return null;
  const t = Date.parse(`${s}T00:00:00Z`);
  return !Number.isNaN(t) && new Date(t).toISOString().slice(0, 10) === s ? s : null; // rejects 2026-02-30
}

// Every shard of the latest split: one shared total, each index exactly once.
export function latestShards(names, config) {
  const re = new RegExp(`^${config}/latest-(\\d+)-of-(\\d+)\\.parquet$`);
  const hits = names.map((n) => [n, re.exec(n)]).filter(([, m]) => m);
  if (!hits.length) throw new Error(`Arena ${config} has no latest parquet`);
  const totals = new Set(hits.map(([, m]) => Number(m[2])));
  const total = [...totals][0];
  const idx = hits.map(([, m]) => Number(m[1])).sort((a, b) => a - b);
  if (totals.size !== 1 || idx.length !== total || idx.some((v, i) => v !== i)) throw new Error(`Arena ${config} latest shards are incomplete: ${hits.map(([n]) => n).join(", ")}`);
  return hits.map(([n]) => n).sort();
}
const finite = (v) => typeof v === "number" && Number.isFinite(v);

// rowsByConfig: { config: [parquet row objects] }. A (model, effort, board)
// that appears more than once is ambiguous and dropped entirely.
export function arenaSnapshot(rowsByConfig, identities, { fetchedAt, revision }) {
  const keyed = new Map(), seen = new Set(), dup = new Set(), unmatched = new Set();
  let publishDate = null;
  for (const [config, rows] of Object.entries(rowsByConfig)) {
    for (const r of rows) {
      for (const k of REQUIRED) if (!(k in r)) throw new Error(`Arena ${config} is missing column ${k}`);
      const m = matchArenaName(r.model_name, identities);
      if (!m) { if (/^(claude|gpt)-/.test(String(r.model_name))) unmatched.add(r.model_name); continue; }
      const board = `${config}/${r.category}`, key = `${m.model}|${m.effort}|${board}`;
      // Any second row for a configuration makes it ambiguous, valid or not.
      if (seen.has(key)) { dup.add(key); continue; }
      seen.add(key);
      const row = { model: m.model, effort: m.effort, board, arenaName: r.model_name,
        rating: num(r.rating), lower: num(r.rating_lower), upper: num(r.rating_upper), votes: num(r.vote_count), publishDate: isoDate(r.leaderboard_publish_date) };
      if (![row.rating, row.lower, row.upper, row.votes].every(finite) || !(row.lower <= row.rating && row.rating <= row.upper) || !row.publishDate) continue;
      keyed.set(key, row);
      if (!publishDate || row.publishDate > publishDate) publishDate = row.publishDate;
    }
  }
  const rows = [...keyed].filter(([k]) => !dup.has(k)).map(([, v]) => v)
    .sort((a, b) => a.board.localeCompare(b.board) || a.model.localeCompare(b.model) || EFFORTS.indexOf(a.effort) - EFFORTS.indexOf(b.effort));
  return { format: "arena-snapshot/v1", source: `${HUB}/datasets/${DATASET}`, revision, attribution: "https://lmarena.ai/ (LMArena leaderboard dataset)",
    fetchedAt, publishDate, rows, ambiguous: [...dup].sort(), unmatched: [...unmatched].sort() };
}

async function getJSON(fetchImpl, url, deadline) {
  const res = await fetchImpl(url, { signal: budgetSignal(deadline, 30_000) });
  if (!res.ok) throw new Error(`Arena ${url} -> ${res.status}`);
  return res.json();
}

const readParquetBuffer = (file) => parquetReadObjects({ file });

async function getParquet(fetchImpl, url, readParquet, deadline) {
  const res = await fetchImpl(url, { signal: budgetSignal(deadline, 120_000) });
  if (!res.ok) throw new Error(`Arena ${url} -> ${res.status}`);
  const buf = await res.arrayBuffer();
  if (buf.byteLength > MAX_FILE_BYTES) throw new Error(`Arena ${url} is ${buf.byteLength} bytes, over the limit`);
  return readParquet(buf);
}

export async function fetchArena(root, identities, { now = new Date(), fetchImpl = fetch, readParquet = readParquetBuffer, deadline = Infinity } = {}) {
  const info = await getJSON(fetchImpl, `${HUB}/api/datasets/${DATASET}`, deadline);
  const revision = info.sha;
  if (!/^[0-9a-f]{40}$/.test(String(revision))) throw new Error("Arena dataset has no commit sha");
  const pinned = await getJSON(fetchImpl, `${HUB}/api/datasets/${DATASET}/revision/${revision}`, deadline);
  const names = (pinned.siblings ?? []).map((s) => s.rfilename);
  const rowsByConfig = {};
  for (const config of CONFIGS) {
    rowsByConfig[config] = [];
    for (const f of latestShards(names, config)) rowsByConfig[config].push(...await getParquet(fetchImpl, `${HUB}/datasets/${DATASET}/resolve/${revision}/${f}`, readParquet, deadline));
    if (!rowsByConfig[config].length) throw new Error(`Arena ${config} decoded no rows at ${revision}`);
  }
  const snap = arenaSnapshot(rowsByConfig, identities, { fetchedAt: now.toISOString(), revision });
  const dir = path.join(root, "state/benchmarks");
  await mkdir(dir, { recursive: true });
  const file = await publishExclusive(dir, `arena-${now.toISOString().replace(/[-:.]/g, "").slice(0, 18)}Z`, snap);
  await writeJSONAtomic(path.join(dir, "latest-arena.json"), { file: path.basename(file), fetchedAt: snap.fetchedAt, revision, publishDate: snap.publishDate, rows: snap.rows.length });
  return { file, rows: snap.rows.length, revision, publishDate: snap.publishDate };
}