import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync, mkdtempSync, writeFileSync, readFileSync as rf } from "node:fs";
import os from "node:os";
import path from "node:path";
import { buildRegistry, compareVersions, parseVersion, emptyLineage, admittedEfforts } from "./model-registry.mjs";
import { judgeClaude, judgeCodex, poolRouting } from "./model-admission.mjs";
import { matchRow, snapshot, effortFromName } from "./model-benchmarks.mjs";
import { probePlan, fairBatch, wantedEfforts, degraded, recheck } from "./model-refresh.mjs";
import { withLock, writeJSONAtomic } from "./model-store.mjs";

const families = JSON.parse(readFileSync(new URL("./model-families.json", import.meta.url), "utf8"));
const NOW = Date.parse("2026-09-29T20:00:00Z");
const HV = { codex: "0.159.0", claude: "2.1.285" };
const m = (id, levels = ["low", "medium", "high", "xhigh", "max"]) => ({ id, thinking: JSON.stringify({ levels }) });
const live = (lists, fetched = "2026-09-29T19:00:00Z") => ({ format: 1, lists: Object.fromEntries(
  Object.entries(lists).map(([k, [provider, models]]) => [k, { provider, fetched_at: fetched, models }])) });
const probes = (id, efforts, extra = {}) => efforts.map((effort) => ({ model: id, effort, ok: true,
  harnessVersion: id.startsWith("claude") ? HV.claude : HV.codex, at: "2026-09-29T18:00:00Z", ...extra }));
// Pool status that reports every listed model ready on every listing account.
const readyStatus = (lm) => ({ checkedAt: "2026-09-29T19:59:00Z", accounts: Object.entries(lm?.lists ?? {}).map(([id, l]) => ({ id, disabled: false,
  routingModels: Object.fromEntries((l.models ?? []).map((x) => [x.id, { status: "ready" }])) })) });
const build = (o) => buildRegistry({ families, harnessVersions: HV, now: NOW, ...o, status: "status" in o ? o.status : readyStatus(o.liveModels) });
const SOL = ["medium", "high", "xhigh"];

test("versions parse from exact ids and compare numerically", () => {
  const rule = (f) => families.families.find((x) => x.family === f);
  assert.equal(parseVersion(rule("sol"), "gpt-6.1-sol"), "6.1");
  assert.equal(parseVersion(rule("opus"), "claude-opus-5-5"), "5.5");
  assert.equal(parseVersion(rule("opus"), "claude-opus-4-5-20251101"), "4.5");
  assert.equal(parseVersion(rule("sol"), "gpt-6-solar"), null);
  assert.equal(parseVersion(rule("sol"), "gpt-6.1-sol & calc"), null);
  assert.ok(compareVersions("6.10", "6.9") > 0);
});

test("fully admitted newer version is promoted and the old one retired", () => {
  const { registry, lineage } = build({ lineage: { ...emptyLineage(), families: { sol: { current: "gpt-6-sol" } },
      identities: { "gpt-6-sol": { family: "sol", provider: "codex", version: "6" } } },
    probes: [...probes("gpt-6.1-sol", SOL), ...probes("gpt-6-sol", SOL)],
    liveModels: live({ a: ["codex", [m("gpt-6.1-sol"), m("gpt-6-sol")]], b: ["codex", [m("gpt-6-sol")]] }) });
  assert.equal(registry.families.sol.current, "gpt-6.1-sol");
  assert.deepEqual(registry.models["gpt-6.1-sol"].usableAccountsByEffort.high, ["a"]);
  assert.ok(lineage.identities["gpt-6-sol"].retiredAt);
  assert.equal(lineage.events.at(-1).event, "promoted");
});

test("F3: admission at only some required efforts does not promote", () => {
  const { registry } = build({ probes: probes("gpt-6.1-sol", ["low", "medium"]),
    liveModels: live({ a: ["codex", [m("gpt-6.1-sol")]] }) });
  assert.equal(registry.families.sol.current, null);
  assert.equal(registry.families.sol.candidate, "gpt-6.1-sol");
  assert.deepEqual(registry.notices.find((n) => n.family === "sol").missingEfforts, ["high", "xhigh"]);
});

test("F3: stale, failed or other-harness evidence does not count", () => {
  const p = [...probes("gpt-6.1-sol", ["medium"], { at: "2026-09-01T00:00:00Z" }),
    ...probes("gpt-6.1-sol", ["high"], { harnessVersion: "0.158.0" }),
    ...probes("gpt-6.1-sol", ["xhigh"]), { ...probes("gpt-6.1-sol", ["xhigh"])[0], ok: false, at: "2026-09-29T19:00:00Z" }];
  assert.deepEqual(admittedEfforts(p, "gpt-6.1-sol", HV.codex, NOW), []);
});

