import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { buildLadders } from "./routing-ladder.mjs";

const now = Date.parse("2026-10-16T00:27:00Z");
const HOUR = 3600000;
const efforts = ["low", "medium", "high", "xhigh", "max"];
const ids = { sol: "synthetic-sol", astra: "synthetic-astra", opus: "synthetic-opus", fable: "synthetic-fable" };
const provider = (family) => ["opus", "fable"].includes(family) ? "claude" : "codex";
const near = (actual, expected, tolerance = 0.000001) => assert.ok(Math.abs(actual - expected) <= tolerance, `${actual} != ${expected} (+/- ${tolerance})`);
function fixture() {
  return {
    rules: { arenaMinVotes: 300, quotaPriceFloor: 0.1, rows: Object.fromEntries(Array.from({ length: 15 }, (_, i) => [String(i + 1), { metrics: ["artificial_analysis_intelligence_index"], failureMultiple: 10 }])) },
    registry: { authorityDigest: "synthetic-authority", families: Object.fromEntries(Object.entries(ids).map(([f, current]) => [f, { current, provider: provider(f) }])), models: {} },
    snapshot: { rows: Object.entries(ids).flatMap(([f, model]) => efforts.map((effort) => ({ model, effort, evaluations: { artificial_analysis_intelligence_index: 50 }, usdPerTask: f === "sol" ? 1 : 2 }))) },
    arena: { rows: [] }, manual: { rows: [] }, attempts: { attempts: [] },
    value: { config: { analysisDays: 14 }, fits: { codex: { status: "ok", pooled: { unitsPerUsd: 0.01 } }, claude: { status: "ok", pooled: { unitsPerUsd: 0.02 } } } },
    sessionUsage: { costBasis: { codex: "usd", claude: "usd" } },
    forecast: { generatedAt: new Date(now).toISOString(), staleAfter: new Date(now + HOUR).toISOString(), providers: {} },
  };
}
function attempts(f, row, family, effort, cost, hours, n = 5) {
  for (let i = 0; i < n; i++) f.attempts.attempts.push({ label: `synthetic-${row}-${family}-${effort}-${i}`, row: String(row), model: ids[family], effort, at: new Date(now - HOUR).toISOString(), apiEquivalentUsd: cost, quotaUnits: cost / 100, hours, rootSid: `synthetic-${row}-${family}-${effort}-${i}` });
}
function pace(demand, target) {
  return { state: "spend", pace: { computedAt: new Date(now).toISOString(), atDemand: { unitsPerHour: demand, lostUnits: 1, creditsLapsing: 0 }, fullUse: { unitsPerHour: target } } };
}

test("AC1: row 7 prices failure and time separately from quota", () => {
  const f = fixture();
  f.snapshot.rows = [];
  for (const [family, effort, p, cost, hours] of [["sol", "high", .525, 3.10, .75], ["astra", "high", .534, 14.03, .40], ["opus", "high", .569, 9.10, 1.26], ["opus", "xhigh", .602, 14.25, 1.20]]) {
    f.snapshot.rows.push({ model: ids[family], effort, evaluations: { artificial_analysis_intelligence_index: p * 100 } });
    attempts(f, 7, family, effort, cost, hours);
  }
  f.forecast.providers = { codex: pace(.644, 1), claude: pace(.940, 1) };
  const row = buildLadders(f, now).rows[7];
  near(row.typicalCost, 11.565); near(row.typicalHours, .975); near(row.hourValue, 11.862, .001);
  assert.deepEqual(row.rungs.map((r) => `${r.family}/${r.effort}`), ["opus/xhigh", "sol/high", "astra/high", "opus/high"]);
  row.rungs.forEach((r, i) => near(r.score, [122.35, 125.38, 126.73, 128.90][i], .05));
  f.rules.rows[7].failureMultiple = 0;
  const cheap = buildLadders(f, now).rows[7].rungs[0];
  assert.equal(cheap.family, "sol"); near(cheap.score, 20.75, .05);
});

