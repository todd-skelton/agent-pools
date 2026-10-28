import { sameIdentity, sameProcess, ownedProxy } from "./health-policy.mjs";

// Before (re)spawning a child, decide what to do with whatever already runs:
//   adopt - the recorded or listening process is the selected build on this config;
//   spawn - nothing holds the port (any older build we could identify was retired);
//   wait  - the port is held by something that is not this pool's proxy
//           ("port-busy"), or by a process whose identity could not be read
//           ("lookup-failed").
// A process is killed only after its identity was read. A failed read is
// retried and is never taken to mean "not ours": on 2026-10-18 the identity
// lookups timed out under load, the old proxy was neither retired nor
// identified, and the deploy sat in port-busy backoff for seven minutes.
export const LOOKUP_ATTEMPTS = 3;

export async function retryLookup(lookup, { attempts = LOOKUP_ATTEMPTS, sleep, onError }) {
  let last;
  for (let attempt = 1; attempt <= attempts; attempt++) {
    try { return await lookup(); } catch (e) {
      last = e;
      await onError?.(e, attempt);
      if (attempt < attempts) await sleep(1000);
    }
  }
  throw last;
}

export async function planTakeover({ name, exe, config, root, port = null, record, processInfo, portOwner, portFree, alive, kill, sleep, event, closing = () => false }) {
  const log = (type, details) => event(type, { name, ...details }).catch(() => {});
  const lookup = (what, fn) => retryLookup(fn, { sleep, onError: (e, attempt) => log("process-lookup-error", {
    lookup: what, attempt, timedOut: e?.killed === true, message: String(e?.message ?? e).slice(0, 200),
  }) });
  let recordUnread = false;
  if (record && alive(record.pid)) {
    let actual = null;
    try { actual = await lookup("recorded", () => processInfo(record.pid)); } catch { recordUnread = true; }
    const ours = sameIdentity(record, actual);
    if (ours && sameProcess(record, actual, exe, config)) return { action: "adopt", pid: Number(record.pid) };
    if (ours) {
      if (closing()) return { action: "wait", reason: "closing" };
      // The process we recorded is alive but is not what install.json now
      // asks for (a new binary or config). Retire it so the replacement can
      // take the port instead of crash-looping against it.
      await log("process-retired", { pid: record.pid, executable: actual.ExecutablePath });
      try { kill(record.pid); } catch {}
      for (let i = 0; i < 40 && alive(record.pid); i++) await sleep(250);
    }
  }
  if (port == null) return { action: "spawn" };
  for (let i = 0; i < 40 && !(await portFree(port)); i++) await sleep(250);
  if (await portFree(port)) return { action: "spawn" };
  // An unrecorded listener that is this pool's own proxy is adopted when it
  // runs the selected build and retired otherwise; spawning against it only
  // crash-loops and overwrites the record with a process that already died.
  let owner;
  try { owner = await lookup("port", () => portOwner(port)); } catch { return { action: "wait", reason: "lookup-failed" }; }
  if (owner && ownedProxy(owner, root, config)) {
    const ownerRecord = { pid: owner.ProcessId, creationDate: owner.CreationDate };
    if (sameProcess(ownerRecord, owner, exe, config)) return { action: "adopt", pid: owner.ProcessId, record: ownerRecord, owner };
    if (closing()) return { action: "wait", reason: "closing", owner };
    await log("process-retired", { pid: owner.ProcessId, executable: owner.ExecutablePath, unrecorded: true });
    try { kill(owner.ProcessId); } catch {}
    for (let i = 0; i < 40 && !(await portFree(port)); i++) await sleep(250);
  }
  if (await portFree(port)) return { action: "spawn" };
  // With the recorded process unidentified and no listener found, the holder
  // is still unknown (it may be that process); keep retrying at lookup pace.
  return { action: "wait", reason: !owner && recordUnread ? "lookup-failed" : "port-busy", owner: owner ?? null };
}
