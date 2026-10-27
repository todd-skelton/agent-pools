// One registry refresh cycle, safe to run on a schedule (hidden, see
// Run-Model-Refresh.vbs). Under a single lock it:
//   1. reads live lists, pool status, lineage and admission evidence;
//   2. probes only what promotion or re-verification needs (bounded per run),
//      and defers probing while a controller release battery is running;
//   3. rebuilds the registry, persisting lineage monotonically;
//   4. takes the daily benchmark snapshots (Artificial Analysis and Arena).
import path from "node:path";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { buildRegistry, emptyLineage, admittedEfforts } from "./model-registry.mjs";
import { admit, readProbes, resolveExe, harnessVersion } from "./model-admission.mjs";
import { fetchBenchmarks } from "./model-benchmarks.mjs";
import { fetchArena } from "./model-arena.mjs";
import { readJSON, writeJSONAtomic, withLock } from "./model-store.mjs";

const MAX_PROBES_PER_RUN = 16;
const BENCHMARK_EVERY_MS = 20 * 3600_000;
// Probes start only within this window; one probe can take 4 more minutes, which
// stays under the 30-minute stale-lock threshold and the 45-minute task limit.
const PROBE_BUDGET_MS = 20 * 60_000;
// Benchmark fetches start only before BENCH_START_MS and stop issuing requests
// at BENCH_DEADLINE_MS (one in flight may add 2 minutes), leaving time to
// publish the registry inside the 45-minute task limit. Deferred work runs next hour.
const BENCH_START_MS = 26 * 60_000;
const BENCH_DEADLINE_MS = 38 * 60_000;

export function batteryRunning() {
  try {
    const out = execFileSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command",
      "@(Get-CimInstance Win32_Process -Filter \"Name='pwsh.exe' OR Name='powershell.exe'\" | Where-Object { $_.CommandLine -match 'controller-release-battery\\.ps1' }).Count"],
      { encoding: "utf8", windowsHide: true, timeout: 30_000 });
    return Number(out.trim()) > 0;
  } catch { return true; } // unknown load: do not probe
}

// Efforts routing may use beyond requiredEfforts: every family/effort in the
// routing policy and every option the latest calibration preview offers. They
// are probed too, so an offered option becomes applicable after the next run.
export function wantedEfforts(policy, preview) {
  const out = {};
  const add = (family, effort) => { if (typeof family === "string" && typeof effort === "string") (out[family] ??= new Set()).add(effort); };
  for (const row of Object.values(policy?.rows ?? {})) for (const s of Object.values(row.slots ?? {})) add(s.family, s.effort);
  for (const s of preview?.slots ?? []) for (const o of [s.proposal, ...(s.consider ?? [])]) if (o) add(o.family, o.effort);
  return out;
}

// Wanted efforts lacking fresh ok evidence, for the current model and any
// newer candidate. Historical models are never probed.
export function probePlan(registry, families, probes, harnessVersions, now, extra = {}) {
  const plan = [];
  for (const rule of families.families) {
    const f = registry.families[rule.family];
    const wanted = [...new Set([...rule.requiredEfforts, ...(extra[rule.family] ?? [])])];
    for (const id of [f.candidate, f.current].filter(Boolean)) {
      const m = registry.models[id];
      if (!m) continue;
      const fresh = admittedEfforts(probes, id, harnessVersions[rule.provider], now);
      const need = wanted.filter((e) => m.efforts.includes(e) && !fresh.includes(e));
      if (need.length) plan.push({ id, provider: rule.provider, efforts: need });
    }
  }
  return plan;
}

// Least recently attempted (model, effort) first, so a target that keeps
// failing or timing out cannot starve the ones after it.
export function fairBatch(plan, probes, limit, now = Date.now()) {
  const last = new Map();
  for (const p of probes) {
    const k = `${p.model}|${p.effort}`, t = Date.parse(p.at ?? "");
    if (Number.isFinite(t) && t <= now + 5 * 60_000 && t > (last.get(k) ?? -Infinity)) last.set(k, t); // future rows cannot park a target
  }
  const items = plan.flatMap((t, i) => t.efforts.map((effort, j) => ({ t, effort, order: i * 100 + j, at: last.get(`${t.id}|${effort}`) ?? -Infinity })));
  items.sort((a, b) => a.at - b.at || a.order - b.order);
  const out = new Map();
  for (const x of items.slice(0, Math.max(0, limit))) {
    const e = out.get(x.t.id) ?? { ...x.t, efforts: [] };
    e.efforts.push(x.effort);
    out.set(x.t.id, e);
  }
  return [...out.values()];
}

