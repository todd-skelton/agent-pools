// Runs Agent Pools' own buildRoutingAttempts (reference/agent-pools/usage-value.mjs at 5c696f6)
// over a fixed session-usage document and orchestrator directory (golden/inputs/routing-attempts/)
// and writes each case's output to golden/outputs/attempts/cases.json, for the copy's value report.
//   node golden/routing-attempts.mjs
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { buildRoutingAttempts } from "../reference/agent-pools/usage-value.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const dir = path.join(here, "inputs/routing-attempts");
const orchestrator = path.join(dir, "orchestrator");
fs.mkdirSync(path.join(orchestrator, "transcripts", "nested"), { recursive: true });
const put = (rel, text) => fs.writeFileSync(path.join(orchestrator, rel), text);

// Sessions: two roots for one provider (a child joins its root's totals), the same root id under
// another provider, one with unknown quota (the root's sum becomes null), one with no end.
const sessionUsage = {
  format: "session-usage/v1", generatedAt: "2026-10-20T12:00:00.000Z", from: "2026-10-06T12:00:00.000Z", costBasis: "list", unattributedRequests: 0,
  sessions: [
    { sid: "s-root", psid: null, rootSid: "s-root", provider: "codex", firstAt: "2026-10-19T10:00:00.000Z", lastAt: "2026-10-19T11:30:00.000Z", apiEquivalentUsd: 0.1, quotaUnits: 0.25 },
    { sid: "s-child", psid: "s-root", rootSid: "s-root", provider: "codex", firstAt: "2026-10-19T09:15:00.000Z", lastAt: "2026-10-19T12:00:00.000Z", apiEquivalentUsd: 0.2, quotaUnits: 0.125 },
    { sid: "s-other", psid: null, rootSid: "s-root", provider: "claude", firstAt: "2026-10-18T00:00:00.000Z", lastAt: "2026-10-18T00:30:00.000Z", apiEquivalentUsd: 1.5, quotaUnits: null },
    { sid: "s-unknown", psid: null, rootSid: "s-unknown", provider: "claude", firstAt: "2026-10-17T08:00:00.000Z", lastAt: "2026-10-17T09:00:00.000Z", apiEquivalentUsd: 0.7, quotaUnits: 2 },
    { sid: "s-unknown-2", psid: "s-unknown", rootSid: "s-unknown", provider: "claude", firstAt: "2026-10-17T08:30:00.000Z", lastAt: "2026-10-17T10:00:00.000Z", apiEquivalentUsd: 0.3, quotaUnits: null },
    { sid: "s-bad-time", psid: null, rootSid: "s-bad-time", provider: "codex", firstAt: "not a date", lastAt: "2026-10-16T10:00:00.000Z", apiEquivalentUsd: 0.05, quotaUnits: 1 },
  ],
};

// Transcripts: the first line names the session (codex: thread.started's thread_id; claude:
// session_id). Others name nothing, or cannot be read as JSON, or are empty.
put("transcripts/codex-root.jsonl", JSON.stringify({ type: "thread.started", thread_id: "s-root" }) + "\n" + JSON.stringify({ type: "turn" }) + "\n");
put("transcripts/codex-child.jsonl", "\uFEFF" + JSON.stringify({ type: "thread.started", thread_id: "s-child" }) + "\r\n");
put("transcripts/claude-other.jsonl", JSON.stringify({ type: "system", session_id: "s-other" }) + "\n");
put("transcripts/claude-unknown.jsonl", JSON.stringify({ session_id: "s-unknown-2" }) + "\n");
put("transcripts/nested/bad-time.jsonl", JSON.stringify({ type: "thread.started", thread_id: "s-bad-time" }) + "\n");
put("transcripts/not-json.jsonl", "{ not json\n" + JSON.stringify({ session_id: "s-root" }) + "\n");
put("transcripts/empty.jsonl", "");
put("transcripts/blank-first.jsonl", "\n" + JSON.stringify({ session_id: "s-root" }) + "\n");
put("transcripts/null.jsonl", "null\n");
put("transcripts/number-id.jsonl", JSON.stringify({ session_id: 7 }) + "\n");
put("transcripts/thread-without-id.jsonl", JSON.stringify({ type: "thread.started", session_id: "s-root" }) + "\n");
put("transcripts/unknown-session.jsonl", JSON.stringify({ session_id: "s-nobody" }) + "\n");

