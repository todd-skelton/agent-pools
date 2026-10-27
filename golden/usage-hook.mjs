// Records every poolUsage call made by Agent Pools' own pool-usage.test.mjs, so the Rust port
// replays the same inputs and must produce the same overlay.
//   node --import ./golden/usage-hook.mjs reference/agent-pools/pool-usage.test.mjs
// writes golden/outputs/pool-usage/cases.json (inputs and the overlay per call).
import { registerHooks } from "node:module";

registerHooks({
  resolve(specifier, context, next) {
    const r = next(specifier, context);
    if (r.url.endsWith("/reference/agent-pools/pool-usage.mjs")) {
      return { ...r, url: new URL("./usage-recorder.mjs", import.meta.url).href, shortCircuit: true };
    }
    return r;
  },
});
