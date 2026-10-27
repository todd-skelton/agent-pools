import test from "node:test";
import assert from "node:assert/strict";
import { poolUsage, UNVERIFIED_GRACE_MS } from "./pool-usage.mjs";

const NOW = 1_789_482_000_000;
// An account whose latest poll failed: last-known rows retained, read `age` ms ago.
const unverified = (age = 120_000) => ({ status: "unknown", usageStale: true, usageUpdatedAt: new Date(NOW - age).toISOString() });
const win = (id, scope, used, dur, resetAt, common = false) => ({ id, scope, label: dur === 604800 ? "Weekly" : "5-hour", used, durationSeconds: dur, common, resetAt });
const codex = (name, used, resetAt, extra = {}) => ({
  provider: "codex", name, status: used >= 100 ? "exhausted" : "available", disabled: false, usageStale: false,
  quotaWindows: [
    win("codex:primary_window", "Codex", used, 604800, resetAt, true),
    win("additional:0:primary_window", "GPT-5.3-Codex-Spark", 0, 18000, NOW + 1000_000),
    win("additional:0:secondary_window", "GPT-5.3-Codex-Spark", 0, 604800, NOW + 5000_000),
  ],
  ...extra,
});
const claude = (name, session, weekly, fable) => ({
  // Claude status derives from the common windows (see normalizeUsage).
  provider: "claude", name, status: session >= 100 || weekly >= 100 ? "exhausted" : "available", disabled: false, usageStale: false,
  quotaWindows: [
    win("limit:0", "All models", session, 18000, NOW + 100_000, true),
    win("limit:1", "All models", weekly, 604800, NOW + 400_000, true),
    win("limit:2", "Fable", fable, 604800, NOW + 400_000),
  ],
});
const upstream = () => ({
  account_id: "acct", plan_type: "pro",
  rate_limit: { allowed: false, limit_reached: true, primary_window: { used_percent: 100, limit_window_seconds: 604800, reset_after_seconds: 1, reset_at: 1 }, secondary_window: null },
  additional_rate_limits: [{ limit_name: "GPT-5.3-Codex-Spark", metered_feature: "codex_bengalfox", rate_limit: {}, normal_model_slug: null }],
  model_usage: { "gpt-6-astra": { available: false } },
  rate_limit_reached_type: { type: "rate_limit_reached", details: "default" },
  rate_limit_upsell: { banner_type: "pro_rate_limit_reached", title: "You're out of Codex messages" },
  credits: { has_credits: false }, rate_limit_reset_credits: { available_count: 1 },
});

test("capped (default): one fresh + two exhausted accounts counts down the last account's worth", () => {
  const out = poolUsage(upstream(), [codex("a", 1, NOW + 600_000_000), codex("b", 100, NOW + 300_000_000), codex("c", 100, NOW + 450_000_000)], { now: NOW });
  assert.equal(out.rate_limit.allowed, true);
  assert.equal(out.rate_limit.limit_reached, false);
  assert.equal(out.rate_limit.primary_window.used_percent, 1);
  assert.equal(out.rate_limit.primary_window.limit_window_seconds, 604800);
  // "Resets on" is the next time capacity returns: the soonest reset of any account.
  assert.equal(out.rate_limit.primary_window.reset_at, Math.round((NOW + 300_000_000) / 1000));
  assert.equal(out.rate_limit_reached_type, null);
  assert.equal(out.rate_limit_upsell, null);
  assert.deepEqual(out.model_usage, {});
  assert.equal(out.additional_rate_limits[0].limit_name, "Codex pool: 1 of 3 available, 99% of 300% left");
  assert.equal(out.additional_rate_limits[0].metered_feature, "codex_pool");
  assert.deepEqual(out.pool, { mode: "capped", accounts: 3, available: 1, unverified: 0, total_remaining_percent: 99 });
  // Signed-in account identity and credits are preserved.
  assert.equal(out.account_id, "acct");
  assert.deepEqual(out.rate_limit_reset_credits, { available_count: 1 });
});

test("capped stays at 0% used while more than one account's worth remains; sum and average do not", () => {
  const accounts = [codex("a", 0, NOW + 1), codex("b", 10, NOW + 2), codex("c", 20, NOW + 3)];
  const used = (mode) => poolUsage(upstream(), accounts, { now: NOW, mode }).rate_limit.primary_window.used_percent;
  assert.equal(used("capped"), 0);
  assert.equal(used("sum"), -170);
  assert.equal(used("average"), 10);
  assert.equal(poolUsage(upstream(), accounts, { now: NOW }).additional_rate_limits[0].limit_name, "Codex pool: 3 of 3 available, 270% of 300% left");
});

