// Read-only: runs Agent Pools' own buildLadders (reference/agent-pools at 5c696f6) over the
// live install's current ladder inputs, as capacity-publisher.mjs loads them, at a fixed now,
// and writes inputs and result to shadow-state/ladder-live.json (gitignored: the attempts and
// registry are not scrubbed). Then: $env:AP_LADDER_LIVE = 'shadow-state/ladder-live.json';
// cargo test -p pool-core --test ladder
//   node golden/ladder-live.mjs [install root, default D:\Users\ToddS\AgentPools] [now, ISO]
// With a now, the result is also compared with that root's state/routing-ladders.json (the
// port's own on a run-copy copy, built at that moment).
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { loadInputs } from "../reference/agent-pools/model-calibrate.mjs";
import { buildLadders } from "../reference/agent-pools/routing-ladder.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = process.argv[2] ?? "D:\\Users\\ToddS\\AgentPools";
const read = (rel, fallback) => {
  try { return JSON.parse(fs.readFileSync(path.join(root, rel), "utf8").replace(/^\uFEFF/, "")); }
  catch (e) { if (fallback !== undefined && e.code === "ENOENT") return fallback; throw e; }
};
const base = await loadInputs(root);
const inputs = { ...base, value: read("state/usage-value.json"), attempts: read("state/routing-attempts.json", { attempts: [] }),
  sessionUsage: read("state/session-usage.json", null), forecast: read("state/capacity-forecast.json") };
// The forecast's pace is fresh for 15 minutes after it is computed; take now just after it.
const now = process.argv[3] ? Date.parse(process.argv[3]) : Date.parse(inputs.forecast.generatedAt) + 60000;
const output = JSON.parse(JSON.stringify(buildLadders(inputs, now)));
delete inputs.policy; // loaded by loadInputs, never read by buildLadders
const out = path.join(here, "../shadow-state/ladder-live.json");
fs.mkdirSync(path.dirname(out), { recursive: true });
fs.writeFileSync(out, JSON.stringify({ format: "ladder-cases/v1", source: root, cases: [{ at: "live", now, inputs, output }] }));
const rungs = Object.values(output.rows).reduce((n, r) => n + r.rungs.length, 0);
console.log(`wrote ${out}: now ${new Date(now).toISOString()}, costBasis ${output.costBasis}, ${rungs} rungs, digest ${output.digest.slice(0, 16)}`);
if (process.argv[3]) {
  const published = read("state/routing-ladders.json");
  const same = JSON.stringify(published) === JSON.stringify(output);
  console.log(`${root} state/routing-ladders.json: digest ${published.digest.slice(0, 16)}, ${same ? "identical" : "DIFFERENT"}`);
  if (!same) process.exitCode = 1;
}
