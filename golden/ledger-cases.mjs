// Runs Agent Pools' own usage-ledger.mjs (reference/agent-pools at 5c696f6) over a fixed set of
// queue records and configs (the test's record and variations of every field the row reads) and
// writes inputs and outputs to golden/outputs/ledger/cases.json, for pool-core::ledger.
//   node golden/ledger-cases.mjs
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { ledgerRow, quotaFromHeaders, tokensOf, clientOf, ledgerConfig } from "../reference/agent-pools/usage-ledger.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const base = {
  timestamp: "2026-10-15T08:47:34.9727204-05:00", latency_ms: 6417, source: "todd@example.com", auth_index: "aaaa",
  access_token_sha256: "deadbeef", client_ip: "127.0.0.1", x_forwarded_for: "10.0.0.1", user_agent: "claude-cli/2.1.299 (external, sdk-cli)",
  tokens: { input_tokens: 2, output_tokens: 666, reasoning_tokens: 108, cached_tokens: 78668, cache_read_tokens: 78668, cache_creation_tokens: 1185, total_tokens: 80521 },
  failed: false, fail: { status_code: 200, body: "" },
  response_headers: { "Anthropic-Ratelimit-Unified-7d-Utilization": ["0.77"], "Anthropic-Ratelimit-Unified-7d-Reset": ["1792288800"], "Anthropic-Ratelimit-Unified-5h-Utilization": ["0.58"], "Anthropic-Ratelimit-Unified-5h-Reset": ["1792072800"], "Request-Id": ["req_secretish"] },
  token_breakdown: { schema_version: 2, quality: "complete", input: { uncached_tokens: 2, cache_read_tokens: 78668, cache_write_tokens: 1185 }, output: { total_tokens: 666, reasoning_tokens: 108 } },
  provider: "claude", model: "claude-opus-5-5", endpoint: "POST /v1/messages", api_key: "k".repeat(64), request_id: "8241b157",
  session_id: "45dbe21a-ce8c-4955-9efb-8e23b9c76915", reasoning_effort: "high", service_tier: "auto",
};
const codexHeaders = { "X-Codex-Primary-Used-Percent": ["11"], "X-Codex-Primary-Window-Minutes": ["10080"], "X-Codex-Primary-Reset-At": ["1792649818"], "X-Codex-Secondary-Used-Percent": ["37.5"], "X-Codex-Secondary-Window-Minutes": ["300"], "X-Codex-Secondary-Reset-At": ["1792072800.5"], "X-Codex-Turn-State": ["gAAAA"] };
const codex = { ...base, provider: "codex", model: "gpt-6-astra", endpoint: "POST /v1/responses", user_agent: "codex_exec/0.162.1 (Windows 10.0.26100; x86_64)", response_headers: codexHeaders, token_breakdown: undefined, tokens: { input_tokens: 1000, cached_tokens: 800, output_tokens: 50, reasoning_tokens: 20 } };
const variants = [
  {}, { failed: true, fail: { status_code: 529, body: "overloaded" } }, { failed: true, fail: undefined }, { failed: true, fail: { status_code: "429" } }, { failed: 0 },
  { auth_index: "zzzz" }, { auth_index: "" }, { auth_index: undefined }, { auth_index: 7 },
  { timestamp: "2026-10-15T23:59:59Z" }, { timestamp: "2026-10-16T00:00:01.5+02:00" }, { timestamp: "2026-10-15" }, { timestamp: "never" }, { timestamp: undefined }, { timestamp: "2026-10-15T08:47:34.123456789Z" },
  { user_agent: "" }, { user_agent: undefined }, { user_agent: "Codex Desktop/26.1016 (Windows)" }, { user_agent: "   spaced   /1" }, { user_agent: "x".repeat(60) + "/1" }, { user_agent: "/leading" },
  { session_id: "" }, { session_id: undefined }, { parent_session_id: "p-1" }, { reasoning_effort: "" }, { service_tier: "", response_service_tier: "priority" }, { service_tier: undefined },
  { model: undefined }, { endpoint: undefined }, { provider: undefined }, { provider: "gemini" }, { latency_ms: "12" }, { latency_ms: "x" }, { request_id: "" },
  { token_breakdown: { input: { uncached_tokens: "5", cache_read_tokens: -3, cache_write_tokens: 2.6 }, output: { total_tokens: 9, reasoning_tokens: null } } },
  { token_breakdown: { input: {}, output: {}, quality: null } }, { token_breakdown: { input: { uncached_tokens: 1 } } }, { token_breakdown: undefined },
  { token_breakdown: undefined, tokens: { input_tokens: 5, cache_read_tokens: 800, cache_creation_tokens: 10, output_tokens: 50 } },
  { token_breakdown: undefined, tokens: undefined }, { token_breakdown: undefined, tokens: { input_tokens: "7", cached_tokens: 0, cache_read_tokens: 3 } },
  { response_headers: undefined }, { response_headers: {} }, { response_headers: { "anthropic-ratelimit-unified-7d-utilization": "0.123456" } },
  { response_headers: { "Anthropic-Ratelimit-Unified-5h-Utilization": [" 0.5 "], "Anthropic-Ratelimit-Unified-5h-Reset": ["0"] } },
  { response_headers: { "Anthropic-Ratelimit-Unified-7d-Utilization": [""], "Anthropic-Ratelimit-Unified-5h-Utilization": ["abc"] } },
  { response_headers: { "Anthropic-Ratelimit-Unified-7d-Utilization": [1], "Anthropic-Ratelimit-Unified-7d-Reset": ["-5"] } },
  { response_headers: { "Anthropic-Ratelimit-Unified-7d-Utilization": ["0.001"], "Anthropic-Ratelimit-Unified-7d-Reset": ["1792288800.4"] } },
];
const codexVariants = [
  {}, { response_headers: { ...codexHeaders, "X-Codex-Primary-Window-Minutes": ["300"], "X-Codex-Secondary-Window-Minutes": ["10080"] } },
  { response_headers: { "X-Codex-Primary-Used-Percent": ["11"], "X-Codex-Primary-Window-Minutes": ["20000"] } },
  { response_headers: { "X-Codex-Primary-Used-Percent": ["11"] } }, { response_headers: { "x-codex-primary-used-percent": "0", "x-codex-primary-window-minutes": "10080" } },
  { tokens: { input_tokens: 100, cached_tokens: 800, output_tokens: 1 } }, { tokens: { input_tokens: 100, cache_read_tokens: 40, cached_tokens: 60 } },
  { token_breakdown: { quality: "partial", input: { uncached_tokens: 3, cache_read_tokens: 4, cache_write_tokens: 0 }, output: { total_tokens: 8, reasoning_tokens: 2 } } },
];
const maps = [[["aaaa", "claude-1-todd.json"]], [], [["7", "seven.json"]], [["", "empty.json"]]];
const rowCases = [];
const strip = (o) => JSON.parse(JSON.stringify(o));
for (const [list, b] of [[variants, base], [codexVariants, codex]])
  for (const v of list)
    for (const m of maps) {
      const record = { ...b, ...v };
      for (const k of Object.keys(v)) if (v[k] === undefined) delete record[k];
      rowCases.push({ record: strip(record), map: m, output: ledgerRow(record, new Map(m)) });
    }
