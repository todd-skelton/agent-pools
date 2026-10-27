// Traces of Agent Pools' refresh cycle (reference/agent-pools/refresh.mjs) over scripted
// scenarios, for pool-core/tests/refresh.rs to replay through the port. Each scenario is data: a
// fake proxy (auth files and replies by "auth_index URL" or URL), saved state, policy config and
// the forecast hooks. Nothing here reaches a provider or the live install.
//   TZ=America/Chicago node golden/refresh-traces.mjs
// Writes golden/outputs/refresh/traces.json: per scenario and cycle, the result, every proxy call,
// event, sample, toast, save and hook call, and the state after.
import fs from "node:fs";
import path from "node:path";
import { createRequire, syncBuiltinESMExports } from "node:module";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const ref = path.join(here, "..", "reference", "agent-pools");
// Claude claims get random request ids; a counter makes them the same on both sides.
const crypto = createRequire(import.meta.url)("node:crypto");
let uuidCounter = 0;
crypto.randomUUID = () => "00000000-0000-4000-8000-" + String(++uuidCounter).padStart(12, "0");
syncBuiltinESMExports();
const { createRefresh } = await import("file://" + path.join(ref, "refresh.mjs"));
const { resetPolicies, migrateState } = await import("file://" + path.join(ref, "resets.mjs"));
const { RESET_BACKOFF_MS } = await import("file://" + path.join(ref, "reset-lifecycle.mjs"));

const fixture = (name) => JSON.parse(fs.readFileSync(path.join(ref, "fixtures", name), "utf8"));
const FX = { codexAvailable: fixture("codex-usage-available.json"), codexExhausted: fixture("codex-usage-exhausted.json"), codexCredits: fixture("codex-credits.json"), claude: fixture("claude-usage.json") };
const T0 = Date.parse("2026-09-24T14:20:00Z");
const ORG = "a0b1c2d3-e4f5-4a6b-8c7d-9e0f1a2b3c4d";
const CODEX_USAGE = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_CREDITS = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const CLAUDE_USAGE = "https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1";
const CLAUDE_CLAIM = "https://api.anthropic.com/api/organizations/" + ORG + "/reset_rate_limits";
const CLAUDE_PROFILE = "https://api.anthropic.com/api/oauth/profile";
const CODEX_PRICING = "https://chatgpt.com/backend-api/checkout_pricing_config/configs/US";
const CODEX_SUB = (n) => "https://chatgpt.com/backend-api/subscriptions?account_id=acct-" + n;
const clone = (v) => structuredClone(v);

function claudeWeeklyOut() { const u = clone(FX.claude); u.seven_day.utilization = 100; for (const l of u.limits) if (l.kind === "weekly_all") l.percent = 100; return u; }
function claudeSessionOut() { const u = clone(FX.claude); u.five_hour.utilization = 100; for (const l of u.limits) if (l.kind === "session") l.percent = 100; return u; }
const noCredits = () => ({ ...clone(FX.codexCredits), credits: [], available_count: 0 });
const file = (provider, n, over = {}) => ({
  id: provider + "-" + n + ".json", name: provider + "-" + n + ".json", auth_index: provider + "-idx-" + n, provider,
  email: n + "@example.invalid", path: "auth/" + provider + "-" + n + ".json", disabled: false, priority: 0,
  ...(provider === "codex" ? { id_token: { chatgpt_account_id: "acct-" + n } } : {}),
  routing: { status: "ready", quota_blocked: false, generation: "1", checked_at: new Date(T0).toISOString(), models: { m: { status: "ready" } } },
  ...over,
});
const codexFiles = () => [file("codex", 1), file("codex", 2)];
const claudeFiles = () => [file("claude", 1), file("claude", 2), file("claude", 3)];