test("fully exhausted pool sets limit flags and a pool-specific banner", () => {
  const out = poolUsage(upstream(), [codex("a", 100, NOW + 300_000), codex("b", 100, NOW + 200_000)], { now: NOW });
  assert.equal(out.rate_limit.allowed, false);
  assert.equal(out.rate_limit.limit_reached, true);
  assert.equal(out.rate_limit.primary_window.used_percent, 100);
  assert.equal(out.rate_limit.primary_window.reset_at, Math.round((NOW + 200_000) / 1000));
  assert.equal(out.rate_limit_reached_type.type, "rate_limit_reached");
  assert.equal(out.rate_limit_upsell.title, "Codex pool exhausted (2 accounts)");
  assert.deepEqual(out.rate_limit_upsell.ctas, []);
  assert.deepEqual(out.model_usage, { "gpt-6-astra": { available: false } });
});

test("per-model buckets: Spark keeps the upstream metered_feature, Claude scopes become their own buckets", () => {
  const out = poolUsage(upstream(), [codex("a", 1, NOW + 1_000_000), claude("c1", 23, 40, 63), claude("c2", 0, 0, 0), claude("c3", 0, 8, 1)], { now: NOW });
  const byFeature = Object.fromEntries(out.additional_rate_limits.map((e) => [e.metered_feature, e]));
  assert.ok(byFeature.codex_bengalfox, "Spark bucket keeps upstream id");
  assert.equal(byFeature.codex_bengalfox.limit_name, "GPT-5.3-Codex-Spark pool: 1 of 1 available, 100% of 100% left");
  assert.equal(byFeature.codex_bengalfox.rate_limit.primary_window.limit_window_seconds, 18000);
  assert.equal(byFeature.codex_bengalfox.rate_limit.secondary_window.limit_window_seconds, 604800);
  assert.ok(byFeature.pool_claude, "Claude common bucket");
  // Claude primary = weekly (longest), secondary = 5-hour. Weekly remaining 60+100+92 = 252, capped to 0 used.
  assert.equal(byFeature.pool_claude.rate_limit.primary_window.used_percent, 0);
  assert.equal(byFeature.pool_claude.rate_limit.primary_window.limit_window_seconds, 604800);
  assert.equal(byFeature.pool_claude.rate_limit.secondary_window.limit_window_seconds, 18000);
  assert.equal(byFeature.pool_claude.limit_name, "Claude pool: 3 of 3 available, 252% of 300% left");
  const fable = poolUsage(upstream(), [codex("a", 1, NOW + 1), claude("c1", 0, 0, 63), claude("c2", 0, 0, 100), claude("c3", 0, 0, 90)], { now: NOW, mode: "average" });
  assert.equal(fable.additional_rate_limits.find((e) => e.metered_feature === "pool_claude_fable").rate_limit.primary_window.used_percent, 84, "integer only: the app-server rejects fractional used_percent");
});

test("no verified Codex data returns upstream unchanged; disabled and row-less accounts add nothing", () => {
  const up = upstream();
  const out = poolUsage(up, [codex("a", 1, NOW + 1, { usageStale: true }), codex("b", 1, NOW + 1, { disabled: true }), codex("c", 1, NOW + 1, { status: "unknown" }), codex("d", 1, NOW + 1, { quotaWindows: [] })], { now: NOW });
  assert.equal(out, up);
  // A disabled account is not part of the pool at all, and a row-less
  // unverified one contributes no capacity but still counts as unverified.
  const rows = poolUsage(up, [codex("a", 50, NOW + 1), codex("b", 1, NOW + 1, { disabled: true }), codex("d", 1, NOW + 1, { ...unverified(), usageStale: false, quotaWindows: [] })], { now: NOW });
  assert.equal(rows.pool.accounts, 1);
  assert.equal(rows.pool.unverified, 1);
  assert.equal(rows.additional_rate_limits[0].limit_name, "Codex pool: 1 of 1 available, 1 unverified, 50% of 100% left");
});

