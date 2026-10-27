import test from "node:test";
import assert from "node:assert/strict";
import { chooseReset, accountOut, normalizeUsage, refillNeedMs, rankAccounts, liveDrainFirst } from "./routing.mjs";
import { processReset, resetExpected, RESET_BACKOFF_MS, externalRedemptions } from "./reset-lifecycle.mjs";
import { creditRiskTransitions } from "./notifications.mjs";
import { canRecoverQuota } from "./quota-recovery.mjs";
import { refreshUsage } from "./usage.mjs";
import { resetPolicy, migrateState, RESET_POLICY_DEFAULTS } from "./resets.mjs";
import { claudeBank, claudeClaimOutcome, claudeClaimError, claudeOrganization, claudeResetBody } from "./provider-claude.mjs";
import { PROVIDERS, PROVIDER_IDS, providerByLogin } from "./providers.mjs";
const migrateResetState = migrateState;
const RESET_REQUEST_ID = { codex: PROVIDERS.codex.requestId, claude: PROVIDERS.claude.requestId };

const now = Date.parse("2026-09-23T20:00:00Z");
const iso = (offset = 0) => new Date(now + offset).toISOString();
const day = 86400000;
const claudePolicy = (over = {}) => ({ ...resetPolicy("claude", undefined, 3), ...over });
// A Claude account as refreshUsage leaves it, built from the provider's own limits block.
// The default weekly reset is 5 days out: past the refill runway (half a week).
const claude = (id, { session = 0, weekly = 0, fable = 0, weeklyReset = 5 * day, credits = 1, status } = {}) => {
  const usage = normalizeUsage("claude", { limits: [
    { kind: "session", group: "session", percent: session, resets_at: iso(3600000) },
    { kind: "weekly_all", group: "weekly", percent: weekly, resets_at: iso(weeklyReset) },
    { kind: "weekly_scoped", group: "weekly", percent: fable, resets_at: iso(weeklyReset), scope: { model: { display_name: "Fable" } } },
  ] }, now);
  return { id, accountId: id, provider: "claude", disabled: false, ...usage, ...(status ? { status } : {}),
    usageUpdatedAt: iso(), creditsUpdatedAt: iso(), creditsKnown: true, creditCount: credits,
    credits: credits ? [{ id: "opus55-launch-promax-20260921", expires_at: iso(29 * day) }] : [] };
};

test("policy defaults: Claude waits for the weekly all-models limit, Codex keeps its existing rule", () => {
  assert.deepEqual(RESET_POLICY_DEFAULTS.claude, { enabled: true, trigger: "weekly", headroomPercent: 0, redeemBeforeLapse: true, maxRunwayPercent: 50 });
  assert.deepEqual(RESET_POLICY_DEFAULTS.codex, { enabled: true, trigger: "any", headroomPercent: 5, redeemBeforeLapse: true });
  assert.equal(resetPolicy("claude", undefined, 3).redemption, "refill");
  assert.equal(resetPolicy("codex", undefined, 4).redemption, "reset");
  assert.equal("maxRunwayPercent" in resetPolicy("codex", { maxRunwayPercent: 10 }, 4), false, "runway applies to refills only");
  assert.equal(resetPolicy("claude", { maxRunwayPercent: 25 }, 3).maxRunwayPercent, 25);
  assert.equal(resetPolicy("claude", { maxRunwayPercent: 120 }, 3).maxRunwayPercent, 50);
  assert.equal(resetPolicy("claude", { redeemBeforeLapse: false }, 3).redeemBeforeLapse, false);
  assert.equal(resetPolicy("codex", { redeemBeforeLapse: "yes" }, 4).problems.length, 1);
  const p = resetPolicy("claude", { headroomPercent: 3 }, 3);
  assert.equal(p.trigger, "weekly"); assert.equal(p.headroomPercent, 3); assert.deepEqual(p.problems, []);
});
test("invalid policy values fall back to defaults and are reported; Codex headroom stays under its 10% gate", () => {
  const p = resetPolicy("claude", { enabled: "yes", trigger: "fable", headroomPercent: -1 }, 3);
  assert.deepEqual([p.enabled, p.trigger, p.headroomPercent], [true, "weekly", 0]);
  assert.equal(p.problems.length, 3);
  assert.equal(resetPolicy("codex", { headroomPercent: 20 }, 4).headroomPercent, 5);
  assert.equal(resetPolicy("codex", { headroomPercent: 10 }, 4).headroomPercent, 10);
  assert.equal(resetPolicy("claude", "on", 3).problems.length, 1);
});
test("an unknown roster size disables automation instead of trusting a partial listing", () => {
  const accounts = [claude("a", { weekly: 100 })];
  assert.equal(resetPolicy("claude", {}, undefined).expectedCount, Infinity);
  assert.equal(chooseReset(accounts, resetPolicy("claude", {}, undefined), now), null);
});