test("F4: efforts and usability are per account; stale lists and blocked routing are unusable", () => {
  const status = { accounts: [{ id: "a", disabled: false, routingModels: { "gpt-6.1-sol": { status: "ready" } } }, { id: "b", disabled: false, routingModels: { "gpt-6.1-sol": { status: "blocked" } } }, { id: "c", disabled: false, routingModels: { "gpt-6.1-sol": { status: "ready" } } }] };
  const lm = live({ a: ["codex", [m("gpt-6.1-sol", ["low"])]], b: ["codex", [m("gpt-6.1-sol")]] });
  lm.lists.c = { provider: "codex", fetched_at: "2026-09-20T00:00:00Z", models: [m("gpt-6.1-sol")] };
  const { registry } = build({ status, liveModels: lm, probes: probes("gpt-6.1-sol", SOL) });
  const r = registry.models["gpt-6.1-sol"];
  assert.deepEqual(r.usableAccountsByEffort.low, ["a"]);
  assert.deepEqual(r.usableAccountsByEffort.high, []);
  assert.equal(registry.families.sol.current, null, "no usable account for required efforts");
});

test("review r3: usability fails closed on missing status and future timestamps", () => {
  const lm = live({ a: ["codex", [m("gpt-6.1-sol")]], b: ["codex", [m("gpt-6.1-sol")]] });
  const usable = (o) => build({ liveModels: lm, probes: probes("gpt-6.1-sol", SOL), ...o }).registry.models["gpt-6.1-sol"].usableAccountsByEffort.high;
  assert.deepEqual(usable({}), ["a", "b"]);
  assert.deepEqual(usable({ status: null }), [], "no pool status: nothing is routable");
  assert.deepEqual(usable({ status: { accounts: [{ id: "a", routingModels: { "gpt-6.1-sol": { status: "ready" } } }] } }), ["a"], "an account absent from status is not usable");
  assert.deepEqual(usable({ status: { accounts: [{ id: "a", routingModels: {} }, { id: "b" }] } }), [], "unknown per-model routing is not usable");
  const future = live({ a: ["codex", [m("gpt-6.1-sol")]] }, "2099-01-01T00:00:00Z");
  assert.deepEqual(build({ liveModels: future, probes: probes("gpt-6.1-sol", SOL) }).registry.models["gpt-6.1-sol"].usableAccountsByEffort.high, [], "future list is stale");
  assert.deepEqual(admittedEfforts(probes("gpt-6.1-sol", SOL, { at: "2099-01-01T00:00:00Z" }), "gpt-6.1-sol", HV.codex, NOW), [], "future probe is not evidence");
  assert.deepEqual(admittedEfforts(probes("gpt-6.1-sol", SOL, { at: "2026-09-29T20:04:00Z" }), "gpt-6.1-sol", HV.codex, NOW), SOL, "small clock skew is tolerated");
});

test("F5: promotion is monotonic and identities survive delisting", () => {
  const lineage = { ...emptyLineage(), generation: 1, families: { sol: { current: "gpt-6.1-sol" } },
    identities: { "gpt-6.1-sol": { family: "sol", provider: "codex", version: "6.1" }, "gpt-6-sol": { family: "sol", provider: "codex", version: "6", retiredAt: "x" } } };
  const { registry } = build({ lineage, probes: probes("gpt-6-sol", SOL), liveModels: live({ a: ["codex", [m("gpt-6-sol")]] }) });
  assert.equal(registry.families.sol.current, "gpt-6.1-sol", "never demoted");
  assert.deepEqual(registry.families.sol.unavailableEfforts, SOL);
  assert.ok(registry.identities["gpt-6.1-sol"], "delisted identity retained");
  assert.ok(registry.notices.some((n) => n.notice === "current-unavailable"));
  assert.equal(registry.generation, 1);
});

test("F1: unsafe ids never enter the registry", () => {
  const { registry } = build({ liveModels: live({ a: ["codex", [m("gpt-7-sol & del x"), m("GPT-7-SOL")]] }) });
  assert.deepEqual(Object.keys(registry.models), []);
});

test("provider mismatch is ignored", () => {
  const { registry } = build({ liveModels: live({ c: ["claude", [m("gpt-6-luna")]] }) });
  assert.equal(registry.models["gpt-6-luna"], undefined);
});