// Replies: a body (sent as JSON), { status_code, body }, { $seq: [reply...] } (one per call, the
// last repeating), { $throw: { message, status } }, or { $hang: true } (never answers).
async function run(s) {
  uuidCounter = 0;
  let clock = T0;
  let files = clone(s.files);
  let routes = clone(s.routes ?? {});
  let hooks = clone(s.hooks ?? {});
  const counts = {};
  let trace;
  const state = migrateState({ accounts: [], resetAutomation: true, ...clone(s.state ?? {}) });
  const reply = (key, r) => {
    if (r && typeof r === "object" && "$seq" in r) { const i = counts[key] = (counts[key] ?? 0) + 1; r = r.$seq[Math.min(i, r.$seq.length) - 1]; }
    return r;
  };
  async function management(endpoint, body, method) {
    trace.calls.push({ endpoint, ...(body !== undefined ? { body } : {}), ...(method !== undefined ? { method } : {}) });
    const special = routes["mgmt:" + endpoint];
    if (special !== undefined) {
      const r = reply("mgmt:" + endpoint, special);
      if (r?.$throw) throw Object.assign(new Error(r.$throw.message), r.$throw.status ? { status: r.$throw.status } : {});
      return clone(r);
    }
    if (endpoint === "auth-files") return { files: clone(files) };
    if (endpoint === "auth-files/fields" || endpoint === "recover-quota") return { changed: false, routing: {} };
    if (endpoint !== "api-call") throw new Error("unexpected endpoint " + endpoint);
    const key = routes[body.auth_index + " " + body.url] !== undefined ? body.auth_index + " " + body.url : body.url;
    let r = reply(key, routes[key]);
    if (r === undefined) throw new Error("no route for " + body.auth_index + " " + body.url);
    if (r?.$hang) return new Promise(() => {});
    if (r?.$throw) throw Object.assign(new Error(r.$throw.message), r.$throw.status ? { status: r.$throw.status } : {});
    return r && typeof r === "object" && "status_code" in r ? clone(r) : { status_code: 200, body: JSON.stringify(r) };
  }
  const hook = (name, fn) => (hooks[name] === undefined ? null : fn);
  const versions = s.versions ?? { claude: "2.1.284" };
  const org = s.org === undefined ? ORG : s.org;
  const make = () => createRefresh({
    state, management,
    event: async (type, details) => { trace.events.push({ type, ...details }); },
    sample: async (type, details) => { trace.samples.push({ type, ...details }); },
    save: async () => { trace.saves.push(clone(state)); },
    toast: async (title, body) => { trace.toasts.push({ title, body }); },
    loadPolicies: async () => resetPolicies(s.config ?? { expectedAccounts: s.expected ?? { codex: 2, claude: 3 } }),
    authFile: async (f) => (f.provider === "claude" && org ? { organization_uuid: org } : null),
    clientVersion: async (provider) => versions[provider],
    forecast: hook("forecast", async (input) => {
      trace.hooks.push({ hook: "forecast", accounts: input.accounts, newSamples: input.newSamples, resetPolicies: input.resetPolicies, resetAutomation: input.resetAutomation });
      const h = hooks.forecast;
      if (h.throw) throw new Error(h.throw);
      return h.doc ?? undefined;
    }),
    creditPlan: hook("creditPlan", ({ policy, now }) => {
      trace.hooks.push({ hook: "creditPlan", provider: policy.provider, now });
      const h = hooks.creditPlan;
      if (h.throw) throw new Error(h.throw);
      return h.ids ? new Set(h.ids) : null;
    }),
    refillDemand: hook("refillDemand", ({ policy, now }) => {
      trace.hooks.push({ hook: "refillDemand", provider: policy.provider, now });
      const h = hooks.refillDemand;
      if (h.throw) throw new Error(h.throw);
      return h.value ?? null;
    }),
    drainFirst: hook("drainFirst", ({ provider, now }) => {
      trace.hooks.push({ hook: "drainFirst", provider, now });
      const h = hooks.drainFirst;
      if (h.throw) throw new Error(h.throw);
      const plan = h.byProvider?.[provider];
      return plan ? new Map(Object.entries(plan)) : null;
    }),
    readPlans: !!s.readPlans,
    ...(s.planTimeoutMs ? { planTimeoutMs: s.planTimeoutMs } : {}),
    now: () => clock,
    sleep: async () => {},
  });
  const { refresh } = make();
  const cycles = [];
  for (const step of s.cycles ?? [{}]) {
    clock += step.advance ?? 0;
    if (step.files) files = clone(step.files);
    for (const [k, v] of Object.entries(step.routes ?? {})) { if (v === null) delete routes[k]; else routes[k] = clone(v); delete counts[k]; }
    if (step.hooks) hooks = { ...hooks, ...clone(step.hooks) };
    trace = { calls: [], events: [], samples: [], toasts: [], saves: [], hooks: [] };
    const result = await refresh();
    cycles.push({ now: clock, result, ...trace, state: clone(state), stateText: JSON.stringify(state, null, 2) });
  }
  return cycles;
}

