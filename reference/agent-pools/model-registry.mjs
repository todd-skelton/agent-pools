// Model registry: observations -> one authority generation.
//
// Inputs are observations: per-account live model lists (state/live-models.json),
// pool readiness (/api/status routingModels), and append-only admission probes
// (state/model-admission.jsonl). The output separates:
//   identities  every exact model ever seen, kept forever for historical readers;
//   families    one current exact model per family, promoted monotonically;
//   models      per-account availability and efforts as observed right now.
// Promotion requires fresh admission (same installed harness version, within
// the TTL) at every requiredEffort on the harness's production path. Nothing
// here demotes: if the current model vanishes it is reported unavailable and
// the owner decides; a newer version never loses to an older one by accident.
import { createHash } from "node:crypto";

export const REGISTRY_FORMAT = "model-registry/v2";
export const LINEAGE_FORMAT = "model-lineage/v1";
export const EFFORTS = Object.freeze(["low", "medium", "high", "xhigh", "max"]);
export const SAFE_ID = /^[a-z0-9][a-z0-9.-]{1,63}$/;
export const ADMISSION_TTL_MS = 14 * 24 * 3600_000;
export const LIST_STALE_MS = 6 * 3600_000;
// Observations stamped further ahead than this are invalid, not fresh.
export const FUTURE_SKEW_MS = 5 * 60_000;

export function parseVersion(rule, id) {
  if (!SAFE_ID.test(id)) return null;
  const m = new RegExp(rule.pattern).exec(id);
  if (!m) return null;
  const g = m.groups ?? {};
  return g.version ?? (g.minor != null ? `${g.major}.${g.minor}` : g.major ?? null);
}

export function compareVersions(a, b) {
  const pa = a.split(".").map(Number), pb = b.split(".").map(Number);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const d = (pa[i] ?? 0) - (pb[i] ?? 0);
    if (d) return d;
  }
  return 0;
}

const parseMaybe = (v) => { if (typeof v !== "string") return v; try { return JSON.parse(v); } catch { return null; } };
const effortsOf = (model) => {
  const levels = parseMaybe(model.thinking)?.levels;
  return Array.isArray(levels) ? EFFORTS.filter((e) => levels.includes(e)) : [];
};

// Latest probe per (model, effort) that is ok, fresh and on the installed harness.
export function admittedEfforts(probes, id, harnessVersion, now) {
  const latest = new Map();
  for (const p of probes) {
    if (p.model !== id || !EFFORTS.includes(p.effort)) continue;
    // A row with an unreadable or future timestamp is quarantined, so it can
    // never outrank (and hide) later valid probes.
    const t = Date.parse(p.at ?? "");
    if (!Number.isFinite(t) || t - now > FUTURE_SKEW_MS) continue;
    const prev = latest.get(p.effort);
    if (!prev || Date.parse(prev.at) < t) latest.set(p.effort, p);
  }
  return EFFORTS.filter((e) => {
    const p = latest.get(e);
    const age = now - Date.parse(p?.at ?? "");
    return p?.ok === true && p.harnessVersion === harnessVersion && Number.isFinite(age) && age >= -FUTURE_SKEW_MS && age <= ADMISSION_TTL_MS;
  });
}

export function emptyLineage() { return { format: LINEAGE_FORMAT, generation: 0, identities: {}, families: {}, events: [] }; }

