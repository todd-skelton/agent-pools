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
4. Writes and the public gateway, in the port only. In progress: the refresh cycle
   (`pool-core::refresh`, refresh.mjs with usage.mjs, quota-recovery.mjs, reset-lifecycle.mjs,
   notifications.mjs, projection.mjs and both adapters) takes its I/O through a trait and matches
   Agent Pools' own cycle on 42 scripted scenarios (61 cycles): every result, proxy call, event,
   sample, toast, save, hook call and final state (`node golden/refresh-traces.mjs`, then
   `cargo test -p pool-core --test refresh`). `pools-shadow run-copy` runs that cycle, the
   capacity publisher, the routing ladders and a manual refresh on a copied state directory
   (below). The public gateway, the inference relay and the backend relay with its usage
   overlay are ported (`gateway/`, below) on Corten's `Relay` and `Upgrade`.
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
| `/live?tab=&filter=&open=` | the open tab, sent again whenever it changes (server-sent events for `<corten-live>`) |
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
own. Each tab's body sits in a `<corten-live>` on `/live?tab=`, which sends the tab, with the
header's count and freshness, whenever it changes:

- a save of `pool-state.json`, the forecast or the value report is noticed within a quarter
  second (their modification times), and the live service's memory (`usageLedger`, from
  `/api/status`) is read every 5 s; either makes open tabs read their inputs again;
- in between, the tab is drawn again each second, so countdowns and "ago" times move;
- a redraw that comes out the same (style numbers to a tenth of a percent) is not sent. The
  cursor is a hash of the fragment, so a page that is current and a reconnect get nothing.

Measured on the live install: a pool-state save at 11:53:58.781Z was on an open page at
11:53:58.949Z (168 ms). The shadow can be no fresher than the live service's saves: usage is
checked every two minutes, and the ledger's row count moves every 10 s or so. The pages
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

### Writes on a copy

`shadow/src/writer/` is the write side, run only on a copy of the install's state:

```powershell
target\release\pools-shadow.exe make-copy shadow-state\copy   # reads the live install only
target\release\pools-shadow.exe run-copy shadow-state\copy    # http://localhost:8321
```

`make-copy` copies the state documents, the four `install.json` keys, the usage-sample logs and
the public price table (`model-prices.json`), and writes stand-in Claude auth files that hold only a fake organization id. `run-copy` refuses
the live install and any directory with `state/secrets.json` (`writer::copy::guard`), starts a
stand-in proxy on a free loopback port (answers from the copy's accounts, refuses every credit
claim, logs each call to `logs/fake-proxy.jsonl`), and then runs:

- the refresh loop: `pool-core::refresh` every 120 s, 30 s after a degraded or follow-up cycle
  (`SHADOW_COPY_REFRESH_SECS` overrides the normal delay), saving `pool-state.json`, events and
  usage samples into the copy;