test("weekly trigger: 5-hour and Fable limits never spend a Claude reset", () => {
  const policy = claudePolicy();
  const sessionOut = [claude("a", { session: 100, weekly: 40 }), claude("b", { session: 100, weekly: 60 }), claude("c", { session: 100, weekly: 99 })];
  assert.ok(sessionOut.every((a) => a.status === "exhausted"), "the provider reports these accounts at a limit");
  assert.equal(chooseReset(sessionOut, policy, now), null);
  const fableOut = [claude("a", { fable: 100, weekly: 70 }), claude("b", { fable: 100, weekly: 99 }), claude("c", { fable: 100, weekly: 50 })];
  assert.equal(chooseReset(fableOut, policy, now), null);
  // The looser "any" trigger is still available by configuration.
  assert.ok(chooseReset(sessionOut, claudePolicy({ trigger: "any" }), now));
});
test("weekly trigger fires only when every Claude account is out on the weekly all-models limit", () => {
  const policy = claudePolicy();
  const pool = [claude("a", { weekly: 100, weeklyReset: 1 * day }), claude("b", { weekly: 100, weeklyReset: 5 * day }), claude("c", { weekly: 100, weeklyReset: 2 * day })];
  assert.deepEqual(chooseReset(pool, policy, now), { accountId: "b", creditId: "opus55-launch-promax-20260921" });
  assert.equal(chooseReset([...pool.slice(0, 2), claude("c", { weekly: 99 })], policy, now), null);
  assert.ok(chooseReset([...pool.slice(0, 2), claude("c", { weekly: 99 })], claudePolicy({ headroomPercent: 1 }), now));
  assert.equal(chooseReset(pool.slice(0, 2), policy, now), null, "an unenrolled account blocks automation");
  assert.equal(chooseReset([...pool.slice(0, 2), { ...pool[2], status: "unknown" }], policy, now), null);
  assert.equal(chooseReset([...pool.slice(0, 2), { ...pool[2], creditsKnown: false }], policy, now), null);
  assert.equal(accountOut({ ...pool[0], windows: [] }, policy), false, "no weekly window is not evidence");
  assert.equal(chooseReset(pool.map((a) => ({ ...a, provider: "codex" })), policy, now), null, "providers never mix");
});
test("grants sharing one expiry inside 24 hours still go to the account furthest from its natural reset", () => {
  const closing = (a) => ({ ...a, credits: [{ ...a.credits[0], expires_at: iso(3600000) }] });
  const pool = [claude("a", { weekly: 100, weeklyReset: 1 * day }), claude("b", { weekly: 100, weeklyReset: 5 * day }), claude("c", { weekly: 100, weeklyReset: 2 * day })].map(closing);
  assert.equal(chooseReset(pool, claudePolicy(), now).accountId, "b");
});

test("credit planner: an out account spends its own lapsing credit early; nothing else changes", () => {
  const policy = claudePolicy();
  const pool = [claude("a", { weekly: 100, weeklyReset: 4 * day }), claude("b", { weekly: 30 }), claude("c", { weekly: 100, weeklyReset: 5 * day })];
  assert.equal(chooseReset(pool, policy, now), null, "without the planner, b's quota blocks every reset");
  assert.equal(chooseReset(pool, policy, now, new Set(), new Set()), null);
  assert.equal(chooseReset(pool, policy, now, new Set(), new Set(["b"])), null, "b is not out");
  assert.deepEqual(chooseReset(pool, policy, now, new Set(), new Set(["a", "b"])), { accountId: "a", creditId: "opus55-launch-promax-20260921", trigger: "before-lapse" });
  // The soonest-expiring of several eligible holders goes first.
  const sooner = pool.map((a) => (a.id === "c" ? { ...a, credits: [{ ...a.credits[0], expires_at: iso(2 * day) }] } : a));
  assert.equal(chooseReset(sooner, policy, now, new Set(), new Set(["a", "c"])).accountId, "c");
  // A refused credit backs off; an unknown bank anywhere blocks; the roster must be complete.
  assert.equal(chooseReset(pool, policy, now, new Set(["a|opus55-launch-promax-20260921"]), new Set(["a"])), null);
  assert.equal(chooseReset([pool[0], { ...pool[1], creditsKnown: false }, pool[2]], policy, now, new Set(), new Set(["a"])), null);
  assert.equal(chooseReset(pool.slice(0, 2), policy, now, new Set(), new Set(["a"])), null);
  // Only the weekly all-models limit counts as out for Claude, as before.
  assert.equal(chooseReset([claude("a", { session: 100, fable: 100, weekly: 60 }), pool[1], pool[2].credits && claude("c", { weekly: 20 })], policy, now, new Set(), new Set(["a"])), null);
  // When every account is out the normal rule decides, planner or not.
  const allOut = [pool[0], claude("b", { weekly: 100, weeklyReset: 6 * day }), pool[2]];
  assert.deepEqual(chooseReset(allOut, policy, now, new Set(), new Set(["a"])), { accountId: "b", creditId: "opus55-launch-promax-20260921" });
});

