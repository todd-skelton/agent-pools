// Benchmark recalibration of the routing policy, preview first.
//
//   node model-calibrate.mjs                 preview only (writes nothing but the preview report)
//   node model-calibrate.mjs --fetch         take a fresh benchmark snapshot, then preview
//   node model-calibrate.mjs --apply latest [--only 4.claude.primary,...] --by Todd
//   node model-calibrate.mjs --set 1.claude.fallback=haiku/low[@provisional],... --by Todd   (fill missing slots only)
//
// A preview pins digests of every input (policy, rules, registry, the AA
// snapshot with its per-task costs, and the Arena snapshot). --apply recomputes from the current inputs and refuses
// unless the result is identical to the preview, so what is applied is exactly
// what was shown. Without --only it applies the "upgrade" and "savings"
// proposals; a "consider" entry is applied only when named in --only.
import path from "node:path";
import { createHash } from "node:crypto";
import { mkdir, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { readJSON, writeJSONAtomic, withLock } from "./model-store.mjs";

const INDEX_METRICS = new Set(["artificial_analysis_intelligence_index", "artificial_analysis_coding_index", "artificial_analysis_math_index"]);
const SLOT = /^(codex|claude)\.(primary|fallback)$/;
const digest = (v) => createHash("sha256").update(JSON.stringify(v)).digest("hex").slice(0, 16);

export async function loadInputs(root) {
  const bench = path.join(root, "state/benchmarks");
  const latest = await readJSON(path.join(bench, "latest.json"));
  const arenaLatest = await readJSON(path.join(bench, "latest-arena.json"), null);
  const inputs = {
    rules: await readJSON(path.join(root, "calibration-rules.json")),
    policy: await readJSON(path.join(root, "state/routing-policy.json")),
    registry: await readJSON(path.join(root, "state/model-registry.json")),
    snapshot: await readJSON(path.join(bench, path.basename(latest.file))),
    arena: arenaLatest ? await readJSON(path.join(bench, path.basename(arenaLatest.file))) : null,
    manual: await readJSON(path.join(bench, "manual-scores.json"), null),
  };
  if (inputs.manual) validateManualScores(inputs.manual);
  inputs.snapshotFile = path.basename(latest.file);
  inputs.arenaFile = arenaLatest ? path.basename(arenaLatest.file) : null;
  return inputs;
}

// Hand-entered leaderboard ratings (manual-scores/v1) for boards with no
// machine feed yet, such as Design Arena before an API key exists. Every
// entry names its source and capture time; rows score them like Arena.
const MANUAL_KEYS = ["board", "model", "effort", "rating", "lower", "upper", "votes", "source", "sourceUrl", "capturedAt", "enteredBy"];
export function validateManualScores(m) {
  if (m?.format !== "manual-scores/v1" || !Array.isArray(m.rows)) throw new Error("manual-scores.json must be manual-scores/v1 with rows[]");
  m.rows.forEach((r, i) => {
    const missing = MANUAL_KEYS.filter((k) => r?.[k] === undefined || r[k] === null || r[k] === "");
    if (missing.length) throw new Error(`manual-scores row ${i} is missing ${missing.join(", ")}`);
    if (![r.rating, r.lower, r.upper, r.votes].every(Number.isFinite) || !(r.lower <= r.rating && r.rating <= r.upper))
      throw new Error(`manual-scores row ${i} needs numeric rating within [lower, upper] and votes`);
    if (!Number.isFinite(Date.parse(r.capturedAt))) throw new Error(`manual-scores row ${i} capturedAt is not a date`);
  });
  return m;
}

export function resolveFamily(registry, family) {
  const f = registry.families?.[family];
  const id = f?.current ?? f?.candidate ?? null;
  const m = id ? registry.models?.[id] : null;
  const isCurrent = Boolean(id) && f?.current === id;
  return { id, isCurrent, admittedEfforts: isCurrent ? (m?.admittedEfforts ?? []) : [], efforts: m?.efforts ?? [], usable: m?.usableAccountsByEffort ?? {} };
}
const admittedAt = (r, effort) => r.isCurrent && r.admittedEfforts.includes(effort);

// The parts of the registry a preview depends on. Timestamps and account
// lists churn hourly and are excluded; harness versions and admission are not.
export function registryDecisive(registry) {
  return { harnessVersions: registry.harnessVersions ?? null,
    families: Object.fromEntries(Object.entries(registry.families ?? {}).map(([k, f]) => [k, { current: f.current ?? null, candidate: f.candidate ?? null }])),
    admitted: Object.fromEntries(Object.entries(registry.models ?? {}).map(([k, m]) => [k, [...(m.admittedEfforts ?? [])].sort()])) };
}

export function scores(snapshot, id, effort, metrics) {
  const row = snapshot.rows.find((r) => r.model === id && r.effort === effort);
  if (!row) return null;
  const values = {};
  for (const k of metrics) {
    const v = row.evaluations?.[k];
    if (typeof v === "number" && Number.isFinite(v)) values[k] = INDEX_METRICS.has(k) ? v : v * 100;
  }
  return { values };
}

// Arena rating on one board for the exact (model, effort), with its interval.
// Below minVotes the rating is treated as absent.
export function arenaScore(arena, id, effort, board, minVotes) {
  const row = arena?.rows?.find((r) => r.model === id && r.effort === effort && r.board === board);
  if (!row || !(row.votes >= minVotes)) return null;
  return { values: { [board]: row.rating }, lower: row.lower, upper: row.upper, votes: row.votes };
}

// Best-rated listing of a model on one board at any effort with enough votes.
export function arenaBestListed(arena, id, board, minVotes) {
  const best = (arena?.rows ?? []).filter((r) => r.model === id && r.board === board && r.votes >= minVotes)
    .sort((a, b) => b.rating - a.rating)[0];
  return best ? { values: { [board]: best.rating }, lower: best.lower, upper: best.upper, votes: best.votes, proxyEffort: best.effort } : null;
}

// Mean difference over metrics both configs have. The row's first (governing)
// metric must be shared; otherwise no comparison is made.
export function compare(inc, cand, metrics, tol = 1) {
  if (!inc || !cand || !(metrics[0] in inc.values) || !(metrics[0] in cand.values)) return null;
  const shared = metrics.filter((k) => k in inc.values && k in cand.values);
  const diff = shared.reduce((s, k) => s + (cand.values[k] - inc.values[k]), 0) / shared.length;
  return { diff: Math.round(diff * 100) / 100, shared, verdict: diff > tol ? "better" : Math.abs(diff) <= tol ? "parity" : "worse" };
}

// Arena: better only when the intervals do not overlap; parity needs
// overlapping intervals and a point difference within parityElo. Anything
// else (a wide interval hiding a real gap) is "uncertain" and never proposed.
export function compareArena(inc, cand, board, parityElo) {
  if (!inc || !cand) return null;
  const raw = cand.values[board] - inc.values[board];
  const verdict = cand.lower > inc.upper ? "better" : cand.upper < inc.lower ? "worse"
    : Math.abs(raw) <= parityElo ? "parity" : "uncertain";
  return { diff: Math.round(raw * 10) / 10, shared: [board], verdict };
}

const usdPerTask = (snapshot, id, effort) => {
  const v = snapshot?.rows?.find((r) => r.model === id && r.effort === effort)?.usdPerTask;
  return typeof v === "number" && Number.isFinite(v) && v > 0 ? v : undefined;
};

// Relative run cost, only from AA weighted USD per task for both configs (it
// includes token volume). Anything else is "unknown" and never a proposal.
export function relativeCost(snapshot, a, b) {
  const ka = `${a.id}/${a.effort}`, kb = `${b.id}/${b.effort}`;
  const ta = usdPerTask(snapshot, a.id, a.effort), tb = usdPerTask(snapshot, b.id, b.effort);
  if (typeof ta === "number" && typeof tb === "number") {
    return { verdict: tb < ta ? "cheaper" : tb > ta ? "costlier" : "same", ratio: tb / ta, basis: `AA USD/task ${ta} -> ${tb}` };
  }
  if (a.id === b.id && a.effort === b.effort) return { verdict: "same", basis: "same configuration" };
  // Effort labels and list prices say nothing reliable about token volume.
  return { verdict: "unknown", basis: `no AA task cost for ${typeof ta === "number" ? kb : ka}` };
}

export function classify(cmp, cost, maxRatio = 2) {
  if (!cmp) return null;
  if (cmp.verdict === "better" && (cost.verdict === "cheaper" || cost.verdict === "same")) return "upgrade";
  if (cmp.verdict === "parity" && cost.verdict === "cheaper") return "savings";
  // Stronger but costlier: worth a look only when the per-task cost is known and bounded.
  if (cmp.verdict === "better" && typeof cost.ratio === "number" && cost.ratio <= maxRatio) return "consider";
  return null;
}

// How a row is scored. "aa" (default) uses metrics from the AA snapshot;
// "arena" uses one Arena board. Unknown sources are configuration errors.
export function rowScorer(rule, inputs) {
  const source = rule.source ?? "aa";
  if (source === "aa") {
    const metrics = rule.metrics ?? [];
    return { source, metrics, ready: metrics.length > 0, missing: rule.reason ?? "no metrics configured",
      score: (id, effort) => scores(inputs.snapshot, id, effort, metrics),
      compare: (a, b) => compare(a, b, metrics, inputs.rules.parityTolerance ?? 1) };
  }
  if (source === "arena" || source === "manual") {
    if (typeof rule.board !== "string" || !rule.board) throw new Error(`${source} row needs a board`);
    const feed = source === "arena" ? inputs.arena : inputs.manual;
    const minVotes = rule.minVotes ?? inputs.rules.arenaMinVotes ?? 300;
    const parityElo = rule.parityElo ?? inputs.rules.arenaParityElo ?? 10;
    const hasBoard = (feed?.rows ?? []).some((r) => r.board === rule.board);
    return { source, metrics: [rule.board], ready: source === "arena" ? Boolean(feed) : hasBoard,
      missing: source === "arena" ? "no Arena snapshot yet (run with --fetch)" : `no manual-scores.json entries for ${rule.board}`,
      score: (id, effort) => arenaScore(feed, id, effort, rule.board, minVotes),
      scoreIncumbent: (id, effort) => arenaScore(feed, id, effort, rule.board, minVotes)
        ?? (rule.incumbentEffortProxy ? arenaBestListed(feed, id, rule.board, minVotes) : null),
      compare: (a, b) => compareArena(a, b, rule.board, parityElo),
      listed: (id) => (feed?.rows ?? []).filter((r) => r.model === id && r.board === rule.board).map((r) => `${r.effort} (${r.votes} votes)`) };
  }
  throw new Error(`unknown calibration source ${source}`);
}

// Evidence older than these limits is not applied: a stopped refresh must not
// leave old availability, admission or benchmark data eligible. The registry
// applies its own limits when it is built (account lists 6h, admission 14d),
// so a registry this fresh bounds those too. The pool status it was built
// from must be as fresh as the registry, because a missing status would let
// every listed account count as routable. Timestamps more than 5 minutes in
// the future are invalid rather than fresh.
const FUTURE_SKEW_H = 5 / 60;
export function staleInputs(inputs, now, rules = inputs.rules) {
  const benchMax = rules.maxBenchmarkAgeHours ?? 48, regMax = rules.maxRegistryAgeHours ?? 3;
  const out = [];
  const check = (label, iso, max) => {
    const t = Date.parse(iso ?? "");
    const h = Number.isFinite(t) ? (now.getTime() - t) / 3600_000 : Infinity;
    if (h < -FUTURE_SKEW_H) out.push(`${label} ${iso} is in the future`);
    else if (h > max) out.push(`${label} ${iso ?? "never"} (limit ${max}h)`);
  };
  check("registry generated", inputs.registry.generatedAt, regMax);
  check("pool status checked", inputs.registry.source?.statusCheckedAt, regMax);
  check("AA snapshot fetched", inputs.snapshot.fetchedAt, benchMax);
  if (inputs.arena) check("Arena snapshot fetched", inputs.arena.fetchedAt, benchMax);
  // Manual entries are hand-maintained, so they get a longer limit, judged per
  // board actually used by a row and by the oldest entry on that board.
  const manualMax = rules.maxManualAgeHours ?? 336;
  for (const board of new Set(Object.values(rules.rows ?? {}).filter((r) => r.source === "manual").map((r) => r.board))) {
    const times = (inputs.manual?.rows ?? []).filter((r) => r.board === board).map((r) => r.capturedAt).sort();
    if (times.length) check(`manual ${board} captured`, times[0], manualMax);
  }
  return out;
}

const COST_RANK = { cheaper: 0, same: 1, unknown: 2, costlier: 3 };

export function buildPreview(inputs) {
  const { rules, policy, registry, snapshot, arena } = inputs;
  const taskCost = (o) => usdPerTask(snapshot, o.model, o.effort) ?? Infinity;
  const slots = [];
  for (const [row, def] of Object.entries(policy.rows)) {
    const rule = rules.rows?.[row] ?? {};
    for (const [slot, cfg] of Object.entries(def.slots)) {
      if (!SLOT.test(slot)) throw new Error(`bad slot ${row}.${slot}`);
      const harness = slot.split(".")[0];
      const inc = resolveFamily(registry, cfg.family);
      const entry = { id: `${row}.${slot}`, row, task: def.task,
        incumbent: { family: cfg.family, effort: cfg.effort, model: inc.id, admitted: admittedAt(inc, cfg.effort) } };
      slots.push(entry);
      if (rule.protected) { entry.status = "protected"; entry.reason = rule.reason; continue; }
      const scorer = rowScorer(rule, inputs);
      entry.source = scorer.source;
      if (!scorer.ready) { entry.status = "no-benchmark"; entry.reason = scorer.missing; continue; }
      const metrics = scorer.metrics;
      if (rule.proxy) entry.proxy = rule.proxy;
      const incScore = inc.id ? (scorer.scoreIncumbent ?? scorer.score)(inc.id, cfg.effort) : null;
      if (!incScore || !(metrics[0] in incScore.values)) {
        entry.status = "incumbent-unscored";
        if (scorer.listed && inc.id) { const l = scorer.listed(inc.id); entry.reason = `${metrics[0]} lists ${inc.id} ${l.length ? `only at ${l.join(", ")}` : "at no effort"}`; }
        continue;
      }
      entry.incumbent.scores = incScore.values;
      if (incScore.proxyEffort) entry.incumbent.proxyEffort = incScore.proxyEffort;
      if (incScore.lower !== undefined) Object.assign(entry.incumbent, { interval: [incScore.lower, incScore.upper], votes: incScore.votes });
      const families = (rules.laneBinding?.[harness] ?? []).filter((f) =>
        !(rule.excludeFamilies ?? []).includes(f) && (f === cfg.family || !(rules.noGeneralExpansion ?? []).includes(f)));
      const found = [];
      for (const family of families) {
        const r = resolveFamily(registry, family);
        if (!r.id) continue;
        for (const effort of r.efforts) {
          if (family === cfg.family && effort === cfg.effort) continue;
          if (!r.usable[effort]?.length) continue;
          const s = scorer.score(r.id, effort);
          const cmp = scorer.compare(incScore, s);
          if (!cmp) continue;
          const cost = relativeCost(snapshot, { id: inc.id, effort: cfg.effort }, { id: r.id, effort });
          const kind = classify(cmp, cost, rules.considerMaxCostRatio ?? 2);
          if (kind) found.push({ kind, family, effort, model: r.id, admitted: admittedAt(r, effort), diff: cmp.diff, metrics: cmp.shared, scores: s.values,
            ...(s.lower !== undefined ? { interval: [s.lower, s.upper], votes: s.votes } : {}), cost });
        }
      }
      const upgrades = found.filter((f) => f.kind === "upgrade")
        .sort((a, b) => b.diff - a.diff || COST_RANK[a.cost.verdict] - COST_RANK[b.cost.verdict]);
      const savings = found.filter((f) => f.kind === "savings").sort((a, b) => taskCost(a) - taskCost(b) || b.diff - a.diff);
      entry.proposal = upgrades[0] ?? savings[0] ?? null;
      entry.consider = found.filter((f) => f.kind === "consider").sort((a, b) => b.diff - a.diff).slice(0, 2);
      entry.status = entry.proposal ? entry.proposal.kind : "keep";
    }
  }
  const inputDigests = { policy: digest(policy), rules: digest(rules), registry: digest(registryDecisive(registry)), snapshot: digest(snapshot), arena: digest(arena ?? null), manual: digest(inputs.manual ?? null) };
  const body = { format: "calibration-preview/v2", policyGeneration: policy.generation, snapshotFile: inputs.snapshotFile, considerMaxCostRatio: rules.considerMaxCostRatio ?? 2,
    snapshotFetchedAt: snapshot.fetchedAt, arenaFile: inputs.arenaFile ?? null, manualEntries: inputs.manual?.rows?.length ?? 0, arenaRevision: arena?.revision ?? null, arenaPublishDate: arena?.publishDate ?? null,
    inputDigests, slots, arenaStandings: arenaStandings(rules, registry, arena) };
  return { ...body, digest: digest(body) };
}

// Informational: where each current family model sits on the Arena boards the
// rules use, at whatever effort Arena lists. Never used to propose.
export function arenaStandings(rules, registry, arena) {
  if (!arena) return [];
  const boards = [...new Set(Object.values(rules.rows ?? {}).filter((r) => r.source === "arena").map((r) => r.board))];
  const ids = new Set(Object.values(registry.families ?? {}).map((f) => f.current).filter(Boolean));
  return boards.map((board) => ({ board, rows: arena.rows.filter((r) => r.board === board && ids.has(r.model))
    .sort((a, b) => b.rating - a.rating).map((r) => ({ model: r.model, effort: r.effort, rating: r.rating, interval: [r.lower, r.upper], votes: r.votes })) }));
}

const fmt = (c) => `${c.family} ${c.effort}${c.model ? ` (${c.model}${c.admitted === false ? ", awaiting admission" : ""})` : ""}`;
const signed = (n) => `${n >= 0 ? "+" : ""}${n}`;
const ci = (o) => (o?.interval ? ` [${o.interval[0].toFixed(1)}, ${o.interval[1].toFixed(1)}], ${o.votes} votes` : "");

export function renderMarkdown(p) {
  const L = ["# Routing calibration preview", "",
    `Policy generation ${p.policyGeneration}; benchmarks ${p.snapshotFile} (fetched ${p.snapshotFetchedAt}); Arena ${p.arenaFile ?? "none"}${p.arenaRevision ? ` (dataset ${p.arenaRevision.slice(0, 10)}, published ${p.arenaPublishDate})` : ""}${p.manualEntries ? `; manual-scores.json ${p.manualEntries} hand-entered ratings` : ""}; preview digest \`${p.digest}\`.`,
    `AA rows: Artificial Analysis metrics (0-1 values shown x100). Arena rows: rating with its interval; "better" needs non-overlapping intervals. Cost is AA weighted USD per task for both configs. upgrade = better at equal or lower cost; savings = parity and cheaper; consider = better at a known cost of at most ${p.considerMaxCostRatio}x per task (applied only if named).`,
    "", "## Proposed changes", ""];
  const props = p.slots.filter((s) => s.proposal);
  if (!props.length) L.push("None.");
  for (const s of props) {
    const q = s.proposal;
    L.push(`- **${s.id}** (${s.task}): ${fmt(s.incumbent)} -> **${fmt(q)}** [${q.kind}] score ${signed(q.diff)} on ${q.metrics.join(", ")}${ci(q)} vs incumbent${s.incumbent.proxyEffort ? ` scored at ${s.incumbent.proxyEffort} (only effort Arena lists)` : ""}${ci(s.incumbent)}; cost ${q.cost.verdict} (${q.cost.basis})${s.proxy ? `; proxy: ${s.proxy}` : ""}`);
  }
  L.push("", "## Worth considering (applied only if named)", "");
  const cons = p.slots.filter((s) => s.consider?.length);
  if (!cons.length) L.push("None.");
  for (const s of cons) for (const c of s.consider) {
    L.push(`- ${s.id}: ${fmt(s.incumbent)} -> ${fmt(c)} score ${signed(c.diff)} on ${c.metrics.join(", ")}${ci(c)}; ${c.cost.ratio.toFixed(2)}x cost (${c.cost.basis}). Name it as \`${s.id}=${c.family}/${c.effort}\``);
  }
  L.push("", "## Unchanged", "");
  for (const s of p.slots.filter((x) => !x.proposal)) L.push(`- ${s.id}: ${fmt(s.incumbent)} - ${s.status}${s.reason ? ` (${s.reason})` : ""}`);
  for (const b of p.arenaStandings ?? []) {
    L.push("", `## Arena ${b.board} (context only, current family models at the efforts Arena lists)`, "");
    if (!b.rows.length) L.push("None listed.");
    for (const r of b.rows) L.push(`- ${r.model} ${r.effort}: ${r.rating.toFixed(1)} [${r.interval[0].toFixed(1)}, ${r.interval[1].toFixed(1)}], ${r.votes} votes`);
  }
  return L.join("\n") + "\n";
}

export async function preview(root, { fetch: doFetch = false, now = new Date(), log = console.log } = {}) {
  if (doFetch) {
    const { fetchBenchmarks } = await import("./model-benchmarks.mjs");
    const { fetchArena } = await import("./model-arena.mjs");
    const regPath = path.join(root, "state/model-registry.json");
    await withLock(regPath, async () => {
      const reg = await readJSON(regPath);
      log(`benchmarks: ${JSON.stringify(await fetchBenchmarks(root, reg.identities, { now }))}`);
      log(`arena: ${JSON.stringify(await fetchArena(root, reg.identities, { now }))}`);
    });
  }
  const inputs = await loadInputs(root);
  const p = buildPreview(inputs);
  const stale = staleInputs(inputs, now);
  if (stale.length) log(`WARNING: stale inputs; --apply will refuse until refreshed: ${stale.join("; ")}`);
  const dir = path.join(root, "state/calibration");
  await mkdir(dir, { recursive: true });
  const file = path.join(dir, `preview-${now.toISOString().replace(/[-:.]/g, "").slice(0, 18)}Z-${p.digest.slice(0, 6)}.json`);
  await writeJSONAtomic(file, p);
  const md = renderMarkdown(p);
  await writeFile(file.replace(/\.json$/, ".md"), md);
  await writeJSONAtomic(path.join(dir, "latest.json"), { file: path.basename(file), digest: p.digest });
  log(md);
  log(`Preview saved: ${file.replace(/\.json$/, ".md")}`);
  return { file, preview: p };
}

// only: "slotId" takes that slot's proposal; "slotId=family/effort" takes that
// listed proposal or consider entry. Anything not in the preview is refused.
export function selectChanges(p, only) {
  if (!only) return p.slots.filter((s) => s.proposal).map((s) => ({ slot: s, pick: s.proposal }));
  const byId = new Map(p.slots.map((s) => [s.id, s]));
  return only.map((spec) => {
    const [id, choice] = spec.split("=");
    const s = byId.get(id);
    if (!s) throw new Error(`unknown slot ${id}`);
    const options = [s.proposal, ...(s.consider ?? [])].filter(Boolean);
    const pick = choice ? options.find((o) => `${o.family}/${o.effort}` === choice) : s.proposal;
    if (!pick) throw new Error(`${spec} is not offered by this preview`);
    return { slot: s, pick };
  });
}

export async function apply(root, previewRef, { only = null, by, now = new Date(), log = console.log } = {}) {
  if (!by) throw new Error("--by <name> is required");
  const dir = path.join(root, "state/calibration");
  const name = previewRef === "latest" ? (await readJSON(path.join(dir, "latest.json"))).file : path.basename(previewRef);
  const shown = await readJSON(path.join(dir, name));
  const policyPath = path.join(root, "state/routing-policy.json");
  // The registry lock is the one model-refresh holds while it rewrites the
  // registry and snapshots, so inputs cannot change between check and commit.
  return withLock(path.join(root, "state/model-registry.json"), () => withLock(policyPath, async () => {
    const inputs = await loadInputs(root);
    const stale = staleInputs(inputs, now);
    if (stale.length) throw new Error(`stale inputs: ${stale.join("; ")}; let the hourly refresh run (or preview with --fetch), review, then apply`);
    const current = buildPreview(inputs);
    if (current.digest !== shown.digest) {
      const changed = Object.keys(shown.inputDigests).filter((k) => shown.inputDigests[k] !== current.inputDigests[k]);
      throw new Error(`inputs changed since ${name} (${changed.join(", ") || "proposals"}); run the preview again and review it`);
    }
    const { digest: savedDigest, ...savedBody } = shown;
    if (digest(savedBody) !== savedDigest) throw new Error(`${name} does not match its own digest; it was edited after the preview`);
    const changes = selectChanges(current, only);
    const unadmitted = changes.filter((c) => c.pick.admitted !== true).map((c) => `${c.slot.id}=${c.pick.family}/${c.pick.effort}`);
    if (unadmitted.length) throw new Error(`not admitted at that effort yet: ${unadmitted.join(", ")}; wait for admission probes, preview again, then apply`);
    if (!changes.length) { log("Nothing to apply."); return null; }
    const policy = await readJSON(policyPath);
    const hist = path.join(root, "state/routing-policy-history");
    await mkdir(hist, { recursive: true });
    await writeJSONAtomic(path.join(hist, `generation-${policy.generation}.json`), policy);
    const next = structuredClone(policy);
    for (const { slot, pick } of changes) {
      const [row, ...rest] = slot.id.split(".");
      const key = rest.join(".");
      const prev = next.rows[row].slots[key];
      next.rows[row].slots[key] = { family: pick.family, effort: pick.effort, placement: "provisional", since: now.toISOString(),
        previous: { family: prev.family, effort: prev.effort, placement: prev.placement },
        evidence: { preview: name, kind: pick.kind, diff: pick.diff, metrics: pick.metrics, cost: pick.cost } };
      log(`${slot.id}: ${prev.family} ${prev.effort} -> ${pick.family} ${pick.effort}`);
    }
    next.generation = policy.generation + 1;
    next.approvedBy = by;
    next.approvedAt = now.toISOString();
    next.basis = `calibration ${name} (digest ${shown.digest})`;
    await writeJSONAtomic(policyPath, next);
    log(`Routing policy generation ${next.generation} written; generation ${policy.generation} kept in state/routing-policy-history/.`);
    return next;
  }));
}

// Fill row slots the policy does not define yet, such as a missing
// cross-harness fallback. Calibration only ever replaces existing slots, so
// this is the only way to add one. It never replaces or removes a slot, and it
// applies the same lane binding, expansion, protection, freshness and
// per-effort admission rules as --apply.
const SET_SPEC = /^(\d{1,2})\.(codex|claude)\.(primary|fallback)=([a-z][a-z0-9-]*)\/(low|medium|high|xhigh|max)(?:@(provisional|override-[A-Za-z0-9._-]+))?$/;
export function parseSetSpecs(list) {
  const seen = new Set();
  return list.map((spec) => {
    const m = SET_SPEC.exec(spec);
    if (!m) throw new Error(`--set ${spec} is not <row>.<codex|claude>.<primary|fallback>=<family>/<effort>[@provisional|@override-<who>]`);
    const [, row, harness, kind, family, effort, placement] = m;
    const id = `${Number(row)}.${harness}.${kind}`;
    if (Number(row) < 1 || Number(row) > 15) throw new Error(`--set ${spec}: row must be 1-15`);
    if (seen.has(id)) throw new Error(`--set names ${id} twice`);
    seen.add(id);
    return { id, row: String(Number(row)), harness, key: `${harness}.${kind}`, family, effort, placement: placement ?? "provisional" };
  });
}

export async function setSlots(root, specs, { by, now = new Date(), log = console.log } = {}) {
  if (!by) throw new Error("--by <name> is required");
  const policyPath = path.join(root, "state/routing-policy.json");
  return withLock(path.join(root, "state/model-registry.json"), () => withLock(policyPath, async () => {
    const rules = await readJSON(path.join(root, "calibration-rules.json"));
    const registry = await readJSON(path.join(root, "state/model-registry.json"));
    const policy = await readJSON(policyPath);
    // Only registry freshness matters here: no benchmark evidence is claimed.
    const stale = staleInputs({ registry, snapshot: { fetchedAt: now.toISOString() }, arena: null }, now, rules);
    if (stale.length) throw new Error(`stale inputs: ${stale.join("; ")}; let the hourly refresh run, then set`);
    const problems = [];
    for (const c of specs) {
      const rule = rules.rows?.[c.row] ?? {};
      const def = policy.rows?.[c.row];
      if (!def) { problems.push(`${c.id}: row ${c.row} is not in the policy`); continue; }
      if (def.slots[c.key]) { problems.push(`${c.id} already holds ${def.slots[c.key].family}/${def.slots[c.key].effort}; replacing a slot goes through preview and --apply`); continue; }
      if (rule.protected) problems.push(`${c.id}: row ${c.row} is protected (${rule.reason})`);
      if (!(rules.laneBinding?.[c.harness] ?? []).includes(c.family)) problems.push(`${c.id}: ${c.family} is not bound to the ${c.harness} lane`);
      if ((rule.excludeFamilies ?? []).includes(c.family)) problems.push(`${c.id}: ${c.family} is excluded from row ${c.row}`);
      const inRow = Object.values(def.slots).some((x) => x.family === c.family);
      if ((rules.noGeneralExpansion ?? []).includes(c.family) && !inRow) problems.push(`${c.id}: ${c.family} may not expand into a new row`);
      const r = resolveFamily(registry, c.family);
      if (!r.id || !r.isCurrent) problems.push(`${c.id}: ${c.family} has no current model`);
      else if (!admittedAt(r, c.effort) || !r.usable[c.effort]?.length) problems.push(`${c.id}: ${r.id} is not admitted with usable accounts at ${c.effort}`);
    }
    if (problems.length) throw new Error(`refused: ${problems.join("; ")}`);
    const hist = path.join(root, "state/routing-policy-history");
    await mkdir(hist, { recursive: true });
    await writeJSONAtomic(path.join(hist, `generation-${policy.generation}.json`), policy);
    const next = structuredClone(policy);
    for (const c of specs) {
      next.rows[c.row].slots[c.key] = { family: c.family, effort: c.effort, placement: c.placement, since: now.toISOString(),
        evidence: { kind: "slot-fill", by, model: resolveFamily(registry, c.family).id } };
      log(`${c.id}: (none) -> ${c.family} ${c.effort} [${c.placement}]`);
    }
    next.generation = policy.generation + 1;
    next.approvedBy = by;
    next.approvedAt = now.toISOString();
    next.basis = `slot fill: ${specs.map((c) => `${c.id}=${c.family}/${c.effort}@${c.placement}`).join(", ")}`;
    await writeJSONAtomic(policyPath, next);
    log(`Routing policy generation ${next.generation} written; generation ${policy.generation} kept in state/routing-policy-history/.`);
    return next;
  }));
}

// Strict: unknown flags, missing values and repeats are errors, so a typo can
// never widen --only into "apply everything".
export function parseArgs(argv) {
  const takes = new Set(["--apply", "--only", "--by", "--set"]), flags = new Set(["--fetch"]);
  const seen = {};
  for (let i = 0; i < argv.length; i++) {
    const k = argv[i];
    if (!takes.has(k) && !flags.has(k)) throw new Error(`unknown argument ${k}`);
    if (k in seen) throw new Error(`${k} given twice`);
    if (flags.has(k)) { seen[k] = true; continue; }
    const v = argv[++i];
    if (v === undefined || v.startsWith("--") || !v.trim()) throw new Error(`${k} needs a value`);
    seen[k] = v;
  }
  if (seen["--apply"] && seen["--fetch"]) throw new Error("--fetch is for previews; apply what you reviewed");
  if (seen["--set"] && (seen["--apply"] || seen["--fetch"] || seen["--only"])) throw new Error("--set stands alone with --by");
  if (seen["--set"] && !seen["--by"]) throw new Error("--set needs --by");
  if (!seen["--apply"] && !seen["--set"] && (seen["--only"] || seen["--by"])) throw new Error("--only and --by need --apply");
  const only = seen["--only"] ? seen["--only"].split(",").map((x) => x.trim()) : null;
  if (only && (only.length === 0 || only.some((x) => !x))) throw new Error("--only has an empty entry");
  const set = seen["--set"] ? parseSetSpecs(seen["--set"].split(",").map((x) => x.trim())) : null;
  return { apply: seen["--apply"] ?? null, only, by: seen["--by"] ?? null, fetch: Boolean(seen["--fetch"]), set };
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const root = path.dirname(fileURLToPath(import.meta.url));
  try {
    const opts = parseArgs(process.argv.slice(2));
    if (opts.set) await setSlots(root, opts.set, { by: opts.by });
    else if (opts.apply) await apply(root, opts.apply, { only: opts.only, by: opts.by });
    else await preview(root, { fetch: opts.fetch });
  } catch (e) { console.error(`model-calibrate: ${e.message}`); process.exitCode = 1; }
}