// A registry published while the network or pool was briefly down (waking from
// sleep, a supervisor restart) can report current models unavailable. The
// hourly run would leave that in place for up to an hour, and the orchestrator
// routes only to usable accounts, so --recheck (every few minutes, see
// Register-Pool-Tasks.ps1) rebuilds without probes or benchmarks while any
// family reports an unavailable required effort, and does nothing otherwise.
export function degraded(registry) {
  if (!registry || typeof registry.families !== "object") return true;
  return Object.values(registry.families).some((f) => (f?.unavailableEfforts?.length ?? 0) > 0);
}

export async function recheck(root, { log = console.log } = {}) {
  const current = await readJSON(path.join(root, "state/model-registry.json"), null).catch(() => null);
  if (!degraded(current)) return { rebuilt: false };
  log(`--- model-refresh --recheck ${new Date().toISOString()} pid=${process.pid}`);
  try { return { rebuilt: true, registry: await refresh(root, { probe: false, benchmarks: false, log }) }; }
  catch (e) {
    // The hourly run holds the lock while probing and publishes when done.
    if (/^lock busy/.test(String(e?.message))) { log("recheck skipped: refresh already running"); return { rebuilt: false }; }
    throw e;
  }
}

export async function refresh(root, { probe = true, benchmarks = true, log = console.log } = {}) {
  return withLock(path.join(root, "state/model-registry.json"), async () => {
    const started = Date.now();
    const families = await readJSON(path.join(root, "model-families.json"));
    const liveModels = await readJSON(path.join(root, "state/live-models.json"));
    const status = await fetch("http://127.0.0.1:8318/api/status", { signal: AbortSignal.timeout(15_000) })
      .then((r) => (r.ok ? r.json() : null)).catch(() => null);
    const lineagePath = path.join(root, "state/model-lineage.json");
    const lineage = await readJSON(lineagePath, emptyLineage());
    const harnessVersions = { claude: harnessVersion(resolveExe("claude")), codex: harnessVersion(resolveExe("codex")) };
    let probes = await readProbes(root);
    let { registry } = buildRegistry({ liveModels, status, families, probes, lineage, harnessVersions });
    const policy = await readJSON(path.join(root, "state/routing-policy.json"), null);
    const previewPtr = await readJSON(path.join(root, "state/calibration/latest.json"), null);
    const preview = previewPtr ? await readJSON(path.join(root, "state/calibration", path.basename(previewPtr.file)), null).catch(() => null) : null;
    const plan = probePlan(registry, families, probes, harnessVersions, Date.now(), wantedEfforts(policy, preview));
    if (plan.length && probe) {
      if (batteryRunning()) { log("admission deferred: controller battery running"); registry.notices.push({ notice: "admission-deferred-battery" }); }
      else {
        await admit(root, fairBatch(plan, probes, MAX_PROBES_PER_RUN), { log, deadline: started + PROBE_BUDGET_MS });
        probes = await readProbes(root);
      }
    }
    const built = buildRegistry({ liveModels, status, families, probes, lineage, harnessVersions });
    registry = built.registry;
    const deadline = started + BENCH_DEADLINE_MS;
    const due = async (pointer) => { const l = await readJSON(path.join(root, "state/benchmarks", pointer), null); const age = Date.now() - Date.parse(l?.fetchedAt ?? ""); return !(age >= -5 * 60_000 && age <= BENCHMARK_EVERY_MS); };
    for (const [pointer, name, run] of [["latest.json", "benchmarks", () => fetchBenchmarks(root, registry.identities, { deadline })],
      ["latest-arena.json", "arena", () => fetchArena(root, registry.identities, { deadline })]]) {
      if (!benchmarks || !(await due(pointer))) continue;
      if (Date.now() - started > BENCH_START_MS) { registry.notices.push({ notice: `${name}-deferred-time-budget` }); continue; }
      try { log(`${name}: ${JSON.stringify(await run())}`); }
      catch (e) { registry.notices.push({ notice: `${name}-failed`, error: String(e.message ?? e) }); }
    }
    registry.benchmarks = await readJSON(path.join(root, "state/benchmarks/latest.json"), null);
    await writeJSONAtomic(lineagePath, built.lineage);
    await writeJSONAtomic(path.join(root, "state/model-registry.json"), registry);
    for (const [family, f] of Object.entries(registry.families)) {
      log(`${family.padEnd(7)} current=${f.current}${f.candidate ? ` candidate=${f.candidate}` : ""}${f.unavailableEfforts.length ? ` unavailable=${f.unavailableEfforts}` : ""}`);
    }
    return registry;
  });
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const root = path.dirname(fileURLToPath(import.meta.url));
  if (process.argv.includes("--recheck")) await recheck(root);
  else {
    console.log(`--- model-refresh ${new Date().toISOString()} pid=${process.pid}`);
    await refresh(root, { probe: !process.argv.includes("--no-probe") });
  }
}
