// Read-only: runs Agent Pools' own poolUsage (reference/agent-pools at 5c696f6) over a root's
// real accounts (state/pool-state.json, each through currentAccount as pool-service.mjs passes
// them), in every mode, against upstream bodies that do and do not name a pooled account, and
// writes the cases to shadow-state/usage-live.json (gitignored: accounts are not scrubbed). Then:
// $env:AP_USAGE_LIVE = 'shadow-state/usage-live.json'; cargo test -p pool-core --test pool_usage
//   node golden/usage-live.mjs [root, default the run-copy copy] [now, ISO]
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { currentAccount } from "../reference/agent-pools/freshness.mjs";
import { poolUsage, POOL_MODES } from "../reference/agent-pools/pool-usage.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = process.argv[2] ?? path.join(here, "../shadow-state/copy");
const state = JSON.parse(fs.readFileSync(path.join(root, "state/pool-state.json"), "utf8").replace(/^\uFEFF/, ""));
const raw = state.accounts ?? [];
// Just after the newest read, so fresh accounts stay fresh; or the given time.
const newest = Math.max(...raw.map((a) => Date.parse(a.usageUpdatedAt)).filter(Number.isFinite));
const nows = process.argv[3] ? [Date.parse(process.argv[3])] : [newest + 60_000, newest + 20 * 60_000, newest + 45 * 60_000];
const signedIn = raw.find((a) => a.provider === "codex" && typeof a.accountId === "string");
const window = (used, seconds, resetAt) => ({ used_percent: used, limit_window_seconds: seconds, reset_after_seconds: 60, reset_at: resetAt });
const upstreams = (now) => [
  { name: "plain", body: { plan_type: "pro", rate_limit: { allowed: true, limit_reached: false, primary_window: window(3, 604800, Math.round(now / 1000) + 86400), secondary_window: null }, additional_rate_limits: [], credits: { has_credits: false } } },
  ...(signedIn ? [
    { name: "signed-in available", body: { account_id: signedIn.accountId, plan_type: "pro", rate_limit: { allowed: true, limit_reached: false, primary_window: window(41, 604800, Math.round(now / 1000) + 3600), secondary_window: window(7, 18000, Math.round(now / 1000) + 600) } } },
    { name: "signed-in exhausted", body: { account_id: signedIn.accountId, rate_limit: { allowed: false, limit_reached: true, primary_window: window(100, 604800, Math.round(now / 1000) + 7200), secondary_window: null }, model_usage: { x: { available: false } } } },
  ] : []),
];
const cases = [];
for (const now of nows)
  for (const { name, body } of upstreams(now))
    for (const mode of POOL_MODES)
      for (const includeClaude of [true, false]) {
        const accounts = raw.map((a) => currentAccount(a, now));
        const out = poolUsage(body, accounts, { mode, now, includeClaude });
        cases.push({ at: `live ${new Date(now).toISOString()} ${name} ${mode}${includeClaude ? "" : " codex-only"}`, upstream: body, rawAccounts: raw, mode, includeClaude, now, output: JSON.parse(JSON.stringify(out)), same: out === body });
      }
const out = path.join(here, "../shadow-state/usage-live.json");
fs.writeFileSync(out, JSON.stringify({ format: "pool-usage-cases/v1", source: root, cases }));
const changed = cases.filter((c) => !c.same).length;
console.log(`wrote ${out}: ${raw.length} accounts, ${cases.length} cases (${changed} rewritten, ${cases.length - changed} unchanged)`);
