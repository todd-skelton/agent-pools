import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createRefresh } from "./refresh.mjs";
import { resetPolicies, migrateState } from "./resets.mjs";
import { redemptionId } from "./routing.mjs";
import { RESET_BACKOFF_MS } from "./reset-lifecycle.mjs";

// The refresh cycle against recorded, scrubbed provider payloads (fixtures/)
// and a fake proxy management API. Nothing here reaches a provider.
const fixture = async (name) => JSON.parse(await readFile(new URL(`./fixtures/${name}`, import.meta.url), "utf8"));
const FX = {
  codexAvailable: await fixture("codex-usage-available.json"),
  codexExhausted: await fixture("codex-usage-exhausted.json"),
  codexCredits: await fixture("codex-credits.json"),
  claude: await fixture("claude-usage.json"),
};
const T0 = Date.parse("2026-09-24T14:20:00Z");
const ORG = "a0b1c2d3-e4f5-4a6b-8c7d-9e0f1a2b3c4d";
const CODEX_USAGE = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_CREDITS = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const CLAUDE_USAGE = "https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1";
const CLAUDE_CLAIM = `https://api.anthropic.com/api/organizations/${ORG}/reset_rate_limits`;
const clone = (v) => structuredClone(v);
const GRANT = FX.claude.cedar_ember.next_grant_id;
const CREDIT = FX.codexCredits.credits[0].id;

// Claude usage with the all-models weekly window at 100% (5-hour untouched).
function claudeWeeklyOut() {
  const u = clone(FX.claude);
  u.seven_day.utilization = 100;
  for (const l of u.limits) if (l.kind === "weekly_all") l.percent = 100;
  return u;
}
function claudeSessionOut() {
  const u = clone(FX.claude);
  u.five_hour.utilization = 100;
  for (const l of u.limits) if (l.kind === "session") l.percent = 100;
  return u;
}
const noCredits = () => ({ ...clone(FX.codexCredits), credits: [], available_count: 0 });

const file = (provider, n, over = {}) => ({
  id: `${provider}-${n}.json`, name: `${provider}-${n}.json`, auth_index: `${provider}-idx-${n}`, provider,
  email: `${n}@example.invalid`, path: `auth/${provider}-${n}.json`, disabled: false, priority: 0,
  ...(provider === "codex" ? { id_token: { chatgpt_account_id: `acct-${n}` } } : {}),
  routing: { status: "ready", quota_blocked: false, generation: "1", checked_at: new Date(T0).toISOString(), models: { "m": { status: "ready" } } },
  ...over,
});

// Fake proxy. `routes` maps "auth_index URL" (or just the URL) to a body, an
// { status_code, body } reply, or a function of the request returning either.
function harness({ files, routes, expected = { codex: 2, claude: 3 }, org = ORG, state: saved = {}, versions = { claude: "2.1.284" }, forecast = null, creditPlan = null, refillDemand = null, drainFirst = null, readPlans = false, planTimeoutMs = 10000 }) {
  const calls = [], events = [], toasts = [], saves = [];
  let clock = T0;
  const state = migrateState({ accounts: [], resetAutomation: true, ...saved });
  async function management(endpoint, body, method) {
    calls.push({ endpoint, body, method });
    if (endpoint === "auth-files") return { files: clone(files) };
    if (endpoint === "auth-files/fields" || endpoint === "recover-quota") return { changed: false, routing: {} };
    if (endpoint !== "api-call") throw new Error("unexpected endpoint " + endpoint);
    let reply = routes[`${body.auth_index} ${body.url}`] ?? routes[body.url];
    if (typeof reply === "function") reply = await reply(body);
    if (reply === undefined) throw new Error("no route for " + body.auth_index + " " + body.url);
    if (reply instanceof Error) throw reply;
    return reply && "status_code" in reply ? reply : { status_code: 200, body: JSON.stringify(reply) };
  }
  const { refresh } = createRefresh({
    state, management,
    event: async (type, details) => { events.push({ type, ...details }); },
    save: async () => { saves.push(clone(state)); },
    toast: async (title, body) => { toasts.push({ title, body }); },
    loadPolicies: async () => resetPolicies({ expectedAccounts: expected }),
    authFile: async (f) => (f.provider === "claude" && org ? { organization_uuid: org } : null),
    clientVersion: async (provider) => versions[provider],
    forecast,
    creditPlan,
    refillDemand,
    drainFirst,
    readPlans,
    planTimeoutMs,
    now: () => clock,
    sleep: async () => {},
  });
  return { state, calls, events, toasts, saves, refresh, advance: (ms) => { clock += ms; },
    providerCalls: (url) => calls.filter((c) => c.endpoint === "api-call" && (!url || c.body.url === url)) };
}
const codexFiles = () => [file("codex", 1), file("codex", 2)];
const claudeFiles = () => [file("claude", 1), file("claude", 2), file("claude", 3)];