const block = (over = {}) => ({
  eligible: true, ineligible_reason: null, at_limit: false, exhausted: [],
  grants: [{ id: "opus55-launch-promax-20260921", label: "Claude Opus 5.5 launch", resets_total: 1, resets_left: 1,
    starts_at: "2026-09-22T16:00:00+00:00", ends_at: "2026-10-22T16:00:00+00:00",
    clears: ["five_hour", "seven_day", "seven_day_overage_included"], paused: false, usable_now: true, use_requires_limit: false }],
  next_grant_id: "opus55-launch-promax-20260921", weekly_resets_at: null, cooldown_until: null, ...over,
});
test("Claude bank reads the observed cedar_ember block", () => {
  const bank = claudeBank(block(), now);
  assert.equal(bank.count, 1);
  assert.deepEqual(bank.credits, [{ id: "opus55-launch-promax-20260921", expires_at: "2026-10-22T16:00:00+00:00", label: "Claude Opus 5.5 launch" }]);
  assert.equal(bank.note, null);
});
test("Claude bank: ineligible, cooling, expired, paused or unnamed grants offer nothing; a missing block is unknown", () => {
  assert.equal(claudeBank(undefined, now), null);
  assert.equal(claudeBank({ grants: [] }, now), null);
  const surface = claudeBank({ eligible: false, ineligible_reason: "surface", grants: [] }, now);
  assert.deepEqual([surface.count, surface.credits.length], [0, 0]);
  assert.match(surface.note, /surface/);
  assert.equal(claudeBank(block({ cooldown_until: iso(60000) }), now).credits.length, 0);
  assert.equal(claudeBank(block({ cooldown_until: iso(-60000) }), now).credits.length, 1);
  assert.equal(claudeBank(block({ next_grant_id: null }), now).credits.length, 0);
  const grant = block().grants[0];
  for (const g of [{ ...grant, ends_at: iso(-1) }, { ...grant, paused: true }, { ...grant, usable_now: false },
    { ...grant, resets_left: 0 }, { ...grant, starts_at: iso(day) }, { ...grant, id: "Bad Id" }])
    assert.equal(claudeBank(block({ grants: [g] }), now).credits.length, 0);
});
test("Claude claim outcomes map onto the shared lifecycle; unconfirmed results keep their identity", () => {
  assert.deepEqual(claudeClaimOutcome({ result: "reset", resets_left: 0 }), { code: "reset" });
  assert.deepEqual(claudeClaimOutcome({ result: "already_used" }), { code: "already_redeemed" });
  assert.deepEqual(claudeClaimOutcome({ result: "not_limited" }), { code: "nothing_to_reset" });
  assert.deepEqual(claudeClaimOutcome({ result: "ineligible" }), { code: "no_credit" });
  assert.deepEqual(claudeClaimOutcome({ result: "unavailable" }), { code: "no_credit" });
  assert.deepEqual(claudeClaimOutcome({ result: "cooldown" }), { code: "deferred" });
  assert.deepEqual(claudeClaimOutcome({ result: "reset", reason: "reset_unconfirmed" }), { code: "unknown" });
  assert.deepEqual(claudeClaimOutcome({ result: "unavailable", reason: "stamp_indeterminate" }), { code: "unknown" });
  assert.deepEqual(claudeClaimOutcome({ result: "brand_new" }), { code: "unknown" });
  assert.deepEqual(claudeClaimError({ status: 429 }), { code: "deferred" });
  assert.deepEqual(claudeClaimError({ status: 404 }), { code: "rejected" });
  assert.throws(() => claudeClaimError(Object.assign(new Error("expired login"), { status: 401 })), /expired login/);
  assert.throws(() => claudeClaimError(Object.assign(new Error("gateway"), { status: 502 })), /gateway/);
  assert.deepEqual(claudeResetBody({ creditId: "g", requestId: "r" }), { program: "cedar_ember", grant_id: "g", request_id: "r" });
  assert.equal(claudeOrganization({ organization_uuid: "a0b1c2d3-e4f5-4a6b-8c7d-9e0f1a2b3c4d" }), "a0b1c2d3-e4f5-4a6b-8c7d-9e0f1a2b3c4d");
  assert.equal(claudeOrganization({ organization_uuid: "../x" }), null);
});
test("legacy Codex reset fields migrate into the per-provider slots once", () => {
  const legacy = { pendingReset: null, lastReset: { outcome: "reset", accountId: "x" } };
  migrateResetState(legacy);
  assert.deepEqual(legacy.resets, { codex: { pending: null, last: { outcome: "reset", accountId: "x" }, refused: {} }, claude: { pending: null, last: null, refused: {} } });
  assert.equal("pendingReset" in legacy, false);
  const codexOnly = migrateState({ exhaustedSince: "2026-09-20T00:00:00.000Z" });
  assert.deepEqual(codexOnly.exhausted, { codex: "2026-09-20T00:00:00.000Z" });
  assert.equal("exhaustedSince" in codexOnly, false);
  const again = structuredClone(legacy);
  assert.deepEqual(migrateResetState(again), legacy);
});

const exhaustedPool = () => [claude("a", { weekly: 100 }), claude("b", { weekly: 100, weeklyReset: 6 * day }), claude("c", { weekly: 100 })];
// Status is only trusted for five minutes; a later cycle sees a fresh pull.
const pulledAt = (accounts, t) => accounts.map((a) => ({ ...a, usageUpdatedAt: new Date(t).toISOString(), creditsUpdatedAt: new Date(t).toISOString() }));
const lifecycle = (state, over = {}) => processReset(state, { policy: claudePolicy(), requestId: RESET_REQUEST_ID.claude,
  now: () => now, save: async () => {}, consume: async () => ({ code: "reset" }), ...over });
