// Stands in for model-registry.mjs, model-refresh.mjs and model-admission.mjs under
// registry-hook.mjs: calls the real functions and keeps each call's arguments (as JSON) and
// result or error message. Defaults that read the clock are filled in, so a replay needs none.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import * as registry from "../reference/agent-pools/model-registry.mjs?real";
import * as refresh from "../reference/agent-pools/model-refresh.mjs?real";
import * as admission from "../reference/agent-pools/model-admission.mjs?real";

const here = path.dirname(fileURLToPath(import.meta.url));
if (process.env.TZ !== "UTC") throw new Error("run with TZ=UTC (poolRouting parses local times)");
const cases = [];
const caller = () => {
  const line = new Error().stack.split("\n").find((l) => l.includes("model-registry.test.mjs"));
  return line ? line.trim().replace(/^at\s+/, "").replace(/.*model-registry\.test\.mjs:/, "test.mjs:").replace(/\)$/, "") : null;
};
// A Set (wantedEfforts' values) as the array of its members, in order.
const json = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v, (_, x) => (x instanceof Set ? [...x] : x))));

function recorded(name, real, fill = (args) => args) {
  return (...given) => {
    const args = fill(given);
    const record = { fn: name, at: caller(), args: args.map(json) };
    cases.push(record);
    try {
      const out = real(...args);
      record.output = json(out);
      return out;
    } catch (e) {
      record.error = e.message;
      throw e;
    }
  };
}

export const REGISTRY_FORMAT = registry.REGISTRY_FORMAT, LINEAGE_FORMAT = registry.LINEAGE_FORMAT, EFFORTS = registry.EFFORTS, SAFE_ID = registry.SAFE_ID;
export const emptyLineage = registry.emptyLineage;
export const parseVersion = recorded("parseVersion", registry.parseVersion);
export const compareVersions = recorded("compareVersions", registry.compareVersions);
export const admittedEfforts = recorded("admittedEfforts", registry.admittedEfforts);
export const buildRegistry = recorded("buildRegistry", registry.buildRegistry, ([o]) => [{ ...o, now: o.now ?? Date.now() }]);
export const judgeClaude = recorded("judgeClaude", admission.judgeClaude);
export const judgeCodex = recorded("judgeCodex", admission.judgeCodex);
export const poolRouting = recorded("poolRouting", admission.poolRouting);
export const wantedEfforts = recorded("wantedEfforts", refresh.wantedEfforts);
export const probePlan = recorded("probePlan", refresh.probePlan, (a) => (a.length < 6 ? [...a, ...Array(5 - a.length).fill(undefined), {}] : a));
export const fairBatch = recorded("fairBatch", refresh.fairBatch, (a) => (a.length < 4 ? [...a, ...Array(3 - a.length).fill(undefined), Date.now()] : a));
export const degraded = recorded("degraded", refresh.degraded);
export const recheck = refresh.recheck;

process.on("exit", () => {
  const dir = path.join(here, "outputs/registry");
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, "cases.json"), JSON.stringify({ format: "registry-cases/v1", source: "reference/agent-pools/model-registry.test.mjs (Agent Pools 5c696f6)", cases }) + "\n");
});
