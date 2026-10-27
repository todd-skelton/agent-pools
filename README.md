# Agent Pools on Corten

A port of Agent Pools (`D:\Users\ToddS\AgentPools`) to Rust and Corten, built beside the live
service. Nothing here writes to the live install, calls a provider, or restarts the pool.

Plan:

1. Golden outputs: what the current JavaScript produces from recorded inputs (`golden/`). Done.
2. `pool-core`: the forecast, routing, reset choice, quota windows, value report and summary in
   Rust with no web dependencies, matching the golden outputs exactly. Done: every golden
   output matches with no tolerance (`cargo test --release`, 15 tests): quota windows,
   normalized usage, routing order, `chooseReset`, both credit banks, reset policies, the
   capacity forecast (now, 72 replayed hours, 22 variants, including a kept drain-first boost), the usage value report and session
   usage (deployed and short windows, plus quota increments, pricing and settings), and
   `/api/summary` (now and five other times or missing documents).
3. A read-only shadow dashboard on Corten, compared with the live `/api/summary` over days.
   In progress: `shadow/` (below) runs beside the live service and checks every summary,
   forecast and value report it publishes; the evidence is collecting.
4. Writes and the public gateway, in the port only.
5. Cutover: only with Todd's go-ahead.

## The shadow

`shadow/` (`pools-shadow`) is the dashboard and `/api/summary` on Corten, built by `pool-core`
from the live install's files. It only reads: `state/pool-state.json`, the forecast, the value
report, the four `install.json` settings the port uses (`capacityForecast`, `resetPolicy`,
`expectedAccounts`, `usageLedger`), `model-prices.json`, the model registry, the sample logs
and the usage ledger. It never opens `auth/` or secrets, and calls the live service with
`GET /api/summary` only.

```powershell
cargo build --release -p pools-shadow
target\release\pools-shadow.exe   # http://localhost:8320 (PORT), settings in shadow/src/main.rs
```

| Route | |
|---|---|
| `/`, `/accounts`, `/usage`, `/policy` | the live dashboard's four tabs (overview, accounts, usage & value, automation), drawn on the server |
| `/live?tab=&filter=&open=` | the open tab, sent again when it changes (server-sent events for `<corten-live>`) |
| `/summary` | `/api/summary` as a page |
| `/api/status` | `/api/status` from the files (`processes`, `holds`, `usageLedger` are `null`: the live service's memory) |
| `/parity`, `/api/parity` | the parity checks: counts per kind, the last difference, the recent checks |
| `/api/summary` | `agent-pools-summary/v1`, written as `JSON.stringify` writes it |

Three checks run, each appended to `shadow-state/parity.jsonl`:

| Check | Live document | Shadow's build | Aligned when |
|---|---|---|---|
| summary (every 60 s) | `GET /api/summary` | `build_summary` over the files read just after, at the live `generatedAt` | the answer names the pool state's `checkedAt`, the forecast and the value report on disk |
| forecast (each one published) | `state/capacity-forecast.json` | `build_forecast` with the document before it as memory | the shadow saw the one before, and a pool state saved in the same refresh cycle |
| value (each report) | `state/usage-value.json` | `build_usage_reports` over the ledger up to the row count the report gives (`coverage.ledgerRows`), with the pool state and forecast on disk at its `generatedAt` | the shadow was watching before the report was built |

The proxy appends to the ledger while the report streams it, so no time cut-off reproduces what
it read (measured on the 04:25 report: cut-offs from its `generatedAt` to its write came out 33
rows under to 37 over). The rows it counted mark where its read ended, and cut there the
rebuilt report matched exactly.

`pools-shadow check-value` rebuilds the live value report once (about 17 s over 611,054 rows)
and prints the check. It uses the pool state and forecast on disk now, so it is only exact while
those have not changed what the report shows.

### The dashboard

`shadow/src/dash/` is `dashboard.html` on Corten: `model.rs` ports its pure functions of `S`
(`/api/status`), `V` (`/api/value`) and `NOW` (names kept, so the two read side by side),
`fmt.rs` its en-US formatting in US Central time with daylight saving (`SHADOW_UTC_STANDARD_HOURS`,
default -6), and `views.rs` its markup as `html!`. The tabs are routes and the account filter
and open row are query parameters, so every view is a link and there is no script of the app's
own. Each tab's body sits in a `<corten-live>` on `/live?tab=`, which draws the tab again every
15 s (the live page polls its status that often) and sends it, with the header's count and
freshness, when it changed; its cursor is a hash of the fragment, so the page and a reconnect
get nothing until something does. The pages
use the shadow's `/api/status`, with the live service's memory keys taken from its last
`/api/status` answer, and `state/usage-value.json`. Sign-in and refresh change the pool, so
they are shown disabled.

`golden/dash-text.mjs` draws every part with the live page's own JavaScript and with
`pools-shadow render-dash` from the same documents at the same moment, and compares the words,
the tooltips and the computed sizes:

```powershell
$env:TZ = 'America/Chicago'
node golden/dash-text.mjs <status.json> <usage-value.json|-> <now ms> [filter] [open account id]
```

On a live `/api/status` and value report (2026-10-27) every part matched for each filter, each
account opened, no value report, and `now` moved from -24 h to +14 days. Getting there fixed one
real difference: Intl rounds a tie away from zero on the shortest decimal (`$1,246.5` is
`$1,247`, `1.005` is `1.01`), where Rust's `{:.0}` keeps the even digit.

The live page sizes meters, bars and timeline marks with `style` attributes, which Corten's CSP
blocks, so the app allows them for every page (`allow_style_attributes`); acceptable for a
loopback, read-only app, and filed as [corten#139 A size or position drawn from data needs style attributes, and allowing them opens style-src-attr 'unsafe-inline' for the whole app](https://github.com/todd-skelton/corten/issues/139).
The fixed inline styles became classes in `shadow/static/dash.css` (the live stylesheet, less
the sign-in drawer).

An unaligned check (the live service read its inputs at another moment than the files show) is
counted apart and never as a match. The shadow's tests (`cargo test --release -p pools-shadow`)
run each check over an install laid out from the golden inputs and require a match.

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
| `forecast/variants.json` | `buildForecast` over edited snapshot accounts: banked Claude refills (redeem now, drain first, kept plans), unlisted and unmodeled credits, policies and automation off, stale hold and expiry, pace reuse, no history, settings and rejected settings; with `forecastConfig` problems and `resetPolicies` | 19 |
| `value/deployed.json`, `value/deployed-session-usage.json` | `buildUsageReports`, deployed config | 1 |
| `value/short-windows.json` | the same with 3/2/5-day windows and 6-hour buckets | 1 |
| `value/quota-increments.json`, `value/cost.json`, `value/config.json` | `quotaIncrements` at the refill and jitter edges, `costOf` on name variants and price shapes, `valueConfig` on valid and rejected settings | 9 / 12 / 9 |
| `summary/now.json` | `buildSummary` (`GET /api/summary`) from the two above | 1 |
| `summary/cases.json` | the same 4 and 10 minutes on (freshness), and without the forecast, the value report or both | 5 |
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
   before accepting a tolerance for it. The forecast has one, `Math.pow(0.5, 1 / halfLifeHours)`;
   `f64::powf` matches it bit for bit here (Windows, MSVC runtime), and every forecast output
   matches with no tolerance.
4. Sorting: the JavaScript sorts are stable and break ties with `localeCompare` on ids. The ids
   here are ASCII, where `localeCompare` and byte order agree except for case and punctuation
   (ICU orders `-` and `_` differently from bytes); match it on these ids, not in general.