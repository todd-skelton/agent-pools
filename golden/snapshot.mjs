// One-time, read-only snapshot of a running Agent Pools install into scrubbed
// golden inputs. It reads files only: no HTTP, no provider calls, nothing in
// the source directory is written.
//   node golden/snapshot.mjs [--source D:\Users\ToddS\AgentPools] [--ledger-days 5]
// Scrubbing (applied as literal replacements to every input text, so ids stay
// consistent across files): each account email becomes userN@example.invalid,
// each banked credit id becomes fixture-credit-N, and
// credit profile fields become fixture values. The run fails if any original
// value is left in what it wrote.
import fs from "node:fs";
import path from "node:path";
import zlib from "node:zlib";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const arg = (name, fallback) => { const i = process.argv.indexOf(name); return i > 0 ? process.argv[i + 1] : fallback; };
const source = path.resolve(arg("--source", "D:\\Users\\ToddS\\AgentPools"));
const ledgerDays = Number(arg("--ledger-days", "5"));
const out = path.join(here, "inputs");
const DAY = 86400000;

const read = (rel) => fs.readFileSync(path.join(source, rel), "utf8").replace(/^\uFEFF/, "");
const readOptional = (rel) => (fs.existsSync(path.join(source, rel)) ? read(rel) : null);

// Read the state first and the logs after, so nothing in the logs predates it.
const poolText = read("state/pool-state.json");
const pool = JSON.parse(poolText);
const forecastText = readOptional("state/capacity-forecast.json");
const install = JSON.parse(read("state/install.json"));
const now = Date.parse(pool.checkedAt);
if (!Number.isFinite(now)) throw new Error("pool-state.json has no checkedAt");

// --- scrub table -------------------------------------------------------------
const replacements = [];
const emails = [...new Set(pool.accounts.map((a) => a.email).filter((e) => typeof e === "string" && e.includes("@")))].sort();
emails.forEach((email, i) => replacements.push([email, `user${i + 1}@example.invalid`]));
const creditIds = new Set();
for (const a of pool.accounts) for (const c of a.credits ?? []) if (typeof c.id === "string") creditIds.add(c.id);
for (const slot of Object.values(pool.resets ?? {})) for (const j of [slot?.pending, slot?.last]) if (typeof j?.creditId === "string") creditIds.add(j.creditId);
for (const key of Object.keys(pool.resets?.codex?.refused ?? {}).concat(Object.keys(pool.resets?.claude?.refused ?? {}))) {
  const id = key.split("|")[1];
  if (id) creditIds.add(id);
}
// Nothing parses a stored credit id, so a counter keeps them distinct and stable.
[...creditIds].sort().forEach((id, i) => replacements.push([id, `fixture-credit-${i + 1}`]));
const profileValues = new Set();
for (const a of pool.accounts) for (const c of a.credits ?? []) for (const k of ["profile_image_url", "profile_user_id", "title", "description"]) if (typeof c[k] === "string" && c[k]) profileValues.add(c[k]);
// Longest first, so an email inside an account id is replaced as part of it consistently.
replacements.sort((a, b) => b[0].length - a[0].length);
const scrub = (text) => replacements.reduce((t, [from, to]) => t.split(from).join(to), text);
const scrubCreditProfile = (doc) => {
  for (const a of doc.accounts ?? []) for (const c of a.credits ?? []) {
    if ("profile_image_url" in c) c.profile_image_url = "https://example.invalid/profile.png";
    if ("profile_user_id" in c) c.profile_user_id = "fixture-user";
    if ("title" in c) c.title = "Fixture credit";
    if ("description" in c) c.description = "Fixture credit description.";
  }
  return doc;
};

// --- writers -----------------------------------------------------------------
fs.mkdirSync(out, { recursive: true });
const written = [];
const writeJSON = (name, value) => { const p = path.join(out, name); fs.writeFileSync(p, JSON.stringify(value, null, 1) + "\n"); written.push(p); };
const writeLinesGz = (name, lines) => { const p = path.join(out, name); fs.writeFileSync(p, zlib.gzipSync(lines.join("\n") + "\n", { level: 9 })); written.push(p); };

