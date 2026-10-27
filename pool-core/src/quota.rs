//! Quota windows (quota-windows.mjs `quotaWindows`) and normalized usage (routing.mjs
//! `normalizeUsage`) from a provider's usage payload.

use crate::js;
use serde::Serialize;
use serde_json::Value;

pub const WEEK_SECONDS: f64 = 604_800.0;

/// One quota window. `scope`, `label` and `durationSeconds` are whatever the payload held
/// (normally a string, a string and a number), as the JavaScript passes them through.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaWindow {
    pub id: String,
    pub scope: Value,
    pub label: Value,
    pub used: Option<f64>,
    pub reset_at: Option<f64>,
    pub duration_seconds: Value,
    pub common: bool,
}

impl QuotaWindow {
    /// `durationSeconds` when it is a number.
    pub fn duration(&self) -> Option<f64> {
        self.duration_seconds.as_f64()
    }
}

fn pct(v: Option<&Value>) -> Option<f64> {
    js::number(v).filter(|f| f.is_finite() && (0.0..=100.0).contains(f))
}

fn reset(v: Option<&Value>) -> Option<f64> {
    let n = match v {
        None | Some(Value::Null) => return None,
        Some(Value::Number(n)) => n.as_f64()? * 1000.0,
        Some(Value::String(s)) => js::date_parse(s)?,
        Some(other) => js::date_parse(&js::to_string(Some(other)))?,
    };
    n.is_finite().then_some(n)
}

/// `human`: underscores to spaces, each word's first character upper-cased (`\b\w`, ASCII).
fn human(v: Option<&Value>) -> String {
    let s = if js::nullish(v) {
        String::new()
    } else {
        js::to_string(v)
    }
    .replace('_', " ");
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::with_capacity(s.len());
    let mut prev_word = false;
    for c in s.chars() {
        out.push(if word(c) && !prev_word {
            c.to_ascii_uppercase()
        } else {
            c
        });
        prev_word = word(c);
    }
    out
}

/// `name`: a string as it is, otherwise its `display_name ?? name ?? id`.
fn name(v: Option<&Value>) -> Option<Value> {
    match v {
        Some(Value::String(_)) => v.cloned(),
        _ => js::coalesce([
            js::get(v, "display_name"),
            js::get(v, "name"),
            js::get(v, "id"),
        ])
        .cloned(),
    }
}

/// `period(seconds, fallback)`. Only a number names a period; any other value takes the
/// fallback (JavaScript would also coerce a numeric string, which no payload sends).
fn period(seconds: Option<&Value>, fallback: Value) -> Value {
    let Some(s) = js::number(seconds) else {
        return fallback;
    };
    if s == WEEK_SECONDS {
        "Weekly".into()
    } else if s == 86_400.0 {
        "Daily".into()
    } else if s >= 3600.0 && s % 3600.0 == 0.0 {
        format!("{}-hour", js::number_to_string(s / 3600.0)).into()
    } else if s > 0.0 && s % 60.0 == 0.0 {
        format!("{}-minute", js::number_to_string(s / 60.0)).into()
    } else {
        fallback
    }
}

/// `x || y` over values.
fn or_value(x: Option<Value>, y: impl FnOnce() -> Value) -> Value {
    match x {
        Some(v) if js::truthy(Some(&v)) => v,
        _ => y(),
    }
}

