// Records every buildLadders call made by Agent Pools' own routing-ladder.test.mjs, so the
// Rust port replays the same inputs and must produce the same documents.
//   node --import ./golden/ladder-hook.mjs reference/agent-pools/routing-ladder.test.mjs
// writes golden/outputs/ladders/cases.json (inputs and the published JSON per call).
import { registerHooks } from "node:module";

registerHooks({
  resolve(specifier, context, next) {
    const r = next(specifier, context);
    if (r.url.endsWith("/reference/agent-pools/routing-ladder.mjs")) {
      return { ...r, url: new URL("./ladder-recorder.mjs", import.meta.url).href, shortCircuit: true };
    }
    return r;
  },
});