test("credit planner in the lifecycle: journaled as before-lapse, off by policy, planner errors only disable it", async () => {
  const early = () => [claude("a", { weekly: 100, weeklyReset: 4 * day }), claude("b", { weekly: 30 }), claude("c", { weekly: 50 })];
  const logged = [];
  const on = claudePolicy({ redeemBeforeLapse: true });
  const state = migrateResetState({ accounts: early(), resetAutomation: true });
  const outcome = await lifecycle(state, { policy: on, plan: () => new Set(["a"]), log: async (type, d) => logged.push({ type, ...d }) });
  assert.equal(outcome, "reset");
  assert.deepEqual([state.resets.claude.last.accountId, state.resets.claude.last.trigger], ["a", "before-lapse"]);
  assert.deepEqual(logged.map((e) => [e.type, e.trigger]), [["reset-attempt", "before-lapse"], ["reset-result", "before-lapse"]]);
  const off = migrateResetState({ accounts: early(), resetAutomation: true });
  assert.equal(await lifecycle(off, { policy: claudePolicy({ redeemBeforeLapse: false }), plan: () => new Set(["a"]) }), undefined, "off by policy");
  const broken = migrateResetState({ accounts: early(), resetAutomation: true });
  const errs = [];
  assert.equal(await lifecycle(broken, { policy: on, plan: () => { throw new Error("boom"); }, log: async (type, d) => errs.push(type) }), undefined);
  assert.deepEqual(errs, ["credit-plan-error"]);
  // An unsent early claim is cancelled if the plan no longer selects it, or now selects another credit.
  const moved = migrateResetState({ accounts: early(), resetAutomation: true });
  moved.resets.claude.pending = { phase: "unsent", accountId: "a", creditId: "opus55-launch-promax-20260921", requestId: "r", trigger: "before-lapse" };
  await lifecycle(moved, { policy: on, plan: () => null });
  assert.equal(moved.resets.claude.last.outcome, "cancelled-before-send");
  const other = migrateResetState({ accounts: early(), resetAutomation: true });
  other.resets.claude.pending = { phase: "unsent", accountId: "a", creditId: "some-other-grant", requestId: "r", trigger: "before-lapse" };
  let sent = 0;
  await lifecycle(other, { policy: on, plan: () => new Set(["a"]), consume: async () => { sent++; return { code: "reset" }; } });
  assert.equal(other.resets.claude.last.creditId, "opus55-launch-promax-20260921");
  assert.equal(sent, 1);
  // An uncertain early claim whose account has since refilled is never resent.
  const cases = [
    ["bank shrank", (a) => ({ ...a, creditCount: 0, credits: [] }), "already_redeemed"],
    ["listing missing, count unchanged", (a) => ({ ...a, credits: [] }), "attention"],
    ["still listed", (a) => a, "attention"],
    ["bank unknown", (a) => ({ ...a, creditsKnown: false, creditCount: 0, credits: [] }), "attention"],
  ];
  for (const [name, edit, expect] of cases) {
    const refilled = [edit(claude("a", { weekly: 5 })), claude("b", { weekly: 30 }), claude("c", { weekly: 50 })];
    const s = migrateResetState({ accounts: refilled, resetAutomation: true });
    s.resets.claude.pending = { phase: "uncertain", accountId: "a", creditId: "opus55-launch-promax-20260921", requestId: "r", trigger: "before-lapse", creditCountAtClaim: 1 };
    let resent = 0; const logs = [];
    await lifecycle(s, { policy: on, consume: async () => { resent++; return { code: "reset" }; }, log: async (t, d) => logs.push({ t, ...d }) });
    assert.equal(resent, 0, name);
    assert.equal(expect === "attention" ? s.resets.claude.pending.phase : s.resets.claude.last.outcome, expect, name);
    if (expect === "already_redeemed") assert.equal(logs.find((l) => l.t === "reset-result").trigger, "before-lapse");
  }
  // When every account is out the all-out rule allows the retry anyway.
  const allOut = [claude("a", { weekly: 100, credits: 1 }), claude("b", { weekly: 100 }), claude("c", { weekly: 100 })];
  const ao = migrateResetState({ accounts: allOut, resetAutomation: true });
  ao.resets.claude.pending = { phase: "uncertain", accountId: "a", creditId: "opus55-launch-promax-20260921", requestId: "r", trigger: "before-lapse", creditCountAtClaim: 1 };
  let aoSent = 0;
  await lifecycle(ao, { policy: on, consume: async () => { aoSent++; return { code: "reset" }; } });
  assert.equal(aoSent, 1);
  // Under the "any" trigger every account can be out on a short window while
  // the claim's account is not weekly-out: the all-out rule still allows it.
  const anyOut = [claude("a", { session: 100, weekly: 60, credits: 1 }), claude("b", { session: 100, weekly: 60 }), claude("c", { session: 100, weekly: 60 })];
  const an = migrateResetState({ accounts: anyOut, resetAutomation: true });
  an.resets.claude.pending = { phase: "uncertain", accountId: "a", creditId: "opus55-launch-promax-20260921", requestId: "r", trigger: "before-lapse", creditCountAtClaim: 1 };
  let anSent = 0;
  await lifecycle(an, { policy: claudePolicy({ trigger: "any", redeemBeforeLapse: true }), consume: async () => { anSent++; return { code: "reset" }; } });
  assert.equal(anSent, 1);
  // A new early claim records the bank size it was made against.
  const rec = migrateResetState({ accounts: early(), resetAutomation: true });
  let seen;
  await lifecycle(rec, { policy: on, plan: () => new Set(["a"]), consume: async (acc, p) => { seen = p; return { code: "reset" }; } });
  assert.equal(seen.creditCountAtClaim, 1);
});
test("credit planner guards: weekly-out only, no spend just before a refill, furthest refill first", () => {
  const policy = claudePolicy({ redeemBeforeLapse: true });
  const b = claude("b", { weekly: 30 });
  // Refill under 24 h away: the redemption would mostly duplicate it.
  assert.equal(chooseReset([claude("a", { weekly: 100, weeklyReset: 0.5 * day }), b, claude("c", { weekly: 10 })], policy, now, new Set(), new Set(["a"])), null);
  // ...unless the credit expires before that refill.
  const expiring = claude("a", { weekly: 100, weeklyReset: 0.5 * day });
  expiring.credits = [{ ...expiring.credits[0], expires_at: iso(0.2 * day) }];
  assert.equal(chooseReset([expiring, b, claude("c", { weekly: 10 })], policy, now, new Set(), new Set(["a"])).accountId, "a");
  // A refused first credit does not fall through to the account's next one.
  const two = claude("a", { weekly: 100, weeklyReset: 3 * day });
  two.credits = [two.credits[0], { id: "second-grant", expires_at: iso(20 * day) }];
  assert.equal(chooseReset([two, b, claude("c", { weekly: 10 })], policy, now, new Set(["a|opus55-launch-promax-20260921"]), new Set(["a"])), null);
  // Expiries minutes apart but not within a day: furthest refill still decides.
  const near = claude("a", { weekly: 100, weeklyReset: 1.1 * day }), far = claude("c", { weekly: 100, weeklyReset: 6 * day });
  near.credits = [{ ...near.credits[0], expires_at: iso(20 * day) }];
  far.credits = [{ ...far.credits[0], expires_at: iso(20 * day + 180000) }];
  assert.equal(chooseReset([near, b, far], policy, now, new Set(), new Set(["a", "c"])).accountId, "c");
  // Same expiry: the account whose refill is furthest away goes first.
  const pool = [claude("a", { weekly: 100, weeklyReset: 2 * day }), b, claude("c", { weekly: 100, weeklyReset: 5 * day })];
  assert.equal(chooseReset(pool, policy, now, new Set(), new Set(["a", "c"])).accountId, "c");
  // Under the "any" trigger a short window alone is not out for early use.
  assert.equal(chooseReset([claude("a", { session: 100, weekly: 60, weeklyReset: 3 * day }), b, claude("c", { weekly: 10 })], claudePolicy({ trigger: "any", redeemBeforeLapse: true }), now, new Set(), new Set(["a"])), null);
});
test("Claude refills keep the reset date: spent only with half the week left, unless the credit expires first", () => {
  const policy = claudePolicy();
  // Every account out, but the furthest reset is 3 days away: a refill there
  // would mostly lapse at that unchanged date, so it waits.
  const short = [claude("a", { weekly: 100, weeklyReset: 1 * day }), claude("b", { weekly: 100, weeklyReset: 3 * day }), claude("c", { weekly: 100, weeklyReset: 2 * day })];
  assert.equal(chooseReset(short, policy, now), null);
  // 3.5 days is exactly half the week.
  const half = [short[0], claude("b", { weekly: 100, weeklyReset: 3.5 * day }), short[2]];
  assert.equal(chooseReset(half, policy, now).accountId, "b");
  // A credit that expires before its account resets has no later chance there: spend it.
  const expiring = claude("a", { weekly: 100, weeklyReset: 1 * day });
  expiring.credits = [{ ...expiring.credits[0], expires_at: iso(0.5 * day) }];
  assert.equal(chooseReset([expiring, short[1], short[2]], policy, now).accountId, "a");
  // One expiring after that reset waits, even inside 24 hours.
  const afterReset = claude("a", { weekly: 100, weeklyReset: 0.25 * day });
  afterReset.credits = [{ ...afterReset.credits[0], expires_at: iso(0.5 * day) }];
  assert.equal(chooseReset([afterReset, short[1], short[2]], policy, now), null);
  // The share is configurable (40% of a week is 2.8 days).
  assert.equal(chooseReset(short, claudePolicy({ maxRunwayPercent: 40 }), now).accountId, "b");
  // Early (before-lapse) use follows the same rule.
  const b = claude("b", { weekly: 30 });
  assert.equal(chooseReset([claude("a", { weekly: 100, weeklyReset: 3 * day }), b, claude("c", { weekly: 10 })], policy, now, new Set(), new Set(["a"])), null);
  assert.equal(chooseReset([claude("a", { weekly: 100, weeklyReset: 4 * day }), b, claude("c", { weekly: 10 })], policy, now, new Set(), new Set(["a"])).trigger, "before-lapse");
  // Nothing is about to be spent, so the exhaustion alert is not held back.
  assert.equal(resetExpected({ resetAutomation: true, resets: {} }, policy, short, now), false);
  // Codex resets restart the window and keep their own rule.
  assert.equal(resetPolicy("codex", undefined, 3).redemption, "reset");
});
test("Claude refill runway follows demand: the refill plus sooner-resetting quota must fit before the reset", () => {
  const hour = 3600000;
  const demand = (unitsPerHour, weights = {}) => claudePolicy({ refillDemand: { unitsPerHour, weights } });
  // All out, furthest reset 3 days away. At 0.0225 units/h one refill takes
  // about 44 h, so it fits; the flat half-week rule alone would wait.
  const short = [claude("a", { weekly: 100, weeklyReset: 1 * day }), claude("b", { weekly: 100, weeklyReset: 3 * day }), claude("c", { weekly: 100, weeklyReset: 2 * day })];
  assert.equal(chooseReset(short, claudePolicy(), now), null);
  assert.equal(chooseReset(short, demand(0.0225), now).accountId, "b");
  assert.equal(Math.round(refillNeedMs(short[1], short, { unitsPerHour: 0.0225 }, now) / hour), 44, "exhausted accounts add nothing");
  // Too little demand to use it in 3 days: the 50% cap applies, and 3 days is short of it.
  assert.equal(chooseReset(short, demand(0.005), now), null);
  // A capacity weight scales the refill (a half-size plan refills half as much).
  assert.equal(Math.round(refillNeedMs(short[1], short, { unitsPerHour: 0.0225, weights: { b: 0.5 } }, now) / hour), 22);
  // Unknown or zero demand: the cap is the rule.
  assert.equal(refillNeedMs(short[1], short, null, now), null);
  assert.equal(refillNeedMs(short[1], short, { unitsPerHour: 0 }, now), null);
  // Early path: quota left on an account that resets sooner is drained first, so it counts.
  const out = claude("a", { weekly: 100, weeklyReset: 3 * day });
  const later = [out, claude("b", { weekly: 30, weeklyReset: 6 * day }), claude("c", { weekly: 10, weeklyReset: 5 * day })];
  const sooner = [out, claude("b", { weekly: 30, weeklyReset: 2 * day }), claude("c", { weekly: 10, weeklyReset: 5 * day })];
  const early = (pool, rate) => chooseReset(pool, demand(rate), now, new Set(), new Set(["a"]));
  assert.equal(early(later, 0.02)?.trigger, "before-lapse", "b and c reset after a: only a's own refill (50 h) must fit in 72 h");
  assert.equal(Math.round(refillNeedMs(out, sooner, { unitsPerHour: 0.02 }, now) / hour), 85, "b's 0.7 left before its reset adds 35 h");
  assert.equal(early(sooner, 0.02), null, "85 h is capped at 84 h, still over the 72 h a has");
  // Sooner quota counts only up to what the demand can use before that account resets.
  const brief = [out, claude("b", { weekly: 0, weeklyReset: 0.25 * day }), claude("c", { weekly: 10, weeklyReset: 5 * day })];
  assert.equal(Math.round(refillNeedMs(out, brief, { unitsPerHour: 0.02 }, now) / hour), 56, "6 h of b at 0.02/h is 0.12 units");
  // An unread account resetting sooner counts as full.
  const unread = [out, { ...claude("b", { weekly: 30, weeklyReset: 2 * day }), status: "unknown" }, claude("c", { weekly: 10, weeklyReset: 5 * day })];
  assert.equal(Math.round(refillNeedMs(out, unread, { unitsPerHour: 0.02 }, now) / hour), 98);
  // A credit expiring before the account resets is still always allowed.
  const lastChance = claude("b", { weekly: 100, weeklyReset: 3 * day });
  lastChance.credits = [{ ...lastChance.credits[0], expires_at: iso(2 * day) }];
  assert.equal(chooseReset([short[0], lastChance, short[2]], demand(0.001), now).accountId, "b");
});
test("drain-first: a holder ranks by its credit expiry while it still lists that credit", () => {
  const hour = 3600000;
  const k = claude("k", { weekly: 1, weeklyReset: 2 * day }), c = claude("c", { weekly: 76, weeklyReset: 5.5 * day, credits: 0 }), o = claude("o", { weekly: 3, weeklyReset: 5.8 * day });
  o.credits = [{ id: "g", expires_at: iso(3.1 * day) }];
  const until = Date.parse(o.credits[0].expires_at);
  assert.deepEqual(rankAccounts([k, c, o]).map((a) => a.id), ["k", "c", "o"], "weekly reset order");
  const plan = liveDrainFirst(new Map([["o", until]]), [k, c, o], now);
  assert.deepEqual(rankAccounts([k, c, o], plan).map((a) => a.id), ["k", "o", "c"], "EDF: kappa 2 days, Outlook's credit 3.1 days, charlie 5.5 days");
  // A boost never moves an account later, and never ahead of an exhausted ordering.
  assert.deepEqual(rankAccounts([k, c, o], new Map([["o", now + 6 * day]])).map((a) => a.id), ["k", "c", "o"]);
  assert.deepEqual(rankAccounts([{ ...o, status: "exhausted" }, c], plan).map((a) => a.id), ["c", "o"]);
  // Only while that credit is listed, the bank is known, and the time is ahead.
  assert.equal(liveDrainFirst(new Map([["o", until + 2 * 60000]]), [o], now), null, "a different credit");
  assert.equal(liveDrainFirst(new Map([["o", until]]), [{ ...o, credits: [] }], now), null, "redeemed");
  assert.equal(liveDrainFirst(new Map([["o", until]]), [{ ...o, creditsKnown: false }], now), null, "bank unknown");
  assert.equal(liveDrainFirst(new Map([["o", until]]), [o], until + 1), null, "expired");
  assert.equal(liveDrainFirst(null, [o], now), null);
  // The refill runway counts a drain-first holder as drained ahead (P8).
  const out = claude("a", { weekly: 100, weeklyReset: 4 * day });
  const demand = { unitsPerHour: 0.02 };
  const later = claude("o", { weekly: 50, weeklyReset: 6 * day });
  later.credits = [{ id: "g", expires_at: iso(2 * day) }];
  const pool = [out, later];
  assert.equal(Math.round(refillNeedMs(out, pool, demand, now) / hour), 50);
  assert.equal(Math.round(refillNeedMs(out, pool, demand, now, new Map([["o", Date.parse(later.credits[0].expires_at)]])) / hour), 75, "o's 0.5 left drains first");
});