test("a healthy cycle reads every account through its adapter with the recorded payloads", async () => {
  const h = harness({ files: [...codexFiles(), ...claudeFiles()], routes: {
    [CODEX_USAGE]: FX.codexAvailable, [CODEX_CREDITS]: FX.codexCredits, [CLAUDE_USAGE]: FX.claude,
  } });
  assert.deepEqual(await h.refresh(), { degraded: false, followUp: false });
  const byId = Object.fromEntries(h.state.accounts.map((a) => [a.id, a]));
  const codex = byId["codex-1.json"], claude = byId["claude-1.json"];
  assert.equal(codex.status, "available");
  assert.equal(codex.remaining, 100 - FX.codexAvailable.rate_limit.primary_window.used_percent);
  assert.deepEqual([codex.creditsKnown, codex.creditCount, codex.credits.map((c) => c.id)], [true, 1, [CREDIT]]);
  assert.equal(codex.accountId, "acct-1");
  assert.equal(claude.status, "available");
  assert.deepEqual([claude.creditsKnown, claude.creditCount, claude.credits.map((c) => c.id)], [true, 1, [GRANT]]);
  assert.equal(claude.modelQuotaRecoveryBlocked, false);
  // Request identity per provider.
  const header = (auth, url) => h.providerCalls(url).find((c) => c.body.auth_index === auth).body.header;
  assert.equal(header("codex-idx-1", CODEX_USAGE)["Chatgpt-Account-Id"], "acct-1");
  assert.equal(header("codex-idx-1", CODEX_USAGE).Originator, "Codex Desktop");
  assert.equal(header("claude-idx-1", CLAUDE_USAGE)["User-Agent"], "claude-cli/2.1.284 (external, cli)", "the version the proxy presents");
  assert.equal(header("claude-idx-1", CLAUDE_USAGE)["x-app"], "cli");
  assert.equal(h.providerCalls(CODEX_CREDITS).length, 2, "Codex reads its bank separately");
  assert.equal(h.providerCalls().filter((c) => c.body.method === "POST").length, 0, "nothing is redeemed");
  // The orchestrator executor reads accounts[].disabled and routingModels from /api/status.
  for (const a of h.state.accounts) { assert.equal(a.disabled, false); assert.equal(a.routingModels.m.status, "ready"); }
  assert.ok(h.calls.some((c) => c.endpoint === "auth-files/fields" && c.method === "PATCH"), "routing priorities are written");
  assert.deepEqual(h.state.exhausted, {});
  assert.equal(h.toasts.length, 0);
  assert.equal(h.saves.length, 1);
});

test("an exhausted Claude pool spends one weekly reset with a journaled claim and asks for a follow-up", async () => {
  let persistedBeforeSend;
  const h = harness({ files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: {
    [CLAUDE_USAGE]: claudeWeeklyOut(),
    [CLAUDE_CLAIM]: () => { persistedBeforeSend = h.saves.at(-1).resets.claude.pending; return { result: "reset", resets_left: 0 }; },
  } });
  assert.deepEqual(await h.refresh(), { degraded: false, followUp: true });
  const [claim] = h.providerCalls(CLAUDE_CLAIM);
  const body = JSON.parse(claim.body.data);
  assert.equal(claim.body.method, "POST");
  assert.deepEqual(Object.keys(body).sort(), ["grant_id", "program", "request_id"]);
  assert.equal(body.program, "cedar_ember");
  assert.equal(body.grant_id, GRANT);
  assert.equal(persistedBeforeSend.requestId, body.request_id, "the request id was saved before it was sent");
  assert.equal(persistedBeforeSend.phase, "uncertain");
  assert.equal(h.state.resets.claude.last.outcome, "reset");
  assert.equal(h.toasts.length, 0, "no exhaustion alert while a reset is being used");
  assert.deepEqual(h.events.filter((e) => e.type.startsWith("reset-")).map((e) => e.type), ["reset-attempt", "reset-result"]);
});