/// `quotaWindows(provider, data)`.
pub fn quota_windows(provider: &str, data: &Value) -> Vec<QuotaWindow> {
    let mut out = Vec::new();
    let mut add = |id: String,
                   scope: Value,
                   label: Value,
                   w: &Value,
                   duration: Option<&Value>,
                   common: bool| {
        if !js::is_object(Some(w)) {
            return;
        }
        let w = Some(w);
        out.push(QuotaWindow {
            id,
            scope,
            label,
            used: pct(js::coalesce([
                js::get(w, "used_percent"),
                js::get(w, "utilization"),
                js::get(w, "percent"),
            ])),
            reset_at: reset(js::coalesce([
                js::get(w, "reset_at"),
                js::get(w, "resets_at"),
            ])),
            duration_seconds: js::coalesce([duration]).cloned().unwrap_or(Value::Null),
            common,
        });
    };
    let data = Some(data);
    if provider == "codex" {
        let mut bucket = |id: &str, scope: Value, r: Option<&Value>| {
            for (key, w) in js::entries(r) {
                if js::is_object(Some(w))
                    && (js::has_key(Some(w), "used_percent") || js::has_key(Some(w), "utilization"))
                {
                    let seconds = js::get(Some(w), "limit_window_seconds");
                    let label = period(
                        seconds,
                        or_value(name(Some(w)), || {
                            human(Some(&Value::String(key.clone()))).into()
                        }),
                    );
                    add(
                        format!("{id}:{key}"),
                        scope.clone(),
                        label,
                        w,
                        seconds,
                        id == "codex",
                    );
                }
            }
        };
        bucket("codex", "Codex".into(), js::get(data, "rate_limit"));
        if let Some(Value::Array(extra)) = js::get(data, "additional_rate_limits") {
            for (i, r) in extra.iter().enumerate() {
                let r = Some(r);
                let scope = js::coalesce([
                    js::get(r, "limit_name"),
                    js::get(r, "display_name"),
                    js::get(r, "metered_feature"),
                ])
                .cloned()
                .unwrap_or_else(|| format!("Additional limit {}", i + 1).into());
                bucket(&format!("additional:{i}"), scope, js::get(r, "rate_limit"));
            }
        }
        for (key, r) in js::entries(data) {
            if key != "rate_limit" && key.ends_with("_rate_limit") {
                let scope = human(Some(&Value::String(
                    key.trim_end_matches("_rate_limit").to_string(),
                )));
                bucket(&key, scope.into(), Some(r));
            }
        }
        return out;
    }
    let limits: &[Value] = match js::get(data, "limits") {
        Some(Value::Array(a)) => a,
        _ => &[],
    };
    let mut seen = std::collections::HashSet::new();
    let (mut session, mut weekly) = (false, false);
    for (i, w) in limits.iter().enumerate() {
        let wo = Some(w);
        if !js::truthy(wo)
            || !(js::has_key(wo, "percent")
                || js::has_key(wo, "utilization")
                || js::has_key(wo, "used_percent"))
        {
            continue;
        }
        let names: Vec<Value> = js::values(js::coalesce([js::get(wo, "scope")]))
            .into_iter()
            .filter_map(|v| name(Some(v)))
            .filter(|v| js::truthy(Some(v)))
            .collect();
        let kind = js::get(wo, "kind");
        let kind_str = kind.and_then(Value::as_str);
        let session_or_weekly_all = matches!(kind_str, Some("session" | "weekly_all"));
        let joined = names
            .iter()
            .map(|v| js::to_string(Some(v)))
            .collect::<Vec<_>>()
            .join(" \u{b7} ");
        let scope = if !joined.is_empty() {
            Value::String(joined)
        } else {
            or_value(js::get(wo, "display_name").cloned(), || {
                if session_or_weekly_all {
                    "All models".into()
                } else {
                    let h = human(kind);
                    if h.is_empty() {
                        "Additional usage".into()
                    } else {
                        h.into()
                    }
                }
            })
        };
        let duration = js::coalesce([js::get(wo, "limit_window_seconds")])
            .cloned()
            .unwrap_or_else(|| {
                if kind_str == Some("session") {
                    18_000.into()
                } else if js::get(wo, "group").and_then(Value::as_str) == Some("weekly")
                    || kind_str.is_some_and(|k| k.starts_with("weekly"))
                {
                    604_800.into()
                } else {
                    Value::Null
                }
            });
        let label = period(Some(&duration), {
            let h = human(js::get(wo, "group"));
            if h.is_empty() {
                "Usage window".into()
            } else {
                h.into()
            }
        });
        let common = session_or_weekly_all && names.is_empty();
        seen.insert(format!(
            "{}:{}",
            js::to_string(Some(&scope)).to_lowercase(),
            js::to_string(Some(&duration))
        ));
        add(
            format!("limit:{i}"),
            scope,
            label,
            w,
            Some(&duration),
            common,
        );
        session |= kind_str == Some("session");
        weekly |= kind_str == Some("weekly_all");
    }
    // Legacy named windows: five_hour, seven_day and their suffixed variants.
    for (key, w) in js::entries(data) {
        let (base, suffix) = match key
            .strip_prefix("five_hour")
            .map(|r| ("five_hour", r))
            .or_else(|| key.strip_prefix("seven_day").map(|r| ("seven_day", r)))
        {
            Some((base, "")) => (base, None),
            Some((base, rest)) => match rest.strip_prefix('_') {
                Some(s) if !s.is_empty() && !s.contains(['\n', '\r', '\u{2028}', '\u{2029}']) => {
                    (base, Some(s))
                }
                _ => continue,
            },
            None => continue,
        };
        let wo = Some(w);
        if !js::is_object(wo) || !js::has_key(wo, "utilization") {
            continue;
        }
        if suffix.is_none() && ((base == "five_hour" && session) || (base == "seven_day" && weekly))
        {
            continue;
        }
        let scope = or_value(name(wo), || {
            human(Some(&Value::String(
                suffix.unwrap_or("All models").to_string(),
            )))
            .into()
        });
        let duration: f64 = if base == "five_hour" {
            18_000.0
        } else {
            WEEK_SECONDS
        };
        if suffix.is_some()
            && seen.contains(&format!(
                "{}:{}",
                js::to_string(Some(&scope)).to_lowercase(),
                js::number_to_string(duration)
            ))
        {
            continue;
        }
        let duration_value = Value::from(duration as u64);
        add(
            key.clone(),
            scope,
            period(Some(&duration_value), "Usage window".into()),
            w,
            Some(&duration_value),
            suffix.is_none(),
        );
    }
    out
}

