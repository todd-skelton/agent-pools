// Records every call Agent Pools' own model-registry.test.mjs makes into the registry
// (model-registry.mjs), the refresh's planning (model-refresh.mjs) and the admission judges
// (model-admission.mjs), so the Rust port replays the same inputs and must give the same results.
//   $env:TZ = 'UTC'; node --import ./golden/registry-hook.mjs reference/agent-pools/model-registry.test.mjs
// writes golden/outputs/registry/cases.json. TZ=UTC: poolRouting reads the pool log's local
// times with Date.parse, so under UTC they are UTC and the replay needs no zone.
import { registerHooks } from "node:module";

const redirected = ["model-registry.mjs", "model-refresh.mjs", "model-admission.mjs"];
// model-refresh.mjs imports model-arena.mjs, which imports hyparquet's parquetReadObjects. The
// registry calls never read a parquet file, so a stand-in that refuses to is enough.
const hyparquet = "data:text/javascript," + encodeURIComponent("export function parquetReadObjects() { throw new Error('hyparquet is not available to the registry recorder'); }");
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "hyparquet") return { url: hyparquet, shortCircuit: true };
    const r = next(specifier, context);
    const fromTest = context.parentURL?.endsWith("/reference/agent-pools/model-registry.test.mjs");
    if (fromTest && redirected.some((m) => r.url.endsWith("/reference/agent-pools/" + m))) {
      return { ...r, url: new URL("./registry-recorder.mjs", import.meta.url).href, shortCircuit: true };
    }
    return r;
  },
});