test("F2: admission judges require exact model, exact reply and pool routing evidence", () => {
  assert.equal(judgeClaude("claude-sonnet-5-5", JSON.stringify({ result: "OK", modelUsage: { "claude-sonnet-5-5": {} } })).ok, true);
  assert.equal(judgeClaude("claude-sonnet-5-5", JSON.stringify({ result: "OK", modelUsage: { "claude-sonnet-5": {} } })).ok, false);
  assert.equal(judgeClaude("claude-sonnet-5-5", JSON.stringify({ result: "OK then more", modelUsage: { "claude-sonnet-5-5": {} } })).ok, false);
  const ev = (t) => [JSON.stringify({ type: "item.completed", item: { type: "agent_message", text: t } }), JSON.stringify({ type: "turn.completed" })].join("\n");
  assert.equal(judgeCodex(ev("OK")).ok, true);
  assert.equal(judgeCodex(ev("OK, sure")).ok, false);
  const log = [
    "[2026-09-29 17:48:14] [3144a313] [info ] [selector.go:1107] session-affinity: cache miss, new binding | session=pck:01a0... auth=codex-a.json provider=mixed model=gpt-6.1-sol",
    "[2026-09-29 17:48:16] [3144a313] [info ] [gin_logger.go:103] 200 |        2.938s |       127.0.0.1 | GET     \"/v1/responses\"",
    "[2026-09-29 17:48:15] [aaaaaaaa] [info ] [selector.go:1107] x auth=codex-b.json provider=mixed model=gpt-6-sol"].join("\n");
  const t0 = Date.parse("2026-09-29T17:48:13"), t1 = Date.parse("2026-09-29T17:48:17");
  assert.deepEqual(poolRouting(log, "gpt-6.1-sol", t0, t1), { account: "codex-a.json", requestId: "3144a313" });
  assert.equal(poolRouting(log, "gpt-6-sol", t0, t1), null, "no 2xx for that request");
});

test("probe plan covers current and candidate required efforts only", () => {
  const { registry } = build({ probes: probes("gpt-6.1-sol", ["medium"]),
    liveModels: live({ a: ["codex", [m("gpt-6.1-sol"), m("gpt-6-sol")]] }) });
  const plan = probePlan(registry, families, probes("gpt-6.1-sol", ["medium"]), HV, NOW);
  assert.deepEqual(plan.find((p) => p.id === "gpt-6.1-sol").efforts, ["high", "xhigh"]);
  assert.equal(plan.find((p) => p.id === "gpt-6-sol"), undefined, "older non-current version is not probed");
});

test("F9: benchmark rows match only exact id plus effort; others stay unmatched", () => {
  const ids = { "gpt-6.1-sol": {}, "gpt-6-sol": {}, "claude-opus-5-5": {} };
  assert.deepEqual(matchRow({ slug: "gpt-6-1-sol-high", name: "GPT-6.1 Sol (high)" }, ids), { model: "gpt-6.1-sol", effort: "high" });
  assert.deepEqual(matchRow({ slug: "gpt-6-1-sol", name: "GPT-6.1 Sol (max)" }, ids), { model: "gpt-6.1-sol", effort: "max" });
  assert.deepEqual(matchRow({ slug: "claude-opus-5-5-xhigh", name: "Claude Opus 5.5 (Adaptive Reasoning, Xhigh Effort, Default Fallback)" }, ids), { model: "claude-opus-5-5", effort: "xhigh" });
  assert.equal(matchRow({ slug: "gpt-6-sol-non-reasoning", name: "GPT-6 Sol (Non-reasoning)" }, ids), null);
  assert.equal(matchRow({ slug: "gpt-6-sol-high", name: "GPT-6 Sol (max)" }, ids), null, "slug and name disagree");
  assert.equal(effortFromName("Claude Fable 5.1 (Adaptive Reasoning, Max Effort, Default Fallback)"), "max");
  // AA renamed Claude rows on 2026-10-03 to "(<Effort>, Default Fallback)".
  assert.deepEqual(matchRow({ slug: "claude-opus-5-5-high", name: "Claude Opus 5.5 (High, Default Fallback)" }, ids), { model: "claude-opus-5-5", effort: "high" });
  assert.deepEqual(matchRow({ slug: "claude-opus-5-5", name: "Claude Opus 5.5 (Max, Default Fallback)" }, ids), { model: "claude-opus-5-5", effort: "max" });
  assert.equal(effortFromName("Claude Opus 4.6 (Non-reasoning, High)"), null);
  assert.equal(snapshot([{ slug: "unknown-model", name: "X (high)" }], ids, "t").rows.length, 0);
});

