// Proxy health: management probe timeout and consecutive misses before a
// restart. Ten-second checks make five misses about fifty seconds, long enough
// for a proxy busy with several streams to answer without being killed.
export const PROXY_HEALTH_TIMEOUT_MS = 8000;
export const PROXY_HEALTH_FAILURES = 5;
// Liveness (/healthz: no locks, no upstream calls) decides whether the proxy is
// down. A proxy whose management reads stall while /healthz answers is still
// serving inference (2026-10-19 14:21 CDT: DNS and TLS to chatgpt.com stalled,
// auth-files took 22-45 s, the restart killed every in-flight turn and each
// replacement was killed again before it finished starting). Such a proxy is
// restarted only after PROXY_DEGRADED_FAILURES consecutive misses (about five
// minutes), as a backstop for a wedged management layer.
export const PROXY_LIVENESS_TIMEOUT_MS = 3000;
export const PROXY_DEGRADED_FAILURES = 30;
// A (re)started proxy is not judged until it first answers or this passes.
// Startup took 12-16 s on a loaded machine, and the old allowance (whatever was
// left of five misses, about 40 s) was not enough during that incident.
export const PROXY_STARTUP_GRACE_MS = 120000;

// Consecutive failures avoid restarting healthy services after a single slow read.
export function createHealthPolicy(threshold = 3) {
  let failures = 0;
  return (healthy) => {
    failures = healthy ? 0 : failures + 1;
    if (failures < threshold) return false;
    failures = 0;
    return true;
  };
}

// Proxy restart decision from one health reading: `managed` is the management
// probe, `alive` the liveness probe (only meaningful when management failed).
// `degraded` is true on the one reading that reaches the plain failure count
// while the proxy is still alive, so the caller can log it once per episode.
export function createProxyRestartPolicy({
  failures = PROXY_HEALTH_FAILURES, degradedFailures = PROXY_DEGRADED_FAILURES,
  startupGraceMs = PROXY_STARTUP_GRACE_MS, now = Date.now,
} = {}) {
  let dead = 0, slow = 0, startedAt = null;
  return {
    started() { startedAt = now(); dead = 0; slow = 0; },
    check({ managed, alive }) {
      if (managed) { startedAt = null; dead = 0; slow = 0; return { restart: false, degraded: false }; }
      if (startedAt !== null && now() - startedAt < startupGraceMs) return { restart: false, degraded: false };
      if (alive) {
        dead = 0; slow++;
        if (slow >= degradedFailures) { slow = 0; return { restart: true, degraded: false }; }
        return { restart: false, degraded: slow === failures };
      }
      slow = 0; dead++;
      if (dead >= failures) { dead = 0; return { restart: true, degraded: false }; }
      return { restart: false, degraded: false };
    },
  };
}

// The recorded process is still the one we started: same PID and creation time.
export function sameIdentity(record, actual) {
  return !!actual && Number(record.pid) === Number(actual.ProcessId) &&
    Number.isFinite(Date.parse(record.creationDate)) &&
    Date.parse(record.creationDate) === Date.parse(actual.CreationDate);
}

// A listener this pool started but no longer has a record for (the record was
// overwritten by a spawn that lost the port race): one of the pool's own proxy
// builds (root/proxy/<version>/cli-proxy-api.exe) running the pool's config.
export function ownedProxy(actual, poolRoot, configPath) {
  const exe = normalizePath(actual?.ExecutablePath);
  const builds = normalizePath(poolRoot).replace(/\\+$/, "") + "\\proxy\\";
  return !!actual && exe.startsWith(builds) && exe.endsWith("\\cli-proxy-api.exe") &&
    !exe.slice(builds.length).includes("..") && usesConfig(actual.CommandLine, configPath);
}

const normalizePath = (v) => String(v ?? "").replaceAll("/", "\\").toLowerCase();
// Every -config argument (quoted or bare) equals configPath. Go's flag package
// lets a repeated flag override an earlier one, so one matching value is not
// enough to know which config the process runs.
export function usesConfig(commandLine, configPath) {
  const values = [...String(commandLine ?? "").matchAll(/(?:^|\s)--?config(?:=|\s+)(?:"([^"]*)"|(\S+))/gi)].map((m) => m[1] ?? m[2]);
  return values.length > 0 && values.every((v) => normalizePath(v) === normalizePath(configPath));
}

// ...and it is running the executable and config that install.json selects now.
export function sameProcess(record, actual, executable, configPath) {
  return sameIdentity(record, actual) &&
    normalizePath(actual.ExecutablePath) === normalizePath(executable) &&
    usesConfig(actual.CommandLine, configPath);
}