test("AC2: quota price follows lost units, reachable steps, then and freshness", () => {
  const f = fixture();
  f.forecast.providers = { codex: pace(.02428, null), claude: pace(.03259, .03467) };
  const c = f.forecast.providers.codex.pace;
  c.fullUse = null;
  c.steps = [{ reachable: true, unitsPerHour: .03768 }, { reachable: true, unitsPerHour: .03 }, { reachable: false, unitsPerHour: .2 }];
  f.forecast.providers.claude.pace.then = { from: new Date(now + 60000).toISOString(), unitsPerHour: .01786 };
  let p = buildLadders(f, now).providers;
  near(p.codex.quotaPrice, .02428 / .03768); near(p.claude.quotaPrice, .03259 / .03467);
  assert.equal(buildLadders(f, now + 60000).providers.claude.quotaPrice, 1);
  c.steps = [{ reachable: false, unitsPerHour: .1 }];
  c.curve = [{ unitsPerHour: .07, lostUnits: 0 }, { unitsPerHour: .04, lostUnits: .05 }, { unitsPerHour: .03, lostUnits: .1 }];
  near(buildLadders(f, now).providers.codex.quotaPrice, .02428 / .04);
  c.atDemand.unitsPerHour = 0;
  assert.equal(buildLadders(f, now).providers.codex.quotaPrice, .1);
  c.atDemand.lostUnits = .05;
  assert.equal(buildLadders(f, now).providers.codex.quotaPrice, 1);
  c.atDemand.lostUnits = 1;
  c.computedAt = new Date(now - 16 * 60000).toISOString();
  assert.equal(buildLadders(f, now).providers.codex.quotaPrice, 1);
  c.computedAt = new Date(now).toISOString();
  f.forecast.staleAfter = new Date(now).toISOString();
  assert.equal(buildLadders(f, now).providers.codex.quotaPrice, 1);
  f.forecast.staleAfter = new Date(now + HOUR).toISOString();
  f.forecast.providers.codex.state = "unknown";
  assert.equal(buildLadders(f, now).providers.codex.quotaPrice, 1);
  f.forecast = null;
  assert.equal(buildLadders(f, now).providers.codex.quotaPrice, 1);
});

test("AC3: five attempts measure medians, four estimate with effort-median hours", () => {
  const f = fixture();
  attempts(f, 7, "sol", "high", 10, 2);
  attempts(f, 7, "astra", "high", 1000, 100, 4);
  attempts(f, 7, "opus", "medium", 30, 4);
  const outlier = f.attempts.attempts[0]; outlier.apiEquivalentUsd = 999; outlier.hours = 999;
  const r = buildLadders(f, now).rows[7];
  const sol = r.rungs.find((x) => x.family === "sol" && x.effort === "high");
  assert.equal(sol.cost, 10); assert.equal(sol.hours, 2); assert.equal(sol.n, 5); assert.equal(sol.costSource, "measured");
  const astra = r.rungs.find((x) => x.family === "astra" && x.effort === "high");
  assert.equal(astra.cost, 25); assert.equal(astra.hours, 2); assert.equal(astra.n, 4); assert.equal(astra.hoursSource, "row-effort");
  assert.equal(r.rowScale, 12.5); assert.equal(r.scaleSource, "row");
  const low = r.rungs.find((x) => x.family === "astra" && x.effort === "low");
  assert.equal(low.hours, 3); assert.equal(low.hoursSource, "row");
  const other = buildLadders(f, now).rows[8];
  assert.equal(other.scaleSource, "all-rows"); assert.equal(other.rungs[0].hoursSource, "all-rows");
  f.attempts.attempts = [];
  const empty = buildLadders(f, now).rows[7];
  assert.equal(empty.scaleSource, "none"); assert.ok(empty.rungs.every((x) => x.cost > 0 && x.costSource !== "measured" && x.hours === null && x.score === null));
});

test("AC3/N3: unlabelled duplicate review dispatches cannot create measured configs or change scales", () => {
  const f = fixture();
  attempts(f, 7, "sol", "high", 10, 2, 4);
  const baseline = buildLadders(f, now);
  const duplicate = { ...f.attempts.attempts[0], label: null, model: null, apiEquivalentUsd: 10000 };
  f.attempts.attempts.push(duplicate, { ...duplicate, model: ids.sol });
  assert.deepEqual(buildLadders(f, now).rows, baseline.rows);
  assert.equal(buildLadders(f, now).rows[7].rungs.find((r) => r.family === "sol" && r.effort === "high").n, 4);
  f.attempts.attempts.push({ ...f.attempts.attempts[0], at: new Date(now - 15 * 24 * HOUR).toISOString() });
  assert.deepEqual(buildLadders(f, now).rows, baseline.rows);
});

test("AC3: one USD provider makes all costs USD; unit estimates prefer own fit then pooled", () => {
  const f = fixture();
  attempts(f, 7, "sol", "high", 10, 2);
  f.sessionUsage.costBasis = { codex: "units", claude: "units" };
  f.value.fits.codex.perModel = { models: { [ids.sol]: { unitsPerUsd: .03 } } };
  let d = buildLadders(f, now);
  assert.equal(d.costBasis, "units");
  assert.equal(d.rows[7].rungs.find((r) => r.family === "sol" && r.effort === "high").cost, .1);
  near(d.rows[7].rungs.find((r) => r.family === "sol" && r.effort === "low").cost, .3);
  near(d.rows[7].rungs.find((r) => r.family === "astra" && r.effort === "low").cost, .2);
  f.sessionUsage.costBasis.claude = "usd";
  d = buildLadders(f, now);
  assert.equal(d.costBasis, "usd");
  assert.equal(d.rows[7].rungs.find((r) => r.family === "sol" && r.effort === "high").cost, 10);
  assert.equal(d.rows[7].rungs.find((r) => r.family === "astra" && r.effort === "low").cost, 20);
});