test("external redemptions: a lower known bank with cleared weekly use, not our own claim, not a natural reset", () => {
  const was = claude("a", { weekly: 100, weeklyReset: 2 * day }), is = claude("a", { weekly: 0, weeklyReset: 2 * day, credits: 0 });
  const since = now - 120000;
  const x = (over = {}) => externalRedemptions({ previous: [was], accounts: [is], resets: {}, since, now, ...over });
  assert.deepEqual(x(), [{ provider: "claude", accountId: "a", creditsBefore: 1, creditsAfter: 0, weeklyUsedBefore: 100, weeklyUsedAfter: 0 }]);
  // A credit that lapsed: usage did not clear.
  assert.deepEqual(x({ accounts: [claude("a", { weekly: 100, weeklyReset: 2 * day, credits: 0 })] }), []);
  // Only a small drop is not a refill.
  assert.deepEqual(x({ previous: [claude("a", { weekly: 15, weeklyReset: 2 * day })] }), []);
  // The weekly reset was due: a natural reset clears usage too.
  assert.deepEqual(x({ previous: [claude("a", { weekly: 100, weeklyReset: -60000 })] }), []);
  // Unverified reads are never evidence.
  assert.deepEqual(x({ previous: [{ ...was, creditsKnown: false }] }), []);
  assert.deepEqual(x({ accounts: [{ ...is, status: "unknown" }] }), []);
  assert.deepEqual(x({ accounts: [{ ...is, usageStale: true }] }), []);
  // Our own claim: pending, or finished since the previous read.
  assert.deepEqual(x({ resets: { claude: { pending: { accountId: "a" } } } }), []);
  assert.deepEqual(x({ resets: { claude: { last: { accountId: "a", at: iso(-60000) } } } }), []);
  assert.deepEqual(x({ resets: { claude: { last: { accountId: "a", at: iso(-3 * 3600000) } } } }), [], "our claim, shown late by the provider");
  assert.equal(x({ resets: { claude: { last: { accountId: "a", at: iso(-day) } } } }).length, 1, "an old claim does not hide a new web refill");
  assert.equal(x({ resets: { claude: { last: { accountId: "b", at: iso(-60000) } } } }).length, 1);
});

