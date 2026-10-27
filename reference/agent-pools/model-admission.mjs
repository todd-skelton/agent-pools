// Admission probes: one tiny prompt per (model, effort) through the native
// harness on the same launch path lanes use (Codex: model_provider=account_pool,
// apps off, --json; Claude: --print with --output-format json). Each probe is
// appended to state/model-admission.jsonl as immutable evidence bound to the
// installed harness version and the pool account that served it.
//
// Safety: executables are resolved to native .exe paths and started without a
// shell; the prompt goes through stdin; ids and efforts are validated first.
import { spawn, execFileSync } from "node:child_process";
import { appendFile, readFile, mkdtemp, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { EFFORTS, SAFE_ID } from "./model-registry.mjs";

const PROMPT = "Reply with exactly: OK";
const TIMEOUT_MS = 240_000;

export function resolveExe(name) {
  const out = execFileSync("where.exe", [name], { encoding: "utf8", windowsHide: true });
  const exe = out.split(/\r?\n/).map((s) => s.trim()).find((s) => /\.exe$/i.test(s));
  if (!exe) throw new Error(`no native ${name}.exe on PATH`);
  return exe;
}

export function harnessVersion(exe) {
  const v = execFileSync(exe, ["--version"], { encoding: "utf8", windowsHide: true, timeout: 30_000 }).trim();
  const m = /(\d+\.\d+\.\d+)/.exec(v);
  return m ? m[1] : v;
}

function run(exe, args, cwd, input) {
  return new Promise((resolve) => {
    const child = spawn(exe, args, { cwd, windowsHide: true, shell: false, stdio: ["pipe", "pipe", "pipe"] });
    let stdout = "", stderr = "";
    const timer = setTimeout(() => child.kill(), TIMEOUT_MS);
    child.stdout.on("data", (d) => { stdout += d; });
    child.stderr.on("data", (d) => { stderr += d; });
    child.on("error", (e) => { clearTimeout(timer); resolve({ code: -1, stdout, stderr: String(e) }); });
    child.on("close", (code) => { clearTimeout(timer); resolve({ code, stdout, stderr }); });
    child.stdin.end(input);
  });
}

export function judgeClaude(id, stdout) {
  let result;
  try { result = JSON.parse(stdout.trim().split(/\r?\n/).at(-1)); } catch { return { ok: false, reason: "unparseable" }; }
  const used = Object.keys(result.modelUsage ?? {});
  if (result.is_error) return { ok: false, reason: "is_error" };
  if (String(result.result ?? "").trim() !== "OK") return { ok: false, reason: "reply-not-OK" };
  if (used.length !== 1 || used[0] !== id) return { ok: false, reason: `modelUsage ${used.join(",") || "empty"}` };
  return { ok: true, servedModel: used[0] };
}

export function judgeCodex(stdout) {
  const events = stdout.split(/\r?\n/).map((l) => { try { return JSON.parse(l); } catch { return null; } }).filter(Boolean);
  const msg = events.filter((e) => e.type === "item.completed" && e.item?.type === "agent_message").at(-1)?.item?.text;
  if (!events.some((e) => e.type === "turn.completed")) return { ok: false, reason: "no-turn-completed" };
  if (String(msg ?? "").trim() !== "OK") return { ok: false, reason: "reply-not-OK" };
  return { ok: true };
}

// The pool logs one selector line per new request binding:
//   [ts] [reqid] ... session-affinity: ... auth=<account> provider=... model=<id>
// A 2xx for that request id inside the probe window proves the pool routed the
// exact requested model to a real account (upstream renaming is out of reach).
export function poolRouting(logText, id, startMs, endMs) {
  const lines = logText.split(/\r?\n/);
  const at = (l) => Date.parse(l.slice(1, 20).replace(" ", "T"));
  const ok = new Set();
  for (const l of lines) {
    const m = /^\[[^\]]+\] \[([0-9a-f]{8})\] .*gin_logger.* (2\d\d) \|/.exec(l);
    if (m) ok.add(m[1]);
  }
  for (const l of lines) {
    const t = at(l);
    if (!(t >= startMs - 2000 && t <= endMs + 2000)) continue;
    const m = /^\[[^\]]+\] \[([0-9a-f]{8})\] .*auth=(\S+) provider=\S+ model=(\S+)/.exec(l);
    if (m && m[3] === id && ok.has(m[1])) return { account: m[2], requestId: m[1] };
  }
  return null;
}

