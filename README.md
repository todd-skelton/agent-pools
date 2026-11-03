# Agent Pools on Corten

Agent Pools in Rust on Corten: the program that runs the pool (`pools-rs.exe`), the scripts
that install and operate it (`deploy/`, `ops/`, `proxy-build/`, `remote-client/`) and the evidence that
it does what the Node version did (`golden/`, `reference/agent-pools/`). The live install is
`D:\Users\ToddS\AgentPools`; since 2026-11-01 it holds only the pool's data (`auth\`, `state\`, `logs\`,
`proxy.yaml`, `model-*.json`, `calibration-rules.json`), the installed builds (`pool-rs\`, `proxy\`) and the
proxy's source and toolchain (`proxy-source\`, `build\`). The Node code was removed then (below).

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
   tunnel still ran on Node then; the tunnel and backup moved later that day (`tunnel`, `backup`),
   and the model refresh and recheck into the service (`Cutover-Pool-Rs.ps1 -Step models`).
6. Node retired (2026-11-01): the port had run every job for four days. Node's eight disabled
   tasks were exported (`D:\Users\ToddS\AgentPools-backups\node-tasks-20261101\`) and removed, the
   `.mjs`, `.vbs` and Node-only scripts deleted from the install, and its git history kept as
   `AgentPools-backups\agentpools-node-history-20261101.bundle`. What was still only there moved
   here: the operator scripts (`ops/`), the proxy patch and build (`proxy-build/`), the remote
   machine installer (`remote-client/`) and the last `dashboard.html` (`reference/agent-pools/`).
   `Cutover-Pool-Rs.ps1`, which moved jobs between Node's tasks and the port's, went with them.

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

`golden/dash-text.mjs` draws every part with Agent Pools' page's own JavaScript (its last
`dashboard.html`, kept in `reference/agent-pools/`; `DASHBOARD_HTML` names another) and with
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

The live page sizes meters, bars and timeline marks with `style` attributes. Here each is a
custom property its component registers with `@property` and sets from data
(`style={WIDTH.pct(..)}`, written as `data-vars`): Corten puts the values in one style block
allowed by its hash, and after a live update the runtime sets them, so no page allows style
attributes ([corten#139 A size or position drawn from data needs style attributes, and allowing them opens style-src-attr 'unsafe-inline' for the whole app](https://github.com/todd-skelton/corten/issues/139),
closed). `dash-text.mjs` compares the live page's `width:42%` with the shadow's
`--meter__width:42%`. The markup is the design system's components (below).

### The design system

The dashboard is drawn with Corten's UI kit (`corten ui`, copied into the app as source) themed
to Agent Pools, and Agent Pools' own components on the same tokens:

| Layer | Where | What |
|---|---|---|
| Theme | `shadow/tokens/` (`ui.tokens.json` light, `ui-dark.tokens.json`, `ui.resolver.json`) | the kit's token names (`--background`, `--card`, `--primary`, `--muted-foreground`, `--border`, `--input`, `--ring`, `--radius`, ...) in Agent Pools' colors, and Agent Pools' own: `--success`, `--warning`, `--info`, `--subtle-foreground`, `--claude`, `--codex`, `--font-sans`, `--font-mono` |
| Kit | `shadow/src/ui/` (`ui.rs` lists them) | `Button`, `Card`, `Table`, `Alert`, `Badge`, `Input`, from `corten ui add` |
| Design system | `shadow/src/design/` | `Chip`, `ProviderDot`, `StatusDot`, `Meter`, `Stats`/`Stat`, `Kpi`, `Eyebrow`, `Segmented` |
| Dashboard | `shadow/src/dash/views.rs`, `signin.rs` and a stylesheet per component beside them | the top bar, attention, provider cards, timeline, the four tabs and the sign-in steps |
| Page | `shadow/static/base.css` | the theme's page colors, the font and element defaults |

Each text color meets WCAG AA (4.5:1) on the surfaces it is drawn on, and `--input` 3:1, in both
themes. The live page's quietest text (`#677c8d`, 3.98:1 on a card) and field border (`#2e4152`,
1.64:1) did not; `--subtle-foreground` (`#7a8fa1`) and `--input` (`#56708a`) replace them. The
dashboard is dark (`data-theme="dark"` on `<html>`); the light theme is the same files without
the attribute. The summary and parity pages follow the system's theme.