test("credit risk: alert once per credit inside 72 h; clear on a plan that saves it, a redemption or expiry", () => {
  const exp = iso(40 * 3600000), far = iso(100 * 3600000);
  const doc = (credits, over = {}) => ({ staleAfter: iso(600000), providers: { claude: { state: "conserve", creditPlan: { credits }, ...over } } });
  const c = (outcome, expiresAt = exp, accountId = "o") => ({ accountId, expiresAt, outcome });
  const state = {};
  assert.deepEqual(creditRiskTransitions(state, doc([c("lapses"), c("lapses", far, "b"), c("used", exp, "k")]), { now }).map((t) => [t.type, t.credit.accountId]), [["credit-at-risk", "o"]]);
  assert.deepEqual(creditRiskTransitions(state, doc([c("lapses")]), { now }), [], "once");
  // Stale, unknown or missing forecasts judge nothing.
  assert.deepEqual(creditRiskTransitions(state, doc([]), { now: now + day }), [], "stale");
  assert.deepEqual(creditRiskTransitions(state, doc([], { state: "unknown" }), { now }), []);
  assert.deepEqual(creditRiskTransitions(state, null, { now }), []);
  assert.deepEqual(Object.keys(state.creditRisk), ["o|" + exp]);
  assert.deepEqual(creditRiskTransitions({ ...structuredClone(state) }, doc([c("used")]), { now }).map((t) => t.reason), ["planned"]);
  assert.deepEqual(creditRiskTransitions({ ...structuredClone(state) }, doc([]), { now }).map((t) => t.reason), ["gone"]);
  assert.deepEqual(creditRiskTransitions(state, null, { now: Date.parse(exp) + 1 }).map((t) => t.reason), ["expired"]);
  assert.deepEqual(state.creditRisk, {});
});

