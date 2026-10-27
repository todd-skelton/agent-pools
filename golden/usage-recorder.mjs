// Stands in for pool-usage.mjs under usage-hook.mjs: calls the real poolUsage and keeps each
// call's inputs (as JSON, the form the port reads) and result or error message.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import * as real from "../reference/agent-pools/pool-usage.mjs?real";

export * from "../reference/agent-pools/pool-usage.mjs?real";

const here = path.dirname(fileURLToPath(import.meta.url));
const cases = [];
const caller = () => {
  const line = new Error().stack.split("\n").find((l) => l.includes("pool-usage.test.mjs"));
  return line ? line.trim().replace(/^at\s+/, "").replace(/.*pool-usage\.test\.mjs:/, "test.mjs:").replace(/\)$/, "") : null;
};
const copy = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));

export function poolUsage(upstream, accounts, options = {}) {
  // The options as the call resolves them; now defaults to the clock, recorded as used.
  const now = options.now ?? Date.now();
  const record = { at: caller(), upstream: copy(upstream), accounts: copy(accounts), mode: options.mode ?? "capped", includeClaude: options.includeClaude ?? true, now };
  cases.push(record);
  try {
    const out = real.poolUsage(upstream, accounts, { ...options, now });
    record.output = copy(out);
    record.same = out === upstream;
    return out;
  } catch (e) {
    record.error = e.message;
    throw e;
  }
}

process.on("exit", () => {
  const dir = path.join(here, "outputs/pool-usage");
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, "cases.json"), JSON.stringify({ format: "pool-usage-cases/v1", source: "reference/agent-pools/pool-usage.test.mjs", cases }, null, 1) + "\n");
});