test("finding 1: refused claims move through the pool one account per cycle, then the pool alerts", async () => {
  const h = harness({ files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: {
    [CLAUDE_USAGE]: claudeWeeklyOut(), [CLAUDE_CLAIM]: { result: "ineligible" },
  } });
  const claimed = [];
  for (let cycle = 0; cycle < 4; cycle++) {
    const before = h.providerCalls(CLAUDE_CLAIM).length;
    assert.equal((await h.refresh()).followUp, cycle < 3, "a refusal asks for a quick retry with the next credit");
    claimed.push(...h.providerCalls(CLAUDE_CLAIM).slice(before).map((c) => c.body.auth_index));
    h.advance(30000);
  }
  assert.equal(new Set(claimed).size, 3, "each account's credit was tried once");
  assert.equal(claimed.length, 3, "no credit was retried inside its backoff");
  assert.equal(Object.keys(h.state.resets.claude.refused).length, 3);
  assert.equal(h.toasts.length, 1, "alert once no reset is left to try");
  assert.match(h.toasts[0].title, /Claude accounts exhausted/);
  assert.equal(h.events.filter((e) => e.type === "pool-exhausted")[0].provider, "claude");
  assert.ok(h.state.exhausted.claude);
  // After the backoff the credits are eligible again.
  h.advance(RESET_BACKOFF_MS.no_credit);
  await h.refresh();
  assert.equal(h.providerCalls(CLAUDE_CLAIM).length, 4);
});

test("credit planner wiring: one out Claude account the plan names spends its own credit early", async () => {
  const one = file("claude", 1);
  const plans = [];
  const h = harness({ files: claudeFiles(), expected: { codex: 0, claude: 3 },
    creditPlan: (input) => { plans.push(input.policy.provider); return new Set([one.id]); },
    routes: {
      [`${one.auth_index} ${CLAUDE_USAGE}`]: claudeWeeklyOut(),
      [CLAUDE_USAGE]: clone(FX.claude),
      [CLAUDE_CLAIM]: { result: "reset", resets_left: 0 },
    } });
  await h.refresh();
  const claims = h.providerCalls(CLAUDE_CLAIM);
  assert.equal(claims.length, 1);
  assert.equal(claims[0].body.auth_index, one.auth_index);
  assert.deepEqual([h.state.resets.claude.last.trigger, h.state.resets.claude.last.outcome], ["before-lapse", "reset"]);
  assert.ok(plans.includes("claude"));
  // Without a plan nothing is claimed while two accounts still have quota.
  const none = harness({ files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: {
    [`${one.auth_index} ${CLAUDE_USAGE}`]: claudeWeeklyOut(), [CLAUDE_USAGE]: clone(FX.claude), [CLAUDE_CLAIM]: { result: "reset" } } });
  await none.refresh();
  assert.equal(none.providerCalls(CLAUDE_CLAIM).length, 0);
});
test("a Claude 5-hour limit alerts without spending the weekly bank", async () => {
  const h = harness({ files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: claudeSessionOut() } });
  await h.refresh();
  assert.ok(h.state.accounts.every((a) => a.status === "exhausted" && a.creditCount === 1));
  assert.equal(h.providerCalls(CLAUDE_CLAIM).length, 0);
  assert.equal(h.toasts.length, 1);
  // Recovery is reported once an account is verified available.
  const recovered = harness({ files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: FX.claude },
    state: { exhausted: { ...h.state.exhausted } } });
  await recovered.refresh();
  assert.equal(recovered.events.filter((e) => e.type === "pool-recovered").length, 1);
  assert.deepEqual(recovered.state.exhausted, {});
});

test("a Claude bank without a readable organization is shown but never claimed", async () => {
  const h = harness({ files: claudeFiles(), org: null, expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: claudeWeeklyOut() } });
  await h.refresh();
  for (const a of h.state.accounts) {
    assert.deepEqual([a.creditsKnown, a.creditCount, a.credits.length], [true, 1, 0]);
    assert.match(a.creditNote, /Organization unknown/);
  }
  assert.equal(h.providerCalls(CLAUDE_CLAIM).length, 0);
  assert.equal(h.toasts.length, 1, "the pool is exhausted and nothing will reset it");
});

test("a missing cedar_ember block leaves the bank unknown and blocks automation", async () => {
  const u = claudeWeeklyOut();
  delete u.cedar_ember;
  const h = harness({ files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: u } });
  await h.refresh();
  for (const a of h.state.accounts) {
    assert.equal(a.creditsKnown, false);
    assert.equal(a.creditNote, "Reset bank not reported");
  }
  assert.equal(h.providerCalls(CLAUDE_CLAIM).length, 0);
});

test("an exhausted Codex pool redeems with its deterministic per-credit request id", async () => {
  const h = harness({ files: codexFiles(), expected: { codex: 2, claude: 0 }, routes: {
    [CODEX_USAGE]: FX.codexExhausted,
    [`codex-idx-1 ${CODEX_CREDITS}`]: FX.codexCredits, [`codex-idx-2 ${CODEX_CREDITS}`]: noCredits(),
    [`${CODEX_CREDITS}/consume`]: { code: "reset" },
  } });
  assert.deepEqual(await h.refresh(), { degraded: false, followUp: true });
  const [claim] = h.providerCalls(`${CODEX_CREDITS}/consume`);
  assert.equal(claim.body.auth_index, "codex-idx-1");
  assert.deepEqual(JSON.parse(claim.body.data), { redeem_request_id: redemptionId("acct-1", CREDIT), credit_id: CREDIT });
  assert.equal(h.state.resets.codex.last.outcome, "reset");
});