The kit's copies are the app's, edited where Agent Pools differs, and each edit is said at the top
of the file: `card` (one header row with a divider, `CardAction`, the dashboard's density),
`table` (dense rows, small uppercase headings, ellipsis), `alert` (`Info`, `Success`, `Warning`,
the role chosen where it is used, `AlertActions`), `badge` (`Success`, `Warning`). `button` and
`input` are as the kit ships them. Run `corten ui diff` in `shadow/` to see the edits and what a
newer kit changed. What the kit lacked is filed against Corten (below, Gaps).

#### Gaps

Filed against Corten from this port; each is worked around here as said:

- [#571 The kit has no meter, so every app draws a share used as a bar of divs with no meter role](https://github.com/todd-skelton/corten/issues/571): `design::Meter`, which has no meter role yet.
- [#572 The kit has no labelled figure (stat) or status dot, so a dashboard writes its own and names status only by color and title](https://github.com/todd-skelton/corten/issues/572): `Stats`, `Kpi`, `StatusDot`, `Chip`.
- [#573 Kit Card header has no slot for actions, so a card with a button in its header is an edit to the copied card](https://github.com/todd-skelton/corten/issues/573): `CardAction` in the copy.
- [#574 Kit tabs styles only TabGroup, so a nav of filter links (aria-current) has no segmented look](https://github.com/todd-skelton/corten/issues/574): `design::Segmented`.
- [#575 Kit Table has no stacked layout for phones, so a wide table only scrolls sideways or each app rewrites it as cards](https://github.com/todd-skelton/corten/issues/575): the phone rules in `accounts_tab.css`.
- [#576 Sizes from data (data-vars) in markup placed with raw() never reach the page's style block, and nothing says so](https://github.com/todd-skelton/corten/issues/576): `dash::Body`, rendered inside each response.
- Already open, with this port's case added: [#552 Kit Badge and Alert have no success, warning or info tone, and every Alert is announced as role=alert](https://github.com/todd-skelton/corten/issues/552) (the tones and `AlertRole` in the copies) and [#561 Kit control heights, spacing and type are literals, so a theme cannot give every control a 44 px touch target without editing each copied file](https://github.com/todd-skelton/corten/issues/561) (the density edits to `card.css` and `table.css`). Also met: [#551 corten ui init on an app that already has tokens adds a second theme instead of mapping onto the app's](https://github.com/todd-skelton/corten/issues/551) (init looked for `App::new` in `lib.rs`, and its `color-scheme` block was moved to `base.css`).

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
  `benchmarks: false` and keeps the snapshot it was copied with (the live service takes them:
  [Live mode](#live-mode)). The stand-in's model lists are
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

The shadow builds Corten with `compress` (`1149c5a` when measured; `3b19711` now). Pages and JSON are compressed per request (zstd 1,
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
config.

The model refresh runs in the service when `pool-rs/pool-rs.json` says `"models": true` (or
`serve-service` is given `--models`; a rehearsal never runs it): hourly at :15 and the recheck
every five minutes, one at a time, as "Agent Pools - model refresh" and "- model recheck" ran
`Run-Model-Refresh.vbs`, with the same lock, logs (`logs/model-refresh.log`,
`logs/model-recheck.log`), evidence rows and registry. A refresh that overruns skips the marks
it missed rather than drifting. The parts that were not ported for the copy are here:

- admission probes with the real tools (`live::admission::Native`, model-admission.mjs): the
  first `codex.exe`/`claude.exe` on `PATH`, no shell, the prompt on stdin, 240 s each, in a
  directory of its own under the temp directory that is removed after; Codex's verdict needs
  the pool's routing line in `logs/main.log`. Probing waits while a
  `controller-release-battery.ps1` runs (found with the process list, not CIM). A tool's
  `--version` is read again only when its executable changes, with a 90 s limit: Node read it
  every run with 30 s, and its timing out failed the whole refresh about 13 times since 10-19;
- the benchmark snapshots (`writer::benchmarks`, model-benchmarks.mjs and model-arena.mjs),
  once the last is 20 hours old, started only in the run's first 26 minutes and stopped at 38:
  Artificial Analysis (the key from `state/secrets.json`, never logged; every page of the cost
  endpoint at one index version, or nothing published) and Arena (the Hugging Face dataset
  pinned to the commit it read; Parquet read with the `parquet` crate, rows only, Snappy),
  each an immutable file published by hard link, then its pointer written atomically.

By hand, against the live install (2026-10-28): `pools-shadow fetch-benchmarks <root> --live
--out <dir>` published both snapshots under `<dir>` in 5 s; the AA one is Node's of 09:15Z
apart from its time and the two speed medians, which change between fetches (63 rows, 35 with
a cost), and the Arena one is the same document. `model-refresh <root> --live --probe codex
gpt-6-luna low` passed with its routing evidence (9 s) and `--probe claude claude-haiku-5-5
low` with its model usage (29 s); `model-refresh <root> --live --no-probe` and Node's
`model-refresh.mjs --no-probe` run one after the other wrote the same registry and lineage
apart from their times (4.7 s and 9.4 s).

Arena published 0 rows from 2026-10-21 to 10-28 on both sides: the dataset began naming models
by display name ("Claude Opus 5.5 (Max)") and the matching took only "<id>-<effort>". Both sides
now also take a display name: case-folded, spaces as dashes, matched to exactly one identity,
the parentheses holding one effort and nothing else. A name with a harness, a thinking budget
or a date ("GPT-5.6 Sol (xHigh, Codex Harness)", "Claude Opus 4.5 (High, 32K, 20251101)") is a
different configuration, and one with no effort ("Claude Opus 4.8") an unknown one; they stay
in `unmatched`. A fetch of revision `6852ae7a` (2026-10-28, 7.6 s) published 555 rows over 34
boards for 19 (model, effort) pairs, every pair of the last good snapshot (10-20, 253 rows) and
ten more, with none ambiguous.

The proxy child keeps Node's record format (`state/process-proxy.json`, `{pid, creationDate}`),
so either supervisor adopts what the other started. The port's `portOwner` reads the TCP
table (`GetExtendedTcpTable`); `Get-NetTCPConnection` took 27 s here, past the 20 s lookup
limit, and a takeover without a record then waited forever (`port-held-unidentified`). A
process's record (`ProcessId`, `CreationDate`, `ExecutablePath`, `CommandLine`) is read with
the process's own calls (`GetProcessTimes`, `QueryFullProcessImageNameW`,
`NtQueryInformationProcess`), not Windows PowerShell's CIM, which took about a second each and
once read nothing for want of a piped output; `a_running_process_reads_with_its_identity`
checks it field for field against CIM. Toasts go through WinRT from the process, not a
PowerShell child. Nothing the port runs starts PowerShell or wscript.

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

`deploy/` holds three scripts. The build makes two programs from one source:
`pools-shadow.exe` (a console program, for commands by hand) and `pools-rs.exe` (the same with
no console window). The scheduled tasks start `pools-rs.exe` directly; until 0.2 they went
through wscript and `Run-Pool-Rs.vbs`, as Agent Pools' own tasks still do.

1. `Install-Pool-Rs.ps1 -Root <install> -Exe <pools-shadow.exe> -Version <v> [-RegisterTasks]`
   copies both programs and the static files into `<install>\pool-rs\<v>` (with `build.json`:
   SHA-256s and commit). A version is never replaced. It writes `pool-rs\pool-rs.json`
   (`current`, `previous`, `tailnet`) and points every registered task of the port's at
   `pool-rs\<v>\pools-rs.exe`. With `-RegisterTasks` it registers "Agent Pools (rs) -
   subscriptions", "- inference relay", "- public gateway", "- cloudflare tunnel" (logon plus a
   5-minute keepalive, IgnoreNew; all but the guardian restart on failure; the gateway and
   tunnel at priority 4) and "- backup" (03:30 daily and at logon). New tasks start disabled.
   `-Use <v>` points `current` and the tasks back at an installed version of 0.2 or later.
   Agent Pools' own files are not changed.
2. `Restart-Pool-Rs.ps1 -Root <install> -Step service|inference|gateway|tunnel|all` starts a
   command again on the current version: the service through `state\guardian-stop.request`
   (the proxy is adopted, not restarted), the others by stopping and starting their task.
3. Until 2026-11-01, `Cutover-Pool-Rs.ps1 -Root <install> -Step gateway|inference|service|tunnel|backup|models [-Undo]`
   moved one job between the Agent Pools task and the port's: it disabled the old task, stopped
   its process (the service through `state\guardian-stop.request`, leaving the proxy running),
   enabled the port's and waited for it to listen and pass its health check. Every job had moved
   by 2026-10-28; the script was removed with the Node code (it is in this repository's history).

The jobs Agent Pools ran through wscript and Windows PowerShell are commands of the program
(`shadow/src/live/ops.rs`): `tunnel <install>` (Run-Cloudflare-Tunnel.vbs), `backup <install>`
(Backup-Pool-Secrets.ps1: the same zip, protected with DPAPI for this account, the newest 14
kept) and `restore [<backup>] [--target <dir>] [--list]` (Restore-Pool-Secrets.ps1, into a new
directory only). Each restores the other's backups.

The scripts refuse the live install without `-Live`, and any other root that is not a rehearsal.
The static files are compiled into the program (`corten::assets::embed!`, since
[corten#268 Static files are a directory found at run time, so a built program cannot be deployed by itself and each app writes its own lookup](https://github.com/todd-skelton/corten/issues/268)
closed), so the install is the two executables and `build.json`.

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

### Operating the install

`ops/` holds the scripts that change the install or this machine's clients, each with `-Root`
(default: the live install, `deploy/common.ps1`'s `$LiveRoot`):

| Script | Does |
|---|---|
| `Set-Pool-Secret.ps1 -Name <key>` | one named secret into `state/secrets.json`, from hidden input |
| `Remote-Client.ps1 -Add <name> \| -Remove <name> \| -List` | machines allowed through the public gateway (`state/remote-clients.json`, key hashes only; the new key goes to the clipboard) |
| `Protect-Pool-Files.ps1 [-Report]` | `auth`, `profiles`, `state`, `logs` and `proxy.yaml` readable by this account, SYSTEM and Administrators only |
| `Set-Desktop-Relay.ps1 -Enable \| -Disable` | the Codex desktop's `chatgpt_base_url` and `CODEX_API_BASE_URL` at the service's backend relay, with its certificate in `state/certs` |
| `Set-Inference-Relay.ps1 -Enable \| -Disable` | `openai_base_url` in `~/.codex/config.toml` at the inference relay, after its task answers `/health`; the replaced value is kept in `state/inference-config.json` for `-Disable` |
| `Set-Public-Gateway.ps1 [-Hostname <h>] [-Remove]` | the Cloudflare tunnel, its DNS route and `state/cloudflared/config.yml`, `install.json`'s `publicGateway`, and the gateway and tunnel tasks |
| `Invoke-PooledLane.ps1 ...` | one chase-sets lane (`.orchestrator/dispatch-lane.ps1`) with the pool's key and the install's `profiles` |
| `Start-Claude-Browser.ps1` | `claude --chrome` signed in as itself, with the pool's variables unset |

`Set-Inference-Relay.ps1`'s config edit is `ops/InferenceConfig.ps1` (inference-config.mjs's, without a
TOML parser: only a plain one-line root key is set or removed, anything else is refused, every
other byte is kept); `pwsh -File ops/tests/InferenceConfig.Tests.ps1` checks it on scratch files.
`remote-client/Connect-AgentPools.ps1 -Url https://<public hostname>` runs on a remote machine: it
points Claude Desktop there at the gateway with the key `Remote-Client.ps1 -Add` issued.

### The proxy build

The pool's proxy is CLIProxyAPI 5b27856 (`<install>\proxy-upstream-5b27856.tar.gz`) with
`proxy-build/pool.patch`, built from `<install>\proxy-source` with the install's own Go and
llvm-mingw (`<install>\build`).

1. `New-Proxy-Source.ps1 -Target <dir>` writes upstream with the patch applied into a new
   directory; `-WritePatch` writes `pool.patch` from `proxy-source`, so the repository keeps what
   each version was built from. The patch includes the 7.2.158-pool.32 Claude Responses message
   phase fix (2026-11-02): completed answers carry `final_answer`; intermediate, tool-use and
   truncated messages carry `commentary`, in both streaming and non-streaming output.
   Regression tests also ensure an earlier text block is not labeled as the final answer.
   7.2.158-pool.33 (2026-11-03) re-reads an installed client's version a minute after a failed
   `--version`, not six hours later: one failed read at start had kept Codex at the compiled
   0.154.0, whose model list hides gpt-6.1-sol, gpt-6-sol and gpt-6-luna.
2. `Build-Pool-Proxy.ps1 -Version 7.2.158-pool.N` runs the proxy's test gate and builds
   `build\candidate.exe`.
3. `Deploy-Pool-Proxy.ps1 -Root <install> -Live -Version 7.2.158-pool.N` builds, installs to
   `proxy\<version>`, points `install.json`'s `proxyBinary` at it and restarts the service, whose
   takeover plan retires the old proxy and starts the new one; `-Rollback` restores the previous
   `install.json`. It refuses while requests are held by the outage hold unless `-Force`.

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

`gateway/` (`pools-gateway`; `run-copy` serves it in front of the stand-in) is public-gateway.mjs on Corten (`3b19711`, with
[corten#162 Upgrade and Relay](https://github.com/todd-skelton/corten/pull/162)). Inference routes
take a machine key and relay the pool's answer as it streams, sent with the pool's key. The
dashboard routes take a session cookie signed as Agent Pools signs it (the same secret keeps
today's cookies valid; a test holds the token to Agent Pools' bytes). Since the dashboard became
several pages, they are all the gateway's dashboard routes, so a remote machine signed in at
`https://<hostname>/login` with its machine key uses the whole dashboard: the four tabs and the
sign-in steps (a page goes to `/login` without a session), their live updates (`/live`), the
stylesheets the gateway does not have itself (`/assets/..`, `/corten/..`, a 404 without a
session), and the dashboard's forms (`/refresh`, `/signin`, `/signin/finish`, `/signin/cancel`):
a form body, from the public origin, sent on to the service from its own origin, so a signed-in
machine can refresh and connect accounts as this PC can. A redirect the service answers goes to
the browser as it is. The summary and parity pages and the service's JSON writes stay closed.
Everything else is closed.
`cargo test -p pools-gateway` runs Agent Pools' 12 gateway tests over real sockets, plus four of
the port's own and three for the dashboard's pages, files and forms: the session token, a client that leaves a stream (the upstream request ends
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
Since then `usage-value.mjs` and `pool-summary.mjs` there are Agent Pools' later versions with actual cost (plan cost spread over requests, `actualCost`), and the value and summary outputs were captured again from them. `dashboard.html` there is Agent Pools' last (90d2df1), for `golden/dash-text.mjs`. Nothing here reads Agent Pools' code from the install any more: `golden/registry-hook.mjs` answers model-arena.mjs's `hyparquet` import with a stand-in (the registry calls never read a parquet file).
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
