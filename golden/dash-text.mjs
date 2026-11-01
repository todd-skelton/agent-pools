// Draws the live dashboard's parts with its own JavaScript (dashboard.html, the pure functions
// above its I/O marker) and with the shadow (pools-shadow render-dash) from the same documents at
// the same moment, and compares them part by part: the words, the tooltips (title attributes)
// and the sizes the page computes (the live page's style attributes; the shadow's custom
// properties, data-vars="--meter__width:42%", named after the property they set).
//
//   node golden/dash-text.mjs <status.json> <value.json|-> <now ms> [filter] [open]
//
// AGENT_POOLS_ROOT finds dashboard.html (read only); SHADOW_EXE the shadow (default
// target/release/pools-shadow.exe). Run with TZ set to the zone the shadow is told
// (SHADOW_UTC_STANDARD_HOURS, Central by default). Exits 1 when a part differs.

import { readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import vm from "node:vm";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const [statusPath, valuePath, nowArg, filter = "", open = ""] = process.argv.slice(2);
if (!statusPath || !valuePath || !nowArg) {
  console.error("usage: node golden/dash-text.mjs <status.json> <value.json|-> <now ms> [filter] [open]");
  process.exit(2);
}
const readJson = (p) => JSON.parse(readFileSync(p, "utf8").replace(/^\uFEFF/, ""));
const root = process.env.AGENT_POOLS_ROOT || "D:\\Users\\ToddS\\AgentPools";
const page = readFileSync(path.join(root, "dashboard.html"), "utf8");
const script = page.slice(page.indexOf("<script>") + 8, page.lastIndexOf("</script>"));
const pure = script.slice(0, script.indexOf("/* ---- I/O below"));

const ctx = vm.createContext({ console, __S: readJson(statusPath), __V: valuePath === "-" ? null : readJson(valuePath), __NOW: Number(nowArg) });
vm.runInContext(pure + `
S = __S; V = __V; NOW = __NOW;
globalThis.__parts = (filter, open) => {
  const items = attentionItems();
  const age = S.checkedAt ? NOW - Date.parse(S.checkedAt) : Infinity;
  return {
    attention: attentionHtml(items),
    count: items.filter((i) => i.sev !== "info").length,
    fresh: S.error ? S.error : "Checked " + clock(S.checkedAt),
    "prov-claude": providerHtml("claude"),
    "prov-codex": providerHtml("codex"),
    timeline: timelineHtml(),
    accounts: accountsHtml(filter || "all", open || null),
    usage: usageHtml(),
    policy: policyHtml(),
  };
};`, ctx);
const js = ctx.__parts(filter, open);

const exe = process.env.SHADOW_EXE || path.join(here, "../target/release/pools-shadow.exe");
const rs = JSON.parse(execFileSync(exe, ["render-dash", statusPath, valuePath, nowArg, filter, open], { encoding: "utf8", maxBuffer: 64 << 20 }));

const decode = (s) => s.replace(/&(amp|lt|gt|quot|#39|middot|mdash);/g, (_, e) => ({ amp: "&", lt: "<", gt: ">", quot: '"', "#39": "'", middot: "\u00b7", mdash: "\u2014" })[e]);
const words = (html) => decode(String(html).replace(/<[^>]*>/g, " ")).replace(/\s+/g, " ").trim();
const titles = (html) => [...String(html).matchAll(/ title="([^"]*)"/g)].map((m) => decode(m[1]));
const sizes = (html) => [...String(html).matchAll(/ style="([^"]*)"/g)].flatMap((m) => [...m[1].matchAll(/(left|width|height|flex):\s*(?:calc\()?(-?[\d.e+-]+)/g)].map((x) => x[1] + ":" + Number(x[2]).toFixed(6)));
const vars = (html) => [...String(html).matchAll(/ data-vars="([^"]*)"/g)].flatMap((m) => [...m[1].matchAll(/--[\w-]+?__(left|width|height|flex):(-?[\d.e+-]+)/g)].map((x) => x[1] + ":" + Number(x[2]).toFixed(6)));

// Where the shadow differs on purpose. Its actions are disabled, with a title saying why; the live
// page's fixed inline styles are classes in shadow/static/dash.css (only sizes from data are
// compared).
const READ_ONLY = "Done on the live dashboard (port 8318): the shadow only reads";
const STATIC = new Set(["width:26%", "width:22%", "grid-template-columns:minmax(0,1.3fr) minmax(0,1fr) 84px", "display:block;font-size:11px", "width:10px;height:10px", "position:static;width:18px;height:10px;padding:0", "margin-left:4px", "width:30%", "width:34%", "margin-bottom:6px", "", "margin-left:auto", "font-size:12px;margin:8px 0 0", "white-space:normal", "border:0;padding-top:0", "color:var(--bad)", "grid-template-columns:minmax(0,1.3fr) minmax(0,1fr) 54px", "grid-template-columns:minmax(0,1fr) auto 70px", "font-size:11px;padding:1px 7px", "font-size:13px", "font-size:12px", "margin-top:16px", "font-size:12px;margin-top:10px", "margin:0 0 12px;font-size:13px", "font-size:12px;margin:12px 0 0", "font-size:12px;margin:0", "margin:0 0 10px;font-size:13px", "align-self:center;margin-right:4px", "left:0"]);
const dropStatic = (html) => String(html).replace(/ style="([^"]*)"/g, (m, s) => (STATIC.has(s) ? "" : m));
for (const k of Object.keys(js)) if (typeof js[k] === "string") js[k] = dropStatic(js[k]);
for (const k of Object.keys(rs)) if (typeof rs[k] === "string") rs[k] = rs[k].replaceAll(' title="' + READ_ONLY + '"', "");
let failed = 0;
const firstDiff = (a, b) => {
  const n = Math.min(a.length, b.length);
  let i = 0;
  while (i < n && a[i] === b[i]) i++;
  return i;
};
for (const k of Object.keys(js)) {
  const checks = typeof js[k] === "string" && k !== "fresh"
    ? [["text", words(js[k]), words(rs[k])], ["titles", titles(js[k]).join("\n"), titles(rs[k]).join("\n")], ["sizes", sizes(js[k]).join(" "), vars(rs[k]).join(" ")]]
    : [["value", String(js[k]), String(rs[k])]];
  for (const [what, a, b] of checks) {
    if (a === b) { console.log("match   " + k + " " + what + " (" + a.length + " chars)"); continue; }
    failed++;
    const i = firstDiff(a, b);
    console.log("DIFFERS " + k + " " + what + " at " + i + ":\n  js:     " + JSON.stringify(a.slice(Math.max(0, i - 80), i + 120)) + "\n  shadow: " + JSON.stringify(b.slice(Math.max(0, i - 80), i + 120)));
  }
}
process.exit(failed ? 1 : 0);