test("Claude bank withholds a grant that needs the server's own limit until the server agrees", () => {
  const needs = (atLimit) => claudeBank(block({ at_limit: atLimit, grants: [{ ...block().grants[0], use_requires_limit: true }] }), now);
  assert.equal(needs(false).credits.length, 0);
  assert.equal(needs(true).credits.length, 1);
  assert.equal(claudeBank(block(), now).credits.length, 1);
});
test("Claude and Codex reset slots are independent", async () => {
  const state = migrateResetState({ accounts: exhaustedPool(), resetAutomation: true });
  state.resets.codex.pending = { phase: "attention", accountId: "gone", creditId: "c", requestId: "keep" };
  const logged = [];
  const outcome = await lifecycle(state, { log: async (type, d) => logged.push({ type, ...d }) });
  assert.equal(outcome, "reset");
  assert.equal(state.resets.claude.last.accountId, "b");
  assert.equal(state.resets.codex.pending.requestId, "keep", "the Codex item is untouched");
  assert.deepEqual(logged.map((e) => [e.type, e.provider]), [["reset-attempt", "claude"], ["reset-result", "claude"]]);
});
test("a refused or deferred credit backs off instead of being retried every refresh", async () => {
  // Only b holds a credit, so any second claim would be the refused one.
  const onlyB = () => exhaustedPool().map((a) => (a.id === "b" ? a : { ...a, creditCount: 0, credits: [] }));
  for (const [code, wait] of [["no_credit", RESET_BACKOFF_MS.no_credit], ["deferred", RESET_BACKOFF_MS.deferred]]) {
    const state = migrateResetState({ accounts: onlyB(), resetAutomation: true });
    let sent = 0;
    const consume = async () => { sent++; return { code }; };
    await lifecycle(state, { consume });
    state.accounts = pulledAt(state.accounts, now + wait - 1);
    await lifecycle(state, { consume, now: () => now + wait - 1 });
    assert.equal(sent, 1, `${code}: no second claim inside the backoff`);
    state.accounts = pulledAt(state.accounts, now + wait);
    await lifecycle(state, { consume, now: () => now + wait });
    assert.equal(sent, 2, `${code}: retried after the backoff`);
  }
});
test("each Claude attempt gets a fresh journaled id; an unknown outcome retries the same id", async () => {
  const state = migrateResetState({ accounts: exhaustedPool(), resetAutomation: true });
  const ids = [];
  let persisted;
  const save = async () => { persisted = structuredClone(state); };
  await assert.rejects(lifecycle(state, { save, consume: async (_, p) => { ids.push(p.requestId); assert.equal(persisted.resets.claude.pending.requestId, p.requestId); return { code: "unknown" }; } }));
  // Quota came back on its own: an uncertain claim is still reconciled with its original id.
  state.accounts = state.accounts.map((a) => ({ ...a, ...claude(a.id, { weekly: 10 }) }));
  await lifecycle(state, { save, consume: async (_, p) => { ids.push(p.requestId); return { code: "nothing_to_reset" }; } });
  assert.equal(ids[0], ids[1]);
  state.accounts = pulledAt(exhaustedPool(), now + RESET_BACKOFF_MS.nothing_to_reset);
  await lifecycle(state, { save, now: () => now + RESET_BACKOFF_MS.nothing_to_reset, consume: async (_, p) => { ids.push(p.requestId); return { code: "reset" }; } });
  assert.notEqual(ids[2], ids[0], "a new attempt never reuses a refused id");
  assert.match(ids[2], /^[0-9a-f-]{36}$/);
});
test("disabling a provider's policy cancels its unsent claim and starts no new one", async () => {
  const state = migrateResetState({ accounts: exhaustedPool(), resetAutomation: true });
  state.resets.claude.pending = { phase: "unsent", accountId: "b", creditId: "opus55-launch-promax-20260921", requestId: "r" };
  await lifecycle(state, { policy: claudePolicy({ enabled: false }), consume: async () => assert.fail("disabled") });
  assert.equal(state.resets.claude.pending, null);
  assert.equal(state.resets.claude.last.outcome, "cancelled-before-send");
});

