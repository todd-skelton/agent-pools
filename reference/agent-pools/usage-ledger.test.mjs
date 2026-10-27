import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, readdir, writeFile, mkdir, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { ledgerRow, quotaFromHeaders, tokensOf, clientOf, ledgerConfig, createUsageLedger, LEDGER_DIR } from "./usage-ledger.mjs";

// Shape of a /v0/management/usage-queue record (CLIProxyAPI redisqueue plugin).
const record = (extra = {}) => ({
  timestamp: "2026-10-15T08:47:34.9727204-05:00",
  latency_ms: 6417,
  source: "todd@example.com",
  auth_index: "aaaa",
  access_token_sha256: "deadbeef",
  client_ip: "127.0.0.1",
  x_forwarded_for: "10.0.0.1",
  user_agent: "claude-cli/2.1.299 (external, sdk-cli)",
  tokens: { input_tokens: 2, output_tokens: 666, reasoning_tokens: 108, cached_tokens: 78668, cache_read_tokens: 78668, cache_creation_tokens: 1185, total_tokens: 80521 },
  failed: false,
  fail: { status_code: 200, body: "" },
  response_headers: {
    "Anthropic-Ratelimit-Unified-7d-Utilization": ["0.77"],
    "Anthropic-Ratelimit-Unified-7d-Reset": ["1792288800"],
    "Anthropic-Ratelimit-Unified-5h-Utilization": ["0.58"],
    "Anthropic-Ratelimit-Unified-5h-Reset": ["1792072800"],
    "Request-Id": ["req_secretish"],
  },
  token_breakdown: { schema_version: 2, quality: "complete", input: { uncached_tokens: 2, cache_read_tokens: 78668, cache_write_tokens: 1185 }, output: { total_tokens: 666, reasoning_tokens: 108 } },
  provider: "claude",
  model: "claude-opus-5-5",
  endpoint: "POST /v1/messages",
  api_key: "k".repeat(64),
  request_id: "8241b157",
  session_id: "45dbe21a-ce8c-4955-9efb-8e23b9c76915",
  reasoning_effort: "high",
  service_tier: "auto",
  ...extra,
});

test("ledger rows keep usage facts and drop credentials, addresses, error bodies and other headers", () => {
  const row = ledgerRow(record({ failed: true, fail: { status_code: 529, body: "overloaded: prompt text here" } }), new Map([["aaaa", "claude-1-todd.json"]]));
  assert.equal(row.at, "2026-10-15T13:47:34.972Z");
  assert.equal(row.acct, "claude-1-todd.json");
  assert.equal(row.ai, undefined);
  assert.equal(row.client, "claude-cli");
  assert.deepEqual([row.in, row.cr, row.cw, row.out, row.rsn], [2, 78668, 1185, 666, 108]);
  assert.equal(row.ok, false);
  assert.equal(row.st, 529);
  assert.deepEqual(row.quota.weekly, { used: 77, resetAt: 1792288800000 });
  assert.deepEqual(row.quota.short, { used: 58, resetAt: 1792072800000 });
  const text = JSON.stringify(row);
  for (const secret of ["k".repeat(64), "deadbeef", "127.0.0.1", "10.0.0.1", "prompt text", "req_secretish", "todd@example.com"]) assert.ok(!text.includes(secret), secret);
});

test("an unknown auth index is kept for later attribution; unparseable records are rejected", () => {
  const row = ledgerRow(JSON.stringify(record({ auth_index: "zzzz" })), new Map());
  assert.equal(row.acct, null);
  assert.equal(row.ai, "zzzz");
  assert.equal(ledgerRow("{not json"), null);
  assert.equal(ledgerRow({ timestamp: "never" }), null);
});

test("Codex headers: the 10080-minute primary window is the weekly quota; opaque headers are not kept", () => {
  const q = quotaFromHeaders("codex", { "X-Codex-Primary-Used-Percent": ["11"], "X-Codex-Primary-Window-Minutes": ["10080"], "X-Codex-Primary-Reset-At": ["1792649818"], "X-Codex-Secondary-Used-Percent": ["0"], "X-Codex-Secondary-Window-Minutes": ["0"], "X-Codex-Turn-State": ["gAAAA"] });
  assert.deepEqual(q, { weekly: { used: 11, resetAt: 1792649818000 } });
  assert.equal(quotaFromHeaders("codex", {}), null);
  assert.equal(quotaFromHeaders("claude", undefined), null);
});