test("AC3: Arena and manual use exact effort and minimum votes; fixed rows retain null ordered rungs", () => {
  const f = fixture();
  f.rules.rows[14] = { source: "arena", board: "synthetic/board", incumbentEffortProxy: true, failureMultiple: 3 };
  f.rules.rows[15] = { source: "manual", board: "synthetic/design", failureMultiple: 3 };
  f.arena.rows = [{ model: ids.sol, effort: "high", board: "synthetic/board", rating: 1400, votes: 300 }, { model: ids.opus, effort: "max", board: "synthetic/board", rating: 1200, votes: 301 }, { model: ids.astra, effort: "high", board: "synthetic/board", rating: 1800, votes: 299 }];
  f.manual.rows = [{ model: ids.fable, effort: "low", board: "synthetic/design", rating: 1300, votes: 300 }];
  f.rules.rows[6] = { fixed: ["opus/high", "astra/high", "sol/high"], failureMultiple: 3 };
  f.rules.rows[9] = { fixed: ["sol/high", "opus/high"] };
  const d = buildLadders(f, now);
  assert.equal(d.rows[14].rungs.length, 2);
  near(d.rows[14].rungs.find((r) => r.family === "sol").p, .5);
  near(d.rows[14].rungs.find((r) => r.family === "opus").p, 1 / (1 + 10 ** .5));
  assert.equal(d.rows[15].rungs.length, 1); assert.equal(d.rows[15].rungs[0].p, .5);
  for (const row of [6, 9]) {
    assert.deepEqual(d.rows[row].rungs.map((r) => `${r.family}/${r.effort}`), f.rules.rows[row].fixed);
    for (const r of d.rows[row].rungs) for (const key of ["p", "cost", "costSource", "hours", "hoursSource", "n", "score"]) assert.equal(r[key], null);
  }
});

test("AC4: only current models enter, all efforts ignore slot filters and availability", () => {
  const f = fixture();
  f.rules.excludeFamilies = ["fable"]; f.rules.noGeneralExpansion = ["fable"];
  f.rules.laneBinding = { codex: ["sol"] }; f.rules.rows[11].excludeFamilies = ["fable"];
  f.registry.families.newcomer = { provider: "codex", current: null, candidate: "synthetic-new" };
  f.registry.models["synthetic-new"] = { admittedEfforts: [], efforts: [], usableAccountsByEffort: {} };
  for (const effort of efforts) f.snapshot.rows.push({ model: "synthetic-new", effort, evaluations: { artificial_analysis_intelligence_index: 60 }, usdPerTask: 1 });
  assert.ok(Object.values(buildLadders(f, now).rows).every((r) => r.rungs.every((x) => x.family !== "newcomer")));
  f.registry.families.newcomer.current = "synthetic-new";
  const d = buildLadders(f, now);
  for (const r of Object.values(d.rows)) assert.deepEqual(r.rungs.filter((x) => x.family === "newcomer").map((x) => x.effort), efforts);
  assert.equal(d.rows[11].rungs.filter((r) => r.family === "fable").length, 5);
  f.snapshot.rows.find((r) => r.model === "synthetic-new").evaluations = {};
  assert.equal(buildLadders(f, now).rows[7].rungs.filter((r) => r.family === "newcomer").length, 4);
});

test("metric mean requires every exact metric; missing benchmark and cost never invent a rung", () => {
  const f = fixture();
  f.rules.rows[7].metrics = ["artificial_analysis_intelligence_index", "hle", "scicode"];
  const aa = f.snapshot.rows.find((r) => r.model === ids.sol && r.effort === "high");
  aa.evaluations = { artificial_analysis_intelligence_index: 60, hle: .4, scicode: .8 };
  let r = buildLadders(f, now).rows[7].rungs;
  assert.equal(r.length, 1); near(r[0].p, .6);
  delete aa.evaluations.hle;
  assert.equal(buildLadders(f, now).rows[7].rungs.length, 0);
  aa.evaluations.hle = .4; aa.usdPerTask = null;
  assert.equal(buildLadders(f, now).rows[7].rungs.length, 0);
  attempts(f, 7, "sol", "high", 10, 2);
  r = buildLadders(f, now).rows[7].rungs;
  assert.equal(r.length, 1); assert.equal(r[0].costSource, "measured");
});