test("2026-09-16 incident: a failed poll on the only account with capacity must not raise the banner", () => {
  // kappa's read timed out (502) while both other accounts were fresh and at 100%.
  const stale = codex("kappa", 77, NOW + 500_000_000, { ...unverified(), error: "Proxy management api-call returned HTTP 502" });
  const out = poolUsage(upstream(), [stale, codex("charlie", 100, NOW + 300_000_000), codex("outlook", 100, NOW + 450_000_000)], { now: NOW });
  assert.equal(out.rate_limit.allowed, true);
  assert.equal(out.rate_limit.limit_reached, false);
  assert.equal(out.rate_limit_upsell, null);
  assert.equal(out.rate_limit_reached_type, null);
  assert.deepEqual(out.model_usage, {});
  // Last-known capacity stays on the meter: 23% of one account left.
  assert.equal(out.rate_limit.primary_window.used_percent, 77);
  assert.equal(out.rate_limit.primary_window.reset_at, Math.round((NOW + 300_000_000) / 1000));
  assert.equal(out.additional_rate_limits[0].limit_name, "Codex pool: 1 of 3 available, 1 unverified, 23% of 300% left");
  assert.deepEqual(out.pool, { mode: "capped", accounts: 3, available: 1, unverified: 1, total_remaining_percent: 23 });
});

test("an unverified account that was last seen exhausted still blocks the banner", () => {
  // Its window may have reset since the last successful read; the pool cannot
  // claim exhaustion without fresh evidence from every account.
  const out = poolUsage(upstream(), [codex("a", 100, NOW + 1_000, unverified()), codex("b", 100, NOW + 2_000)], { now: NOW });
  assert.equal(out.rate_limit.limit_reached, false);
  assert.equal(out.rate_limit.allowed, true);
  assert.equal(out.rate_limit_upsell, null);
  assert.equal(out.rate_limit.primary_window.used_percent, 100, "meter still shows nothing left");
  assert.equal(out.pool.unverified, 1);
  // Once verified, the banner is allowed.
  const verified = poolUsage(upstream(), [codex("a", 100, NOW + 1_000), codex("b", 100, NOW + 2_000)], { now: NOW });
  assert.equal(verified.rate_limit.limit_reached, true);
  assert.equal(verified.rate_limit_upsell.title, "Codex pool exhausted (2 accounts)");
});

test("unverified Claude accounts keep their rows but Claude buckets appear only with verified data", () => {
  const withStale = poolUsage(upstream(), [codex("a", 1, NOW + 1), claude("c1", 0, 100, 0), { ...claude("c2", 0, 50, 0), ...unverified() }], { now: NOW });
  const bucket = withStale.additional_rate_limits.find((e) => e.metered_feature === "pool_claude");
  assert.equal(bucket.limit_name, "Claude pool: 1 of 2 available, 1 unverified, 50% of 200% left");
  assert.equal(bucket.rate_limit.limit_reached, false);
  const noneVerified = poolUsage(upstream(), [codex("a", 1, NOW + 1), { ...claude("c2", 0, 50, 0), ...unverified() }], { now: NOW });
  assert.equal(noneVerified.additional_rate_limits.some((e) => e.metered_feature.startsWith("pool_claude")), false);
});

test("unknown mode is rejected", () => {
  assert.throws(() => poolUsage(upstream(), [codex("a", 1, NOW + 1)], { mode: "max" }), /Unknown pool mode/);
});

test("unverified evidence expires: past the grace window the account is dropped and cannot hide a real exhaustion", () => {
  const exhausted = [codex("b", 100, NOW + 300_000), codex("c", 100, NOW + 400_000)];
  const inGrace = poolUsage(upstream(), [codex("a", 77, NOW + 500_000, unverified(UNVERIFIED_GRACE_MS)), ...exhausted], { now: NOW });
  assert.equal(inGrace.rate_limit.limit_reached, false);
  assert.equal(inGrace.pool.unverified, 1);
  const expired = poolUsage(upstream(), [codex("a", 77, NOW + 500_000, unverified(UNVERIFIED_GRACE_MS + 1)), ...exhausted], { now: NOW });
  assert.equal(expired.rate_limit.limit_reached, true);
  assert.equal(expired.rate_limit_upsell.title, "Codex pool exhausted (2 accounts)");
  assert.deepEqual(expired.pool, { mode: "capped", accounts: 2, available: 0, unverified: 0, total_remaining_percent: 0 });
  // No successful read on record (or a clock skewed into the future) is no evidence either.
  for (const stamp of [{}, { usageUpdatedAt: null }, { usageUpdatedAt: "garbage" }, { usageUpdatedAt: new Date(NOW + 60_000).toISOString() }])
    assert.equal(poolUsage(upstream(), [codex("a", 77, NOW + 500_000, { status: "unknown", usageStale: true, ...stamp }), ...exhausted], { now: NOW }).rate_limit.limit_reached, true, JSON.stringify(stamp));
});

