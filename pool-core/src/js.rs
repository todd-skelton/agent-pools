//! The JavaScript semantics the reference relies on, so the port reads loosely typed provider
//! JSON and orders things exactly as Agent Pools does. Held to `golden/outputs/js/semantics.json`.

use serde_json::Value;
use std::cmp::Ordering;

/// `Boolean(v)`.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// `v == null` (undefined or null).
pub fn nullish(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null))
}

/// `a ?? b` over optional values.
pub fn coalesce<'a>(vs: impl IntoIterator<Item = Option<&'a Value>>) -> Option<&'a Value> {
    vs.into_iter().find(|v| !nullish(*v)).flatten()
}

/// A JSON number as `f64`; `None` for any other type (`typeof v === "number"` is false).
pub fn number(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

/// `Number.isFinite(v)`: a number, and finite.
pub fn finite(v: Option<&Value>) -> Option<f64> {
    number(v).filter(|f| f.is_finite())
}

/// `typeof v === "object" && v !== null` (arrays included).
pub fn is_object(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Object(_) | Value::Array(_)))
}

/// `key in v` for an object (an array has no such named keys).
pub fn has_key(v: Option<&Value>, key: &str) -> bool {
    matches!(v, Some(Value::Object(m)) if m.contains_key(key))
}

/// `v?.[key]`.
pub fn get<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    match v {
        Some(Value::Object(m)) => m.get(key),
        _ => None,
    }
}

/// `ToNumber(v)`, for arithmetic on values that are not always numbers. `None` stands for
/// undefined (NaN).
pub fn to_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => string_to_number(s),
        Some(Value::Array(a)) => match a.as_slice() {
            [] => 0.0,
            [one] => to_number(Some(one)),
            _ => f64::NAN,
        },
        Some(Value::Object(_)) => f64::NAN,
    }
}