test("F8: lock serializes writers and atomic write leaves no temp files", async () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), "reg-lock-"));
  const file = path.join(dir, "r.json");
  const order = [];
  await Promise.all([1, 2, 3].map((i) => withLock(file, async () => { order.push(`in${i}`); await new Promise((r) => setTimeout(r, 30)); order.push(`out${i}`); await writeJSONAtomic(file, { i }); })));
  for (let k = 0; k < order.length; k += 2) assert.equal(order[k].slice(2), order[k + 1].slice(3));
  assert.ok([1, 2, 3].includes(JSON.parse(rf(file, "utf8")).i));
});

test("review r3: a renewed lock is never taken over, and release never removes a successor's lock", async () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), "lock-"));
  const file = path.join(dir, "x.json");
  const order = [];
  const holder = withLock(file, async () => { order.push("a-start"); await new Promise((r) => setTimeout(r, 700)); order.push("a-end"); }, { renewMs: 50 });
  await new Promise((r) => setTimeout(r, 50));
  await withLock(file, async () => { order.push("b"); }, { staleMs: 300, waitMs: 5000 });
  await holder;
  assert.deepEqual(order, ["a-start", "a-end", "b"], "a lock older than staleMs but still renewed is respected");
  const { rm: rmp, writeFile: wf } = await import("node:fs/promises");
  await withLock(file, async () => {
    await rmp(file + ".lock");
    await wf(file + ".lock", JSON.stringify({ token: "successor" }));
  });
  assert.equal(JSON.parse(rf(file + ".lock", "utf8")).token, "successor", "the successor's lock survives the first holder's release");
});

test("review r3: probe batches rotate by last attempt so a failing target cannot starve later ones", () => {
  const plan = [{ id: "gpt-6.1-sol", provider: "codex", efforts: ["medium", "high"] }, { id: "claude-opus-5-5", provider: "claude", efforts: ["low", "medium"] }];
  const tried = [{ model: "gpt-6.1-sol", effort: "medium", at: "2026-09-29T19:00:00Z", ok: false }, { model: "gpt-6.1-sol", effort: "high", at: "2026-09-29T19:05:00Z", ok: false }];
  assert.deepEqual(fairBatch(plan, tried, 2), [{ id: "claude-opus-5-5", provider: "claude", efforts: ["low", "medium"] }]);
  assert.deepEqual(fairBatch(plan, tried, 3).map((t) => `${t.id}:${t.efforts}`), ["claude-opus-5-5:low,medium", "gpt-6.1-sol:medium"]);
  assert.deepEqual(fairBatch(plan, [], 16).map((t) => t.efforts.length), [2, 2]);
  assert.deepEqual(fairBatch(plan, [], 0), []);
});
test("review r4: concurrent takeover of a stale lock yields one owner at a time", async () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), "lock-race-"));
  const file = path.join(dir, "x.json");
  const { utimes, writeFile: wf } = await import("node:fs/promises");
  for (let round = 0; round < 5; round++) {
    await wf(file + ".lock", JSON.stringify({ token: "dead" }));
    const old = new Date(Date.now() - 3600_000);
    await utimes(file + ".lock", old, old);
    let inside = 0, maxInside = 0;
    const body = async () => { inside++; maxInside = Math.max(maxInside, inside); await new Promise((r) => setTimeout(r, 30)); inside--; };
    await Promise.all(Array.from({ length: 6 }, () => withLock(file, body, { staleMs: 1000, waitMs: 10_000 })));
    assert.equal(maxInside, 1, `round ${round}: two holders overlapped`);
  }
});