test("Claude quota recovery clears only long blocks and never while a model-specific limit is out", async () => {
  const account = { id: "a", auth_index: "i", provider: "claude", status: "available",
    usageUpdatedAt: iso(), usageCheckStartedAt: iso(-1000) };
  const snapshot = (retryIn) => ({ id: "a", auth_index: "i", provider: "claude", routing: { generation: "3", quota_blocked: true,
    models: { "claude-sonnet-5": { status: "blocked", next_retry_after: iso(retryIn) }, "claude-opus-5": { status: "ready", next_retry_after: "0001-01-01T00:00:00Z" } } } });
  assert.equal(canRecoverQuota(account, snapshot(3 * day), now), true, "a weekly block lifted by a reset");
  assert.equal(canRecoverQuota(account, snapshot(120000), now), false, "a two-minute rate limit lapses on its own");
  const fableOut = { id: "a", provider: "claude", status: "unknown" };
  await refreshUsage(fableOut, null, async () => ({ limits: [{ kind: "session", percent: 1 }, { kind: "weekly_all", percent: 69 },
    { kind: "weekly_scoped", percent: 100, scope: { model: { display_name: "Fable" } } }] }), () => iso());
  assert.equal(fableOut.modelQuotaRecoveryBlocked, true);
  const fableOk = { id: "a", provider: "claude", status: "unknown" };
  await refreshUsage(fableOk, null, async () => ({ limits: [{ kind: "session", percent: 1 }, { kind: "weekly_all", percent: 69 },
    { kind: "weekly_scoped", percent: 83, scope: { model: { display_name: "Fable" } } }] }), () => iso());
  assert.equal(fableOk.modelQuotaRecoveryBlocked, false);
});

// Finding 1 (2026-09-24 review): one refused claim used to stop every other
// account's credit for the whole backoff.
test("a refused credit is skipped on its own; the next account's credit is claimed on the next cycle", async () => {
  const state = migrateState({ accounts: exhaustedPool(), resetAutomation: true });
  const claims = [];
  const consume = async (account) => { claims.push(account.id); return { code: claims.length === 1 ? "no_credit" : "reset" }; };
  await lifecycle(state, { consume });
  assert.deepEqual(claims, ["b"], "the furthest natural reset goes first");
  assert.deepEqual(Object.keys(state.resets.claude.refused), ["b|opus55-launch-promax-20260921"]);
  await lifecycle(state, { consume, now: () => now + 60000 });
  assert.equal(claims.length, 2, "another account is tried inside the backoff");
  assert.notEqual(claims[1], "b");
  assert.equal(state.resets.claude.last.outcome, "reset");
});
test("a refused Codex credit leaves that account's next credit eligible", () => {
  const codexAccount = (id, credits) => ({ id, provider: "codex", status: "exhausted", creditsKnown: true, creditCount: credits.length,
    credits: credits.map((c, i) => ({ id: c, expires_at: iso((i + 2) * day) })) });
  const policy = { ...resetPolicy("codex", undefined, 2) };
  const pool = [codexAccount("a", ["a1", "a2"]), codexAccount("b", [])];
  assert.deepEqual(chooseReset(pool, policy, now), { accountId: "a", creditId: "a1" });
  assert.deepEqual(chooseReset(pool, policy, now, new Set(["a|a1"])), { accountId: "a", creditId: "a2" });
  assert.equal(chooseReset(pool, policy, now, new Set(["a|a1", "a|a2"])), null);
});
test("refusals expire after their backoff and are pruned from saved state", async () => {
  const state = migrateState({ accounts: exhaustedPool(), resetAutomation: true });
  state.resets.claude.refused = {
    "b|opus55-launch-promax-20260921": { outcome: "deferred", at: iso(-RESET_BACKOFF_MS.deferred) },
    "old|x": { outcome: "no_credit", at: iso(-2 * RESET_BACKOFF_MS.no_credit) },
    "junk|y": { outcome: "reset", at: iso() },
  };
  const claims = [];
  await lifecycle(state, { consume: async (a) => { claims.push(a.id); return { code: "reset" }; } });
  assert.deepEqual(claims, ["b"], "an expired deferral no longer skips the credit");
  assert.deepEqual(state.resets.claude.refused, {});
});

test("resetExpected: an alert is held back only while automation will act", () => {
  const pool = exhaustedPool();
  const policy = claudePolicy();
  const base = () => migrateState({ accounts: pool, resetAutomation: true });
  assert.equal(resetExpected(base(), policy, pool, now), true);
  assert.equal(resetExpected({ ...base(), resetAutomation: false }, policy, pool, now), false);
  assert.equal(resetExpected(base(), claudePolicy({ enabled: false }), pool, now), false);
  const sessionOnly = [claude("a", { session: 100 }), claude("b", { session: 100 }), claude("c", { session: 100 })];
  assert.equal(resetExpected(base(), policy, sessionOnly, now), false, "the weekly trigger will not spend a bank on a 5-hour limit");
  const refused = base();
  for (const a of pool) refused.resets.claude.refused[`${a.id}|opus55-launch-promax-20260921`] = { outcome: "no_credit", at: iso() };
  assert.equal(resetExpected(refused, policy, pool, now), false, "every credit is backing off");
  const inFlight = base();
  inFlight.resets.claude.pending = { phase: "uncertain", accountId: "b", creditId: "g", requestId: "r" };
  assert.equal(resetExpected(inFlight, policy, sessionOnly, now), true, "an uncertain claim is still being reconciled");
  inFlight.resets.claude.pending.phase = "attention";
  assert.equal(resetExpected(inFlight, policy, pool, now), false, "attention needs a person");
});

test("provider registry: every adapter implements the interface and ids cannot hit the prototype", async () => {
  const fns = ["accountId", "headers", "modelRecoveryBlocked", "readBank", "redeem", "requestId"];
  for (const id of PROVIDER_IDS) {
    const p = PROVIDERS[id];
    assert.equal(p.id, id);
    for (const key of ["name", "loginId", "clientId", "usageUrl"]) assert.equal(typeof p[key], "string", `${id}.${key}`);
    for (const key of fns) assert.equal(typeof p[key], "function", `${id}.${key}`);
    assert.ok(Number.isFinite(p.maxHeadroomPercent) && Number.isFinite(p.minRecoverableBlockMs));
    assert.equal(providerByLogin(p.loginId), p);
  }
  for (const bad of ["constructor", "__proto__", "toString", "hasOwnProperty"]) {
    assert.equal(PROVIDERS[bad], undefined);
    assert.equal(bad in PROVIDERS, false);
  }
  assert.equal(providerByLogin("claude"), null, "login ids are the proxy's, not the auth-file provider");
  assert.equal(providerByLogin("anthropic").id, "claude");
});
