import { appendFile, mkdir, readdir, unlink } from "node:fs/promises";
import path from "node:path";

// Usage ledger: one compact row per proxied request, drained from the proxy's
// per-request usage queue (GET /v0/management/usage-queue, CLIProxyAPI
// redisqueue). Reading the queue removes the records, so this must be the only
// reader, and records expire in the proxy after redis-usage-queue-retention-
// seconds (default 60), so it is drained every few seconds.
//
// Rows keep only what usage-value.mjs needs: time, account, model, client,
// session, effort, service tier, outcome, token counts by type, and the quota
// the provider reported in that response's rate-limit headers. The queue
// record's api_key, access token hash, client IP, forwarded-for chain, error
// body and every other response header (Codex sends an opaque turn-state
// token) are dropped and never written.

export const LEDGER_DIR = "logs/usage-ledger";
export const LEDGER_FILE = /^raw-(\d{4}-\d{2}-\d{2})\.jsonl$/;
export const DEFAULTS = Object.freeze({
  enabled: true,
  drainIntervalSeconds: 10,
  rawRetentionDays: 35,
  // Rows kept in memory while the ledger file cannot be written. The queue
  // records are already popped, so they are held rather than dropped.
  maxPendingRows: 100000,
});

export function ledgerConfig(input) {
  const config = { ...DEFAULTS };
  const problems = [];
  if (input === undefined || input === null) return { config, problems };
  if (typeof input !== "object") return { config, problems: ["usageLedger must be an object"] };
  if ("enabled" in input) {
    if (typeof input.enabled === "boolean") config.enabled = input.enabled;
    else problems.push("usageLedger.enabled must be true or false");
  }
  for (const [key, min, max] of [["drainIntervalSeconds", 2, 50], ["rawRetentionDays", 1, 400]]) {
    if (!(key in input)) continue;
    const v = input[key];
    if (Number.isInteger(v) && v >= min && v <= max) config[key] = v;
    else problems.push(`usageLedger.${key} must be an integer from ${min} to ${max}`);
  }
  return { config, problems };
}

// Case-insensitive header read; Go's http.Header marshals values as arrays.
function header(headers, name) {
  if (!headers || typeof headers !== "object") return undefined;
  const wanted = name.toLowerCase();
  for (const [k, v] of Object.entries(headers)) {
    if (k.toLowerCase() !== wanted) continue;
    const value = Array.isArray(v) ? v[0] : v;
    return typeof value === "string" ? value.trim() : undefined;
  }
  return undefined;
}
const num = (v) => {
  if (v === undefined || v === "") return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
};
// Epoch seconds (as sent in both providers' headers) to milliseconds.
const epochMs = (v) => {
  const n = num(v);
  return n !== null && n > 0 ? Math.round(n * 1000) : null;
};

// The quota the provider reported with this response, as used percentages
// (0-100) and reset times (ms). Claude's unified 7d window is the weekly
// all-models limit (limit:1 in usage reads) and 5h the short limiter; Codex's
// primary window is weekly when its length is 10080 minutes.
export function quotaFromHeaders(provider, headers) {
  if (provider === "claude") {
    const weekly = num(header(headers, "anthropic-ratelimit-unified-7d-utilization"));
    const short = num(header(headers, "anthropic-ratelimit-unified-5h-utilization"));
    const out = {};
    if (weekly !== null) out.weekly = { used: Math.round(weekly * 10000) / 100, resetAt: epochMs(header(headers, "anthropic-ratelimit-unified-7d-reset")) };
    if (short !== null) out.short = { used: Math.round(short * 10000) / 100, resetAt: epochMs(header(headers, "anthropic-ratelimit-unified-5h-reset")) };
    return Object.keys(out).length ? out : null;
  }
  if (provider === "codex") {
    const out = {};
    for (const [slot, prefix] of [["primary", "x-codex-primary"], ["secondary", "x-codex-secondary"]]) {
      const used = num(header(headers, prefix + "-used-percent"));
      const minutes = num(header(headers, prefix + "-window-minutes"));
      if (used === null || !minutes) continue;
      const reading = { used, resetAt: epochMs(header(headers, prefix + "-reset-at")) };
      if (minutes === 10080) out.weekly = reading;
      else if (minutes < 10080) out.short = reading;
    }
    return Object.keys(out).length ? out : null;
  }
  return null;
}

const int = (v) => (Number.isFinite(Number(v)) && Number(v) > 0 ? Math.round(Number(v)) : 0);

// Token counts by billing type. The proxy's v2 breakdown is authoritative
// (input split into uncached / cache read / cache write; output includes
// reasoning); without it, OpenAI-style input_tokens include cached tokens and
// Anthropic-style ones do not.
export function tokensOf(record) {
  const b = record?.token_breakdown;
  if (b && b.input && b.output) {
    return {
      in: int(b.input.uncached_tokens), cr: int(b.input.cache_read_tokens), cw: int(b.input.cache_write_tokens),
      out: int(b.output.total_tokens), rsn: int(b.output.reasoning_tokens), q: String(b.quality ?? "unknown"),
    };
  }
  const t = record?.tokens ?? {};
  const cached = int(t.cache_read_tokens || t.cached_tokens);
  const openai = record?.provider === "codex";
  return {
    in: Math.max(0, int(t.input_tokens) - (openai ? cached : 0)), cr: cached, cw: int(t.cache_creation_tokens),
    out: int(t.output_tokens), rsn: int(t.reasoning_tokens), q: "fallback",
  };
}