const rows = [
  { kind: "dispatch", ts: "2026-10-19T10:05:00.000Z", transcript: "transcripts/codex-root.jsonl", label: "a-r1", row: "8", model: "gpt-6.1-sol", effort: "high" },
  { kind: "dispatch", ts: "2026-10-19T10:06:00Z", transcript: "transcripts/codex-root.jsonl", label: "a-r1-again" },
  { kind: "dispatch", at: "2026-10-19T10:07:00.000Z", transcript: "transcripts/codex-child.jsonl", label: 12, row: 3, model: null, effort: "low" },
  { kind: "dispatch", ts: null, at: "2026-10-18T00:10:00.000Z", transcript: "transcripts/claude-other.jsonl", label: "c-other" },
  { kind: "dispatch", ts: "2026-10-17T08:45:00.000-05:00", transcript: "transcripts/claude-unknown.jsonl", label: "c-unknown", row: { n: 1 } },
  { kind: "dispatch", ts: "2026-10-16T09:00:00.000Z", transcript: "transcripts/nested/../nested/bad-time.jsonl", label: "bad-time" },
  { kind: "dispatch", ts: "2026-10-19T10:08:00.000Z", transcript: path.join(orchestrator, "transcripts/codex-root.jsonl"), label: "absolute" },
  { kind: "dispatch", ts: "2026-10-05T00:00:00.000Z", transcript: "transcripts/codex-root.jsonl", label: "before-from" },
  { kind: "dispatch", ts: "2026-10-21T00:00:00.000Z", transcript: "transcripts/codex-root.jsonl", label: "after-generated" },
  { kind: "dispatch", ts: "2026-10-06T12:00:00.000Z", transcript: "transcripts/codex-root.jsonl", label: "at-from" },
  { kind: "dispatch", ts: "2026-10-20T12:00:00.000Z", transcript: "transcripts/codex-root.jsonl", label: "at-generated" },
  { kind: "result", ts: "2026-10-19T10:05:00.000Z", transcript: "transcripts/codex-root.jsonl", label: "not-dispatch" },
  { kind: "dispatch", ts: "yesterday", transcript: "transcripts/codex-root.jsonl", label: "bad-ts" },
  { kind: "dispatch", ts: "2026-10-19T10:05:00.000Z", transcript: "", label: "empty-transcript" },
  { kind: "dispatch", ts: "2026-10-19T10:05:00.000Z", transcript: 5, label: "number-transcript" },
  { kind: "dispatch", ts: "2026-10-19T10:05:00.000Z", label: "no-transcript" },
  { kind: "dispatch", ts: "2026-10-19T10:05:00.000Z", transcript: "transcripts/missing.jsonl", label: "missing" },
  { kind: "dispatch", ts: "2026-10-19T10:05:00.000Z", transcript: "transcripts/not-json.jsonl", label: "not-json" },
  { kind: "dispatch", ts: "2026-10-19T10:05:00.000Z", transcript: "transcripts/empty.jsonl", label: "empty" },
  { kind: "dispatch", ts: "2026-10-19T10:05:00.000Z", transcript: "transcripts/blank-first.jsonl", label: "blank-first" },
  { kind: "dispatch", ts: "2026-10-19T10:05:00.000Z", transcript: "transcripts/null.jsonl", label: "null" },
  { kind: "dispatch", ts: "2026-10-19T10:05:00.000Z", transcript: "transcripts/number-id.jsonl", label: "number-id" },
  { kind: "dispatch", ts: "2026-10-19T10:05:00.000Z", transcript: "transcripts/thread-without-id.jsonl", label: "thread-without-id" },
  { kind: "dispatch", ts: "2026-10-19T10:05:00.000Z", transcript: "transcripts/unknown-session.jsonl", label: "unknown-session" },
  { kind: "dispatch", ts: 1792400000000, transcript: "transcripts/codex-root.jsonl", label: "number-ts" },
  { kind: "dispatch", ts: "2026-10-19T10:09:00.000Z", transcript: "transcripts/codex-root.jsonl", label: "no-model-or-effort", row: null },
];
put("dispatch-log.jsonl", rows.map((r) => JSON.stringify(r)).join("\n") + "\n\n" + "not json\n" + "[1,2]\n" + "42\n" + JSON.stringify({ kind: "dispatch", ts: "2026-10-19T10:10:00.000Z", transcript: "transcripts/codex-root.jsonl", label: "after-junk" }) + "\r\n");
fs.writeFileSync(path.join(dir, "session-usage.json"), JSON.stringify(sessionUsage, null, 2));

const cases = [];
for (const [name, root] of [["no-root", null], ["empty-root", ""], ["fixture", "orchestrator"], ["no-dispatch-log", "transcripts"]]) {
  const resolved = root ? path.join(dir, root) : root;
  const out = await buildRoutingAttempts(sessionUsage, resolved);
  // The output names the directory it was given; the test gives the same relative one.
  cases.push({ name, root, output: JSON.parse(JSON.stringify(out).split(JSON.stringify(dir).slice(1, -1)).join("<dir>")) });
}
fs.mkdirSync(path.join(here, "outputs/attempts"), { recursive: true });
fs.writeFileSync(path.join(here, "outputs/attempts/cases.json"), JSON.stringify(cases, null, 2) + "\n");
console.log(cases.map((c) => c.name + ": " + (c.output.attempts.length) + " attempts").join("\n"));
