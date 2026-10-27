// Runs the pinned Agent Pools JavaScript (reference/agent-pools, commit in
// golden/README.md) over golden/inputs and writes what it produces to
// golden/outputs. A Rust port must reproduce every output from the same input.
//   node golden/capture.mjs            write golden/outputs
//   node golden/capture.mjs --check    recompute and compare, write nothing (exit 1 on a difference)
// The reference never reads the clock here: Date.now throws while it runs, so
// every result is a function of the inputs and the case's `now` alone.
import fs from "node:fs";
import path from "node:path";
import zlib from "node:zlib";
import { createHash } from "node:crypto";
import { fileURLToPath } from "node:url";
import { buildForecast, forecastConfig, HOUR } from "../reference/agent-pools/capacity-forecast.mjs";
import { buildUsageReports, valueConfig, quotaIncrements, costOf } from "../reference/agent-pools/usage-value.mjs";
import { buildSummary } from "../reference/agent-pools/pool-summary.mjs";
import { chooseReset, rankAccounts, normalizeUsage } from "../reference/agent-pools/routing.mjs";
import { quotaWindows } from "../reference/agent-pools/quota-windows.mjs";
import { resetPolicies } from "../reference/agent-pools/resets.mjs";
import { currentAccount } from "../reference/agent-pools/freshness.mjs";
import { PROVIDERS, PROVIDER_IDS } from "../reference/agent-pools/providers.mjs";
import { availableCredits } from "../reference/agent-pools/provider-codex.mjs";
import { claudeBank } from "../reference/agent-pools/provider-claude.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const inputs = path.join(here, "inputs");
const outputs = path.join(here, "outputs");
const check = process.argv.includes("--check");
const DAY = 24 * HOUR;

