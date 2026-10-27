# Agent Pools on Corten

A port of Agent Pools (`D:\Users\ToddS\AgentPools`) to Rust and Corten, built beside the live
service. Nothing here writes to the live install, calls a provider, or restarts the pool.

Plan:

1. Golden outputs: what the current JavaScript produces from recorded inputs (`golden/`). Done.
2. `pool-core`: the forecast, routing, reset choice, quota windows and value report in Rust with
   no web dependencies, matching the golden outputs exactly. In progress: quota windows,
   normalized usage, routing order, `chooseReset` and both credit banks match every case
   (`cargo test`); the capacity forecast, the value report and the summary are next.
3. A read-only shadow dashboard on Corten, compared with the live `/api/summary` over days.
4. Writes and the public gateway, in the port only.
5. Cutover: only with Todd's go-ahead.

## Golden outputs

`reference/agent-pools/` is the JavaScript at Agent Pools commit `5c696f6`, copied with
`git archive` (the files the outputs come from; none of them had uncommitted changes).

`golden/inputs/` is a scrubbed snapshot of the live install, taken once by
`node golden/snapshot.mjs` (read-only; see `manifest.json` for when and how much):

| File | What |
|---|---|
| `pool-state.json` | accounts, windows, banks, reset journal |
| `install.json` | only `capacityForecast`, `resetPolicy`, `expectedAccounts`, `usageLedger` |
| `capacity-forecast.json` | the last published forecast (the forecast's hysteresis memory) |
| `usage-samples.jsonl.gz` | every usage sample up to the snapshot |
| `usage-ledger.jsonl.gz` | five days of ledger rows, without `ep`, `st`, `lat`, `q` (never read by the value report) |
| `model-prices.json`, `model-registry.json` | as deployed |
| `provider/*.json` | Agent Pools' recorded, scrubbed provider payloads |

Scrubbing: each account email is `userN@example.invalid` everywhere it appears (account ids
included, so ids still join across files), each banked credit id is `fixture-credit-N`, and
credit profile fields are fixture values. The snapshot fails if any original value is left.

`node golden/capture.mjs` runs the reference over the inputs and writes `golden/outputs/`;
`node golden/capture.mjs --check` recomputes and fails on any difference (deterministic: the
reference never reads the clock, `Date.now` throws while it runs). Node 24.

| Output | Function | Cases |
|---|---|---|
| `forecast/now.json` | `buildForecast` at the snapshot, with the published forecast as memory | 1 |
| `forecast/replay.json` | `buildForecast` hourly for 72 hours, each hour's output the next one's memory | 72 |
| `value/deployed.json`, `value/deployed-session-usage.json` | `buildUsageReports`, deployed config | 1 |
| `value/short-windows.json` | the same with 3/2/5-day windows and 6-hour buckets | 1 |
| `summary/now.json` | `buildSummary` (`GET /api/summary`) from the two above | 1 |
| `routing/choose-reset.json` | `chooseReset`: times, all-out/one-short, bank unknown, synthetic credits, skips, before-lapse sets, refill demand, both triggers | 1,512 (562 choose a credit, 130 before lapse) |
| `routing/rank-accounts.json` | `rankAccounts`, with and without drain-first | 16 |
| `quota/usage.json` | `quotaWindows` and `normalizeUsage` on payloads and edge cases | 33 |
| `quota/banks.json` | `availableCredits` (Codex) and `claudeBank` | 42 |
| `js/semantics.json` | `localeCompare`, `Date.parse` (every timestamp shape in the inputs) and number-to-string, as Node 24 answers them | 13,074 / 1,597 / 520 |

Case files hold `input` and `result` per case. In them, a non-finite number is
`{"$number": "Infinity"}` (or `-Infinity`, `NaN`), and the function's `Set` and `Map`
arguments are arrays. The documents (`forecast`, `value`, `summary`) are plain
`JSON.stringify` output, as they are published.

## Matching them in Rust

1. Compare parsed values, not bytes: objects by key, arrays in order, numbers as `f64` with
   exact equality. Key order and number spelling (`1e21`, `5e-7`) differ between serializers
   and do not matter to readers.
2. Except digests. `decisionDigest` and the pace key are the first 16 hex digits of
   SHA-256 over `JSON.stringify(value)`, so for the digested values the port needs JavaScript's
   exact text: insertion key order and ECMAScript number-to-string. A digest mismatch with
   matching values means the serializer, not the logic.
3. `Math.exp`, `Math.pow` and similar appear in the forecast and value report (7 places).
   V8's are fdlibm ports; use the `libm` crate (also fdlibm-derived) rather than the platform's
   so the last bit agrees. If a value still differs in the last place, record which and why
   before accepting a tolerance for it.
4. Sorting: the JavaScript sorts are stable and break ties with `localeCompare` on ids. The ids
   here are ASCII, where `localeCompare` and byte order agree except for case and punctuation
   (ICU orders `-` and `_` differently from bytes); match it on these ids, not in general.