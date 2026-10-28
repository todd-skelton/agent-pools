import test from "node:test";
import assert from "node:assert/strict";
import { normalizeUsage } from "./routing.mjs";
import { currentAccount, invalidateAccount, STATUS_MAX_AGE_MS } from "./freshness.mjs";
import { processReset } from "./reset-lifecycle.mjs";
import { PROVIDERS } from "./providers.mjs";
import { createHealthPolicy, createProxyRestartPolicy, sameIdentity, sameProcess, ownedProxy, usesConfig } from "./health-policy.mjs";
const now = Date.parse("2026-09-13T02:00:00Z");
const date = (offset = 0) => new Date(now + offset).toISOString();
const a = () => ({ id: "one", accountId: "provider-one", provider: "codex", status: "exhausted",
  usageUpdatedAt: date(), creditsUpdatedAt: date(), routingCheckedAt: date(), routingStatus: "ready",
  creditsKnown: true, creditCount: 1, credits: [{ id: "c", expires_at: date(86400000) }], quotaWindows: [{ used: 100 }] });
const state = () => ({ accounts: [a()], resetAutomation: true, resets: { codex: { pending: null, last: null } } });
const policy = { provider: "codex", enabled: true, trigger: "any", headroomPercent: 5, expectedCount: 1 };
const requestId = PROVIDERS.codex.requestId;

