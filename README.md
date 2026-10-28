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
4. Writes and the public gateway, in the port only. Done on a copy, short of cutover (2026-10-28): the refresh cycle
   (`pool-core::refresh`, refresh.mjs with usage.mjs, quota-recovery.mjs, reset-lifecycle.mjs,
   notifications.mjs, projection.mjs and both adapters) takes its I/O through a trait and matches
   Agent Pools' own cycle on 42 scripted scenarios (61 cycles): every result, proxy call, event,
   sample, toast, save, hook call and final state (`node golden/refresh-traces.mjs`, then
   `cargo test -p pool-core --test refresh`). `pools-shadow run-copy` runs that cycle, the
   capacity publisher, the routing ladders and a manual refresh on a copied state directory
   (below). The public gateway, the inference relay and the backend relay with its usage
   overlay are ported (`gateway/`, below) on Corten's `Relay` and `Upgrade`. The model registry's
   build (`pool-core::registry`, model-registry.mjs: versions, lineage, efforts, the probe plan,
   the admission judges and the routing view) matches every call Agent Pools' own registry tests
   make (50), and `pools-shadow check-registry` rebuilds the live `state/model-registry.json`
   from its inputs, reading only (measured 2026-10-27: 20 models, generation 7, the same document
   and digest). The hourly refresh and the recheck run on the copy, probing through the stand-in
   rather than the real `codex` and `claude` tools (below). The supervisor's decisions
   (`pool-core::supervise`: health-policy.mjs's restart policies and process identity,
   proxy-takeover.mjs's adopt, spawn or wait plan) pass Agent Pools' own tests of them
   (`cargo test -p pool-core --test supervise`, 12, from platform.test.mjs and
   proxy-takeover.test.mjs, copied to `reference/agent-pools/`).
5. Cutover: only with Todd's go-ahead. Ready to rehearse (2026-10-28): the live commands
   (`guardian`, `serve-service`, `serve-inference`, `serve-gateway`, below) run each Node
   process's job with its ports, files and event logs, and were rehearsed on a made-up install
   with the stand-in as a separate proxy process. The install (`deploy/`, below) and every
   cutover step, forward and back, were rehearsed against Agent Pools' own Node processes on
   the same made-up install. Done live (2026-10-28, version 0.1.0-bff8a30): gateway at
   15:46Z (about 16 s down), inference relay at 15:56Z (about 14 s), service with the tailnet
   dashboard at 16:00Z (about 10 s, proxy adopted, not restarted). Node's tasks for those four
   jobs are disabled, not removed. Model refresh, model recheck, backup and the cloudflare
   tunnel still run on Node.

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
the read-only shadow shows them disabled; on a copy they work (below).

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
the public price table (`model-prices.json`), the model refresh's inputs (`model-families.json`,
the proxy's `state/live-models.json`, the lineage, the admission evidence and the calibration
preview), and writes stand-in Claude auth files that hold only a fake organization id. `run-copy` refuses
the live install and any directory with `state/secrets.json` (`writer::copy::guard`), starts a
stand-in proxy on a free loopback port (answers from the copy's accounts, refuses every credit
claim, logs each call to `logs/fake-proxy.jsonl`), and then runs:

- the refresh loop: `pool-core::refresh` every 120 s, 30 s after a degraded or follow-up cycle
  (`SHADOW_COPY_REFRESH_SECS` overrides the normal delay), saving `pool-state.json`, events and
  usage samples into the copy;
- reset claims (refresh.mjs `processReset`) within each cycle: a claim is journaled and saved
  before it is sent, and the stand-in refuses it (`no_credit` for Codex, `ineligible` for
  Claude), so the credit is backed off for 30 minutes and the next one is tried; nothing is
  spent (`an_exhausted_pool_claims_a_reset_on_the_copy_and_the_refusal_backs_that_credit_off`);