const one = file("claude", 1), three = file("claude", 3);
const soon = clone(FX.claude); soon.cedar_ember.grants[0].ends_at = "2026-09-28T13:00:00+00:00";
const until = Date.parse(soon.cedar_ember.grants[0].ends_at);
const spentClaude = clone(FX.claude); spentClaude.cedar_ember.grants = [];
const reauth = { code: "reauth_required", http_status: 400, message: 'token refresh failed with status 400: {"error": "invalid_grant"}',
  since: new Date(T0 - 3600000).toISOString(), last_failed_at: new Date(T0).toISOString(), attempts: 12 };
const retrying = { code: "refresh_failed", http_status: 0, message: "context deadline exceeded", since: new Date(T0).toISOString(), last_failed_at: new Date(T0).toISOString(), attempts: 1 };
const riskAt = new Date(T0 + 40 * 3600000).toISOString();
const riskDoc = (outcome) => ({ staleAfter: new Date(T0 + 3600000).toISOString(), providers: { claude: { state: "conserve", demand: { unitsPerHour: 0.02 },
  creditPlan: { method: "counterfactual", credits: [{ accountId: "claude-2.json", expiresAt: riskAt, action: "drain-first", drainBy: riskAt, unitsLeft: 0.9, outcome, needUnitsPerHour: 0.03 }] } } } });
const PRICING = { currency_config: { symbol_code: "USD", pro: { month: { amount: 200 } }, prolite: { month: { amount: 100 } } } };
const sub = (plan, until) => ({ plan_type: plan, billing_period: "monthly", billing_currency: "USD", will_renew: true, active_until: until });
const blocked = (over = {}) => ({ routing: { status: "blocked", quota_blocked: true, generation: "g7", checked_at: new Date(T0).toISOString(),
  models: { m: { status: "blocked", next_retry_after: new Date(T0 + 3 * 3600000).toISOString() } } }, ...over });
const all = { [CODEX_USAGE]: FX.codexAvailable, [CODEX_CREDITS]: FX.codexCredits, [CLAUDE_USAGE]: FX.claude };