test("token fallback: OpenAI input includes cached tokens, Anthropic input does not", () => {
  assert.deepEqual(tokensOf({ provider: "codex", tokens: { input_tokens: 1000, cached_tokens: 800, output_tokens: 50 } }), { in: 200, cr: 800, cw: 0, out: 50, rsn: 0, q: "fallback" });
  assert.deepEqual(tokensOf({ provider: "claude", tokens: { input_tokens: 5, cache_read_tokens: 800, cache_creation_tokens: 10, output_tokens: 50 } }), { in: 5, cr: 800, cw: 10, out: 50, rsn: 0, q: "fallback" });
  assert.equal(clientOf("codex_exec/0.162.1 (Windows)"), "codex_exec");
  assert.equal(clientOf(""), "unknown");
});

test("config validation falls back to defaults", () => {
  assert.equal(ledgerConfig({ drainIntervalSeconds: 1 }).config.drainIntervalSeconds, 10);
  assert.match(ledgerConfig({ drainIntervalSeconds: 1 }).problems[0], /drainIntervalSeconds/);
  assert.equal(ledgerConfig({ enabled: false }).config.enabled, false);
});

async function tempRoot() { return mkdtemp(path.join(os.tmpdir(), "usage-ledger-")); }

test("drain pops every batch, writes rows by UTC day, and records nothing it cannot parse", async () => {
  const root = await tempRoot();
  try {
    const queue = [record(), record({ timestamp: "2026-10-15T23:59:59Z" }), record({ timestamp: "2026-10-16T00:00:01Z" }), "{bad"];
    const calls = [];
    const ledger = createUsageLedger({
      root, batch: 2, now: () => Date.parse("2026-10-16T00:00:05Z"),
      popQueue: async (count) => { calls.push(count); return queue.splice(0, count); },
      accounts: () => [{ id: "claude-1.json", auth_index: "aaaa" }],
    });
    const stats = await ledger.drain();
    assert.deepEqual(calls, [2, 2, 2]);
    assert.equal(stats.rows, 3);
    assert.equal(stats.dropped, 1);
    const day1 = (await readFile(path.join(root, LEDGER_DIR, "raw-2026-10-15.jsonl"), "utf8")).trim().split("\n");
    const day2 = (await readFile(path.join(root, LEDGER_DIR, "raw-2026-10-16.jsonl"), "utf8")).trim().split("\n");
    assert.equal(day1.length, 2);
    assert.equal(day2.length, 1);
    assert.equal(JSON.parse(day2[0]).acct, "claude-1.json");
  } finally { await rm(root, { recursive: true, force: true }); }
});

test("a failed write keeps popped rows and writes them on the next drain", async () => {
  const root = await tempRoot();
  try {
    // A file where the ledger directory should be makes mkdir fail.
    await mkdir(path.join(root, "logs"), { recursive: true });
    await writeFile(path.join(root, LEDGER_DIR), "blocker");
    const events = [];
    let queue = [record()];
    const ledger = createUsageLedger({ root, popQueue: async (n) => queue.splice(0, n), accounts: () => [], event: async (t, d) => events.push([t, d]) });
    let stats = await ledger.drain();
    assert.equal(stats.pending, 1);
    assert.equal(events[0][0], "usage-ledger-problem");
    await rm(path.join(root, LEDGER_DIR));
    stats = await ledger.drain();
    assert.equal(stats.pending, 0);
    assert.equal(stats.rows, 1);
    assert.equal((await readdir(path.join(root, LEDGER_DIR))).length, 1);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test("disabled ledger leaves the queue alone; old raw files are pruned; drains are single flight", async () => {
  const root = await tempRoot();
  try {
    let pops = 0;
    const disabled = createUsageLedger({ root, popQueue: async () => { pops++; return []; }, accounts: () => [], readConfig: async () => ({ usageLedger: { enabled: false } }) });
    await disabled.drain();
    assert.equal(pops, 0);

    await mkdir(path.join(root, LEDGER_DIR), { recursive: true });
    await writeFile(path.join(root, LEDGER_DIR, "raw-2026-01-01.jsonl"), "{}\n");
    await writeFile(path.join(root, LEDGER_DIR, "raw-2026-10-14.jsonl"), "{}\n");
    await writeFile(path.join(root, LEDGER_DIR, "notes.txt"), "keep");
    let release;
    const gate = new Promise((r) => (release = r));
    const ledger = createUsageLedger({ root, now: () => Date.parse("2026-10-15T12:00:00Z"), popQueue: async () => { pops++; await gate; return []; }, accounts: () => [] });
    const a = ledger.drain(), b = ledger.drain();
    release();
    await Promise.all([a, b]);
    assert.equal(pops, 1);
    assert.deepEqual((await readdir(path.join(root, LEDGER_DIR))).sort(), ["notes.txt", "raw-2026-10-14.jsonl"]);
  } finally { await rm(root, { recursive: true, force: true }); }
});