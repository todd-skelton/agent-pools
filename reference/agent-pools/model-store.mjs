// Small file helpers shared by the model registry tools: exclusive lock files,
// unique temp files and rename retries so concurrent writers never interleave.
import { open, readFile, writeFile, rename, rm, stat, utimes } from "node:fs/promises";
import { randomBytes } from "node:crypto";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export async function readJSON(file, fallback) {
  try { return JSON.parse(await readFile(file, "utf8")); }
  catch (e) { if (fallback !== undefined && e.code === "ENOENT") return fallback; throw e; }
}

export async function writeJSONAtomic(file, value) {
  const tmp = `${file}.${process.pid}.${randomBytes(4).toString("hex")}.tmp`;
  await writeFile(tmp, JSON.stringify(value, null, 2) + "\n");
  for (let i = 0; ; i++) {
    try { await rename(tmp, file); return; }
    catch (e) {
      if (!["EPERM", "EBUSY", "EACCES"].includes(e.code) || i >= 20) { await rm(tmp, { force: true }); throw e; }
      await sleep(100 * (i + 1)); // a reader holding the file open on Windows
    }
  }
}

// Reclaiming a stale lock is serialised by a short-lived guard file, and the
// staleness is re-checked under the guard, so two contenders cannot both take
// over: the second sees the first one's fresh lock and waits. A guard is held
// for milliseconds; one older than RECLAIM_GUARD_STALE_MS belongs to a
// reclaimer that died and is removed.
const RECLAIM_GUARD_STALE_MS = 60_000;
async function reclaimIfStale(lock, staleMs) {
  const guard = lock + ".reclaim";
  let g;
  try { g = await open(guard, "wx"); }
  catch (e) {
    if (!["EEXIST", "EPERM", "EACCES", "EBUSY"].includes(e.code)) throw e;
    const age = await stat(guard).then((s) => Date.now() - s.mtimeMs, () => 0);
    if (age > RECLAIM_GUARD_STALE_MS) await rm(guard, { force: true });
    return;
  }
  try {
    await g.close();
    const s = await stat(lock).catch(() => null);
    if (s && Date.now() - s.mtimeMs > staleMs) await rm(lock, { force: true });
  } finally { await rm(guard, { force: true }); }
}

// Runs fn while holding <file>.lock. The holder renews the lock's mtime every
// renewMs, so only a lock whose holder stopped renewing (crashed or killed)
// ages past staleMs and is taken over. Release removes the lock only if it
// still carries this holder's token.
export async function withLock(file, fn, { waitMs = 60_000, staleMs = 30 * 60_000, renewMs = 60_000 } = {}) {
  const lock = file + ".lock";
  const token = `${process.pid}-${randomBytes(6).toString("hex")}`;
  const start = Date.now();
  for (;;) {
    try {
      const h = await open(lock, "wx");
      await h.writeFile(JSON.stringify({ pid: process.pid, token, at: new Date().toISOString() }));
      await h.close();
      break;
    } catch (e) {
      // Windows reports a lock file that is being deleted as EPERM/EACCES/EBUSY.
      if (!["EEXIST", "EPERM", "EACCES", "EBUSY"].includes(e.code)) throw e;
      const age = await stat(lock).then((s) => Date.now() - s.mtimeMs, () => 0);
      if (age > staleMs) { await reclaimIfStale(lock, staleMs); continue; }
      if (Date.now() - start > waitMs) throw new Error(`lock busy: ${lock}`);
      await sleep(250);
    }
  }
  const renew = setInterval(() => { const t = new Date(); utimes(lock, t, t).catch(() => {}); }, renewMs);
  renew.unref?.();
  try { return await fn(); } finally {
    clearInterval(renew);
    const held = await readFile(lock, "utf8").then((s) => JSON.parse(s).token, () => null);
    if (held === token) await rm(lock, { force: true });
  }
}