test("review r4: a future-dated probe cannot hide later valid probes or park its target", () => {
  const p = [...probes("gpt-6.1-sol", ["high"], { at: "2099-01-01T00:00:00Z", ok: true }), ...probes("gpt-6.1-sol", ["high"], { at: "2026-09-29T19:00:00Z" })];
  assert.deepEqual(admittedEfforts(p, "gpt-6.1-sol", HV.codex, NOW), ["high"]);
  const plan = [{ id: "gpt-6.1-sol", provider: "codex", efforts: ["high"] }, { id: "claude-opus-5-5", provider: "claude", efforts: ["low"] }];
  const tried = [{ model: "gpt-6.1-sol", effort: "high", at: "2099-01-01T00:00:00Z" }, { model: "claude-opus-5-5", effort: "low", at: "2026-09-29T19:00:00Z" }];
  assert.deepEqual(fairBatch(plan, tried, 1, NOW).map((t) => t.id), ["gpt-6.1-sol"], "the future row counts as never attempted");
});
test("review r5: efforts the policy uses or a preview offers are probed, not only requiredEfforts", () => {
  const lm = live({ a: ["claude", [m("claude-opus-5-5")]] });
  const pr = probes("claude-opus-5-5", ["low", "medium", "high"]);
  const { registry } = build({ liveModels: lm, probes: pr });
  assert.deepEqual(probePlan(registry, families, pr, HV, NOW), [], "required efforts are all admitted");
  const policy = { rows: { "5": { slots: { "claude.primary": { family: "opus", effort: "high" } } } } };
  const preview = { slots: [{ proposal: null, consider: [{ family: "opus", effort: "xhigh" }] }, { proposal: { family: "sonnet", effort: "max" }, consider: [] }] };
  const extra = wantedEfforts(policy, preview);
  assert.deepEqual([...extra.opus].sort(), ["high", "xhigh"]);
  assert.deepEqual(probePlan(registry, families, pr, HV, NOW, extra), [{ id: "claude-opus-5-5", provider: "claude", efforts: ["xhigh"] }]);
  const after = build({ liveModels: lm, probes: [...pr, ...probes("claude-opus-5-5", ["xhigh"])] }).registry;
  assert.ok(after.models["claude-opus-5-5"].admittedEfforts.includes("xhigh"), "a passing probe makes the offered effort applicable");
  assert.deepEqual(wantedEfforts(null, null), {});
});
test("an unchanged list re-read reported by pool status keeps the account fresh", () => {
  const lm = live({ a: ["codex", [m("gpt-6.1-sol")]] }, "2026-09-29T04:00:00Z"); // file untouched for 16h
  const st = (modelList) => ({ accounts: [{ id: "a", disabled: false, modelList, routingModels: { "gpt-6.1-sol": { status: "ready" } } }] });
  const usable = (s) => build({ liveModels: lm, probes: probes("gpt-6.1-sol", SOL), status: s }).registry.models["gpt-6.1-sol"].usableAccountsByEffort.high;
  assert.deepEqual(usable(st(undefined)), [], "file time alone is stale");
  assert.deepEqual(usable(st({ source: "live", fetchedAt: "2026-09-29T19:45:00Z", error: null })), ["a"], "live re-read time from status counts");
  // The proxy keeps routing on its last good list after a failed re-read and
  // reports that list's time as fetchedAt, so a transient failure (DNS after
  // wake, 2026-10-03 21:28) must not make the account unusable.
  assert.deepEqual(usable(st({ source: "live", fetchedAt: "2026-09-29T19:45:00Z", error: "lookup chatgpt.com: i/o timeout" })), ["a"], "a failed re-read keeps the last good read time");
  assert.deepEqual(usable(st({ source: "live", fetchedAt: "2026-09-29T10:00:00Z", error: "401" })), [], "a last good read past the stale limit does not count");
  assert.deepEqual(usable(st({ source: "pending", fetchedAt: "2026-09-29T19:45:00Z" })), [], "only a live read counts");
  assert.deepEqual(usable(st({ source: "live", fetchedAt: "2099-01-01T00:00:00Z" })), [], "a future status time is invalid");
});

test("recheck rebuilds only while a family reports an unavailable effort", async () => {
  assert.equal(degraded(null), true, "a missing registry is rebuilt");
  assert.equal(degraded({ families: { sol: { unavailableEfforts: [] }, opus: { unavailableEfforts: [] } } }), false);
  assert.equal(degraded({ families: { sol: { unavailableEfforts: ["high"] }, opus: { unavailableEfforts: [] } } }), true);
  const root = mkdtempSync(path.join(os.tmpdir(), "recheck-"));
  const file = path.join(root, "state", "model-registry.json");
  await import("node:fs/promises").then((f) => f.mkdir(path.dirname(file), { recursive: true }));
  const healthy = JSON.stringify({ families: { sol: { unavailableEfforts: [] } } });
  writeFileSync(file, healthy);
  const lines = [];
  assert.deepEqual(await recheck(root, { log: (l) => lines.push(l) }), { rebuilt: false });
  assert.deepEqual(lines, [], "a healthy registry is left alone without logging");
  assert.equal(rf(file, "utf8"), healthy);
});