test("the desktop's own upstream payload verifies the signed-in pooled account, even past the grace window", () => {
  const exhausted = [codex("b", 100, NOW + 300_000), codex("c", 100, NOW + 400_000)];
  const up = { ...upstream(), account_id: "acct-a", rate_limit: { allowed: true, limit_reached: false, primary_window: { used_percent: 77, limit_window_seconds: 604800, reset_after_seconds: 500, reset_at: Math.round(NOW / 1000) + 500 }, secondary_window: null } };
  const a = codex("a", 60, NOW + 500_000, { accountId: "acct-a", ...unverified(UNVERIFIED_GRACE_MS + 1) });
  const out = poolUsage(up, [a, ...exhausted], { now: NOW });
  assert.equal(out.rate_limit.limit_reached, false);
  assert.equal(out.rate_limit_upsell, null);
  assert.equal(out.rate_limit.primary_window.used_percent, 77, "upstream reading replaces the stale one");
  assert.deepEqual(out.pool, { mode: "capped", accounts: 3, available: 1, unverified: 0, total_remaining_percent: 23 });
  // Upstream that says exhausted for a stale account is also evidence.
  const upX = { ...up, rate_limit: { allowed: false, limit_reached: true, primary_window: { used_percent: 100, limit_window_seconds: 604800, reset_after_seconds: 500, reset_at: Math.round(NOW / 1000) + 500 }, secondary_window: null } };
  const outX = poolUsage(upX, [codex("a", 60, NOW + 500_000, { accountId: "acct-a", ...unverified() }), ...exhausted], { now: NOW });
  assert.equal(outX.rate_limit.limit_reached, true);
  assert.equal(outX.rate_limit_upsell.title, "Codex pool exhausted (3 accounts)");
  // An unusable upstream body or a non-matching id changes nothing.
  const outNo = poolUsage({ ...up, account_id: "someone-else" }, [a, ...exhausted], { now: NOW });
  assert.equal(outNo.rate_limit.limit_reached, true);
  const outBad = poolUsage({ ...up, rate_limit: {} }, [a, ...exhausted], { now: NOW });
  assert.equal(outBad.rate_limit.limit_reached, true);
});

test("explicit provider verdicts override percentages on common windows", () => {
  const exhausted = [codex("b", 100, NOW + 300_000), codex("c", 100, NOW + 400_000)];
  // allowed:true at 100% used: the provider says go, so the pool is not exhausted.
  const allowedAt100 = poolUsage(upstream(), [codex("a", 100, NOW + 500_000, { status: "available" }), ...exhausted], { now: NOW });
  assert.equal(allowedAt100.rate_limit.limit_reached, false);
  assert.equal(allowedAt100.rate_limit_upsell, null);
  assert.equal(allowedAt100.pool.available, 1);
  // allowed:false at 0% used: no capacity, whatever the meter says.
  const deniedAt0 = poolUsage(upstream(), [codex("a", 0, NOW + 500_000, { status: "exhausted" }), ...exhausted], { now: NOW });
  assert.equal(deniedAt0.rate_limit.limit_reached, true);
  assert.equal(deniedAt0.rate_limit_upsell.title, "Codex pool exhausted (3 accounts)");
  assert.equal(deniedAt0.rate_limit.primary_window.used_percent, 100);
  // Via the upstream-evidence path too.
  const up = { ...upstream(), account_id: "acct-a", rate_limit: { allowed: false, limit_reached: true, primary_window: { used_percent: 0, limit_window_seconds: 604800, reset_after_seconds: 500, reset_at: Math.round(NOW / 1000) + 500 }, secondary_window: null } };
  const viaUpstream = poolUsage(up, [codex("a", 10, NOW + 500_000, { accountId: "acct-a" }), ...exhausted], { now: NOW });
  assert.equal(viaUpstream.rate_limit.limit_reached, true);
  // Upstream evidence without a common window (only a Spark bucket) is not evidence.
  const sparkOnly = { ...upstream(), account_id: "acct-a", rate_limit: {}, additional_rate_limits: [{ limit_name: "GPT-5.3-Codex-Spark", metered_feature: "codex_bengalfox", rate_limit: { allowed: true, primary_window: { used_percent: 0, limit_window_seconds: 18000, reset_at: 1 } }, normal_model_slug: null }] };
  const stale = codex("a", 77, NOW + 500_000, { accountId: "acct-a", ...unverified() });
  const out = poolUsage(sparkOnly, [stale, ...exhausted], { now: NOW });
  assert.equal(out.pool.unverified, 1, "account stays unverified");
  assert.equal(out.rate_limit.limit_reached, false);
});