// The client product from its User-Agent ("claude-cli", "codex_exec",
// "Codex Desktop", ...), without version or platform detail.
export function clientOf(userAgent) {
  const s = String(userAgent ?? "").trim();
  if (!s) return "unknown";
  const name = s.split("/")[0].trim();
  return name.slice(0, 40) || "unknown";
}

// Queue record (object or JSON text) to ledger row, or null when unusable.
// accountsByAuthIndex maps the proxy's auth_index to the pool account id.
export function ledgerRow(raw, accountsByAuthIndex = new Map()) {
  let r = raw;
  if (typeof r === "string") { try { r = JSON.parse(r); } catch { return null; } }
  if (!r || typeof r !== "object") return null;
  const at = Date.parse(r.timestamp);
  if (!Number.isFinite(at)) return null;
  const provider = String(r.provider ?? "unknown");
  const authIndex = String(r.auth_index ?? "");
  const tier = String(r.response_service_tier || r.service_tier || "");
  const row = {
    at: new Date(at).toISOString(),
    prov: provider,
    model: String(r.model ?? "unknown"),
    acct: accountsByAuthIndex.get(authIndex) ?? null,
    ...(accountsByAuthIndex.has(authIndex) ? {} : { ai: authIndex || null }),
    ep: String(r.endpoint ?? ""),
    client: clientOf(r.user_agent),
    sid: r.session_id || null,
    ...(r.parent_session_id ? { psid: r.parent_session_id } : {}),
    eff: r.reasoning_effort || null,
    tier: tier || null,
    ok: !r.failed,
    st: Number(r.fail?.status_code) || (r.failed ? 500 : 200),
    lat: Number(r.latency_ms) || 0,
    ...tokensOf(r),
    rid: r.request_id || null,
  };
  const quota = quotaFromHeaders(provider, r.response_headers);
  if (quota) row.quota = quota;
  return row;
}

// Drains the proxy queue into logs/usage-ledger/raw-YYYY-MM-DD.jsonl (UTC day
// of each request). popQueue(count) returns the next records (removing them);
// accounts() returns the current pool accounts (id, auth_index).
export function createUsageLedger({ root, popQueue, accounts, readConfig = async () => ({}), event = async () => {}, now = Date.now, batch = 1000, maxBatchesPerDrain = 50 }) {
  const dir = path.join(root, LEDGER_DIR);
  let pending = [];
  let draining = null;
  let lastProblem = "";
  let lastPruneDay = null;
  let config = { ...DEFAULTS };
  let stats = { drainedAt: null, rows: 0, dropped: 0, pending: 0, lastError: null };

  async function problem(message, details = {}) {
    stats.lastError = message || null;
    if (message === lastProblem) return;
    lastProblem = message;
    if (message) await event("usage-ledger-problem", { problem: message, ...details }).catch(() => {});
  }
  async function write(rows) {
    if (!rows.length) return;
    await mkdir(dir, { recursive: true });
    const byDay = new Map();
    for (const row of rows) {
      const day = row.at.slice(0, 10);
      if (!byDay.has(day)) byDay.set(day, []);
      byDay.get(day).push(JSON.stringify(row));
    }
    for (const [day, lines] of byDay) await appendFile(path.join(dir, `raw-${day}.jsonl`), lines.join("\n") + "\n");
  }
  async function prune() {
    const today = new Date(now()).toISOString().slice(0, 10);
    if (lastPruneDay === today) return;
    lastPruneDay = today;
    const cutoff = new Date(Date.parse(today) - config.rawRetentionDays * 86400000).toISOString().slice(0, 10);
    let names = [];
    try { names = await readdir(dir); } catch { return; }
    for (const name of names) {
      const m = LEDGER_FILE.exec(name);
      if (m && m[1] < cutoff) await unlink(path.join(dir, name)).catch(() => {});
    }
  }
  async function drainOnce() {
    let install = {};
    try { install = (await readConfig()) ?? {}; } catch {}
    const parsed = ledgerConfig(install.usageLedger);
    config = parsed.config;
    // Disabled: leave the queue alone (records expire in the proxy).
    if (!config.enabled) { await problem(parsed.problems.join("; ")); return stats; }
    const map = new Map((accounts() ?? []).filter((a) => a?.auth_index).map((a) => [String(a.auth_index), a.id]));
    let readError = null;
    for (let i = 0; i < maxBatchesPerDrain; i++) {
      let items;
      try { items = await popQueue(batch); } catch (e) { readError = e.message; break; }
      if (!Array.isArray(items) || !items.length) break;
      for (const item of items) {
        const row = ledgerRow(item, map);
        if (row) pending.push(row); else stats.dropped++;
      }
      if (items.length < batch) break;
    }
    if (pending.length > config.maxPendingRows) {
      stats.dropped += pending.length - config.maxPendingRows;
      pending = pending.slice(pending.length - config.maxPendingRows);
    }
    let writeError = null;
    if (pending.length) {
      const rows = pending;
      try { await write(rows); pending = pending.slice(rows.length); stats.rows += rows.length; }
      catch (e) { writeError = e.message; }
    }
    await prune().catch(() => {});
    stats = { ...stats, drainedAt: new Date(now()).toISOString(), pending: pending.length };
    const message = [...parsed.problems, readError && "queue read failed: " + readError, writeError && "write failed: " + writeError].filter(Boolean).join("; ");
    await problem(message, writeError ? { pending: pending.length } : {});
    return stats;
  }
  // Single flight: overlapping ticks share one drain.
  function drain() {
    if (!draining) draining = drainOnce().finally(() => { draining = null; });
    return draining;
  }
  return { drain, stats: () => ({ ...stats, enabled: config.enabled }), intervalMs: () => config.drainIntervalSeconds * 1000 };
}