test("score ties use cost, family and effort order; missing fits never invent units", () => {
  const f = fixture();
  for (const r of f.snapshot.rows) r.usdPerTask = 1;
  const r = buildLadders(f, now).rows[7].rungs;
  assert.deepEqual(r.slice(0, 5).map((x) => `${x.family}/${x.effort}`), efforts.map((e) => `astra/${e}`));
  f.sessionUsage.costBasis = { codex: "units", claude: "units" };
  f.value.fits.codex = { status: "insufficient-data" };
  assert.ok(buildLadders(f, now).rows[7].rungs.every((x) => x.provider !== "codex"));
  f.sessionUsage = null;
  assert.equal(buildLadders(f, now).costBasis, "usd");
});

test("zero timing and capability stay explicit; invalid rows cannot supply measured fallback", () => {
  const f = fixture();
  attempts(f, 7, "sol", "high", 10, 0);
  const d = buildLadders(f, now), sol = d.rows[7].rungs.find((r) => r.family === "sol" && r.effort === "high");
  assert.equal(sol.n, 5); assert.equal(sol.cost, 10); assert.equal(sol.hours, 0); assert.equal(sol.score, null);
  f.attempts.attempts.forEach((a) => { a.row = null; });
  assert.ok(buildLadders(f, now).rows[7].rungs.every((r) => r.n === 0 && r.hours === null));
  f.snapshot.rows.find((r) => r.model === ids.sol && r.effort === "high").evaluations.artificial_analysis_intelligence_index = 0;
  const zero = buildLadders(f, now).rows[7].rungs.find((r) => r.family === "sol" && r.effort === "high");
  assert.equal(zero.p, 0); assert.equal(zero.score, null);
});

test("installed rules pin fixed orders, quota floor and every failure tier", async () => {
  const rules = JSON.parse(await readFile(new URL("./calibration-rules.json", import.meta.url), "utf8"));
  assert.equal(rules.quotaPriceFloor, .1);
  for (const [multiple, rows] of [[1, [1, 13]], [3, [2, 3, 5, 6, 10, 12, 14, 15]], [10, [4, 7, 8, 11]]])
    for (const row of rows) assert.equal(rules.rows[row].failureMultiple, multiple);
  assert.equal(rules.rows[9].failureMultiple, undefined);
  assert.deepEqual(rules.rows[6].fixed, ["opus/high", "astra/high", "sol/high"]);
  assert.deepEqual(rules.rows[9].fixed, ["sol/high", "opus/high"]);
});

test("document digest is full SHA256, excludes document times, and never includes credentials", () => {
  const f = fixture();
  f.registry.models[ids.sol] = { token: "synthetic-secret-never-publish", email: "synthetic-private@example.invalid" };
  const before = structuredClone(f);
  const d = buildLadders(f, now), later = buildLadders(f, now + 1000);
  assert.equal(d.format, "routing-ladders/v1"); assert.equal(Object.keys(d.rows).length, 15);
  assert.equal(d.staleAfter, new Date(now + .5 * HOUR).toISOString());
  assert.match(d.digest, /^[a-f0-9]{64}$/); assert.equal(d.digest, later.digest);
  const { generatedAt, staleAfter, digest, ...payload } = d;
  assert.equal(digest, createHash("sha256").update(JSON.stringify(payload)).digest("hex"));
  assert.equal(d.inputs.registryAuthorityDigest, "synthetic-authority");
  assert.equal(JSON.stringify(d).includes("synthetic-secret"), false); assert.equal(JSON.stringify(d).includes("example.invalid"), false);
  assert.deepEqual(f, before);
});

test("published integration exposes GET ladders to local/bridge and tailnet; npm test includes ladder controls", async () => {
  const service = await readFile(new URL("./pool-service.mjs", import.meta.url), "utf8");
  const proxy = await readFile(new URL("./tailnet-dashboard-proxy.mjs", import.meta.url), "utf8");
  assert.match(service, /const readOnlyStatus = req.method === "GET" && \[[^\]]*"\/api\/ladders"/);
  assert.match(service, /req.method === "GET" && url.pathname === "\/api\/ladders"/);
  assert.match(service, /capacity\.ladders\(\)/);
  assert.match(proxy, /const readable = new Set\(\[[^\]]*"\/api\/ladders"/);
  const pkg = JSON.parse(await readFile(new URL("./package.json", import.meta.url), "utf8"));
  assert.ok(pkg.scripts.test.includes("routing-ladder.test.mjs"));
});