export function buildRegistry({ liveModels, status = null, families, probes = [], lineage = emptyLineage(),
  harnessVersions = {}, now = Date.now() }) {
  if (liveModels?.format !== 1 || typeof liveModels.lists !== "object") throw new Error("live-models format 1 required");
  lineage = structuredClone(lineage);
  const iso = new Date(now).toISOString();
  const accountStatus = new Map((status?.accounts ?? []).map((a) => [a.id, a]));

  // Per exact model: each account that lists it, with that account's own efforts.
  const observed = new Map(); // provider\0id -> { id, provider, accounts: [] }
  for (const [account, list] of Object.entries(liveModels.lists)) {
    const acct = accountStatus.get(account);
    // The proxy rewrites live-models.json only when a list changes; an
    // unchanged re-read refreshes only its in-memory time, which the pool
    // status reports as modelList.fetchedAt. Use the newer of the two, and the
    // status time only for a live list of this same account. After a failed
    // re-read the proxy keeps routing on the last good list and fetchedAt
    // stays that list's read time (modelList.error carries the failure), so
    // a transient failure does not make the account unusable; LIST_STALE_MS
    // still bounds how long a list that cannot be re-read counts.
    const fileAt = Date.parse(list?.fetched_at ?? "");
    const ml = acct?.modelList;
    const statusAt = ml?.source === "live" ? Date.parse(ml.fetchedAt ?? "") : NaN;
    const fetchedAt = Math.max(Number.isFinite(fileAt) ? fileAt : -Infinity, Number.isFinite(statusAt) ? statusAt : -Infinity);
    const stale = !Number.isFinite(fetchedAt) || now - fetchedAt > LIST_STALE_MS || fetchedAt - now > FUTURE_SKEW_MS;
    for (const model of list?.models ?? []) {
      const id = model.id ?? model.slug;
      if (typeof id !== "string" || !SAFE_ID.test(id)) continue; // never carry unsafe identifiers onward
      const key = `${list.provider}\u0000${id}`;
      const entry = observed.get(key) ?? { id, provider: list.provider, accounts: [] };
      const routing = acct?.routingModels?.[id]?.status ?? null;
      entry.accounts.push({ account, efforts: effortsOf(model), fetchedAt: Number.isFinite(fetchedAt) ? new Date(fetchedAt).toISOString() : null, stale,
        disabled: acct?.disabled === true, accountStatus: acct?.status ?? null, routing,
        // Fail closed: an account is usable only when the pool status names it
        // and explicitly reports this model ready on it.
        usable: !stale && Boolean(acct) && acct.disabled !== true && routing === "ready" });
      observed.set(key, entry);
    }
  }

  const registry = { format: REGISTRY_FORMAT, generatedAt: iso, harnessVersions,
    source: { accountLists: Object.keys(liveModels.lists).length, statusCheckedAt: status?.checkedAt ?? null },
    families: {}, models: {}, notices: [] };

  for (const rule of families.families) {
    const fam = lineage.families[rule.family] ?? (lineage.families[rule.family] = { current: null, promotedAt: null });
    const members = [];
    for (const entry of observed.values()) {
      if (entry.provider !== rule.provider) continue;
      const version = parseVersion(rule, entry.id);
      if (version == null) continue;
      const ident = lineage.identities[entry.id] ?? (lineage.identities[entry.id] = { family: rule.family, provider: rule.provider, version, firstSeen: iso });
      ident.lastListed = iso;
      members.push({ ...entry, version });
    }
    members.sort((a, b) => compareVersions(b.version, a.version) || a.id.localeCompare(b.id));
    const harness = harnessVersions[rule.provider] ?? null;
    for (const m of members) {
      const liveEfforts = EFFORTS.filter((e) => m.accounts.some((a) => a.efforts.includes(e)));
      const admitted = admittedEfforts(probes, m.id, harness, now).filter((e) => liveEfforts.includes(e));
      registry.models[m.id] = { family: rule.family, provider: rule.provider, version: m.version, role: null,
        efforts: liveEfforts, admittedEfforts: admitted,
        usableAccountsByEffort: Object.fromEntries(liveEfforts.map((e) => [e, m.accounts.filter((a) => a.usable && a.efforts.includes(e)).map((a) => a.account).sort()])),
        accounts: m.accounts.sort((a, b) => a.account.localeCompare(b.account)) };
    }
    // Promotion: highest version, at or above the floor and the current version,
    // admitted at every required effort and usable on at least one account per effort.
    const eligible = members.filter((m) => {
      if (compareVersions(m.version, rule.minVersion) < 0) return false;
      const cur = fam.current && lineage.identities[fam.current];
      if (cur && compareVersions(m.version, cur.version) <= 0) return false;
      const r = registry.models[m.id];
      return rule.requiredEfforts.every((e) => r.admittedEfforts.includes(e) && r.usableAccountsByEffort[e]?.length > 0);
    });
    if (eligible[0]) {
      const from = fam.current;
      fam.current = eligible[0].id; fam.promotedAt = iso;
      if (from) (lineage.identities[from].retiredAt ??= iso);
      lineage.generation += 1;
      lineage.events.push({ at: iso, generation: lineage.generation, family: rule.family, event: "promoted", from, to: fam.current });
    }
    const current = fam.current;
    const newest = members.find((m) => compareVersions(m.version, rule.minVersion) >= 0);
    const candidate = newest && current && compareVersions(newest.version, lineage.identities[current].version) > 0 ? newest.id
      : (!current && newest ? newest.id : null);
    for (const [id, m] of Object.entries(registry.models)) {
      if (m.family !== rule.family) continue;
      m.role = id === current ? "current" : id === candidate ? "candidate" : "historical";
    }
    const curModel = current ? registry.models[current] : null;
    const unavailable = current ? rule.requiredEfforts.filter((e) => !(curModel?.usableAccountsByEffort[e]?.length > 0)) : [];
    if (current && unavailable.length) registry.notices.push({ family: rule.family, notice: "current-unavailable", model: current, efforts: unavailable });
    if (candidate) {
      const c = registry.models[candidate];
      registry.notices.push({ family: rule.family, notice: "candidate-awaiting-admission", model: candidate,
        missingEfforts: rule.requiredEfforts.filter((e) => !c.admittedEfforts.includes(e)) });
    }
    registry.families[rule.family] = { provider: rule.provider, current, candidate, requiredEfforts: rule.requiredEfforts,
      promotedAt: fam.promotedAt, unavailableEfforts: unavailable,
      historical: Object.entries(lineage.identities).filter(([id, i]) => i.family === rule.family && id !== current).map(([id]) => id).sort() };
  }
  registry.identities = lineage.identities;
  registry.generation = lineage.generation;
  // Digest over the authority-bearing part only, so dispatch can record exactly what it resolved against.
  registry.authorityDigest = createHash("sha256").update(JSON.stringify({ g: lineage.generation,
    f: Object.fromEntries(Object.entries(registry.families).map(([k, v]) => [k, v.current])) })).digest("hex").slice(0, 16);
  return { registry, lineage };
}