test("an unknown redemption outcome keeps its identity, records the error and retries it", async () => {
  let reply = { status_code: 502, body: "bad gateway" };
  const h = harness({ files: codexFiles(), expected: { codex: 2, claude: 0 }, routes: {
    [CODEX_USAGE]: FX.codexExhausted,
    [`codex-idx-1 ${CODEX_CREDITS}`]: FX.codexCredits, [`codex-idx-2 ${CODEX_CREDITS}`]: noCredits(),
    [`${CODEX_CREDITS}/consume`]: () => reply,
  } });
  assert.deepEqual(await h.refresh(), { degraded: false, followUp: true });
  const pending = h.state.resets.codex.pending;
  assert.equal(pending.phase, "uncertain");
  assert.match(h.state.resets.codex.error.message, /HTTP 502/);
  assert.equal(h.events.filter((e) => e.type === "reset-error")[0].provider, "codex");
  reply = { code: "already_redeemed" };
  h.advance(30000);
  await h.refresh();
  const ids = h.providerCalls(`${CODEX_CREDITS}/consume`).map((c) => JSON.parse(c.body.data).redeem_request_id);
  assert.deepEqual(ids, [pending.requestId, pending.requestId]);
  assert.equal(h.state.resets.codex.pending, null);
  assert.equal(h.state.resets.codex.error, undefined, "the error clears once the outcome is known");
});

test("a transient usage failure is retried once, then leaves the account unverified and asks for a quick re-read", async () => {
  let attempts = 0;
  const h = harness({ files: codexFiles(), routes: {
    [`codex-idx-1 ${CODEX_USAGE}`]: () => { attempts++; return { status_code: 502, body: "" }; },
    [`codex-idx-2 ${CODEX_USAGE}`]: FX.codexAvailable, [CODEX_CREDITS]: FX.codexCredits,
  } });
  assert.deepEqual(await h.refresh(), { degraded: true, followUp: false });
  assert.equal(attempts, 2);
  const a = h.state.accounts.find((x) => x.id === "codex-1.json");
  assert.deepEqual([a.status, a.creditsKnown, a.errorTransient], ["unknown", false, true]);
  assert.equal(h.events.find((e) => e.type === "refresh-degraded").accounts[0].id, "codex-1.json");
});

test("a proxy failure invalidates saved status instead of reusing it", async () => {
  const h = harness({ files: [], routes: {}, state: { accounts: [{ id: "x", provider: "codex", status: "available", usageUpdatedAt: new Date(T0).toISOString() }] } });
  const broken = createRefresh({ state: h.state, management: async () => { throw new Error("proxy down"); },
    event: async (type, d) => h.events.push({ type, ...d }), save: async () => {}, loadPolicies: async () => resetPolicies({}) });
  assert.deepEqual(await broken.refresh(), { degraded: true, followUp: false });
  assert.equal(h.state.accounts[0].status, "unknown");
  assert.equal(h.state.error, "proxy down");
  assert.equal(h.events.at(-1).type, "refresh-error");
});

test("a failed or malformed Codex bank read leaves the bank unknown but keeps verified usage", async () => {
  for (const credits of [null, { credits: "soon" }, { status_code: 500, body: "" }]) {
    const h = harness({ files: codexFiles(), routes: { [CODEX_USAGE]: FX.codexAvailable, [CODEX_CREDITS]: credits } });
    assert.deepEqual(await h.refresh(), { degraded: false, followUp: false });
    for (const a of h.state.accounts) {
      assert.equal(a.status, "available", JSON.stringify(credits));
      assert.deepEqual([a.creditsKnown, a.credits.length, a.error], [false, 0, undefined]);
    }
  }
});

test("auth files for unknown providers are ignored", async () => {
  const h = harness({ files: [file("gemini", 1), file("constructor", 2), ...codexFiles()], routes: {
    [CODEX_USAGE]: FX.codexAvailable, [CODEX_CREDITS]: FX.codexCredits,
  } });
  await h.refresh();
  assert.deepEqual(h.state.accounts.map((a) => a.provider), ["codex", "codex"]);
});

test("without a client version from the proxy Claude reads go out without Claude Code identity", async () => {
  const h = harness({ files: claudeFiles(), versions: {}, routes: { [CLAUDE_USAGE]: FX.claude } });
  await h.refresh();
  const header = h.providerCalls(CLAUDE_USAGE)[0].body.header;
  assert.equal(header["User-Agent"], undefined);
  assert.equal(header["x-app"], undefined);
  assert.equal(header["anthropic-beta"], "oauth-2025-04-20");
  assert.ok(h.state.accounts.every((a) => a.status === "available"), "usage still reads");
});

