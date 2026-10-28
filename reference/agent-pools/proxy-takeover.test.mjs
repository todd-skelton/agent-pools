import test from "node:test";
import assert from "node:assert/strict";
import { planTakeover, retryLookup } from "./proxy-takeover.mjs";

const root = "D:\\Pool";
const config = "D:\\Pool\\proxy.yaml";
const build = (v) => `D:\\Pool\\proxy\\${v}\\cli-proxy-api.exe`;
const proc = (pid, v, created = "2026-10-16T08:58:09.406-05:00") => ({ ProcessId: pid, CreationDate: created, ExecutablePath: build(v), CommandLine: `${build(v)} -config ${config}` });
const timeout = () => Object.assign(new Error("Command failed: powershell.exe"), { killed: true, signal: "SIGTERM" });

// A simulated machine: processes by PID, one port holder, lookups that fail
// on demand. Killing a process frees the port it holds.
function world({ procs = {}, holder = null, infoFailures = {}, ownerFailures = 0 } = {}) {
  const live = new Map(Object.entries(procs).map(([pid, p]) => [Number(pid), p]));
  const w = { killed: [], events: [], infoCalls: 0, ownerCalls: 0, holder };
  const fails = { ...infoFailures };
  Object.assign(w, {
    alive: (pid) => live.has(Number(pid)),
    kill: (pid) => { w.killed.push(Number(pid)); live.delete(Number(pid)); if (w.holder === Number(pid)) w.holder = null; },
    portFree: async () => w.holder == null,
    processInfo: async (pid) => { w.infoCalls++; if ((fails[pid] ?? 0) > 0) { fails[pid]--; throw timeout(); } return live.get(Number(pid)) ?? null; },
    portOwner: async () => { w.ownerCalls++; if (ownerFailures > 0) { ownerFailures--; throw timeout(); } return w.holder == null ? null : live.get(w.holder) ?? { ProcessId: w.holder, ExecutablePath: "C:\\other\\app.exe", CommandLine: "app.exe" }; },
    sleep: async () => {},
    event: async (type, details) => { w.events.push({ type, ...details }); },
  });
  return w;
}
const plan = (w, extra = {}) => planTakeover({ name: "proxy", exe: build("pool.25"), config, root, port: 8317, ...w, ...extra });
const record = { pid: 13724, creationDate: "2026-10-16T08:58:09.406-05:00" };

test("deploy: a recorded old build is retired even when its first identity reads time out (2026-10-18)", async () => {
  const w = world({ procs: { 13724: proc(13724, "pool.24") }, holder: 13724, infoFailures: { 13724: 2 } });
  const result = await plan(w, { record });
  assert.deepEqual(result, { action: "spawn" });
  assert.deepEqual(w.killed, [13724]);
  assert.equal(w.events.filter((e) => e.type === "process-lookup-error").length, 2);
  assert.ok(w.events.every((e) => e.type !== "port-busy"));
  assert.equal(w.events.find((e) => e.type === "process-lookup-error").timedOut, true);
});

test("deploy: when the recorded read keeps failing, the port holder is identified and retired instead", async () => {
  const w = world({ procs: { 13724: proc(13724, "pool.24") }, holder: 13724, infoFailures: { 13724: 3 } });
  const result = await plan(w, { record });
  assert.deepEqual(result, { action: "spawn" });
  assert.deepEqual(w.killed, [13724]);
  assert.ok(w.events.some((e) => e.type === "process-retired" && e.unrecorded));
});

test("an unidentifiable holder is never killed and is reported as lookup-failed, not port-busy", async () => {
  for (const ownerFailures of [3, 0]) {
    // ownerFailures 0 with a recorded-read failure: the port lookup answers but finds no listener on 127.0.0.1.
    const w = world({ procs: { 13724: proc(13724, "pool.24") }, holder: 13724, infoFailures: { 13724: 99 }, ownerFailures });
    if (!ownerFailures) w.portOwner = async () => { w.ownerCalls++; return null; };
    const result = await plan(w, { record });
    assert.equal(result.action, "wait");
    assert.equal(result.reason, "lookup-failed");
    assert.deepEqual(w.killed, []);
  }
});

test("a port held by something that is not this pool's proxy is port-busy and left alone", async () => {
  const w = world({ holder: 999 });
  const result = await plan(w, { record: null });
  assert.equal(result.action, "wait");
  assert.equal(result.reason, "port-busy");
  assert.equal(result.owner.ProcessId, 999);
  assert.deepEqual(w.killed, []);
});

test("the selected build is adopted, from the record or from the port", async () => {
  const recorded = world({ procs: { 13724: proc(13724, "pool.25") }, holder: 13724 });
  assert.deepEqual(await plan(recorded, { record }), { action: "adopt", pid: 13724 });
  const unrecorded = world({ procs: { 13724: proc(13724, "pool.25") }, holder: 13724 });
  const result = await plan(unrecorded, { record: null });
  assert.equal(result.action, "adopt");
  assert.deepEqual(result.record, record);
  assert.deepEqual([...recorded.killed, ...unrecorded.killed], []);
});

test("a reused PID (same number, different creation time) is not killed through the record", async () => {
  const w = world({ procs: { 13724: { ...proc(13724, "pool.24", "2026-10-18T19:00:00.000-05:00"), ExecutablePath: "C:\\other\\app.exe", CommandLine: "app.exe" } }, holder: 13724 });
  const result = await plan(w, { record });
  assert.equal(result.reason, "port-busy");
  assert.deepEqual(w.killed, []);
});

test("a dead recorded PID is not looked up; a free port spawns at once", async () => {
  const w = world();
  assert.deepEqual(await plan(w, { record }), { action: "spawn" });
  assert.equal(w.infoCalls, 0);
  assert.equal(w.ownerCalls, 0);
});

test("nothing is retired once the supervisor is closing", async () => {
  const w = world({ procs: { 13724: proc(13724, "pool.24") }, holder: 13724 });
  const result = await plan(w, { record, closing: () => true });
  assert.equal(result.action, "wait");
  assert.deepEqual(w.killed, []);
});

test("retryLookup returns the first success and rethrows the last failure", async () => {
  let calls = 0;
  assert.equal(await retryLookup(async () => { if (++calls < 3) throw timeout(); return "ok"; }, { sleep: async () => {} }), "ok");
  calls = 0;
  await assert.rejects(retryLookup(async () => { calls++; throw new Error(`fail ${calls}`); }, { sleep: async () => {} }), /fail 3/);
});
