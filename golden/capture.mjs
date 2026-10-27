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
import { buildUsageReports, valueConfig } from "../reference/agent-pools/usage-value.mjs";
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

// --- 3. /api/summary ---------------------------------------------------------------
const summary = await run(() => buildSummary({
  state: { ...pool, accounts: pool.accounts.map((a) => currentAccount(a, now)) },
  providers: Object.fromEntries(PROVIDER_IDS.map((id) => [id, { name: PROVIDERS[id].name }])),
  forecast: roundTrip(forecastNow), value: roundTrip(deployed.valueReport), now,
}));
emit("summary/now.json", summary);

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