test("each account reports where its routed models came from", async () => {
  const live = { source: "live", fetched_at: new Date(T0 - 60000).toISOString(), checked_at: new Date(T0).toISOString(), models: 13 };
  const failed = { source: "pending", checked_at: new Date(T0).toISOString(), models: 0, error: "model list returned HTTP 401" };
  const h = harness({ files: [file("claude", 1, { live_models: live }), file("claude", 2, { live_models: failed }), file("claude", 3)],
    routes: { [CLAUDE_USAGE]: FX.claude } });
  await h.refresh();
  const byId = Object.fromEntries(h.state.accounts.map((a) => [a.id, a]));
  assert.deepEqual(byId["claude-1.json"].modelList, { source: "live", fetchedAt: live.fetched_at, checkedAt: live.checked_at, models: 13, error: null });
  assert.deepEqual(byId["claude-2.json"].modelList, { source: "pending", fetchedAt: null, checkedAt: failed.checked_at, models: 0, error: failed.error });
  assert.equal(byId["claude-3.json"].modelList, undefined, "older proxies report nothing");
});

test("a login the provider rejected is named, alerts once, and clears only after a verified read", async () => {
  // 2026-09-29: the proxy logged invalid_grant for claude-3 from 08:01; the
  // usage read answered 401, then 429, and nothing said "sign in again".
  const reauth = { code: "reauth_required", http_status: 400, message: 'token refresh failed with status 400: {"error": "invalid_grant"}',
    since: new Date(T0 - 3600000).toISOString(), last_failed_at: new Date(T0).toISOString(), attempts: 12 };
  const files = [file("claude", 1), file("claude", 2), file("claude", 3, { refresh_error: reauth })];
  const routes = { [CLAUDE_USAGE]: FX.claude, [`claude-idx-3 ${CLAUDE_USAGE}`]: { status_code: 429, body: "{}" } };
  const h = harness({ files, routes });
  await h.refresh();
  const dead = h.state.accounts.find((a) => a.id === "claude-3.json");
  assert.deepEqual(dead.refreshError, { code: "reauth_required", httpStatus: 400, message: reauth.message, since: reauth.since, lastFailedAt: reauth.last_failed_at, attempts: 12 });
  assert.equal(dead.error, "Account provider returned HTTP 429", "the usage verdict is kept as is");
  assert.equal(h.state.accounts.find((a) => a.id === "claude-1.json").refreshError, undefined, "healthy accounts, and older proxies, report nothing");
  const raised = h.events.filter((e) => e.type === "reauth-required");
  assert.deepEqual(raised.map((e) => [e.accountId, e.provider, e.since]), [["claude-3.json", "claude", reauth.since]]);
  assert.equal(h.toasts.length, 1);
  assert.match(h.toasts[0].title, /Claude sign-in expired/);
  assert.match(h.toasts[0].body, /3@example\.invalid/);
  assert.deepEqual(h.events.find((e) => e.type === "refresh-degraded").accounts,
    [{ id: "claude-3.json", error: "Account provider returned HTTP 429", transient: false, refreshError: "reauth_required" }]);
  assert.ok(h.saves.at(-1).reauth["claude-3.json"], "the alert is saved with the pool state");

  h.advance(120000);
  await h.refresh();
  assert.equal(h.toasts.length, 1, "one alert per episode");

  // A proxy restart forgets the failure; the dead login still reads 429.
  files[2] = file("claude", 3);
  h.advance(120000);
  await h.refresh();
  assert.equal(h.events.filter((e) => e.type === "reauth-cleared").length, 0);
  assert.ok(h.state.reauth["claude-3.json"]);

  // New login: no refresh failure and a verified usage read.
  delete routes[`claude-idx-3 ${CLAUDE_USAGE}`];
  h.advance(120000);
  await h.refresh();
  assert.deepEqual(h.events.filter((e) => e.type === "reauth-cleared").map((e) => [e.accountId, e.reason]), [["claude-3.json", "recovered"]]);
  assert.deepEqual(h.state.reauth, {});
  assert.equal(h.state.accounts.find((a) => a.id === "claude-3.json").status, "available");
  assert.equal(h.toasts.length, 1);
});