test("structured-only Claude exhaustion controls both display and routing", () => {
  const result = normalizeUsage("claude", { limits: [
    { kind: "session", percent: 10 }, { kind: "weekly_all", percent: 100 },
    { kind: "weekly_scoped", percent: 0, scope: { model: "Fable" } },
  ] }, now);
  assert.equal(result.status, "exhausted");
  assert.equal(result.quotaWindows.filter(w => w.common).length, 2);
  assert.equal(result.remaining, 0);
});
test("structured Claude values supersede conflicting legacy data without treating scoped limits as common", () => {
  const result = normalizeUsage("claude", { five_hour: { utilization: 100 }, seven_day: { utilization: 100 },
    limits: [{ kind: "session", percent: 10 }, { kind: "weekly_all", percent: 20 },
      { kind: "weekly_scoped", percent: 100, scope: { model: "Fable" } }] }, now);
  assert.equal(result.status, "available");
  assert.equal(result.remaining, 80);
});
test("startup and failed-refresh invalidation retain percentages but revoke availability and credits", () => {
  const account = a();
  invalidateAccount(account);
  assert.equal(account.status, "unknown"); assert.equal(account.routingStatus, "unknown");
  assert.equal(account.creditsKnown, false); assert.equal(account.quotaWindows[0].used, 100);
});
test("time alone expires old status, routing and credits; future timestamps are invalid", () => {
  for (const timestamp of [date(-STATUS_MAX_AGE_MS - 1), date(1), null]) {
    const result = currentAccount({ ...a(), usageUpdatedAt: timestamp, routingCheckedAt: timestamp, creditsUpdatedAt: timestamp }, now);
    assert.equal(result.status, "unknown"); assert.equal(result.routingStatus, "unknown"); assert.equal(result.creditsKnown, false);
  }
});
test("unsent reset is cancelled when new status recovers quota", async () => {
  const s = state(); s.accounts[0].status = "available";
  s.resets.codex.pending = { phase: "unsent", accountId: "one", creditId: "c", requestId: "stable" };
  await processReset(s, { policy, requestId, now: () => now, save: async () => {}, consume: async () => assert.fail("must not consume") });
  assert.equal(s.resets.codex.pending, null); assert.equal(s.resets.codex.last.outcome, "cancelled-before-send");
});
test("uncertain reset retries its original identity even after quota recovery or automation disable", async () => {
  const s = state(); s.accounts[0].status = "available"; s.resetAutomation = false;
  s.resets.codex.pending = { phase: "uncertain", accountId: "one", creditId: "c", requestId: "stable" };
  const saved = [];
  await processReset(s, { policy, requestId, now: () => now, save: async () => saved.push(structuredClone(s)),
    consume: async (_, p) => { assert.equal(p.requestId, "stable"); assert.equal(saved[0].resets.codex.pending.phase, "uncertain"); return { code: "already_redeemed" }; } });
  assert.equal(s.resets.codex.pending, null); assert.equal(saved.at(-1).resets.codex.last.outcome, "already_redeemed");
});
test("automation disable cancels unsent resets; missing retry identity requires attention", async () => {
  const s = state(); s.resetAutomation = false;
  s.resets.codex.pending = { phase: "unsent", accountId: "one", creditId: "c", requestId: "stable" };
  const deps = { policy, requestId, now: () => now, save: async () => {}, consume: async () => assert.fail("must not consume") };
  await processReset(s, deps); assert.equal(s.resets.codex.pending, null);
  s.resets.codex.pending = { phase: "uncertain", accountId: "one", creditId: "c" };
  await processReset(s, deps); assert.equal(s.resets.codex.pending.phase, "attention");
});
test("missing/disabled uncertain reset becomes explicit attention and retains its identity", async () => {
  for (const accounts of [[], [{ ...a(), disabled: true }]]) {
    const s = { ...state(), accounts }; s.resets.codex.pending = { accountId: "one", creditId: "c", requestId: "stable" };
    await processReset(s, { policy, requestId, now: () => now, save: async () => {}, consume: async () => assert.fail("unavailable account") });
    assert.equal(s.resets.codex.pending.phase, "attention"); assert.equal(s.resets.codex.pending.requestId, "stable");
  }
});
test("crash/timeout retains reset identity and terminal state is persisted before any routing work", async () => {
  const s = state(); let persisted;
  const deps = { policy, requestId, now: () => now, save: async () => { persisted = structuredClone(s); },
    consume: async () => { throw Error("timeout"); } };
  await assert.rejects(processReset(s, deps));
  assert.equal(persisted.resets.codex.pending.phase, "uncertain");
  const id = persisted.resets.codex.pending.requestId;
  const logged = [];
  await processReset(s, { ...deps, log: async (type, d) => logged.push({ type, ...d }), consume: async (_, p) => { assert.equal(p.requestId, id); return { code: "reset" }; } });
  assert.equal(persisted.resets.codex.pending, null); assert.equal(persisted.resets.codex.last.outcome, "reset");
  assert.deepEqual(logged, [{ type: "reset-attempt", provider: "codex", accountId: "one", creditId: "c", requestId: id }, { type: "reset-result", provider: "codex", accountId: "one", creditId: "c", requestId: id, outcome: "reset" }]);
});
test("failed journal write prevents sending a reset; stale telemetry cannot authorize it", async () => {
  const s = state();
  await assert.rejects(processReset(s, { policy, requestId, now: () => now, save: async () => { throw Error("disk full"); }, consume: async () => assert.fail("not journaled") }));
  const stale = state(); stale.accounts[0].usageUpdatedAt = date(-STATUS_MAX_AGE_MS - 1);
  await processReset(stale, { policy, requestId, now: () => now, save: async () => {}, consume: async () => assert.fail("stale") });
  assert.equal(stale.resets.codex.pending, null);
});
test("health recovery requires consecutive failures and exact process identity", () => {
  const restart = createHealthPolicy();
  assert.equal(restart(false), false); assert.equal(restart(true), false);
  assert.equal(restart(false), false); assert.equal(restart(false), false); assert.equal(restart(false), true);
  const record = { pid: 42, creationDate: date() };
  const actual = { ProcessId: 42, CreationDate: date(), ExecutablePath: "C:/pool/proxy.exe", CommandLine: "proxy.exe -config C:/pool/proxy.yaml" };
  assert.equal(sameProcess(record, actual, "C:/pool/proxy.exe", "C:/pool/proxy.yaml"), true);
  assert.equal(sameProcess(record, { ...actual, CreationDate: date(1) }, "C:/pool/proxy.exe", "C:/pool/proxy.yaml"), false);
  assert.equal(sameProcess(record, actual, "C:/pool/proxy.exe", "C:/other/proxy.yaml"), false);
  // A live recorded process on a superseded binary keeps its identity (so it is retired, not ignored).
  assert.equal(sameIdentity(record, { ...actual, ExecutablePath: "C:/pool/old.exe" }), true);
  assert.equal(sameIdentity(record, null), false);
  assert.equal(sameIdentity(record, { ...actual, ProcessId: 43 }), false);
});
test("proxy restarts need a dead proxy, not slow management, and spare a starting one", () => {
  let clock = 0;
  const policy = createProxyRestartPolicy({ failures: 3, degradedFailures: 6, startupGraceMs: 100, now: () => clock });
  const read = (managed, alive) => policy.check({ managed, alive });
  // Management stalls while /healthz answers (upstream DNS/TLS outage): flagged once, no restart.
  const slow = Array.from({ length: 5 }, () => read(false, true));
  assert.deepEqual(slow.map((d) => d.restart), [false, false, false, false, false]);
  assert.deepEqual(slow.map((d) => d.degraded), [false, false, true, false, false]);
  // ...unless it stays wedged past the degraded limit.
  assert.equal(read(false, true).restart, true);
  // A dead proxy restarts after the plain failure count; a good reading resets it.
  assert.equal(read(false, false).restart, false); assert.equal(read(true, true).restart, false);
  assert.equal(read(false, false).restart, false); assert.equal(read(false, false).restart, false);
  assert.equal(read(false, false).restart, true);
  // A just-started proxy is not judged during its grace period...
  policy.started();
  for (let i = 0; i < 10; i++) { clock += 9; assert.equal(read(false, false).restart, false); }
  // ...which ends when it passes, or as soon as it first answers.
  clock += 20; assert.equal(read(false, false).restart, false); assert.equal(read(false, false).restart, false);
  assert.equal(read(false, false).restart, true);
  policy.started(); assert.equal(read(true, true).restart, false);
  for (let i = 0; i < 2; i++) assert.equal(read(false, false).restart, false);
  assert.equal(read(false, false).restart, true);
});
test("an unrecorded port holder is ours only when it is a pool proxy build on the pool config", () => {
  const holder = { ProcessId: 7, CreationDate: date(), ExecutablePath: "D:\\Pool\\proxy\\7.2.158-pool.16\\cli-proxy-api.exe", CommandLine: "D:\\Pool\\proxy\\7.2.158-pool.16\\cli-proxy-api.exe -config D:\\Pool\\proxy.yaml" };
  assert.equal(ownedProxy(holder, "D:/Pool/", "D:/Pool/proxy.yaml"), true);
  assert.equal(ownedProxy(holder, "D:/Pool", "D:/Other/proxy.yaml"), false);
  assert.equal(ownedProxy({ ...holder, ExecutablePath: "D:\\Pool\\tools\\cli-proxy-api.exe" }, "D:/Pool", "D:/Pool/proxy.yaml"), false);
  assert.equal(ownedProxy({ ...holder, ExecutablePath: "D:\\Pool\\proxy\\x\\node.exe" }, "D:/Pool", "D:/Pool/proxy.yaml"), false);
  assert.equal(ownedProxy({ ...holder, ExecutablePath: "D:\\PoolTwo\\proxy\\x\\cli-proxy-api.exe" }, "D:/Pool", "D:/Pool/proxy.yaml"), false);
  assert.equal(ownedProxy(null, "D:/Pool", "D:/Pool/proxy.yaml"), false);
  // The config argument must be the pool config exactly, not merely mention it.
  assert.equal(ownedProxy({ ...holder, CommandLine: "x.exe -config D:\\Pool\\proxy.yaml.other" }, "D:/Pool", "D:/Pool/proxy.yaml"), false);
  assert.equal(ownedProxy({ ...holder, CommandLine: "x.exe -config D:\\Other.yaml --note D:\\Pool\\proxy.yaml" }, "D:/Pool", "D:/Pool/proxy.yaml"), false);
  assert.equal(ownedProxy({ ...holder, ExecutablePath: "D:\\Pool\\proxy\\..\\..\\x\\cli-proxy-api.exe" }, "D:/Pool", "D:/Pool/proxy.yaml"), false);
  assert.equal(usesConfig('x.exe -config "D:\\Pool\\proxy.yaml"', "D:/Pool/proxy.yaml"), true);
  assert.equal(usesConfig("x.exe --config=D:/Pool/proxy.yaml", "D:\\Pool\\proxy.yaml"), true);
  assert.equal(usesConfig("x.exe", "D:/Pool/proxy.yaml"), false);
  assert.equal(usesConfig("x.exe -config D:/Pool/proxy.yaml -config D:/Other/proxy.yaml", "D:/Pool/proxy.yaml"), false);
  assert.equal(usesConfig("x.exe -config D:/Pool/proxy.yaml -config=D:/Pool/proxy.yaml", "D:/Pool/proxy.yaml"), true);
  // Adopted only when it is also the build install.json selects.
  const record = { pid: 7, creationDate: holder.CreationDate };
  assert.equal(sameProcess(record, holder, "D:/Pool/proxy/7.2.158-pool.16/cli-proxy-api.exe", "D:/Pool/proxy.yaml"), true);
  assert.equal(sameProcess(record, holder, "D:/Pool/proxy/7.2.158-pool.17/cli-proxy-api.exe", "D:/Pool/proxy.yaml"), false);
});