/// `normalizeUsage(provider, data, now)`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub status: &'static str,
    pub windows: Vec<QuotaWindow>,
    pub quota_windows: Vec<QuotaWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    pub next_reset: Option<f64>,
    pub rank_reset: Option<f64>,
    pub rank_remaining: Option<f64>,
}

pub fn normalize_usage(provider: &str, data: &Value, now: f64) -> Usage {
    let rows = quota_windows(provider, data);
    let windows: Vec<QuotaWindow> = rows.iter().filter(|w| w.common).cloned().collect();
    let valid = !windows.is_empty() && windows.iter().all(|w| w.used.is_some());
    let limit = if provider == "codex" {
        js::get(Some(data), "rate_limit")
    } else {
        None
    };
    let malformed = js::truthy(limit)
        && ["allowed", "limit_reached"].iter().any(|k| {
            let v = js::get(limit, k);
            !js::nullish(v) && !matches!(v, Some(Value::Bool(_)))
        });
    let mut status = "unknown";
    if !malformed {
        let flag = |k| js::get(limit, k).and_then(Value::as_bool);
        if flag("allowed") == Some(false) || flag("limit_reached") == Some(true) {
            status = "exhausted";
        } else if flag("allowed") == Some(true) {
            status = "available";
        } else if valid {
            status = if windows.iter().any(|w| w.used.is_some_and(|u| u >= 100.0)) {
                "exhausted"
            } else {
                "available"
            };
        }
    }
    let future: Vec<f64> = windows
        .iter()
        .filter_map(|w| w.reset_at)
        .filter(|r| *r > now)
        .collect();
    let mut sorted: Vec<&QuotaWindow> = windows.iter().collect();
    js::sort_by(&mut sorted, |a, b| {
        let d = |w: &QuotaWindow| {
            if w.duration_seconds.is_null() {
                0.0
            } else {
                js::to_number(Some(&w.duration_seconds))
            }
        };
        let r = |w: &QuotaWindow| w.reset_at.unwrap_or(f64::NEG_INFINITY);
        js::or_else(d(b) - d(a), || r(b) - r(a))
    });
    let longest = sorted.first().copied();
    Usage {
        status,
        remaining: (valid && status != "unknown").then(|| {
            windows
                .iter()
                .map(|w| 100.0 - w.used.unwrap_or(f64::NAN))
                .fold(f64::INFINITY, js_min)
                .max(0.0)
        }),
        next_reset: (!future.is_empty())
            .then(|| future.iter().copied().fold(f64::INFINITY, js_min)),
        rank_reset: longest.and_then(|w| w.reset_at),
        rank_remaining: longest.and_then(|w| w.used).map(|u| (100.0 - u).max(0.0)),
        quota_windows: rows,
        windows,
    }
}

/// `Math.min` of two: NaN wins, and -0 is below +0.
fn js_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == b {
        if a.is_sign_negative() {
            a
        } else {
            b
        }
    } else {
        a.min(b)
    }
}