test("a refresh failure the proxy is still retrying is shown but does not alert", async () => {
  const retrying = { code: "refresh_failed", http_status: 0, message: "context deadline exceeded", since: new Date(T0).toISOString(), last_failed_at: new Date(T0).toISOString(), attempts: 1 };
  const h = harness({ files: [file("claude", 1, { refresh_error: retrying }), file("claude", 2), file("claude", 3)], routes: { [CLAUDE_USAGE]: FX.claude } });
  await h.refresh();
  const a = h.state.accounts.find((x) => x.id === "claude-1.json");
  assert.deepEqual([a.status, a.refreshError.code, a.refreshError.httpStatus], ["available", "refresh_failed", null]);
  assert.equal(h.events.filter((e) => e.type.startsWith("reauth")).length, 0);
  assert.equal(h.toasts.length, 0);
});

test("the capacity forecast gets each cycle's accounts and samples, and its failure never fails the cycle", async () => {
  const seen = [];
  const routes = { [CODEX_USAGE]: FX.codexAvailable, [CODEX_CREDITS]: FX.codexCredits, [CLAUDE_USAGE]: FX.claude };
  const h = harness({ files: [...codexFiles(), ...claudeFiles()], routes, forecast: async (input) => { seen.push(input); } });
  await h.refresh();
  assert.equal(seen.length, 1);
  assert.equal(seen[0].accounts.length, 5);
  assert.ok(seen[0].accounts.every((a) => Array.isArray(a.quotaWindows) && Array.isArray(a.credits)));
  assert.ok(seen[0].newSamples.length >= 5, "first sight samples every window");
  assert.ok(seen[0].newSamples.some((s) => s.common === false), "model-specific windows are sampled");
  assert.equal(seen[0].resetAutomation, true);
  assert.ok(seen[0].resetPolicies.codex && seen[0].resetPolicies.claude);
  const broken = harness({ files: codexFiles(), routes, expected: { codex: 2, claude: 0 }, forecast: async () => { throw new Error("boom"); } });
  assert.deepEqual(await broken.refresh(), { degraded: false, followUp: false });
  assert.equal(broken.saves.length, 1);
  assert.deepEqual(broken.events.filter((e) => e.type === "capacity-forecast-error").map((e) => e.message), ["boom"]);
});
test("refill demand wiring: only refill providers get the forecast's demand; a failure falls back", async () => {
  const seen = [];
  const asked = [];
  const h = harness({ files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: clone(FX.claude) },
    refillDemand: ({ policy }) => { asked.push(policy.provider); return { unitsPerHour: 0.02, weights: {} }; },
    forecast: async ({ resetPolicies }) => { seen.push(resetPolicies); } });
  await h.refresh();
  assert.deepEqual(asked, ["claude"]);
  assert.deepEqual(seen[0].claude.refillDemand, { unitsPerHour: 0.02, weights: {} });
  assert.equal("refillDemand" in seen[0].codex, false);
  const broken = harness({ files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: clone(FX.claude) },
    refillDemand: () => { throw new Error("boom"); } });
  await broken.refresh();
  assert.deepEqual(broken.events.filter((e) => e.type === "refill-demand-error").map((e) => e.message), ["boom"]);
  assert.equal(broken.state.error, null, "the refresh still completes");
});

test("drain-first wiring: a planned refill holder ranks first while it still lists that credit", async () => {
  const three = file("claude", 3);
  // claude-3's refill expires two days before its weekly reset.
  const soon = clone(FX.claude); soon.cedar_ember.grants[0].ends_at = "2026-09-28T13:00:00+00:00";
  const until = Date.parse(soon.cedar_ember.grants[0].ends_at);
  const seen = [];
  const run = async (plan, over = {}) => {
    const h = harness({ files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: clone(FX.claude), [`${three.auth_index} ${CLAUDE_USAGE}`]: soon, ...over },
      drainFirst: ({ provider }) => (provider === "claude" ? plan : null), forecast: async ({ resetPolicies }) => { seen.push(resetPolicies.claude); } });
    await h.refresh();
    return Object.fromEntries(h.state.accounts.map((a) => [a.id, a.priority]));
  };
  // Same reset date everywhere: ties go by id, so claude-1 leads without a plan.
  assert.equal((await run(null))["claude-1.json"], 100);
  assert.equal(seen.at(-1).drainFirst, undefined);
  const boosted = await run(new Map([[three.id, until]]));
  assert.equal(boosted[three.id], 100, "drained first");
  assert.deepEqual(seen.at(-1).drainFirst, new Map([[three.id, until]]), "the refill runway check sees the same order");
  // A plan for a credit the account no longer lists (redeemed or replaced) is ignored.
  assert.equal((await run(new Map([[three.id, until + 3600000]])))[three.id], 98);
  const spent = clone(FX.claude); spent.cedar_ember.grants = [];
  assert.equal((await run(new Map([[three.id, until]]), { [`${three.auth_index} ${CLAUDE_USAGE}`]: spent }))[three.id], 98);
  // A failing hook ranks as before and is logged.
  const broken = harness({ files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: clone(FX.claude) }, drainFirst: () => { throw new Error("boom"); } });
  await broken.refresh();
  assert.ok(broken.events.some((e) => e.type === "drain-first-error" && e.message === "boom"));
  assert.equal(broken.state.error, null);
});