- the capacity publisher after each save: the forecast rebuilt and published, then the routing
  ladders (`pool-core::ladder`, routing-ladder.mjs with model-calibrate.mjs's scores) from it,
  their file inputs read again after an hour, published to `state/routing-ladders.json` and
  served at `/api/ladders` (503 before there are any, as the live service answers);
- the dashboard, where Refresh is a form posting to `/refresh` (Corten's cross-site check
  applies: a cross-site POST gets 403). The read-only shadow keeps it disabled and answers 404.
- the proxy watch (pool-service.mjs `checkProxy`, every 10 s): the stand-in's health, its outage
  hold counters (`holds` on `/api/status`, `holdSince` saved with the state, start and clear
  logged and toasted), and the first refresh when the proxy was not up at start;
- the service's API: `GET /api/health`, `/api/capacity`, `/api/value`, and `POST /api/refresh`,
  `/api/login`, `/api/callback` behind pool-service.mjs's same-origin JSON check (8 KiB), the sign-in
  calls answered by the stand-in, which signs nothing in. The read-only shadow serves the
  documents and answers 404 for the rest;
- the desktop relay on `http://127.0.0.1:8322/backend-api` (`SHADOW_COPY_RELAY_PORT`; Agent
  Pools' is 8000): `gateway::backend` in front of the stand-in's `/backend-api`, never chatgpt.com,
  with `/wham/usage` rewritten by `pool_core::pool_usage` over the copy's saved accounts
  (`poolUsageMode` from the copy's install.json, else `capped`; make-copy does not copy that key).
  Plain HTTP, since the live install's `state/certs` is not copied (logged as
  `desktop-relay-tls-missing`, as Agent Pools does). A port that is taken is said once (event and
  toast) and tried again every 30 s until it frees, as `listenDesktopRelay` does. Overlay errors go
  to the copy's `logs/pool-events.jsonl`;
- the usage ledger (usage-ledger.mjs's drain, `writer::ledger`): the stand-in's `usage-queue`
  popped in batches of 1,000 every `drainIntervalSeconds` (10) while the proxy answers, rows
  (`pool_core::ledger`) appended to `logs/usage-ledger/raw-YYYY-MM-DD.jsonl` by UTC day, rows
  held across a failed write (up to 100,000), files pruned past `rawRetentionDays`, a problem
  logged once per change, stats as `usageLedger` on `/api/status` and the dashboard. The live
  proxy's queue is never read (reading it removes records). The stand-in makes up
  `SHADOW_STANDIN_REQUESTS_PER_HOUR` (30) requests on the copy's accounts, client `stand-in`,
  with quota headers from its drifting windows;
- the value report (`writer::report`): `writeValueReport` two minutes after the start, then every
  `reportIntervalMinutes` (60), writing `state/usage-value.json` and `state/session-usage.json`
  from the copy's ledger, samples, state and forecast. Not ported: `state/routing-attempts.json`
  (the join with the orchestrator's dispatch log, outside the install); the copied file stays;
- the inference relay on `http://127.0.0.1:8323/v1` (`SHADOW_COPY_INFERENCE_PORT`; Agent Pools'
  8319) and the public gateway on `127.0.0.1:8324` for `Host: pool-copy.localhost`
  (`SHADOW_COPY_GATEWAY_PORT`; Agent Pools' 8330 behind the tunnel), both `writer::edge` in front
  of the stand-in, which answers inference with a canned reply in each provider's shape (streamed
  when asked; the WebSocket for `/v1/responses` too) and queues each answer for the ledger. Every
  key is made up for the run and written to the copy's `stand-in/` (`keys.json` has the sign-in
  the relay accepts and the registered machine `copy-test`'s key). The desktop's
  `~/.codex/auth.json`, `state/secrets.json` and `state/remote-clients.json` are never read.

Not ported from pool-service.mjs, and why:

1. `POST /api/shutdown` (the guardian's graceful stop) and `closing` (503 for writes while
   stopping): a Corten app cannot start its own graceful shutdown
   ([corten#174 An app cannot begin its own graceful shutdown](https://github.com/todd-skelton/corten/issues/174)).
   The copy stops on Ctrl-C (tasks told, the ledger's last drain, requests finished).
2. Starting, adopting and restarting the proxy child (`child`, `adopt`, `planTakeover`) and
   `processes` on `/api/status`: the copy's proxy is the in-process stand-in. This is the live
   install's process management; it belongs to cutover.
3. `state/routing-attempts.json` (the value report's join with the orchestrator's dispatch log).

Measured on 2026-10-27: a POST at 13:11:12.40Z started a cycle at 13:11:12.58Z; cycles saved the
copy's pool state and forecast, and appended usage samples only for windows whose use changed.
The ladders the copy published at 14:01:41.935Z were byte for byte what Agent Pools' own
`buildLadders` makes from the copy's files at that moment
(`node golden/ladder-live.mjs shadow-state\copy 2026-10-27T14:01:41.935Z`).
At 16:07Z the copy's desktop relay answered `/backend-api/wham/usage` for no account id and for
each of the copy's 4 Codex account ids (200, 1,559 to 1,563 bytes, rewritten), each byte for byte
what Agent Pools' own `poolUsage` makes from the same stand-in body and saved accounts at a moment
within the request (`node golden/usage-relay.mjs`).

Ledger rows are held to Agent Pools' own functions over 230 queue records, 18 configs and 7 user
agents (`node golden/ledger-cases.mjs`, `cargo test -p pool-core --test ledger_rows`); the drain
to usage-ledger.test.mjs's three drain tests (`writer::ledger::tests`). A drain's write error
text is Rust's, not Node's.

The ladders are held to every `buildLadders` call in Agent Pools' routing-ladder.test.mjs (36,
recorded by `node --import ./golden/ladder-hook.mjs reference/agent-pools/routing-ladder.test.mjs`);
12 of its 13 tests pass there, the other reads pool-service.mjs, which is not copied, and makes
no `buildLadders` call)
and, when `AP_LADDER_LIVE` names a file from `node golden/ladder-live.mjs`, to the live
install's own inputs (398 rungs; not committed, since its inputs are not scrubbed):
`cargo test -p pool-core --test ladder`.

### The public gateway

`gateway/` (`pools-gateway`; `run-copy` serves it in front of the stand-in) is public-gateway.mjs on Corten (`77bcbbb`, with
[corten#162 Upgrade and Relay](https://github.com/todd-skelton/corten/pull/162)). Inference routes
take a machine key and relay the pool's answer as it streams, sent with the pool's key. The
dashboard routes take a session cookie signed as Agent Pools signs it (the same secret keeps
today's cookies valid; a test holds the token to Agent Pools' bytes). Everything else is closed.
`cargo test -p pools-gateway` runs Agent Pools' 12 gateway tests over real sockets, plus four of
the port's own: the session token, a client that leaves a stream (the upstream request ends
with it), one that holds Corten's own failures under `/v1` to the gateway's shape, and one that holds a
relayed page to its own CSP.

Where it differs, by design or because of Corten:

1. Who is asking is Corten's `ClientIp` (the nearest `X-Forwarded-For` address behind the
   tunnel on loopback), where Agent Pools reads `CF-Connecting-IP`. cloudflared sets both.
2. A name that is not the gateway's gets Corten's 421 before routing (Agent Pools: 404).
   `localhost` and IP addresses, which Corten answers, get the 404.
3. Failures Corten answers itself under `/v1` (unknown URL, wrong method, cross-site write) are
   written in the gateway's Anthropic-style shape by `Api::errors`
   ([corten#171](https://github.com/todd-skelton/corten/pull/171), for
   [corten#163 An API cannot shape the failures Corten answers itself](https://github.com/todd-skelton/corten/issues/163)),
   with Corten's statuses: a wrong method is 405 where Agent Pools answers 404. Outside `/v1` they
   are the pages' (the HTML error page). Machine keys are still checked in the handlers; with
   `Api::errors` they can move into an `Identify`, not done yet.
4. A relayed page keeps its own Content-Security-Policy
   ([corten#172](https://github.com/todd-skelton/corten/pull/172), for
   [corten#164 A Relay loses the upstream's Content-Security-Policy](https://github.com/todd-skelton/corten/issues/164)).
   With `AP_GATEWAY_LIVE=http://127.0.0.1:8318`, a test signs in to the gateway and reads the live
   service's own dashboard through it (GETs only): the page's 102,700 bytes and its own policy
   arrive unchanged, and `/api/status` lists 7 accounts (measured 2026-10-27). Not loaded in a
   browser: the gateway answers only its public hostname.

### The inference relay

`gateway::inference` is inference-relay.mjs (with inference-auth.mjs's `fileCredentials`): Codex
on this machine sends `/v1/responses` over HTTP or a WebSocket (Corten's `Upgrade`, frames by
tokio-tungstenite) with its own sign-in. The relay checks the sign-in and sends the request on
with the pool's key. Every WebSocket frame is admitted again, so signing out ends an open socket.
A refused connect is retried while the pool restarts. `cargo test -p pools-gateway --test
inference` runs Agent Pools' 22 relay tests over real sockets, plus the head timeout, (its 5 tests of Codex's config
file are not the relay's). Disabling the per-frame admission check fails the sign-out test.

Differences: Corten's 426, 404, 405 and 421 where Agent Pools answers 404 or 403 (`src/inference.rs`
lists them). The server's limits are Corten's
([corten#173 App::header_timeout and App::max_connections](https://github.com/todd-skelton/corten/pull/173), for
[corten#170 A client that never finishes its request head holds a connection open forever](https://github.com/todd-skelton/corten/issues/170)):
a request head that takes over 30 s closes the connection without an answer (Node answers 408),
and past 128 connections the rest wait in the system's queue (Node closes them). A port test
holds a half-sent head to the timeout; turning the timeout off fails it. On Windows a
refused loopback connect takes about 2 s (measured 2,076 ms), so the restart test brings the pool
up after 3 s rather than Agent Pools' 0.6 s.

### The backend relay and the usage overlay

`gateway::backend` is backend-relay.mjs, the desktop relay: everything under `/backend-api/` goes
to the upstream (chatgpt.com in service) with the caller's own credentials and streams back, and a
WebSocket handshake is relayed with `Upgrade::switch` and then tunneled byte for byte. Only
`GET /backend-api/wham/usage` is rewritten, by `pool_core::pool_usage` (pool-usage.mjs), so the
desktop's banner and usage panel show the pool. `cargo test -p pools-gateway --test backend` runs
Agent Pools' 6 relay tests over real sockets plus one for the host and cross-site checks; a tunnel
that stops passing bytes fails the echo case. `cargo test -p pool-core --test pool_usage` replays
all 38 `poolUsage` calls Agent Pools' 16 pool-usage tests make (recorded by
`node --import ./golden/usage-hook.mjs reference/agent-pools/pool-usage.test.mjs`), byte for byte;
moving the grace boundary or the "available" verdict's floor fails 3 of them.

Differences (`src/backend.rs` lists them): a switch the relay cannot carry is answered (Corten's
404, a 502, the host check's 403) where Agent Pools drops the connection; the host check covers
switches too; a browser's cross-site write or switch is Corten's 403 unless
`Config::allow_origins` names its origin. TLS for the listener is the program's (`serve_on`), with
`load_relay_tls` reading `state/certs` as Agent Pools does.

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