fn string_to_number(s: &str) -> f64 {
    let t = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let radix = |p: &str, r| {
        u64::from_str_radix(p, r)
            .map(|n| n as f64)
            .unwrap_or(f64::NAN)
    };
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return radix(h, 16);
    }
    if let Some(o) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
        return radix(o, 8);
    }
    if let Some(b) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        return radix(b, 2);
    }
    let ok = t
        .bytes()
        .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'));
    if ok {
        t.parse().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

/// `String(v)`, as a template literal or `Array.prototype.join` writes a value.
pub fn to_string(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| {
                if x.is_null() {
                    String::new()
                } else {
                    to_string(Some(x))
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// `Number.prototype.toString()`: ECMAScript's shortest round-trip form.
pub fn number_to_string(f: f64) -> String {
    ryu_js::Buffer::new().format(f).to_string()
}

/// `Object.entries(v ?? {})`: an object's integer-like keys ascending, then the rest in
/// insertion order; an array's indexes; a string's characters (UTF-16 units as JS has them,
/// here by `char`).
pub fn entries(v: Option<&Value>) -> Vec<(String, &Value)> {
    match v {
        Some(Value::Object(m)) => {
            let index = |k: &str| -> Option<u32> {
                let n: u32 = k.parse().ok()?;
                (n != u32::MAX && n.to_string() == k).then_some(n)
            };
            let mut ints: Vec<(u32, &String, &Value)> = m
                .iter()
                .filter_map(|(k, v)| index(k).map(|n| (n, k, v)))
                .collect();
            ints.sort_by_key(|(n, ..)| *n);
            ints.into_iter()
                .map(|(_, k, v)| (k.clone(), v))
                .chain(
                    m.iter()
                        .filter(|(k, _)| index(k).is_none())
                        .map(|(k, v)| (k.clone(), v)),
                )
                .collect()
        }
        Some(Value::Array(a)) => a
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v))
            .collect(),
        _ => Vec::new(),
    }
}

/// `Object.values(v ?? {})`.
pub fn values(v: Option<&Value>) -> Vec<&Value> {
    entries(v).into_iter().map(|(_, v)| v).collect()
}

/// A comparator's result as JavaScript's sort reads it: negative, positive, or (zero or NaN)
/// equal.
pub fn order(x: f64) -> Ordering {
    if x < 0.0 {
        Ordering::Less
    } else if x > 0.0 {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

/// `a || b` for comparator chains: `a` unless it is 0 or NaN.
pub fn or_else(a: f64, b: impl FnOnce() -> f64) -> f64 {
    if a == 0.0 || a.is_nan() {
        b()
    } else {
        a
    }
}

/// `Array.prototype.sort(cmp)` as V8 runs it. Under 64 elements (every sort in Agent Pools)
/// this is V8's TimSort step for step: the leading run (reversed when strictly descending) and
/// binary insertion of the rest, so even an inconsistent comparator gives V8's order. Longer
/// arrays use a stable merge, which agrees whenever the comparator is consistent. It never
/// panics on an inconsistent comparator, unlike `slice::sort_by`.
pub fn sort_by<T>(items: &mut [T], mut user_cmp: impl FnMut(&T, &T) -> f64) {
    // V8's SortCompareUserFn: a NaN result counts as 0.
    let mut cmp = |a: &T, b: &T| {
        let x = user_cmp(a, b);
        if x.is_nan() {
            0.0
        } else {
            x
        }
    };
    let n = items.len();
    if n < 2 {
        return;
    }
    if n >= 64 {
        merge_sort(items, &mut cmp);
        return;
    }
    // CountAndMakeRun(0, n)
    let mut run = 2;
    let descending = cmp(&items[1], &items[0]) < 0.0;
    while run < n {
        let o = cmp(&items[run], &items[run - 1]);
        if (descending && o >= 0.0) || (!descending && o < 0.0) {
            break;
        }
        run += 1;
    }
    if descending {
        items[..run].reverse();
    }
    // BinaryInsertionSort(0, run, n)
    for start in run..n {
        let (mut left, mut right) = (0, start);
        while left < right {
            let mid = left + ((right - left) >> 1);
            if cmp(&items[start], &items[mid]) < 0.0 {
                right = mid;
            } else {
                left = mid + 1;
            }
        }
        items[left..=start].rotate_right(1);
    }
}

fn merge_sort<T>(items: &mut [T], cmp: &mut impl FnMut(&T, &T) -> f64) {
    // Stable top-down merge sort over indexes (O(n log n), never panics), then the items are
    // permuted into that order: a later element moves before an earlier one only when strictly
    // less.
    let n = items.len();
    let mut order: Vec<usize> = (0..n).collect();
    let mut buffer = vec![0usize; n];
    let mut width = 1;
    while width < n {
        let mut lo = 0;
        while lo < n {
            let mid = (lo + width).min(n);
            let hi = (lo + 2 * width).min(n);
            let (mut i, mut j, mut k) = (lo, mid, lo);
            while i < mid && j < hi {
                if cmp(&items[order[j]], &items[order[i]]) < 0.0 {
                    buffer[k] = order[j];
                    j += 1;
                } else {
                    buffer[k] = order[i];
                    i += 1;
                }
                k += 1;
            }
            buffer[k..k + (mid - i)].copy_from_slice(&order[i..mid]);
            k += mid - i;
            buffer[k..k + (hi - j)].copy_from_slice(&order[j..hi]);
            lo = hi;
        }
        std::mem::swap(&mut order, &mut buffer);
        width *= 2;
    }
    // Apply the permutation in place: position p takes the item that was at order[p].
    let mut done = vec![false; n];
    for start in 0..n {
        if done[start] {
            continue;
        }
        let mut p = start;
        loop {
            done[p] = true;
            let next = order[p];
            if next == start {
                break;
            }
            items.swap(p, next);
            p = next;
        }
    }
}

/// `a.localeCompare(b)` in Node's default locale (en-US, ICU root collation), for printable
/// ASCII: punctuation and symbols first (not ignored), then digits, then letters with case
/// deciding only when everything else is equal (lowercase first). Other characters order by
/// code point after all of these.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    const PUNCTUATION: &str = " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    let primary = |c: char| -> u32 {
        if let Some(i) = PUNCTUATION.find(c) {
            i as u32
        } else if c.is_ascii_digit() {
            100 + (c as u32 - '0' as u32)
        } else if c.is_ascii_alphabetic() {
            200 + (c.to_ascii_lowercase() as u32 - 'a' as u32)
        } else {
            1000 + c as u32
        }
    };
    let (pa, pb) = (a.chars().map(primary), b.chars().map(primary));
    match pa.cmp(pb) {
        Ordering::Equal => {}
        o => return o,
    }
    for (x, y) in a.chars().zip(b.chars()) {
        if x != y {
            return if x.is_ascii_lowercase() {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
    }
    Ordering::Equal
}

/// `Date.parse(s)` in milliseconds, for the ISO forms V8 reads: `YYYY`, `YYYY-MM`,
/// `YYYY-MM-DD` (UTC), and those with `T`, `t` or a space, `HH:mm`, optional `:ss` and a
/// fraction of any length (truncated to milliseconds), and `Z`, `z` or `±HH:mm`. A day up to 31
/// rolls into the next month and `24:00` is the next day, as in V8. A time with no offset is
/// local time in JavaScript, which this does not guess: `None`, like any other form.
pub fn date_parse(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize, n: usize| -> Option<i64> {
        let part = b.get(*i..*i + n)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        *i += n;
        Some(
            part.iter()
                .fold(0i64, |acc, d| acc * 10 + i64::from(d - b'0')),
        )
    };
    let year = match b.first()? {
        b'+' | b'-' => {
            let negative = b[0] == b'-';
            i = 1;
            let y = digits(&mut i, 6)?;
            if negative && y == 0 {
                return None;
            }
            if negative {
                -y
            } else {
                y
            }
        }
        _ => digits(&mut i, 4)?,
    };
    let (mut month, mut day) = (1, 1);
    if b.get(i) == Some(&b'-') {
        i += 1;
        month = digits(&mut i, 2)?;
        if b.get(i) == Some(&b'-') {
            i += 1;
            day = digits(&mut i, 2)?;
        }
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (mut hour, mut minute, mut second, mut millis, mut offset) = (0, 0, 0, 0, Some(0i64));
    if i < b.len() {
        if !matches!(b[i], b'T' | b't' | b' ') {
            return None;
        }
        i += 1;
        hour = digits(&mut i, 2)?;
        if b.get(i) != Some(&b':') {
            return None;
        }
        i += 1;
        minute = digits(&mut i, 2)?;
        if b.get(i) == Some(&b':') {
            i += 1;
            second = digits(&mut i, 2)?;
            if b.get(i) == Some(&b'.') {
                i += 1;
                let start = i;
                while b.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
                if i == start {
                    return None;
                }
                let frac = &b[start..i];
                millis = (0..3).fold(0, |acc, k| {
                    acc * 10 + frac.get(k).map_or(0, |d| i64::from(d - b'0'))
                });
            }
        }
        offset = match b.get(i) {
            None => None,
            Some(b'Z' | b'z') => {
                i += 1;
                Some(0)
            }
            Some(sign @ (b'+' | b'-')) => {
                let sign = if *sign == b'-' { -1 } else { 1 };
                i += 1;
                let h = digits(&mut i, 2)?;
                if b.get(i) != Some(&b':') {
                    return None;
                }
                i += 1;
                let m = digits(&mut i, 2)?;
                if h > 23 || m > 59 {
                    return None;
                }
                Some(sign * (h * 60 + m))
            }
            _ => return None,
        };
        if i != b.len() {
            return None;
        }
        if hour > 24
            || minute > 59
            || second > 59
            || (hour == 24 && (minute, second, millis) != (0, 0, 0))
        {
            return None;
        }
    }
    let offset = offset?;
    let days = days_from_civil(year, month, 1) + (day - 1);
    let ms = days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1000 + millis
        - offset * 60_000;
    (ms.abs() <= 8_640_000_000_000_000).then_some(ms as f64)
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `expiryMs` (routing.mjs): seconds when a number, `Date.parse` otherwise, `Infinity` for
/// nothing or an unreadable value.
pub fn expiry_ms(v: Option<&Value>) -> f64 {
    let n = match v {
        None | Some(Value::Null) => return f64::INFINITY,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN) * 1000.0,
        Some(Value::String(s)) => date_parse(s).unwrap_or(f64::NAN),
        Some(other) => date_parse(&to_string(Some(other))).unwrap_or(f64::NAN),
    };
    if n.is_finite() {
        n
    } else {
        f64::INFINITY
    }
}

/// A number as a JSON value the way `JSON.stringify` publishes it: `null` when not finite.
pub fn number_value(f: f64) -> Value {
    serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)
}

/// `JSON.stringify(v)` with no spacing: object keys in JavaScript's own-key order (see
/// [`entries`]), numbers as ECMAScript writes them, strings escaped as V8 does.
pub fn stringify(v: &Value) -> String {
    let mut out = String::new();
    write_json(&mut out, v);
    out
}

fn write_json(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match n.as_f64().filter(|f| f.is_finite()) {
            Some(f) => out.push_str(&number_to_string(f)),
            None => out.push_str("null"),
        },
        Value::String(s) => write_quoted(out, s),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(out, x);
            }
            out.push(']');
        }
        Value::Object(_) => {
            out.push('{');
            for (i, (k, x)) in entries(Some(v)).into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_quoted(out, &k);
                out.push(':');
                write_json(out, x);
            }
            out.push('}');
        }
    }
}