test("exhaustion is decided per account: a slot reported by only some accounts cannot exhaust the pool", () => {
  const a = { ...codex("a", 20, NOW + 700_000), status: "available" };
  a.quotaWindows = [win("codex:primary_window", "Codex", 20, 604800, NOW + 700_000, true)];
  const b = { ...codex("b", 10, NOW + 700_000), status: "exhausted" };
  b.quotaWindows = [win("codex:primary_window", "Codex", 10, 604800, NOW + 700_000, true), win("codex:secondary_window", "Codex", 100, 18000, NOW + 3_600_000, true)];
  const out = poolUsage(upstream(), [a, b], { now: NOW });
  assert.equal(out.rate_limit.limit_reached, false);
  assert.equal(out.rate_limit.allowed, true);
  assert.equal(out.rate_limit_upsell, null);
  assert.equal(out.pool.available, 1);
});

test("an exhausted pool advertises the soonest common-window reset, not the primary window's", () => {
  const a = { ...codex("a", 20, NOW + 7 * 86_400_000), status: "exhausted" };
  a.quotaWindows = [win("codex:primary_window", "Codex", 20, 604800, NOW + 7 * 86_400_000, true), win("codex:secondary_window", "Codex", 100, 18000, NOW + 3_600_000, true)];
  const out = poolUsage(upstream(), [a], { now: NOW });
  assert.equal(out.rate_limit.limit_reached, true);
  assert.equal(out.rate_limit.primary_window.reset_at, Math.round((NOW + 3_600_000) / 1000));
  assert.equal(out.rate_limit.primary_window.reset_after_seconds, 3600);
  assert.equal(out.rate_limit_upsell.reset_at, Math.round((NOW + 3_600_000) / 1000));
  // Claude common bucket behaves the same way.
  const c = { ...claude("c1", 100, 20, 0), status: "exhausted" };
  const cl = poolUsage(upstream(), [codex("x", 1, NOW + 1), c], { now: NOW }).additional_rate_limits.find((e) => e.metered_feature === "pool_claude");
  assert.equal(cl.rate_limit.limit_reached, true);
  assert.equal(cl.rate_limit.primary_window.reset_at, Math.round((NOW + 100_000) / 1000), "five-hour reset comes first");
});

test("average mode uses verdict-adjusted percentages like the other modes", () => {
  const deniedAt0 = poolUsage(upstream(), [codex("a", 0, NOW + 500_000, { status: "exhausted" })], { now: NOW, mode: "average" });
  assert.equal(deniedAt0.rate_limit.primary_window.used_percent, 100);
  assert.equal(deniedAt0.rate_limit.limit_reached, true);
  const allowedAt100 = poolUsage(upstream(), [codex("a", 100, NOW + 500_000, { status: "available" })], { now: NOW, mode: "average" });
  assert.equal(allowedAt100.rate_limit.primary_window.used_percent, 99);
  assert.equal(allowedAt100.rate_limit.limit_reached, false);
});

test("recovery time is the last blocking window per account, earliest account across the pool", () => {
  // Weekly at 100% resets in 7 days; 5-hour at 20% resets in 1 hour: capacity returns in 7 days.
  const a = { ...codex("a", 100, NOW + 7 * 86_400_000), status: "exhausted" };
  a.quotaWindows = [win("codex:primary_window", "Codex", 100, 604800, NOW + 7 * 86_400_000, true), win("codex:secondary_window", "Codex", 20, 18000, NOW + 3_600_000, true)];
  const one = poolUsage(upstream(), [a], { now: NOW });
  assert.equal(one.rate_limit.primary_window.reset_at, Math.round((NOW + 7 * 86_400_000) / 1000));
  // Both at 100%: the later of the two.
  a.quotaWindows[1].used = 100;
  assert.equal(poolUsage(upstream(), [a], { now: NOW }).rate_limit.primary_window.reset_at, Math.round((NOW + 7 * 86_400_000) / 1000));
  // A second account blocked only by its 5-hour window recovers first, so the pool does.
  const b = { ...codex("b", 10, NOW + 6 * 86_400_000), status: "exhausted" };
  b.quotaWindows = [win("codex:primary_window", "Codex", 10, 604800, NOW + 6 * 86_400_000, true), win("codex:secondary_window", "Codex", 100, 18000, NOW + 7_200_000, true)];
  const two = poolUsage(upstream(), [a, b], { now: NOW });
  assert.equal(two.rate_limit.limit_reached, true);
  assert.equal(two.rate_limit.primary_window.reset_at, Math.round((NOW + 7_200_000) / 1000));
  assert.equal(two.rate_limit_upsell.reset_at, Math.round((NOW + 7_200_000) / 1000));
});