const scenarios = [
  { name: "healthy", files: [...codexFiles(), ...claudeFiles()], routes: all },
  { name: "claude-weekly-out-claims-one-reset", files: claudeFiles(), expected: { codex: 0, claude: 3 },
    routes: { [CLAUDE_USAGE]: claudeWeeklyOut(), [CLAUDE_CLAIM]: { result: "reset", resets_left: 0 } } },
  { name: "refused-claims-move-through-the-pool", files: claudeFiles(), expected: { codex: 0, claude: 3 },
    routes: { [CLAUDE_USAGE]: claudeWeeklyOut(), [CLAUDE_CLAIM]: { result: "ineligible" } },
    cycles: [{}, { advance: 30000 }, { advance: 30000 }, { advance: 30000 }, { advance: 30000 + RESET_BACKOFF_MS.no_credit }] },
  { name: "credit-planner-early-claim", files: claudeFiles(), expected: { codex: 0, claude: 3 }, hooks: { creditPlan: { ids: [one.id] } },
    routes: { [one.auth_index + " " + CLAUDE_USAGE]: claudeWeeklyOut(), [CLAUDE_USAGE]: FX.claude, [CLAUDE_CLAIM]: { result: "reset", resets_left: 0 } } },
  { name: "no-plan-no-early-claim", files: claudeFiles(), expected: { codex: 0, claude: 3 },
    routes: { [one.auth_index + " " + CLAUDE_USAGE]: claudeWeeklyOut(), [CLAUDE_USAGE]: FX.claude, [CLAUDE_CLAIM]: { result: "reset" } } },
  { name: "claude-session-out-alerts-then-recovers", files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: claudeSessionOut() },
    cycles: [{}, { advance: 120000, routes: { [CLAUDE_USAGE]: FX.claude } }] },
  { name: "claude-bank-without-organization", files: claudeFiles(), org: null, expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: claudeWeeklyOut() } },
  { name: "claude-missing-cedar-ember", files: claudeFiles(), expected: { codex: 0, claude: 3 },
    routes: { [CLAUDE_USAGE]: (() => { const u = claudeWeeklyOut(); delete u.cedar_ember; return u; })() } },
  { name: "codex-exhausted-redeems", files: codexFiles(), expected: { codex: 2, claude: 0 }, routes: {
    [CODEX_USAGE]: FX.codexExhausted, ["codex-idx-1 " + CODEX_CREDITS]: FX.codexCredits, ["codex-idx-2 " + CODEX_CREDITS]: noCredits(),
    [CODEX_CREDITS + "/consume"]: { code: "reset" } } },
  { name: "codex-unknown-outcome-keeps-identity", files: codexFiles(), expected: { codex: 2, claude: 0 }, routes: {
    [CODEX_USAGE]: FX.codexExhausted, ["codex-idx-1 " + CODEX_CREDITS]: FX.codexCredits, ["codex-idx-2 " + CODEX_CREDITS]: noCredits(),
    [CODEX_CREDITS + "/consume"]: { $seq: [{ status_code: 502, body: "bad gateway" }, { code: "already_redeemed" }] } },
    cycles: [{}, { advance: 30000 }] },
  { name: "transient-usage-failure", files: codexFiles(), routes: {
    ["codex-idx-1 " + CODEX_USAGE]: { status_code: 502, body: "" }, ["codex-idx-2 " + CODEX_USAGE]: FX.codexAvailable, [CODEX_CREDITS]: FX.codexCredits } },
  { name: "transient-then-recovers-on-retry", files: codexFiles(), routes: {
    ["codex-idx-1 " + CODEX_USAGE]: { $seq: [{ status_code: 503, body: "" }, FX.codexAvailable] }, ["codex-idx-2 " + CODEX_USAGE]: FX.codexAvailable, [CODEX_CREDITS]: FX.codexCredits } },
  { name: "proxy-down", files: [], routes: { "mgmt:auth-files": { $throw: { message: "proxy down" } } },
    state: { accounts: [{ id: "x", provider: "codex", status: "available", usageUpdatedAt: new Date(T0).toISOString() }] } },
  { name: "codex-bank-null", files: codexFiles(), routes: { [CODEX_USAGE]: FX.codexAvailable, [CODEX_CREDITS]: null } },
  { name: "codex-bank-malformed", files: codexFiles(), routes: { [CODEX_USAGE]: FX.codexAvailable, [CODEX_CREDITS]: { credits: "soon" } } },
  { name: "codex-bank-500", files: codexFiles(), routes: { [CODEX_USAGE]: FX.codexAvailable, [CODEX_CREDITS]: { status_code: 500, body: "" } } },
  { name: "unknown-providers-ignored", files: [file("gemini", 1), file("constructor", 2), ...codexFiles()], routes: all },
  { name: "claude-without-client-version", files: claudeFiles(), versions: {}, routes: { [CLAUDE_USAGE]: FX.claude } },
  { name: "model-list-source", files: [file("claude", 1, { live_models: { source: "live", fetched_at: new Date(T0 - 60000).toISOString(), checked_at: new Date(T0).toISOString(), models: 13 } }),
    file("claude", 2, { live_models: { source: "pending", checked_at: new Date(T0).toISOString(), models: 0, error: "model list returned HTTP 401" } }), file("claude", 3)],
    routes: { [CLAUDE_USAGE]: FX.claude } },
  { name: "reauth-episode", files: [file("claude", 1), file("claude", 2), file("claude", 3, { refresh_error: reauth })],
    routes: { [CLAUDE_USAGE]: FX.claude, ["claude-idx-3 " + CLAUDE_USAGE]: { status_code: 429, body: "{}" } },
    cycles: [{}, { advance: 120000 }, { advance: 120000, files: claudeFiles() }, { advance: 120000, routes: { ["claude-idx-3 " + CLAUDE_USAGE]: null } }] },
  { name: "refresh-still-retrying", files: [file("claude", 1, { refresh_error: retrying }), file("claude", 2), file("claude", 3)], routes: { [CLAUDE_USAGE]: FX.claude } },
  { name: "forecast-input", files: [...codexFiles(), ...claudeFiles()], routes: all, hooks: { forecast: {} } },
  { name: "forecast-failure", files: codexFiles(), routes: all, expected: { codex: 2, claude: 0 }, hooks: { forecast: { throw: "boom" } } },
  { name: "refill-demand", files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: FX.claude },
    hooks: { refillDemand: { value: { unitsPerHour: 0.02, weights: {} } }, forecast: {} } },
  { name: "refill-demand-failure", files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: FX.claude }, hooks: { refillDemand: { throw: "boom" } } },
  ...[["drain-first-none", null], ["drain-first-boost", { claude: { [three.id]: until } }], ["drain-first-stale-plan", { claude: { [three.id]: until + 3600000 } }]].map(([name, plan]) => ({
    name, files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: FX.claude, [three.auth_index + " " + CLAUDE_USAGE]: soon },
    hooks: { drainFirst: { byProvider: plan }, forecast: {} } })),
  { name: "drain-first-credit-spent", files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: FX.claude, [three.auth_index + " " + CLAUDE_USAGE]: spentClaude },
    hooks: { drainFirst: { byProvider: { claude: { [three.id]: until } } } } },
  { name: "drain-first-failure", files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: FX.claude }, hooks: { drainFirst: { throw: "boom" } } },
  { name: "external-redemption", files: claudeFiles(), expected: { codex: 0, claude: 3 },
    routes: { [one.auth_index + " " + CLAUDE_USAGE]: claudeWeeklyOut(), [CLAUDE_USAGE]: FX.claude },
    cycles: [{}, { advance: 120000, routes: { [one.auth_index + " " + CLAUDE_USAGE]: spentClaude } }, { advance: 120000 }] },
  { name: "own-redemption-not-external", files: claudeFiles(), expected: { codex: 0, claude: 3 }, hooks: { creditPlan: { ids: [one.id] } },
    routes: { [one.auth_index + " " + CLAUDE_USAGE]: claudeWeeklyOut(), [CLAUDE_USAGE]: FX.claude, [CLAUDE_CLAIM]: { result: "reset", resets_left: 0 } },
    cycles: [{}, { advance: 30000, routes: { [one.auth_index + " " + CLAUDE_USAGE]: spentClaude } }] },
  { name: "credit-at-risk", files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: FX.claude }, hooks: { forecast: { doc: riskDoc("lapses") } },
    cycles: [{}, {}, { hooks: { forecast: { doc: riskDoc("used") } } }] },
  { name: "plan-reads", readPlans: true, versions: { claude: "2.1.284", codex: "0.162.1" }, files: [file("codex", 1), file("codex", 2), file("claude", 1)], routes: {
    ...all, [CODEX_SUB(1)]: sub("pro", "2026-11-04T00:00:00Z"), [CODEX_SUB(2)]: sub("prolite", "2026-10-20T00:00:00Z"), [CODEX_PRICING]: PRICING,
    [CLAUDE_PROFILE]: { organization: { rate_limit_tier: "default_claude_max_20x", organization_type: "claude_max", subscription_status: "active" } } },
    cycles: [{}, { advance: 60000 }, { advance: 12 * 3600000, routes: { [CODEX_SUB(1)]: { status_code: 403, body: "{}" } } }, { advance: 60000 }, { advance: 3600000 },
      { advance: 3600000, routes: { [CODEX_SUB(1)]: sub("pro", "2026-11-04T00:00:00Z") } }] },
  { name: "plan-read-hangs", readPlans: true, planTimeoutMs: 200, files: [file("claude", 1)], expected: { codex: 0, claude: 1 },
    routes: { [CLAUDE_USAGE]: FX.claude, [CLAUDE_PROFILE]: { $hang: true } } },
  { name: "quota-recovery-clears-a-long-block", files: [file("claude", 1, blocked()), file("claude", 2), file("claude", 3)], expected: { codex: 0, claude: 3 },
    routes: { [CLAUDE_USAGE]: FX.claude, "mgmt:recover-quota": { changed: true, routing: { status: "ready", quota_blocked: false, generation: "g8", checked_at: new Date(T0).toISOString(), models: { m: { status: "ready" } } } } } },
  { name: "quota-recovery-conflict", files: [file("claude", 1, blocked()), file("claude", 2), file("claude", 3)], expected: { codex: 0, claude: 3 },
    routes: { [CLAUDE_USAGE]: FX.claude, "mgmt:recover-quota": { $throw: { message: "conflict", status: 409 } } } },
  { name: "quota-recovery-short-block-left", files: [file("claude", 1, blocked({ routing: { ...blocked().routing, models: { m: { status: "blocked", next_retry_after: new Date(T0 + 60000).toISOString() } } } })), file("claude", 2), file("claude", 3)],
    expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: FX.claude, "mgmt:recover-quota": { changed: true, routing: {} } } },
  { name: "codex-recovery-fails", files: [file("codex", 1, blocked()), file("codex", 2)], routes: { ...all, "mgmt:recover-quota": { $throw: { message: "Proxy management recover-quota returned HTTP 500", status: 500 } } } },
  { name: "disabled-account", files: [file("codex", 1, { disabled: true }), file("codex", 2)], routes: all,
    state: { accounts: [{ id: "codex-1.json", provider: "codex", plan: { plan: "pro" }, planCheckedAt: new Date(T0 - 1000).toISOString() }] } },
  { name: "unsent-claim-cancelled-when-policy-off", files: codexFiles(), routes: all,
    config: { expectedAccounts: { codex: 2, claude: 3 }, resetPolicy: { codex: { enabled: false } } },
    state: { resets: { codex: { pending: { accountId: "codex-1.json", creditId: "c1", phase: "unsent", requestId: "r1" }, last: null } } } },
  { name: "attention-without-account", files: codexFiles(), routes: all,
    state: { resets: { codex: { pending: { accountId: "gone.json", creditId: "c1", phase: "uncertain", requestId: "r1" }, last: null } } } },
];

const out = [];
for (const s of scenarios) out.push({ name: s.name, scenario: s, cycles: await run(s) });
const dir = path.join(here, "outputs", "refresh");
fs.mkdirSync(dir, { recursive: true });
fs.writeFileSync(path.join(dir, "traces.json"), JSON.stringify({ t0: T0, tz: process.env.TZ ?? null, fixtures: FX, traces: out }, null, 1) + "\n");
console.log(out.length + " scenarios, " + out.reduce((n, t) => n + t.cycles.length, 0) + " cycles");