async function probe({ provider, exe, id, effort, cwd, poolLog }) {
  const start = Date.now();
  if (provider === "claude") {
    const r = await run(exe, ["--model", id, "--effort", effort, "--print", "--output-format", "json"], cwd, PROMPT);
    const verdict = r.code === 0 ? judgeClaude(id, r.stdout) : { ok: false, reason: `exit ${r.code}` };
    return { ...verdict, durationMs: Date.now() - start };
  }
  const r = await run(exe, ["exec", "--skip-git-repo-check", "-m", id, "-c", "model_provider=account_pool",
    "-c", `model_reasoning_effort=${effort}`, "-c", "features.apps=false", "--json", "-"], cwd, PROMPT);
  let verdict = r.code === 0 ? judgeCodex(r.stdout) : { ok: false, reason: `exit ${r.code}` };
  const end = Date.now();
  if (verdict.ok && poolLog) {
    await new Promise((res) => setTimeout(res, 1500)); // let the pool flush its log line
    const routed = poolRouting(await readFile(poolLog, "utf8").catch(() => ""), id, start, end);
    verdict = routed ? { ...verdict, account: routed.account, poolRequestId: routed.requestId } : { ok: false, reason: "no-pool-routing-evidence" };
  }
  return { ...verdict, durationMs: end - start };
}

// targets: [{ id, provider, efforts }]. Appends one evidence row per probe.
// deadline (ms epoch): no new probe starts after it, so a run stays inside the
// scheduler limit and the refresh lock's stale threshold.
export async function admit(root, targets, { log = console.log, deadline = Infinity } = {}) {
  const exes = {}, versions = {};
  for (const p of ["claude", "codex"]) { exes[p] = resolveExe(p); versions[p] = harnessVersion(exes[p]); }
  const cwd = await mkdtemp(path.join(os.tmpdir(), "model-admission-"));
  const out = path.join(root, "state/model-admission.jsonl");
  const poolLog = path.join(root, "logs/main.log");
  const rows = [];
  try {
    for (const t of targets) {
      if (!SAFE_ID.test(t.id) || !(t.provider in exes)) throw new Error(`refusing unsafe target ${t.id}`);
      for (const effort of t.efforts) {
        if (!EFFORTS.includes(effort)) throw new Error(`refusing effort ${effort}`);
        if (Date.now() >= deadline) { log(`admission budget spent; ${t.id} ${effort} left for the next run`); continue; }
        const res = await probe({ provider: t.provider, exe: exes[t.provider], id: t.id, effort, cwd, poolLog });
        const row = { at: new Date().toISOString(), model: t.id, provider: t.provider, effort, harnessVersion: versions[t.provider],
          ok: res.ok, reason: res.ok ? null : res.reason, servedModel: res.servedModel ?? null, account: res.account ?? null,
          poolRequestId: res.poolRequestId ?? null, durationMs: res.durationMs };
        await appendFile(out, JSON.stringify(row) + "\n");
        rows.push(row);
        log(`${t.id} ${effort}: ${res.ok ? "ok" : "FAILED " + res.reason}`);
      }
    }
  } finally { await rm(cwd, { recursive: true, force: true }); }
  return { rows, versions };
}

export async function readProbes(root) {
  const text = await readFile(path.join(root, "state/model-admission.jsonl"), "utf8").catch((e) => (e.code === "ENOENT" ? "" : Promise.reject(e)));
  return text.split(/\r?\n/).filter(Boolean).map((l) => JSON.parse(l));
}