- the capacity publisher after each save: the forecast rebuilt and published, then the routing
  ladders (`pool-core::ladder`, routing-ladder.mjs with model-calibrate.mjs's scores) from it,
  their file inputs read again after an hour, published to `state/routing-ladders.json` and
  served at `/api/ladders` (503 before there are any, as the live service answers);
- the dashboard, where Refresh is a form posting to `/refresh` (Corten's cross-site check
  applies: a cross-site POST gets 403), and so is each "Refresh", "Retry now" and "Refresh
  usage" (a row's comes back to that row). The read-only shadow keeps them disabled and answers 404;
- the dashboard's sign-in (`shadow/src/dash/signin.rs`): "Add account", "Connect ...", "Sign in
  again" and "Reconnect" open `/signin`, the live page's drawer as a page with no script of the
  app's own: choose a provider, "Open ... sign-in" (`POST /signin`, the proxy's
  `<loginId>-auth-url`), then the provider's page as a link, and a box for the localhost link a
  phone ends on (`POST /signin/finish`, checked as `finishSignin` checks it, then the proxy's
  `oauth-callback`) or "Cancel sign-in". The pending sign-in (`pendingSignin`, 15 minutes) is
  kept on the server, so the "Finish sign-in" banner is on every open page, and `waitForSignin`
  (a refresh now and every fourth of up to 24 checks) runs whether a page is open or not; a new
  account or a cleared sign-in confirms it ("Signed in: ... is connected", on the banner for two
  minutes). The stand-in signs nothing in, so on the copy a sign-in is submitted and never
  confirmed. Differences: the provider's page is a link rather than a tab the script opens (what
  the live page shows when its tab is blocked), and there is no "Paste from clipboard" button or
  submit-on-paste. Measured on 2026-10-27: Codex and Claude sign-ins started from the copy's
  pages reached the stand-in (`codex-auth-url`, `oauth-callback`, `anthropic-auth-url`), each
  start and a row's "Refresh usage" ran a cycle, and the banner followed a navigation;
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
  from the copy's ledger, samples, state and forecast, and `state/routing-attempts.json`
  (`writer::attempts`, `buildRoutingAttempts`: each dispatch in the orchestrator's
  `dispatch-log.jsonl` joined to the session its transcript started). make-copy does not carry
  install.json's `orchestratorRoot`, so the copy writes Agent Pools' "no orchestratorRoot" report
  unless one is added to the copy's install.json. Held to Agent Pools' own function over a fixture
  orchestrator (`node golden/routing-attempts.mjs`: 4 cases, 11 attempts), and to the live file:
  `pools-shadow check-attempts` rebuilds the live `state/routing-attempts.json` from the live
  `state/session-usage.json` and the orchestrator's log, reading only (measured 2026-10-27: 5,481
  attempts, the same document);
- the model refresh (`writer::models`, model-refresh.mjs with model-store.mjs): three minutes
  after the start and then hourly (`SHADOW_COPY_MODEL_FIRST_SECS`, `SHADOW_COPY_MODEL_REFRESH_SECS`),
  under `state/model-registry.json.lock` (renewed while held, taken over only when stale, released
  only by its holder), the registry built from the live model lists, the copy's own `/api/status`,
  the families, the evidence and the lineage; the efforts the plan lacks probed (16 at most, least
  recently tried first, none started after 20 minutes); built again and written atomically with
  the lineage. Every five minutes (`SHADOW_COPY_MODEL_RECHECK_SECS`) the recheck rebuilds without
  probes while a family reports an unavailable effort. Output goes to the copy's
  `logs/model-refresh.log`. The probes never run `codex` or `claude` (Agent Pools' do, and that
  is a provider call): `writer::models::StandIn` sends the admission prompt to the stand-in, which
  answers it `OK`, queues it for the ledger and logs the pool's selector and status lines to the
  copy's `logs/main.log`; the answer is judged as the tool's output with Agent Pools' judges and,
  for Codex, its routing evidence from that log. Rows carry `standIn: true`, and the harness
  versions are the ones the copied registry recorded, so copied evidence keeps counting. The
  benchmark and arena fetches (external sites, one with a key) are not ported; the copy runs as
  `benchmarks: false` and keeps the snapshot it was copied with. The stand-in's model lists are
  re-read hourly and on `POST live-models/refresh` (which `/api/refresh` sends). Agent Pools'
  lock and recheck tests are ported (`writer::models::tests`), and
  `a_copy_admits_models_through_the_stand_in_and_publishes_its_registry` runs a refresh end to end;
