// Read-only against a run-copy copy: asks the copy's desktop relay for /backend-api/wham/usage
// (the port's backend relay and overlay over the copy's accounts) and compares the bytes with
// Agent Pools' own poolUsage (reference/agent-pools at 5c696f6) on the same upstream body (the
// stand-in's, fetched directly), the same saved accounts and the same moment.
//   node golden/usage-relay.mjs [copy root] [relay origin, default http://127.0.0.1:8322]
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { currentAccount } from "../reference/agent-pools/freshness.mjs";
import { poolUsage, POOL_MODES } from "../reference/agent-pools/pool-usage.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = process.argv[2] ?? path.join(here, "../shadow-state/copy");
const relay = process.argv[3] ?? "http://127.0.0.1:8322";
const log = fs.readFileSync(path.join(here, "../shadow-state/copy-run.log"), "utf8");
const standIn = log.match(/stand-in proxy at (http:\/\/127\.0\.0\.1:\d+)/)[1];
const read = (rel) => JSON.parse(fs.readFileSync(path.join(root, rel), "utf8").replace(/^\uFEFF/, ""));
const install = read("state/install.json");
const mode = POOL_MODES.includes(install.poolUsageMode) ? install.poolUsageMode : POOL_MODES[0];

const ids = [null, ...read("state/pool-state.json").accounts.filter((a) => a.provider === "codex" && typeof a.accountId === "string").map((a) => a.accountId)];
let failed = 0;
for (const id of ids) {
  const headers = id ? { "chatgpt-account-id": id } : {};
  let result;
  for (let attempt = 0; attempt < 5 && !result; attempt++) {
    const before = await (await fetch(standIn + "/backend-api/wham/usage", { headers })).text();
    const stateBefore = fs.readFileSync(path.join(root, "state/pool-state.json"), "utf8");
    const t0 = Date.now();
    const response = await fetch(relay + "/backend-api/wham/usage", { headers });
    const text = await response.text();
    const t1 = Date.now();
    const after = await (await fetch(standIn + "/backend-api/wham/usage", { headers })).text();
    const stateAfter = fs.readFileSync(path.join(root, "state/pool-state.json"), "utf8");
    // The stand-in's use drifts and the writer saves every cycle; try again if either moved.
    if (before !== after || stateBefore !== stateAfter) continue;
    const upstream = JSON.parse(before);
    const accounts = JSON.parse(stateBefore.replace(/^\uFEFF/, "")).accounts;
    let match = null;
    for (let now = t0; now <= t1 && !match; now++) {
      const out = poolUsage(upstream, accounts.map((a) => currentAccount(a, now)), { mode, now });
      if (JSON.stringify(out) === text) match = now;
    }
    const expected = JSON.stringify(poolUsage(upstream, accounts.map((a) => currentAccount(a, t0)), { mode, now: t0 }));
    result = { status: response.status, bytes: text.length, match, window: t1 - t0, expected, text, rewritten: text !== JSON.stringify(upstream) };
  }
  if (!result) { console.log(`${id ?? "(no account id)"}: the inputs kept moving; not compared`); failed++; continue; }
  const ok = result.status === 200 && result.match !== null;
  if (!ok) failed++;
  console.log(`${id ? id.slice(0, 8) + "…" : "(no account id)"}: HTTP ${result.status}, ${result.bytes} bytes, rewritten ${result.rewritten}, ${ok ? `identical to poolUsage at a moment within the ${result.window} ms request` : "DIFFERENT"}`);
  if (!ok) { console.log(" relay:   " + result.text.slice(0, 400)); console.log(" agent pools: " + result.expected.slice(0, 400)); }
}
console.log(`mode ${mode}; ${ids.length - failed} of ${ids.length} identical`);
if (failed) process.exitCode = 1;