test("a credit redeemed outside AgentPools is logged once as an external reset-result", async () => {
  const one = file("claude", 1);
  let usage = claudeWeeklyOut();
  const h = harness({ files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: {
    [`${one.auth_index} ${CLAUDE_USAGE}`]: () => usage, [CLAUDE_USAGE]: clone(FX.claude) } });
  await h.refresh();
  assert.equal(h.events.filter((e) => e.type === "reset-result").length, 0);
  // Redeemed on the web: usage cleared, bank one smaller, reset date unchanged.
  usage = clone(FX.claude); usage.cedar_ember.grants = [];
  h.advance(120000);
  await h.refresh();
  const ext = h.events.filter((e) => e.type === "reset-result");
  assert.deepEqual(ext, [{ type: "reset-result", provider: "claude", accountId: one.id, creditsBefore: 1, creditsAfter: 0, weeklyUsedBefore: 100, weeklyUsedAfter: 0, outcome: "reset", source: "external" }]);
  h.advance(120000);
  await h.refresh();
  assert.equal(h.events.filter((e) => e.type === "reset-result").length, 1, "not repeated");
});

test("our own redemption is not reported as external on the next read", async () => {
  const one = file("claude", 1);
  let usage = claudeWeeklyOut();
  const h = harness({ files: claudeFiles(), expected: { codex: 0, claude: 3 }, creditPlan: () => new Set([one.id]), routes: {
    [`${one.auth_index} ${CLAUDE_USAGE}`]: () => usage, [CLAUDE_USAGE]: clone(FX.claude), [CLAUDE_CLAIM]: { result: "reset", resets_left: 0 } } });
  await h.refresh();
  assert.equal(h.providerCalls(CLAUDE_CLAIM).length, 1);
  usage = clone(FX.claude); usage.cedar_ember.grants = [];
  h.advance(30000);
  await h.refresh();
  assert.deepEqual(h.events.filter((e) => e.type === "reset-result").map((e) => e.source ?? "pool"), ["pool"]);
});

test("a credit the plan still loses is alerted once, and cleared when the plan saves it", async () => {
  let outcome = "lapses";
  const expiresAt = new Date(T0 + 40 * 3600000).toISOString();
  const doc = () => ({ staleAfter: new Date(T0 + 3600000).toISOString(), providers: { claude: { state: "conserve", demand: { unitsPerHour: 0.02 },
    creditPlan: { method: "counterfactual", credits: [{ accountId: "claude-2.json", expiresAt, action: "drain-first", drainBy: expiresAt, unitsLeft: 0.9, outcome, needUnitsPerHour: 0.03 }] } } } });
  const h = harness({ files: claudeFiles(), expected: { codex: 0, claude: 3 }, routes: { [CLAUDE_USAGE]: clone(FX.claude) }, forecast: async () => doc() });
  await h.refresh();
  await h.refresh();
  assert.equal(h.toasts.length, 1);
  assert.match(h.toasts[0].title, /Claude refill at risk/);
  assert.match(h.toasts[0].body, /^2@example\.invalid: its refill expires .* current pace \(3\.4 acct-weeks\/week\)\. It needs about 5\.0 acct-weeks\/week until then\.$/);
  assert.deepEqual(Object.keys(h.state.creditRisk), ["claude-2.json|" + expiresAt]);
  assert.equal(h.events.filter((e) => e.type === "credit-at-risk").length, 1);
  outcome = "used";
  await h.refresh();
  assert.deepEqual(h.events.filter((e) => e.type === "credit-risk-cleared").map((e) => e.reason), ["planned"]);
  assert.deepEqual(h.state.creditRisk, {});
});

const CODEX_SUB = (n) => `https://chatgpt.com/backend-api/subscriptions?account_id=acct-${n}`;
const CODEX_PRICING = "https://chatgpt.com/backend-api/checkout_pricing_config/configs/US";
const CLAUDE_PROFILE = "https://api.anthropic.com/api/oauth/profile";
const PRICING = { currency_config: { symbol_code: "USD", pro: { month: { amount: 200 } }, prolite: { month: { amount: 100 } } } };
const sub = (plan, until) => ({ plan_type: plan, billing_period: "monthly", billing_currency: "USD", will_renew: true, active_until: until });