- the guardian's stop: `POST /api/shutdown` with the run's management key as a Bearer
  (`managementKey` in `stand-in/keys.json`; anything else is 403 `Unauthorized`) answers 202,
  logs `supervisor-stopping`, sets `closing` on `/api/health`, and begins Corten's graceful
  shutdown (`corten::shutdown`, [corten#178 corten::shutdown](https://github.com/todd-skelton/corten/pull/178) for [corten#174 An app cannot begin its own graceful shutdown](https://github.com/todd-skelton/corten/issues/174)):
  writes get 503 `Service stopping`, the tasks are told (a cycle in progress finishes, the
  ledger drains a last time), and every listener closes. A signal stops the copy the same way.
  Measured on 2026-10-27: after the 202, the dashboard, desktop relay, inference relay and
  gateway refused connections within 117, 148, 168 and 192 ms, and the process exited;
- the inference relay on `http://127.0.0.1:8323/v1` (`SHADOW_COPY_INFERENCE_PORT`; Agent Pools'
  8319) and the public gateway on `127.0.0.1:8324` for `Host: pool-copy.localhost`
  (`SHADOW_COPY_GATEWAY_PORT`; Agent Pools' 8330 behind the tunnel), both `writer::edge` in front
  of the stand-in, which answers inference with a canned reply in each provider's shape (streamed
  when asked; the WebSocket for `/v1/responses` too) and queues each answer for the ledger. Every
  key is made up for the run and written to the copy's `stand-in/` (`keys.json` has the sign-in
  the relay accepts and the registered machine `copy-test`'s key). The desktop's
  `~/.codex/auth.json`, `state/secrets.json` and `state/remote-clients.json` are never read.

#### Answering from memory

A copy answers as pool-service.mjs does, from what it holds (`writer::memory`): the pool state as
the refresh cycle last saved it, the forecast and routing ladders as last published, the value
report as last written, and install.json as read at the start (reset policies as the last cycle
loaded them). No request reads a file. Each change tells open dashboard pages to redraw
(`notice-memory`), so a copy no longer looks at file times. The desktop relay's usage overlay reads
the same accounts.

`/api/status` and `/api/summary` depend on the moment only through each account's three readings
(usage, routing, credits: fresh for five minutes), so they are built once per change and kept
(`served::Answers`) until an input changes or a reading turns stale; `/api/summary`'s
`generatedAt` is set into the kept bytes per request. `/api/capacity` and `/api/ladders` are
written out once per publication, and `/api/value` sends the bytes the report task wrote. The
read-only shadow keeps the same answers over the live install's files, each parsed once per save
(`sources::Install::doc`). Measured on 2026-10-28 (release build, 300 sequential requests, server
CPU per request, the copy before and after, then live `pool-service.mjs`):

| Route | Before (parse cache) | From memory | Live |
|---|---|---|---|
| `/api/summary` | 1.09 ms | 0.36 ms | 0.94 ms |
| `/api/status` | 2.14 ms | 0.16 ms | 0.57 ms |
| `/` | 2.24 ms | 0.63 ms | 0.78 ms (a static file) |
| `/api/capacity` | 0.68 ms | 0.10 ms | 0.16 ms |
| `/api/ladders` | 3.70 ms | 0.10 ms | 0.63 ms |
| `/api/value` | 0.42 ms | 0.10 ms | 0.78 ms |

#### Compression

The shadow builds Corten (`1149c5a`) with `compress`. Pages and JSON are compressed per request (zstd 1,
brotli 2 or gzip 1, by the client's `Accept-Encoding`). The dashboard's live stream is compressed
as one stream per connection, so a redraw that repeats the last one costs only what changed.
pool-service.mjs compresses nothing. Measured on 2026-10-28:

1. `/api/status` is 30,229 bytes as it is and 4,547 with zstd.
2. A `/live?tab=overview` stream over 120 s received 8 patches, 149,769 bytes uncompressed.
   Ten zstd streams in the same window received 8.2 to 8.3 KB each.
3. The process's working set rose from 45.0 MB idle to 54.3 MB with the eleven streams open
   (Corten holds a 480 KiB zstd context per stream). Server CPU was 578 ms per 120 s idle and
   1,859 ms per 120 s with the streams; a refresh cycle can fall in either window.

Cargo features are unified per build, so the gateway and relays in the same binary compress
too. A relayed `/v1/responses` stream asked for with `Accept-Encoding: zstd` comes back
`content-encoding: zstd` (314 bytes against 726), where Agent Pools' relays pass the stream as
it is. That was [corten#205 With compress on, a Relay's answer is re-encoded per connection, so a gateway holds a 480 KiB coder for every relayed model stream and cannot opt out](https://github.com/todd-skelton/corten/issues/205);
since `1149c5a` ([corten#240 A Relay's answer is passed on as it came with compress on](https://github.com/todd-skelton/corten/pull/240))
a `Relay`'s answer passes as it came, and `Relay::compressed()` opts back in.

### Live mode

`shadow/src/live/` runs Agent Pools' own processes, one command each, on an install named on
the command line (nothing defaults to the live one):

| Command | Agent Pools | Ports (install.json, default) |
|---|---|---|
| `guardian <root>` | pool-guardian.mjs | none |
| `serve-service <root>` | pool-service.mjs, with the desktop relay and the proxy child | `supervisorPort` 8318, `desktopRelayPort` 8000 (TLS) |
| `serve-inference <root>` | inference-service.mjs | `inferenceRelayPort` 8319 (TLS) |
| `serve-gateway <root>` | public-gateway-service.mjs | `publicGateway.port` 8330 |

`Install::open` accepts the live install only with `--live`, refuses `--live` on any other
root, and opens any other root only as a rehearsal (`state/rehearsal.json`) apart from the
live install. `pools-shadow make-rehearsal <dest>` makes one: make-copy's files plus made-up
secrets, desktop sign-in, machine key and TLS pair (rcgen, in no certificate store), ports
8340-8344, and this program at `proxy/0.0.0-standin/cli-proxy-api.exe`, which runs as the
stand-in when started as the proxy is (`-config <root>\proxy.yaml`) and refuses any other
config. The model refresh stays with Agent Pools' scheduled tasks.

The proxy child keeps Node's record format (`state/process-proxy.json`, `{pid, creationDate}`),
so either supervisor adopts what the other started. The port's `portOwner` reads the TCP
table (`GetExtendedTcpTable`); `Get-NetTCPConnection` took 27 s here, past the 20 s lookup
limit, and a takeover without a record then waited forever (`port-held-unidentified`). Its
`powershell` pipes the child's output; without that the CIM lookups read nothing, every spawn
logged `process-record-error`, and `a_running_process_reads_with_its_identity` caught it.

Rehearsed on 2026-10-28 (`shadow-state/rehearsal`, scripts `target/rehearse-*.ps1`):

1. The guardian started the service, which spawned the stand-in and wrote its record.
   `/api/health`, the dashboard and `/api/summary` answered 200, and so did the desktop relay over
   HTTPS (plain HTTP failed). The inference relay over HTTPS answered `POST /v1/responses` 200
   with the desktop token and 401 with another. The gateway gave 200 with the machine key and
   401 with another.
2. A service started with the proxy running and no record found it by its port, adopted it
   (`process-adopted`) and wrote the record.
3. The adopted proxy, ended, was gone within 10 s (`process-exit`, `adopted`) and a new one was
   spawned with a new record. The spawned one, ended, came back after 10 s (`process-exit`,
   `retryInMs` 10000).
4. `state/guardian-stop.request` stopped the guardian, which stopped the service through
   `/api/shutdown` (`supervisor-stopping`). The proxy kept running, and the next guardian's
   service adopted the same PID from the record.
5. The service, ended, was restarted by the guardian (`supervisor-exit`), and the new one
   adopted the same proxy.

Resources, measured over 60 s on 2026-10-28 at about 13:19Z. The Node processes serve the real pool,
and the port serves the rehearsal with no outside traffic, so CPU is not like for like:

| Process | Node working set / private | Port working set / private | CPU per 60 s, Node / port |
|---|---|---|---|
| guardian | 20.2 / 73.6 MB | 11.3 / 2.0 MB | 62 / 47 ms |
| service | 74.8 / 124.6 MB | 35.9 / 26.3 MB | 1,031 / 516 ms |
| inference relay | 58.3 / 110.9 MB | 10.9 / 2.2 MB | 2,359 / 0 ms |
| public gateway | 6.2 / 76.1 MB | 9.9 / 2.2 MB | 0 / 0 ms |

### Install and cutover

`deploy/` holds three scripts and the wrapper:

1. `Install-Pool-Rs.ps1 -Root <install> -Exe <pools-shadow.exe> -Version <v> [-RegisterTasks]`
   copies the build and its static files into `<install>\pool-rs\<v>` (with `build.json`: SHA-256
   and commit). A version is never replaced. It writes `pool-rs\Run-Pool-Rs.vbs` and
   `pool-rs\pool-rs.json` (`current`, `previous`, `tailnet`), and registers
   "Agent Pools (rs) - subscriptions", "- inference relay" and "- public gateway" as Agent Pools
   registers its own (logon plus a 5-minute keepalive, IgnoreNew; the relays restart on
   failure, the gateway at priority 4). New tasks start disabled. `-Use <v>` points
   `current` back at an installed version. Agent Pools' own files are not changed; the
   service reads its tailnet view from `pool-rs.json`, not tailnet-dashboard-proxy.mjs.
2. `Run-Pool-Rs.vbs <command>` starts `pool-rs\<current>\pools-shadow.exe <command> <install>`
   through wscript with no console, waiting, as Run-Pool-Guardian.vbs starts node. It adds
   `--live` unless the install is a rehearsal.
3. `Cutover-Pool-Rs.ps1 -Root <install> -Step gateway|inference|service [-Undo] [-Tailnet]`
   moves one job between the Node task and the port's. It disables the Node task (never deletes
   it) and stops the Node process: for the service through `state\guardian-stop.request`, which
   leaves the proxy running. It then enables and starts the port's task and waits for the port
   to listen, the health check to pass and, for the service, the same proxy PID to be adopted.
   `-Undo` is the same step backwards. A step that is already done is safe to run again.
   `-WhatIf` changes nothing.

Both scripts refuse the live install without `-Live`, and any other root that is not a rehearsal.
Installed programs find their static files beside themselves (`static\shadow`,
`static\gateway`). Corten has no way to build them in:
[corten#268 Static files are a directory found at run time, so a built program cannot be deployed by itself and each app writes its own lookup](https://github.com/todd-skelton/corten/issues/268).

Rehearsed on 2026-10-28 on `shadow-state/rehearsal`. Agent Pools' own `.mjs` files were copied
there and run by rehearsal tasks ("Agent Pools rehearsal (node) - subscriptions", "- public
gateway") on the rehearsal's ports, against the stand-in proxy. The port's tasks were
"Agent Pools rehearsal (rs) - ...". Times are measured:

1. Node's service adopted the proxy the port had started (PID 142292) from its record.
2. Gateway: Node to the port with an outage of 17 s (stopped 14:07:30Z, the port listening and
   answering at 14:07:47Z). Back to Node took 16 s, and forward again was a no-op. There is no
   Node inference relay on a rehearsal (it reads the desktop's sign-in), so that step was the
   port's side only: listening and `/health` 200 in 41 s, most of it waiting on lookups.
3. Service, three times (forward, back, forward): Node's guardian stopped on the stop file in
   4-5 s, and the next service adopted proxy 142292 each time ("adopted, not restarted"). The
   port's `/api/health` was down for 27-40 s per handover, Node's for 36 s.
4. Killing the guardian and the service together left the pool down until the next 5-minute
   keepalive (185 s in one run, 203 s in another), then it came back on the same proxy. The
   same holds for Agent Pools: its guardian is not watched by anything but that trigger.
5. A cold start from the task to `/api/health` 200 took 25.7 s. Of that, 16.8 s passed before the
   guardian process existed (Task Scheduler and wscript) and 8 s more before the service did.
   Where those 8 s go is not yet measured; the guardian only reads the secrets and asks
   `/api/health` (1 s timeout) before it starts the service.

Found and fixed while rehearsing: the scripts read the port's owner from `netstat`, since
`Get-NetTCPConnection` took 27 s per call here. They also match the service's command line with
the install path unquoted, as the guardian starts it.

### The tailnet view

`shadow/src/tailnet.rs` is tailnet-dashboard-proxy.mjs without a proxy. Agent Pools runs a
proxy on 127.0.0.1:8327 behind `tailscale serve` that rewrites `Host` and `Origin`, forwards
the dashboard's reads, and forwards `login`, `callback` and `refresh` only from the tailnet
page's origin and the owner's `Tailscale-User-Login`. Here `SHADOW_TAILNET_ORIGIN`
(`https://<machine>.<tailnet>.ts.net:8443`) and `SHADOW_TAILNET_OWNER` make the app answer the
tailnet name itself (`Policy::host`, `Policy::allow_origin`), and every write the dashboard
makes (the refresh and sign-in forms, and `/api/refresh`, `/api/login`, `/api/callback`) takes a
`Caller<Owner>`: anyone on this PC, and from the tailnet only the owner's login (403 "Owner
tailnet identity required" otherwise). A request carrying a Tailscale identity when no tailnet is
set is refused a write. `tailscale serve` would point at the app's port instead of 8327; that
change is part of cutover and was not made. Differences: every read the pages use is served on
the tailnet (their live updates, assets and the JSON reads), where the proxy forwards a fixed
list; a refused write is Corten's error page (JSON for a JSON client), where the proxy answers
JSON. `the_tailnet_reads_and_only_its_owner_writes` holds the rules through the router; an
`Owner` that is always local fails it.

Not ported from pool-service.mjs, and why:

1. Starting, adopting and restarting the proxy child (`child`, `adopt`, `planTakeover`) and
   `processes` on `/api/status`: the copy's proxy is the in-process stand-in. Live mode has
   them (below).

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

`gateway/` (`pools-gateway`; `run-copy` serves it in front of the stand-in) is public-gateway.mjs on Corten (`1149c5a`, with
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
   are the pages' (the HTML error page).
4. The machine key is Corten's `Caller<Machine>` (`caller = Machine` on the `/v1` routes, so a
   route without it does not compile), with Agent Pools' words on its refusals
   (`Refused::message`, `Refused::too_many`;
   [corten#177 A Caller refusal cannot say why](https://github.com/todd-skelton/corten/issues/177)):
   401 `Invalid API key`, and 429 `Too many failed attempts; try again later` before the key is
   looked at. Two differences: the 429 also sends `Retry-After` (seconds until the address's
   15-minute window ends), and a malformed `/v1/models/{id}` is a 404 only once the key is good
   (Agent Pools answers it before the key).
5. A relayed page keeps its own Content-Security-Policy
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

Differences: Corten's 404, 405 and 421 where Agent Pools answers 404 or 403 (`src/inference.rs`
lists them). A `GET /v1/responses` that does not ask to switch gets the relay's own 404, as Agent
Pools answers it ([corten#176 Option<Upgrade> and SwitchOr](https://github.com/todd-skelton/corten/pull/176)). The server's limits are Corten's
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
| `registry/cases.json` | every `model-registry.mjs` call in `model-registry.test.mjs` (`TZ=UTC node --import ./golden/registry-hook.mjs reference/agent-pools/model-registry.test.mjs`; the hook records each call, Sets as arrays) | 50 |
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