for (const text of ["{not json", "null", "5", "[]", JSON.stringify({ ...base, auth_index: "aaaa" }), JSON.stringify({ timestamp: "2026-10-15T00:00:00Z" })])
  rowCases.push({ record: text, map: maps[0], output: ledgerRow(text, new Map(maps[0])) });
const configCases = [undefined, null, 5, "x", {}, { enabled: false }, { enabled: "no" }, { drainIntervalSeconds: 1 }, { drainIntervalSeconds: 2 }, { drainIntervalSeconds: 50 }, { drainIntervalSeconds: 51 }, { drainIntervalSeconds: 10.5 }, { drainIntervalSeconds: "10" }, { rawRetentionDays: 400 }, { rawRetentionDays: 0 }, { rawRetentionDays: 35, drainIntervalSeconds: 3, enabled: true }, { enabled: false, rawRetentionDays: 401 }, []]
  .map((input) => ({ input: input === undefined ? "__undefined__" : input, output: ledgerConfig(input) }));
const clientCases = ["", "  ", "claude-cli/2.1", "a/b/c", " Codex Desktop /1", "x".repeat(50), "\u00e9t\u00e9/1"].map((ua) => ({ ua, output: clientOf(ua) }));
const out = path.join(here, "outputs/ledger");
fs.mkdirSync(out, { recursive: true });
fs.writeFileSync(path.join(out, "cases.json"), JSON.stringify({ format: "ledger-cases/v1", source: "reference/agent-pools/usage-ledger.mjs", rows: rowCases, configs: configCases, clients: clientCases }, null, 1) + "\n");
console.log(`${rowCases.length} rows (${rowCases.filter((c) => c.output === null).length} rejected), ${configCases.length} configs, ${clientCases.length} clients`);