fn write_quoted(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `createHash("sha256").update(JSON.stringify(value)).digest("hex").slice(0, 16)`.
pub fn digest(v: &Value) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(stringify(v).as_bytes());
    hash.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// `Math.round(x)`: the nearest integer, ties toward +Infinity (V8's ceil-based form, so
/// 0.49999999999999994 rounds to 0).
pub fn round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let r = x.ceil();
    if r - 0.5 > x {
        r - 1.0
    } else {
        r
    }
}

/// `Math.min(...xs)`: `Infinity` for none, `NaN` if any is `NaN`, -0 below +0.
pub fn min(xs: impl IntoIterator<Item = f64>) -> f64 {
    let mut r = f64::INFINITY;
    for x in xs {
        if x.is_nan() {
            return f64::NAN;
        }
        if x < r || (x == 0.0 && r == 0.0 && x.is_sign_negative()) {
            r = x;
        }
    }
    r
}

/// `Math.max(...xs)`: `-Infinity` for none, `NaN` if any is `NaN`, +0 above -0.
pub fn max(xs: impl IntoIterator<Item = f64>) -> f64 {
    let mut r = f64::NEG_INFINITY;
    for x in xs {
        if x.is_nan() {
            return f64::NAN;
        }
        if x > r || (x == 0.0 && r == 0.0 && r.is_sign_negative()) {
            r = x;
        }
    }
    r
}

/// `new Date(ms).toISOString()`. The time is truncated to a whole millisecond as `Date` does;
/// a time outside `Date`'s range (where JavaScript throws a `RangeError`) is clamped to it.
pub fn iso(ms: f64) -> String {
    let t = if ms.is_nan() {
        0
    } else {
        ms.trunc().clamp(-8.64e15, 8.64e15) as i64
    };
    let days = t.div_euclid(86_400_000);
    let rem = t.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    let year = if (0..=9999).contains(&y) {
        format!("{y:04}")
    } else if y < 0 {
        format!("-{:06}", -y)
    } else {
        format!("+{y:06}")
    };
    format!(
        "{year}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

/// A proleptic Gregorian date from days since 1970-01-01 (Howard Hinnant's algorithm).
/// The proleptic Gregorian (year, month, day) of a day count from 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `Date.parse(String(v))` for a value read from JSON: `NaN` when unreadable.
pub fn date_parse_value(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::String(s)) => date_parse(s),
        other => date_parse(&to_string(other)),
    }
    .unwrap_or(f64::NAN)
}