// --- inputs ------------------------------------------------------------------
const json = (rel) => JSON.parse(fs.readFileSync(path.join(inputs, rel), "utf8"));
const jsonLines = (rel) => zlib.gunzipSync(fs.readFileSync(path.join(inputs, rel))).toString("utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l));
const manifest = json("manifest.json");
const now = Date.parse(manifest.now);
const pool = json("pool-state.json");
const install = json("install.json");
const previousForecast = fs.existsSync(path.join(inputs, "capacity-forecast.json")) ? json("capacity-forecast.json") : null;
const prices = json("model-prices.json");
const registry = fs.existsSync(path.join(inputs, "model-registry.json")) ? json("model-registry.json") : null;
const samples = jsonLines("usage-samples.jsonl.gz").sort((a, b) => Date.parse(a.at) - Date.parse(b.at));
const ledger = jsonLines("usage-ledger.jsonl.gz");
const provider = (name) => json(path.join("provider", name));

// --- clock trap and serialization --------------------------------------------
const realNow = Date.now;
const trap = () => { throw new Error("the reference read the clock; pass now explicitly"); };
async function run(fn) {
  Date.now = trap;
  try { return await fn(); } finally { Date.now = realNow; }
}
function runSync(fn) {
  Date.now = trap;
  try { return fn(); } finally { Date.now = realNow; }
}
// Documents are published with JSON.stringify, so they are compared that way
// (Infinity and NaN become null, as on the wire). Case files keep them, tagged.
const roundTrip = (v) => JSON.parse(JSON.stringify(v));
const tagged = (_, v) =>
  typeof v === "number" && !Number.isFinite(v) ? { $number: String(v) }
  : v instanceof Set ? { $set: [...v] }
  : v instanceof Map ? { $map: [...v] }
  : v;

const produced = new Map();
const emit = (rel, value, replacer = null) => produced.set(rel, JSON.stringify(value, replacer, 1) + "\n");

// --- 1. capacity forecast ------------------------------------------------------
const { config: forecastCfg, problems: forecastProblems } = forecastConfig(install.capacityForecast);
const policies = resetPolicies(install);
const resetAutomation = pool.resetAutomation !== false;
const samplesFor = (t) => samples.filter((s) => { const at = Date.parse(s.at); return at >= t - (forecastCfg.lookbackHours + 48) * HOUR && at <= t; });

const forecastNow = await run(() => buildForecast({
  accounts: pool.accounts, samples: samplesFor(now), config: forecastCfg, now, previous: previousForecast, resetPolicies: policies, resetAutomation,
}));
emit("forecast/now.json", forecastNow);

// Hourly replay from no memory, as experiments/capacity-replay.mjs does: window
// shapes and credits from the snapshot, used/resetAt per hour from the newest
// sample at or before it. Exercises the state machine, dwell and balance ramp.
const REPLAY_HOURS = 72;
const replay = [];
{
  const end = Math.floor(now / HOUR) * HOUR;
  let previous = null;
  for (let t = end - (REPLAY_HOURS - 1) * HOUR; t <= end; t += HOUR) {
    const latest = new Map();
    for (const s of samples) { if (Date.parse(s.at) > t) break; latest.set(s.accountId + "|" + s.window, s); }
    const accounts = pool.accounts.map((a) => ({
      ...a, usageStale: false, status: "available",
      quotaWindows: (a.quotaWindows ?? []).map((w) => { const s = latest.get(a.id + "|" + w.id); return s ? { ...w, used: s.used, resetAt: s.resetAt } : null; }).filter(Boolean),
    }));
    const doc = await run(() => buildForecast({ accounts, samples: samplesFor(t), config: forecastCfg, now: t, previous, resetPolicies: {}, resetAutomation }));
    previous = roundTrip(doc);
    replay.push({ now: new Date(t).toISOString(), forecast: doc });
  }
}
emit("forecast/replay.json", { hours: REPLAY_HOURS, input: "accounts from pool-state.json with windows from the newest sample at or before each hour; previous = the hour before (none for the first)", steps: replay });

// Variants over the same snapshot that reach what it does not: banked Claude refills (the
// counterfactual planner, drain-first, redeem-now, keep), unlisted and unmodeled credits,
// policies and automation off, stale holds, spend, other settings and rejected ones. Credits
// are synthetic; inputs are stored in full so a port replays them without this script.
const isoAt = (t) => new Date(t).toISOString();
const claudeIds = pool.accounts.filter((a) => a.provider === "claude").map((a) => a.id);
const editClaude = (accts, fn) => accts.map((a) => (a.provider === "claude" ? fn(a, claudeIds.indexOf(a.id)) : a));
const banked = (list) => ({ creditsKnown: true, creditCount: list.length, credits: list.map(([id, t]) => ({ id, expires_at: t === null ? null : isoAt(t) })) });
const weekly = (a, used, resetAt) => ({ ...a, quotaWindows: (a.quotaWindows ?? []).map((w) => (w.id === "limit:1" ? { ...w, used, resetAt: resetAt === undefined ? w.resetAt : resetAt } : w)) });
const staggered = (t) => (a, i) => ({ ...a, ...banked([[`syn-c${i + 1}-a`, t + (i + 1) * 30 * HOUR], [`syn-c${i + 1}-b`, t + (i + 4) * DAY]]) });
const keepAccounts = editClaude(pool.accounts, (a, i) => ({ ...weekly(a, [5, 20, 50][i], now + 6 * DAY), ...banked([[`syn-k${i + 1}`, now + [12 * HOUR, 60 * HOUR, 5 * DAY][i]]]) }));
const variantSpecs = [
  { name: "claude-banked", at: now, previous: "snapshot", accounts: editClaude(pool.accounts, staggered(now)) },
  { name: "claude-banked-reuse", at: now + 5 * 60000, previous: "claude-banked", accounts: editClaude(pool.accounts, staggered(now)) },
  { name: "claude-banked-later", at: now + 7 * HOUR, previous: "claude-banked", accounts: editClaude(pool.accounts, staggered(now)) },
  { name: "claude-all-out", at: now, previous: null, accounts: editClaude(pool.accounts, (a, i) => staggered(now)(weekly(a, 100), i)) },
  { name: "claude-one-left", at: now, previous: null, accounts: editClaude(pool.accounts, (a, i) => staggered(now)(weekly(a, i === 1 ? 35 : 100, now + (3 + i) * DAY), i)) },
  { name: "claude-urgent-unlisted", at: now, previous: null, accounts: editClaude(pool.accounts, (a, i) =>
    i === 0 ? { ...a, creditsKnown: true, creditCount: 3, credits: [{ id: "syn-u", expires_at: isoAt(now + 12 * HOUR) }] }
    : i === 1 ? { ...a, creditsKnown: true, creditCount: 2, credits: [] }
    : { ...a, creditsKnown: true, creditCount: 2, credits: [{ id: "syn-camel", expiresAt: isoAt(now + 9 * DAY) }, { id: "syn-none", expires_at: null }] }) },
  { name: "claude-low-runway", at: now, previous: "snapshot", install: { ...install, resetPolicy: { claude: { maxRunwayPercent: 10 } } }, accounts: editClaude(pool.accounts, staggered(now)) },
  { name: "policies-off", at: now, previous: "snapshot", install: { ...install, resetPolicy: { claude: { enabled: false, trigger: "sometimes" }, codex: { enabled: false, headroomPercent: 50 } } }, accounts: editClaude(pool.accounts, staggered(now)) },
  { name: "automation-off", at: now, previous: "snapshot", resetAutomation: false, accounts: editClaude(pool.accounts, staggered(now)) },
  { name: "claude-fresh-spend", at: now, previous: null, accounts: editClaude(pool.accounts, (a) => weekly(a, 10, now + 30 * HOUR)) },
  { name: "claude-fresh-from-conserve", at: now + 3 * HOUR, previous: "snapshot", accounts: editClaude(pool.accounts, (a) => weekly(a, 10, now + 30 * HOUR)) },
  { name: "stale-hold", at: now + 20 * 60000, previous: "snapshot", accounts: pool.accounts.map((a) => ({ ...a, usageStale: true })) },
  { name: "stale-expired", at: now + 45 * 60000, previous: "stale-hold", accounts: pool.accounts.map((a) => ({ ...a, usageStale: true })) },
  { name: "stale-recovered", at: now + 50 * 60000, previous: "stale-expired", accounts: pool.accounts },
  { name: "no-history", at: now, previous: null, samples: "none", accounts: pool.accounts },
  { name: "settings", at: now, previous: "snapshot", capacityForecast: { horizonDays: 7, maxSteps: 2, margin: 1, spendEnterUnits: 0.1, planWindowDays: 3, mode: "shadow", weights: { "codex:pro": 2, [claudeIds[1]]: 0.5 } }, accounts: editClaude(pool.accounts, staggered(now)) },
  { name: "settings-rejected", at: now, previous: null, capacityForecast: { margin: -1, maxSteps: 2.5, weights: { a: 0 }, mode: "loud", lookbackHours: "168", halfLifeHours: null, staleHoldMinutes: 1e9 }, accounts: pool.accounts },
  { name: "settings-not-object", at: now, previous: null, capacityForecast: "active", accounts: pool.accounts },
  // keep: a drain-first boost already in place stays while it is not worse at any pace. The
  // same accounts two minutes on, with the boost in the previous plan and without it.
  { name: "claude-keep-base", at: now, previous: "snapshot", accounts: keepAccounts },
  { name: "claude-keep", at: now + 2 * 60000, previous: "claude-keep-base", previousDrainFirst: [{ accountId: claudeIds[0], until: isoAt(now + 12 * HOUR) }], accounts: keepAccounts },
  { name: "claude-keep-none", at: now + 2 * 60000, previous: "claude-keep-base", accounts: keepAccounts },
  { name: "disabled-and-unknown", at: now, previous: "snapshot", accounts: pool.accounts.map((a, i) => (i === 0 ? { ...a, disabled: true } : i === 4 ? { ...a, status: "unknown" } : a)) },
];
const variantDocs = new Map();
const variants = [];
for (const v of variantSpecs) {
  const inst = v.install ?? install;
  const capacityInput = "capacityForecast" in v ? v.capacityForecast : inst.capacityForecast;
  const { config, problems } = forecastConfig(capacityInput);
  const pols = resetPolicies(inst);
  const prev = v.previous === "snapshot" ? previousForecast : v.previous ? roundTrip(variantDocs.get(v.previous)) : null;
  // previousDrainFirst replaces the Claude plan's drainFirst in the previous document.
  if (v.previousDrainFirst) prev.providers.claude.creditPlan.drainFirst = v.previousDrainFirst;
  const doc = await run(() => buildForecast({
    accounts: v.accounts, samples: v.samples === "none" ? [] : samplesFor(v.at).filter((s) => Date.parse(s.at) >= v.at - (config.lookbackHours + 48) * HOUR),
    config, now: v.at, previous: prev, resetPolicies: pols, resetAutomation: v.resetAutomation ?? resetAutomation,
  }));
  variantDocs.set(v.name, doc);
  variants.push({ name: v.name, input: { now: v.at, previous: v.previous, previousDrainFirst: v.previousDrainFirst ?? null, accounts: v.accounts, install: inst, capacityForecast: capacityInput ?? null, resetAutomation: v.resetAutomation ?? resetAutomation, samples: v.samples ?? "samplesFor(now)" },
    result: { problems, resetPolicies: roundTrip(pols), forecast: doc } });
}
emit("forecast/variants.json", { function: "capacity-forecast.mjs buildForecast over edited snapshot accounts; previous names an earlier case, \"snapshot\" (inputs/capacity-forecast.json) or none; previousDrainFirst, when set, replaces previous.providers.claude.creditPlan.drainFirst", cases: variants });
// --- 2. usage value report -------------------------------------------------------
async function valueCase(usageLedger) {
  const { config, problems } = valueConfig(usageLedger);
  async function* rows() { yield* ledger; }
  const { valueReport, sessionUsage } = await run(() => buildUsageReports({
    rows: rows(), samples, prices, now, forecast: roundTrip(forecastNow), registry,
    accounts: pool.accounts.map((a) => ({ id: a.id, provider: a.provider, email: a.email, auth_index: a.auth_index, plan: a.plan ?? null, quotaWindows: a.quotaWindows ?? [] })),
    config, weights: install.capacityForecast?.weights ?? {},
  }));
  valueReport.problems = problems;
  return { valueReport, sessionUsage, config };
}
const deployed = await valueCase(install.usageLedger);
emit("value/deployed.json", deployed.valueReport);
emit("value/deployed-session-usage.json", deployed.sessionUsage);
const short = await valueCase({ ...(install.usageLedger ?? {}), analysisDays: 3, valueDays: 2, wasteDays: 5, bucketHours: 6 });
emit("value/short-windows.json", short.valueReport);

// Direct cases for the report's helpers: quota increments around the refill and jitter
// edges, pricing names and token types, and usageLedger settings.
{
  const stream = (rows) => rows.map(([h, used, resetH]) => ({ at: now + h * HOUR, used, resetAt: resetH === null ? NaN : now + resetH * HOUR }));
  const incrementCases = [
    ["climb with out-of-order reads", [[0, 10, 100], [1, 12, 100], [1.1, 11, 100], [2, 12, 100], [3, 15, 100]]],
    ["drop of exactly 5 is a refill", [[0, 50, 100], [1, 45, 100], [2, 47, 100]]],
    ["drop of 4.9 is noise", [[0, 50, 100], [1, 45.1, 100], [2, 52, 100]]],
    ["reset moved within jitter", [[0, 20, 100], [1, 22, 100.9], [2, 25, 101.5]]],
    ["reset moved past jitter, early", [[0, 60, 100], [1, 5, 268], [2, 9, 268]]],
    ["natural end", [[0, 70, 2], [3, 0, 170], [4, 4, 170]]],
    ["unknown reset", [[0, 10, null], [1, 30, null], [2, 2, null]]],
    ["bad readings skipped", [[0, 10, 100], [1, NaN, 100], [2, 15, 100]]],
    ["empty", []],
  ].map(([name, rows]) => { const s = stream(rows); return { name, input: s, result: runSync(() => quotaIncrements(s)) }; });
  emit("value/quota-increments.json", { function: "usage-value.mjs quotaIncrements(stream)", cases: incrementCases }, tagged);
  const rowsFor = [
    { model: "gpt-6.1-sol", in: 1000, cr: 50000, cw: 0, out: 300 },
    { model: "openai/GPT-6.1-SOL-high", in: 10, cr: 0, cw: 0, out: 5, tier: "priority" },
    { model: "gpt-6-astra", in: 300000, cr: 0, cw: 1000, out: 10, tier: "priority" },
    { model: "claude-opus-5-5-20261001", in: 7, cr: 9, cw: 11, out: 13 },
    { model: "claude-fable-5-2026-10-01", in: 1, out: 1 },
    { model: "gpt-6.1-sol-mini", in: 1, out: 1 },
    { model: null, in: 1 },
    { model: "x/y/gpt-6.1-sol", in: 1 },
  ];
  const custom = { models: { "m-plain": { input: 3, output: 6 }, "m-long": { input: 1, output: 2, longContext: { threshold: 10, input: 5 } }, "m-strings": { input: "2", output: null, cacheRead: "0.5" } } };
  const costCases = [];
  for (const row of rowsFor) costCases.push({ name: `deployed prices ${row.model}`, input: { row, prices: "deployed" }, result: runSync(() => costOf(row, prices)) });
  for (const row of [{ model: "m-plain", in: 4, cr: 4, cw: 4, out: 4 }, { model: "m-long", in: 6, cr: 3, cw: 2, out: 1 }, { model: "m-long", in: 5, cr: 3, cw: 2, out: 1 }, { model: "m-strings", in: 1, cr: 1, cw: 1, out: 1 }])
    costCases.push({ name: `custom prices ${row.model} ${row.in}`, input: { row, prices: custom }, result: runSync(() => costOf(row, custom)) });
  emit("value/cost.json", { function: "usage-value.mjs costOf(row, prices) (prices \"deployed\" = inputs/model-prices.json)", cases: costCases }, tagged);
  const configCases = [undefined, null, {}, { analysisDays: 3, valueDays: 5 }, { analysisDays: 2.5, wasteDays: 91, bucketHours: "6", reportIntervalMinutes: 4 },
    { planPrices: { "codex:pro": 200, claude: 100, bad: 0, worse: "1" } }, { planPrices: [1] },
    { modelLimits: { claude: { Fable: { kind: "cap" }, Other: { shareOfCommon: 0.3 }, Both: { kind: "reserve", shareOfCommon: 2 }, Bad: { kind: "huge" }, Empty: {}, Zero: { shareOfCommon: 0 } }, codex: 5 } },
    { modelLimits: "no" }]
    .map((input, i) => { const { config, problems } = valueConfig(input); return { name: `valueConfig ${i}`, input: input === undefined ? "undefined" : input, result: { config: roundTrip(config), problems } }; });
  emit("value/config.json", { function: "usage-value.mjs valueConfig(input) (input \"undefined\" = no argument)", cases: configCases }, tagged);
}
// --- 3. /api/summary ---------------------------------------------------------------
const summary = await run(() => buildSummary({
  state: { ...pool, accounts: pool.accounts.map((a) => currentAccount(a, now)) },
  providers: Object.fromEntries(PROVIDER_IDS.map((id) => [id, { name: PROVIDERS[id].name }])),
  forecast: roundTrip(forecastNow), value: roundTrip(deployed.valueReport), now,
}));
emit("summary/now.json", summary);

// The same at other times and without the forecast or value report: freshness turns accounts
// unknown after five minutes, and a missing document leaves its blocks null.
{
  const summaryCases = [];
  for (const [name, at, withForecast, withValue] of [["four minutes on", now + 4 * 60000, true, true], ["ten minutes on", now + 10 * 60000, true, true],
    ["no forecast", now, false, true], ["no value report", now, true, false], ["neither, an hour on", now + HOUR, false, false]]) {
    const result = runSync(() => buildSummary({
      state: { ...pool, accounts: pool.accounts.map((a) => currentAccount(a, at)) },
      providers: Object.fromEntries(PROVIDER_IDS.map((id) => [id, { name: PROVIDERS[id].name }])),
      forecast: withForecast ? roundTrip(forecastNow) : null, value: withValue ? roundTrip(deployed.valueReport) : null, now: at,
    }));
    summaryCases.push({ name, input: { now: at, forecast: withForecast, value: withValue }, result });
  }
  emit("summary/cases.json", { function: "pool-summary.mjs buildSummary over freshness.mjs currentAccount at other times; forecast/value true = forecast/now.json and value/deployed.json", cases: summaryCases });
}
// --- 4. reset choice (routing.mjs chooseReset) and routing order ----------------------
const routingFields = (a) => ({
  id: a.id, provider: a.provider, disabled: !!a.disabled, status: a.status, creditsKnown: a.creditsKnown === true,
  creditCount: a.creditCount ?? 0, remaining: a.remaining, rankReset: a.rankReset ?? null, rankRemaining: a.rankRemaining ?? null,
  windows: (a.windows ?? []).map((w) => ({ id: w.id, common: !!w.common, durationSeconds: w.durationSeconds ?? null, used: w.used ?? null, resetAt: w.resetAt ?? null })),
  credits: (a.credits ?? []).map((c) => ({ id: c.id, expires_at: c.expires_at ?? c.expiresAt ?? null })),
});
const iso = (t) => new Date(t).toISOString();
const transforms = {
  "as-is": (accts) => accts,
  "all-out": (accts) => accts.map((a) => ({ ...a, status: "exhausted", remaining: 0, rankRemaining: 0, windows: a.windows.map((w) => (w.common && w.durationSeconds === 604800 ? { ...w, used: 100 } : w)) })),
  "all-out-one-bank-unknown": (accts) => transforms["all-out"](accts).map((a, i) => (i === 0 ? { ...a, creditsKnown: false } : a)),
  "all-but-one-out": (accts) => transforms["all-out"](accts).map((a, i) => (i === accts.length - 1 ? accts[i] : a)),
};
const creditSets = {
  "as-is": (accts) => accts,
  "staggered": (accts, t) => accts.map((a, i) => ({ ...a, creditsKnown: true, creditCount: 2, credits: [
    { id: `syn-${i + 1}-a`, expires_at: iso(t + (i + 1) * 20 * HOUR) }, { id: `syn-${i + 1}-b`, expires_at: iso(t + (i + 5) * DAY) }] })),
  "urgent-tie": (accts, t) => accts.map((a, i) => ({ ...a, creditsKnown: true, creditCount: 1, credits: [{ id: `syn-${i + 1}-u`, expires_at: iso(t + 12 * HOUR) }] })),
};
const resetCases = [];
for (const id of PROVIDER_IDS) {
  const base = pool.accounts.filter((a) => a.provider === id).map(routingFields);
  const policy = { ...policies[id] };
  delete policy.problems;
  const demands = PROVIDERS[id].redemption === "refill" ? [null, { unitsPerHour: 0.02, weights: {} }, { unitsPerHour: 0.5, weights: {} }] : [null];
  for (const offset of [0, DAY, 3 * DAY]) {
    const t = now + offset;
    for (const [tName, transform] of Object.entries(transforms)) for (const [cName, credits] of Object.entries(creditSets)) {
      const accounts = transform(credits(base, t));
      const firstCredit = accounts.find((a) => a.credits.length);
      for (const skip of [[], firstCredit ? [`${firstCredit.id}|${firstCredit.credits[0].id}`] : null].filter(Boolean))
      for (const beforeLapse of [null, [accounts[0]?.id], accounts.map((a) => a.id)])
      for (const refillDemand of demands)
      for (const trigger of ["any", "weekly"]) {
        const p = { ...policy, trigger, ...(refillDemand ? { refillDemand } : {}) };
        const input = structuredClone({ accounts, policy: p, now: t, skip, beforeLapse });
        const result = runSync(() => chooseReset(accounts, p, t, new Set(skip), beforeLapse ? new Set(beforeLapse) : null));
        resetCases.push({ name: `${id} +${offset / DAY}d ${tName} credits:${cName} skip:${skip.length} beforeLapse:${beforeLapse?.length ?? 0} demand:${refillDemand?.unitsPerHour ?? "none"} ${trigger}`, input, result });
      }
    }
  }
}
emit("routing/choose-reset.json", { function: "routing.mjs chooseReset(accounts, policy, now, new Set(skip), beforeLapse && new Set(beforeLapse))", cases: resetCases }, tagged);

const rankCases = [];
for (const [tName, transform] of Object.entries(transforms)) for (const offset of [0, 3 * DAY]) {
  const accounts = transform(creditSets.staggered(pool.accounts.map(routingFields), now + offset));
  for (const drainFirst of [null, new Map([[accounts.at(-1).id, now + offset + 6 * HOUR]])]) {
    const ranked = runSync(() => rankAccounts(accounts, drainFirst)).map((a) => a.id);
    rankCases.push({ name: `${tName} +${offset / DAY}d drainFirst:${drainFirst ? 1 : 0}`, input: { accounts, drainFirst: drainFirst ? [...drainFirst] : null }, result: ranked });
  }
}
emit("routing/rank-accounts.json", { function: "routing.mjs rankAccounts(accounts, drainFirst && new Map(drainFirst))", cases: rankCases }, tagged);

// --- 5. provider payloads: quota windows, normalized usage, banks ----------------------
const claudeFixture = provider("claude-usage.json");
const payloads = [
  ["codex", "fixture available", provider("codex-usage-available.json")],
  ["codex", "fixture exhausted", provider("codex-usage-exhausted.json")],
  ["claude", "fixture", claudeFixture],
  ["codex", "additional and named buckets", { rate_limit: { allowed: true, limit_reached: false, primary_window: { used_percent: 40, limit_window_seconds: 18000, reset_at: 1793100000 }, secondary_window: { used_percent: 12.5, limit_window_seconds: 604800, reset_at: 1793500000 } },
    additional_rate_limits: [{ limit_name: "GPT-6 Astra", rate_limit: { primary_window: { used_percent: 100, limit_window_seconds: 604800, reset_at: 1793400000 } } }, { rate_limit: { primary_window: { utilization: 7, limit_window_seconds: 3600 } } }],
    code_review_rate_limit: { primary_window: { used_percent: 3, limit_window_seconds: 86400, reset_at: 1793200000 } } }],
  ["codex", "malformed flags", { rate_limit: { allowed: "yes", primary_window: { used_percent: 10, limit_window_seconds: 604800, reset_at: 1793500000 } } }],
  ["codex", "used out of range", { rate_limit: { primary_window: { used_percent: 140, limit_window_seconds: 604800, reset_at: 1793500000 } } }],
  ["codex", "no flags, at limit", { rate_limit: { primary_window: { used_percent: 100, limit_window_seconds: 604800, reset_at: 1793500000 } } }],
  ["codex", "empty", {}],
  ["claude", "structured scopes", { limits: [{ kind: "session", percent: 10, resets_at: "2026-10-27T05:00:00+00:00" }, { kind: "weekly_all", percent: 40, resets_at: "2026-10-30T13:00:00+00:00" },
    { kind: "weekly_scoped", group: "weekly", percent: 100, resets_at: "2026-10-30T13:00:00+00:00", scope: { model: { display_name: "Fable" } } },
    { kind: "daily_scoped", percent: 20, limit_window_seconds: 86400, scope: { model: { display_name: "Fable" }, surface: { name: "Cowork" } } }, { kind: "weekly_all" }],
    five_hour: { utilization: 99, resets_at: null }, seven_day: { utilization: 1 }, seven_day_opus: { utilization: 50, resets_at: "2026-10-30T13:00:00+00:00" } }],
  ["claude", "legacy only", { five_hour: { utilization: 35.5, resets_at: "2026-10-27T05:00:00+00:00" }, seven_day: { utilization: 100, resets_at: "2026-10-30T13:00:00+00:00" }, seven_day_sonnet: { utilization: 2, resets_at: null }, nimbus_quill: { utilization: 0 } }],
  ["claude", "empty", {}],
];
const nows = [now, Date.parse("2026-09-24T12:00:00Z"), Date.parse("2026-10-30T14:00:00Z")];
const usageCases = [];
for (const [p, name, data] of payloads) for (const t of nows)
  usageCases.push({ name: `${p} ${name} @${iso(t)}`, input: { provider: p, data, now: t }, result: runSync(() => ({ quotaWindows: quotaWindows(p, data), normalizeUsage: normalizeUsage(p, data, t) })) });
emit("quota/usage.json", { function: "quota-windows.mjs quotaWindows(provider, data) and routing.mjs normalizeUsage(provider, data, now)", cases: usageCases }, tagged);

const codexCredits = provider("codex-credits.json");
const creditCases = [];
const creditPayloads = [["fixture", codexCredits], ["mixed", { credits: [
  { id: "c-late", reset_type: "codex_rate_limits", status: "available", expires_at: "2026-11-20T00:00:00Z" },
  { id: "c-soon", resetType: "codexRateLimits", status: "available", expiresAt: "2026-10-28T00:00:00Z" },
  { id: "c-used", reset_type: "codex_rate_limits", status: "redeemed", expires_at: "2026-11-01T00:00:00Z" },
  { id: "c-other", reset_type: "something_else", status: "available", expires_at: "2026-11-01T00:00:00Z" },
  { id: "c-noexp", reset_type: "codex_rate_limits", status: "available" }] }], ["none", {}]];
for (const [name, data] of creditPayloads) for (const t of nows) creditCases.push({ name: `availableCredits ${name} @${iso(t)}`, input: { data, now: t }, result: runSync(() => availableCredits(data, t)) });
const bank = claudeFixture.cedar_ember;
const grant = bank.grants[0];
const bankVariants = [
  ["fixture", bank], ["missing", undefined], ["not eligible", { ...bank, eligible: false, ineligible_reason: "surface" }],
  ["cooling", { ...bank, cooldown_until: "2026-10-01T00:00:00Z" }], ["paused", { ...bank, grants: [{ ...grant, paused: true }] }],
  ["needs limit, not at limit", { ...bank, grants: [{ ...grant, use_requires_limit: true }] }], ["needs limit, at limit", { ...bank, at_limit: true, grants: [{ ...grant, use_requires_limit: true }] }],
  ["next id elsewhere", { ...bank, next_grant_id: "other-grant" }], ["two grants", { ...bank, grants: [grant, { ...grant, id: "fixture-second-grant", resets_left: 2, ends_at: "2026-11-30T00:00:00+00:00" }] }],
  ["bad id", { ...bank, grants: [{ ...grant, id: "Bad Id!" }], next_grant_id: "Bad Id!" }], ["not started", { ...bank, grants: [{ ...grant, starts_at: "2026-12-01T00:00:00+00:00" }] }],
];
for (const [name, block] of bankVariants) for (const t of nows) creditCases.push({ name: `claudeBank ${name} @${iso(t)}`, input: { block: block ?? null, now: t }, result: runSync(() => claudeBank(block, t)) });
emit("quota/banks.json", { function: "provider-codex.mjs availableCredits(data, now) and provider-claude.mjs claudeBank(block, now)", cases: creditCases }, tagged);

// --- 6. JavaScript semantics the port relies on --------------------------------------
// localeCompare (id tie-breaks), Date.parse (provider timestamps) and number-to-string
// (labels, notes, digests), as this Node's V8 and ICU answer them.
let seed = 42;
const random = () => { seed = (seed + 0x6d2b79f5) | 0; let x = Math.imul(seed ^ (seed >>> 15), 1 | seed); x = (x + Math.imul(x ^ (x >>> 7), 61 | x)) ^ x; return ((x ^ (x >>> 14)) >>> 0) / 4294967296; };
const ascii = Array.from({ length: 95 }, (_, i) => String.fromCharCode(32 + i));
const idChars = "abcxyzABCXYZ0129-_.@ |~";
const randomId = () => Array.from({ length: Math.floor(random() * 7) }, () => idChars[Math.floor(random() * idChars.length)]).join("");
const comparePairs = [];
for (const a of ascii) for (const b of ascii) comparePairs.push([a, b]);
for (let i = 0; i < 4000; i++) comparePairs.push([randomId(), randomId()]);
const ids = pool.accounts.map((a) => a.id);
for (const a of ids) for (const b of ids) comparePairs.push([a, b]);
const dateStrings = ["2026-09-22T20:42:33.558403Z", "2026-09-22T20:42:33.5Z", "2026-09-30T13:00:00+00:00", "2026-09-30T13:00:00-05:30", "2026-10-28", "2026-10", "2026",
  "2026-10-28T00:00Z", "2026-10-28 00:00:00Z", "2026-10-28T24:00:00Z", "2026-02-30T00:00:00Z", "2026-10-28t00:00:00z", "+002026-10-28T00:00:00Z", "Bad", "", "2026-10-28T00:00:00.Z",
  "2026-13-01T00:00:00Z", "2026-10-32T00:00:00Z", "2026-10-28T25:00:00Z", "2026-10-28T00:60:00Z", "2026-10-28T00:00:60Z", "2026-10-28T00:00:00+24:00", "1969-12-31T23:59:59.999Z", "0000-01-01T00:00:00Z"];
for (let i = 0; i < 300; i++) {
  const t = Math.floor(random() * 4e12) - 1e12;
  const s = new Date(t).toISOString();
  dateStrings.push(s, s.replace(".", ",").replace(/,\d+/, ""), s.slice(0, 19) + "+0" + Math.floor(random() * 10) + ":30", s.slice(0, 10), s.slice(0, 16) + "Z");
}
// Every timestamp shape in the recorded inputs (digits folded to 9), up to 40 strings each, so a
// stricter parser in the port is still held to everything Agent Pools actually reads.
const recorded = new Map();
const looksLikeDate = /^[+-]?\d{4,6}-\d\d(-\d\d)?([T ]\d\d:\d\d)?/;
const collect = (v) => {
  if (typeof v === "string") {
    if (!looksLikeDate.test(v)) return;
    const shape = v.replace(/\d/g, "9");
    const list = recorded.get(shape) ?? [];
    if (list.length < 40 && !list.includes(v)) list.push(v);
    recorded.set(shape, list);
  } else if (v && typeof v === "object") for (const x of Object.values(v)) collect(x);
};
for (const doc of [pool, previousForecast, registry, samples, ledger, ...fs.readdirSync(path.join(inputs, "provider")).map((n) => provider(n))]) collect(doc);
for (const s of [...recorded.values()].flat()) dateStrings.push(s);const numbers = [0, -0, 1, -1, 1.5, 0.1 + 0.2, 1e21, 1e-7, 5e-7, 123456789012345680000, 2 ** 53, 2 ** 53 + 2, 1 / 3, 100, 604800, 18000, 3.5, -2.25e-10, 1.7976931348623157e308, 5e-324];
for (let i = 0; i < 500; i++) numbers.push((random() - 0.5) * 10 ** Math.floor(random() * 40 - 20));
emit("js/semantics.json", {
  node: process.version, icu: process.versions.icu, locale: Intl.Collator().resolvedOptions().locale,
  localeCompare: comparePairs.map(([a, b]) => [a, b, Math.sign(a.localeCompare(b))]),
  dateParse: [...new Set(dateStrings)].map((s) => [s, Number.isFinite(Date.parse(s)) ? Date.parse(s) : null]),
  recordedDateShapes: [...recorded.keys()].sort(),
  numberToString: numbers.map((n) => [n, String(n)]),
});
// --- write or check --------------------------------------------------------------------
const sha = (text) => createHash("sha256").update(text).digest("hex");
const index = { format: "agent-pools-golden-outputs/v1", inputs: manifest.now, forecastProblems, files: Object.fromEntries([...produced].sort(([a], [b]) => a.localeCompare(b)).map(([rel, text]) => [rel, { bytes: Buffer.byteLength(text), sha256: sha(text) }])) };
produced.set("index.json", JSON.stringify(index, null, 1) + "\n");
let differences = 0;
for (const [rel, text] of produced) {
  const file = path.join(outputs, rel);
  if (check) {
    const before = fs.existsSync(file) ? fs.readFileSync(file, "utf8") : null;
    if (before !== text) { differences++; console.log(`differs: ${rel}`); }
  } else {
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, text);
  }
}
const nonNull = resetCases.filter((c) => c.result).length;
console.log(`${check ? "checked" : "wrote"} ${produced.size} files; chooseReset ${resetCases.length} cases (${nonNull} choose a credit, ${resetCases.filter((c) => c.result?.trigger).length} before-lapse); usage ${usageCases.length}; banks ${creditCases.length}; replay ${replay.length} hours`);
if (check && differences) { console.log(`${differences} file(s) differ`); process.exit(1); }