// Stands in for routing-ladder.mjs under ladder-hook.mjs: calls the real buildLadders and
// keeps each call's inputs (as JSON, the form the port reads) and result or error message.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import * as real from "../reference/agent-pools/routing-ladder.mjs?real";

const here = path.dirname(fileURLToPath(import.meta.url));
const cases = [];
const caller = () => {
  const line = new Error().stack.split("\n").find((l) => l.includes("routing-ladder.test.mjs"));
  return line ? line.trim().replace(/^at\s+/, "").replace(/.*routing-ladder\.test\.mjs:/, "test.mjs:").replace(/\)$/, "") : null;
};

export function buildLadders(inputs, now) {
  const record = { at: caller(), now, inputs: JSON.parse(JSON.stringify(inputs ?? null)) };
  cases.push(record);
  try {
    const out = real.buildLadders(inputs, now);
    record.output = JSON.parse(JSON.stringify(out));
    return out;
  } catch (e) {
    record.error = e.message;
    throw e;
  }
}

process.on("exit", () => {
  const dir = path.join(here, "outputs/ladders");
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, "cases.json"), JSON.stringify({ format: "ladder-cases/v1", source: "reference/agent-pools/routing-ladder.test.mjs", cases }) + "\n");
});