test("plan reads: provider price for Codex, list price for the Claude tier, read twice a day, last plan kept on failure", async () => {
  let codexSubFails = false;
  const h = harness({ readPlans: true, versions: { claude: "2.1.284", codex: "0.162.1" }, files: [file("codex", 1), file("codex", 2), file("claude", 1)], routes: {
    [CODEX_USAGE]: FX.codexAvailable, [CODEX_CREDITS]: FX.codexCredits, [CLAUDE_USAGE]: FX.claude,
    [CODEX_SUB(1)]: () => (codexSubFails ? { status_code: 403, body: "{}" } : sub("pro", "2026-11-04T00:00:00Z")),
    [CODEX_SUB(2)]: sub("prolite", "2026-10-20T00:00:00Z"),
    [CODEX_PRICING]: PRICING,
    [CLAUDE_PROFILE]: { organization: { rate_limit_tier: "default_claude_max_20x", organization_type: "claude_max", subscription_status: "active" } },
  } });
  await h.refresh();
  const byId = () => Object.fromEntries(h.state.accounts.map((a) => [a.id, a]));
  let a = byId();
  assert.deepEqual([a["codex-1.json"].plan.plan, a["codex-1.json"].plan.price.usdPerMonth, a["codex-1.json"].plan.price.source], ["pro", 200, "provider"]);
  assert.deepEqual([a["codex-2.json"].plan.label, a["codex-2.json"].plan.price.usdPerMonth], ["ChatGPT Pro 100", 100]);
  assert.equal(a["codex-1.json"].plan.renewsAt, "2026-11-04T00:00:00Z");
  assert.deepEqual([a["claude-1.json"].plan.plan, a["claude-1.json"].plan.price.usdPerMonth, a["claude-1.json"].plan.price.source], ["max_20x", 200, "list"]);
  assert.equal(h.providerCalls(CODEX_PRICING).length, 1, "the price table is shared across accounts");
  const billing = h.providerCalls(CODEX_SUB(1))[0].body.header;
  assert.equal(billing["User-Agent"], "codex_cli_rs/0.162.1", "billing reads present the Codex CLI");
  assert.equal(billing.Authorization, "Bearer $TOKEN$", "extra headers cannot replace the token placeholder");
  // Within the TTL no billing call is made and the plan is carried.
  h.advance(60000);
  await h.refresh();
  assert.equal(h.providerCalls(CODEX_SUB(1)).length, 1);
  assert.equal(byId()["codex-1.json"].plan.plan, "pro");
  // After the TTL a failed read keeps the last plan and records the error.
  codexSubFails = true;
  h.advance(12 * 3600000);
  await h.refresh();
  a = byId();
  assert.equal(h.providerCalls(CODEX_SUB(1)).length, 2);
  assert.equal(a["codex-1.json"].plan.plan, "pro");
  assert.match(a["codex-1.json"].planError, /403/);
  assert.equal(a["codex-1.json"].status, "available", "a plan failure never costs the account its usage");
  assert.equal(h.events.filter((e) => e.type === "plan-read-error" && e.accountId === "codex-1.json").length, 1);
  // Retried an hour later, not every cycle.
  h.advance(60000);
  await h.refresh();
  assert.equal(h.providerCalls(CODEX_SUB(1)).length, 2);
  h.advance(3600000);
  await h.refresh();
  assert.equal(h.providerCalls(CODEX_SUB(1)).length, 3);
  assert.equal(h.events.filter((e) => e.type === "plan-read-error" && e.accountId === "codex-1.json").length, 1, "the same failure is logged once");
  codexSubFails = false;
  h.advance(3600000);
  await h.refresh();
  assert.equal(h.providerCalls(CODEX_SUB(1)).length, 4);
  assert.equal(byId()["codex-1.json"].planError, undefined);
});

test("plan reads are off unless asked for", async () => {
  const h = harness({ files: [file("codex", 1)], expected: { codex: 1, claude: 0 }, routes: { [CODEX_USAGE]: FX.codexAvailable, [CODEX_CREDITS]: FX.codexCredits } });
  await h.refresh();
  assert.equal(h.providerCalls(CODEX_SUB(1)).length, 0);
  assert.equal(h.state.accounts[0].plan, undefined);
});

test("a plan read that hangs is abandoned without holding the refresh", async () => {
  const h = harness({ readPlans: true, planTimeoutMs: 200, files: [file("claude", 1)], expected: { codex: 0, claude: 1 }, routes: {
    [CLAUDE_USAGE]: FX.claude,
    [CLAUDE_PROFILE]: () => new Promise(() => {}),
  } });
  const started = Date.now();
  // Real timer: the harness clock is fake, the plan timeout is not.
  const r = await h.refresh();
  assert.ok(Date.now() - started < 5000);
  assert.equal(h.state.accounts[0].status, "available");
  assert.equal(h.state.accounts[0].planError, "Plan read timed out");
  assert.equal(r.degraded, false);
});
