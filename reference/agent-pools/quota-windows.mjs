// Canonical quota windows shared by display, routing, and recovery.
const pct = (v) =>
  typeof v === "number" && Number.isFinite(v) && v >= 0 && v <= 100 ? v : null;
const reset = (v) => {
  if (v == null) return null;
  const n = typeof v === "number" ? v * 1000 : Date.parse(v);
  return Number.isFinite(n) ? n : null;
};
const human = (v) =>
  String(v ?? "")
    .replaceAll("_", " ")
    .replace(/\b\w/g, (c) => c.toUpperCase());
const name = (v) =>
  typeof v === "string" ? v : (v?.display_name ?? v?.name ?? v?.id);
function period(seconds, fallback = "Usage window") {
  if (seconds === 604800) return "Weekly";
  if (seconds === 86400) return "Daily";
  if (seconds >= 3600 && seconds % 3600 === 0) return `${seconds / 3600}-hour`;
  if (seconds > 0 && seconds % 60 === 0) return `${seconds / 60}-minute`;
  return fallback;
}
export function quotaWindows(provider, data) {
  const out = [];
  const add = (id, scope, label, w, durationSeconds, common = false) => {
    if (!w || typeof w !== "object") return;
    out.push({
      id,
      scope,
      label,
      used: pct(w.used_percent ?? w.utilization ?? w.percent),
      resetAt: reset(w.reset_at ?? w.resets_at),
      durationSeconds: durationSeconds ?? null,
      common,
    });
  };
  if (provider === "codex") {
    const bucket = (id, scope, r) => {
      for (const [key, w] of Object.entries(r ?? {})) {
        if (
          w &&
          typeof w === "object" &&
          ("used_percent" in w || "utilization" in w)
        )
          add(
            `${id}:${key}`,
            scope,
            period(w.limit_window_seconds, name(w) || human(key)),
            w,
            w.limit_window_seconds,
            id === "codex",
          );
      }
    };
    bucket("codex", "Codex", data.rate_limit);
    for (const [i, r] of (Array.isArray(data.additional_rate_limits)
      ? data.additional_rate_limits
      : []
    ).entries())
      bucket(
        `additional:${i}`,
        r.limit_name ??
          r.display_name ??
          r.metered_feature ??
          `Additional limit ${i + 1}`,
        r.rate_limit,
      );
    for (const [key, r] of Object.entries(data)) {
      if (key !== "rate_limit" && key.endsWith("_rate_limit"))
        bucket(key, human(key.replace(/_rate_limit$/, "")), r);
    }
  } else {
    const limits = Array.isArray(data.limits) ? data.limits : [];
    const seen = new Set();
    let session = false,
      weekly = false;
    for (const [i, w] of limits.entries()) {
      if (!w || !("percent" in w || "utilization" in w || "used_percent" in w))
        continue;
      const names = Object.values(w.scope ?? {})
        .map(name)
        .filter(Boolean);
      const scope =
        names.join(" · ") ||
        w.display_name ||
        (["session", "weekly_all"].includes(w.kind)
          ? "All models"
          : human(w.kind) || "Additional usage");
      const duration =
        w.limit_window_seconds ??
        (w.kind === "session"
          ? 18000
          : w.group === "weekly" || w.kind?.startsWith("weekly")
            ? 604800
            : null);
      add(
        `limit:${i}`,
        scope,
        period(duration, human(w.group) || "Usage window"),
        w,
        duration,
        ["session", "weekly_all"].includes(w.kind) && names.length === 0,
      );
      seen.add(`${scope.toLowerCase()}:${duration}`);
      if (w.kind === "session") session = true;
      if (w.kind === "weekly_all") weekly = true;
    }
    // Legacy named windows remain supported; opaque provider experiment fields do not get invented names.
    for (const [key, w] of Object.entries(data)) {
      const match = key.match(/^(five_hour|seven_day)(?:_(.+))?$/);
      if (!match || !w || typeof w !== "object" || !("utilization" in w))
        continue;
      if (
        !match[2] &&
        ((match[1] === "five_hour" && session) ||
          (match[1] === "seven_day" && weekly))
      )
        continue;
      const scope = name(w) || human(match[2] || "All models");
      const duration = match[1] === "five_hour" ? 18000 : 604800;
      if (match[2] && seen.has(`${scope.toLowerCase()}:${duration}`)) continue;
      add(key, scope, period(duration), w, duration, !match[2]);
    }
  }
  return out;
}