writeJSON("pool-state.json", scrubCreditProfile(JSON.parse(scrub(poolText))));
writeJSON("install.json", Object.fromEntries(["capacityForecast", "resetPolicy", "expectedAccounts", "usageLedger"].filter((k) => k in install).map((k) => [k, install[k]])));
if (forecastText) writeJSON("capacity-forecast.json", JSON.parse(scrub(forecastText)));
writeJSON("model-prices.json", JSON.parse(read("model-prices.json")));
const registryText = readOptional("state/model-registry.json");
if (registryText) writeJSON("model-registry.json", JSON.parse(scrub(registryText)));

// Usage samples: the publisher's load order (.1, then the current log), up to now.
const sampleLines = [];
for (const rel of ["logs/usage-samples.jsonl.1", "logs/usage-samples.jsonl"]) {
  const text = readOptional(rel);
  if (!text) continue;
  for (const line of text.split("\n")) {
    if (!line.trim()) continue;
    let s; try { s = JSON.parse(line); } catch { continue; }
    if (Date.parse(s?.at) <= now) sampleLines.push(scrub(line.trim()));
  }
}
writeLinesGz("usage-samples.jsonl.gz", sampleLines);

// Ledger: rows in (now - ledgerDays, now], without the fields the value report never reads.
const DROP = ["ep", "st", "lat", "q"];
const from = now - ledgerDays * DAY;
const ledgerLines = [];
const names = fs.readdirSync(path.join(source, "logs/usage-ledger")).filter((n) => /^raw-(\d{4}-\d{2}-\d{2})\.jsonl$/.test(n)).sort();
for (const name of names) {
  const day = Date.parse(name.slice(4, 14) + "T00:00:00Z");
  if (day + 2 * DAY < from || day - DAY > now) continue;
  for (const line of read(path.join("logs/usage-ledger", name)).split("\n")) {
    if (!line.trim()) continue;
    let row; try { row = JSON.parse(scrub(line)); } catch { continue; }
    const at = Date.parse(row?.at);
    if (!(at > from && at <= now)) continue;
    for (const k of DROP) delete row[k];
    ledgerLines.push(JSON.stringify(row));
  }
}
writeLinesGz("usage-ledger.jsonl.gz", ledgerLines);

// Provider payloads recorded and scrubbed earlier (Agent Pools fixtures/).
fs.mkdirSync(path.join(out, "provider"), { recursive: true });
for (const name of fs.readdirSync(path.join(source, "fixtures")).filter((n) => n.endsWith(".json"))) {
  const p = path.join(out, "provider", name);
  fs.copyFileSync(path.join(source, "fixtures", name), p);
  written.push(p);
}

// --- leak check ----------------------------------------------------------------
const secrets = [...emails, ...creditIds, ...profileValues, ...emails.map((e) => e.split("@")[0]).filter((l) => l.length >= 5)];
for (const p of written) {
  const text = p.endsWith(".gz") ? zlib.gunzipSync(fs.readFileSync(p)).toString("utf8") : fs.readFileSync(p, "utf8");
  for (const s of secrets) if (text.includes(s)) throw new Error(`scrub left an original value in ${path.basename(p)}`);
}

const manifest = {
  format: "agent-pools-golden-inputs/v1",
  takenAt: new Date().toISOString(),
  now: new Date(now).toISOString(),
  ledgerDays,
  counts: { accounts: pool.accounts.length, samples: sampleLines.length, ledgerRows: ledgerLines.length },
  scrubbed: { emails: emails.length, creditIds: creditIds.size, creditProfileValues: profileValues.size },
  files: written.map((p) => path.relative(out, p).split(path.sep).join("/")).sort(),
};
writeJSON("manifest.json", manifest);
console.log(JSON.stringify(manifest, null, 1));