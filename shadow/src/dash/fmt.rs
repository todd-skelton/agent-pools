//! The live dashboard's formatting (dashboard.html: `num`, `usd`, `day`, `dt`, `clock`, `inT`,
//! `ago`), in en-US. The browser wrote times in the viewer's zone; the server writes them in
//! one zone, a US zone with daylight saving time (`SHADOW_UTC_STANDARD_HOURS`, -6: Central).

use pool_core::js;
use serde_json::Value;

pub const H: f64 = 3_600_000.0;
pub const D: f64 = 86_400_000.0;

/// A US zone: `standard` hours east of UTC, an hour more from the second Sunday in March to the
/// first Sunday in November, switching at 2:00 local time.
#[derive(Clone, Copy, Debug)]
pub struct Zone {
    pub standard: i32,
}

impl Zone {
    pub fn from_env() -> Zone {
        let standard = std::env::var("SHADOW_UTC_STANDARD_HOURS").ok().and_then(|v| v.parse().ok()).unwrap_or(-6);
        Zone { standard }
    }

    /// Hours east of UTC at `ms`.
    pub fn offset_hours(&self, ms: f64) -> i32 {
        let (year, _, _) = civil(ms);
        let start = nth_sunday(year, 3, 2) + f64::from(2 - self.standard) * H;
        let end = nth_sunday(year, 11, 1) + f64::from(2 - (self.standard + 1)) * H;
        if ms >= start && ms < end { self.standard + 1 } else { self.standard }
    }

    /// Local time fields at `ms`.
    pub fn local(&self, ms: f64) -> Local {
        let l = ms + f64::from(self.offset_hours(ms)) * H;
        let (_, month, day) = civil(l);
        let days = (l / D).floor();
        let weekday = ((days as i64 + 4).rem_euclid(7)) as usize;
        let minute_of_day = ((l - days * D) / 60_000.0).floor() as i64;
        Local { month, day, weekday, hour: (minute_of_day / 60) as u32, minute: (minute_of_day % 60) as u32 }
    }

    /// The UTC instant of local midnight on the day of `ms`.
    pub fn day_start(&self, ms: f64) -> f64 {
        let l = ms + f64::from(self.offset_hours(ms)) * H;
        let midnight = (l / D).floor() * D;
        // The offset at midnight (a switch day changes it at 2:00, after midnight).
        let guess = midnight - f64::from(self.standard) * H;
        midnight - f64::from(self.offset_hours(guess)) * H
    }
}

pub struct Local {
    pub month: u32,
    pub day: u32,
    pub weekday: usize,
    pub hour: u32,
    pub minute: u32,
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn civil(ms: f64) -> (i64, u32, u32) {
    let days = (ms / D).floor() as i64;
    let (y, m, d) = js::civil_from_days(days);
    (y, m as u32, d as u32)
}

/// Midnight UTC of the `n`th Sunday of `month` in `year`.
fn nth_sunday(year: i64, month: u32, n: u32) -> f64 {
    let first = days_from_civil(year, month, 1);
    let weekday = (first + 4).rem_euclid(7);
    let to_sunday = (7 - weekday) % 7;
    (first + to_sunday + 7 * i64::from(n - 1)) as f64 * D
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `ts(v)`: a number as is, text as `Date.parse` reads it.
pub fn ts(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => js::date_parse(s).unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

pub fn ok_time(v: Option<&Value>) -> bool {
    ts(v).is_finite()
}

fn hour12(h: u32) -> (u32, &'static str) {
    let ap = if h < 12 { "AM" } else { "PM" };
    (if h.is_multiple_of(12) { 12 } else { h % 12 }, ap)
}

/// `Tue, Oct 27`
pub fn day_ms(z: Zone, ms: f64) -> String {
    if !ms.is_finite() {
        return "?".into();
    }
    let l = z.local(ms);
    format!("{}, {} {}", WEEKDAYS[l.weekday], MONTHS[l.month as usize - 1], l.day)
}

/// `4:19 AM`
pub fn clock_ms(z: Zone, ms: f64) -> String {
    if !ms.is_finite() {
        return "?".into();
    }
    let l = z.local(ms);
    let (h, ap) = hour12(l.hour);
    format!("{h}:{:02} {ap}", l.minute)
}

/// `Tue, Oct 27, 4:19 AM`
pub fn dt_ms(z: Zone, ms: f64) -> String {
    if !ms.is_finite() {
        return "?".into();
    }
    format!("{}, {}", day_ms(z, ms), clock_ms(z, ms))
}

pub fn day(z: Zone, v: Option<&Value>) -> String {
    day_ms(z, ts(v))
}

pub fn dt(z: Zone, v: Option<&Value>) -> String {
    dt_ms(z, ts(v))
}

pub fn clock(z: Zone, v: Option<&Value>) -> String {
    clock_ms(z, ts(v))
}

/// `Tu 28`, the timeline's tick.
pub fn tick(z: Zone, ms: f64) -> (String, bool) {
    let l = z.local(ms);
    (format!("{} {}", &WEEKDAYS[l.weekday][..2], l.day), l.weekday == 0 || l.weekday == 6)
}

/// `inT(v)`: `3d 4h`, `2h 5m`, `12m`, `due`, or empty.
pub fn in_t(ms: f64, now: f64) -> String {
    let left = ms - now;
    if !left.is_finite() {
        return String::new();
    }
    if left <= 0.0 {
        return "due".into();
    }
    let m = (left / 60_000.0).ceil() as i64;
    if m >= 1440 {
        format!("{}d {}h", m / 1440, (m % 1440) / 60)
    } else if m >= 60 {
        format!("{}h {}m", m / 60, m % 60)
    } else {
        format!("{m}m")
    }
}

/// `ago(v)`
pub fn ago(v: Option<&Value>, now: f64) -> String {
    let t = ts(v);
    if !t.is_finite() {
        return "never".into();
    }
    let m = ((now - t) / 60_000.0).floor().max(0.0) as i64;
    if m < 1 {
        "just now".into()
    } else if m < 60 {
        format!("{m}m ago")
    } else if m < 1440 {
        format!("{}h ago", m / 60)
    } else {
        format!("{}d ago", m / 1440)
    }
}

/// `x` (not negative) to `places` fraction digits as Intl rounds: the shortest decimal that
/// reads back as `x` (`Display`), a tie away from zero (`1246.5` is `1247`, `1.005` is
/// `1.01`), where `format!("{:.2}")` rounds the binary value, half to even.
fn half_expand(x: f64, places: usize) -> String {
    let shortest = format!("{x}");
    let (int, frac) = shortest.split_once('.').unwrap_or((&shortest, ""));
    let mut frac = frac.to_string();
    let next = frac.as_bytes().get(places).copied();
    frac.truncate(places);
    while frac.len() < places {
        frac.push('0');
    }
    let keep = if places == 0 { int.to_string() } else { format!("{int}.{frac}") };
    let mut digits: Vec<u8> = keep.bytes().collect();
    if next.is_some_and(|d| d >= b'5') {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, b'1');
                break;
            }
            i -= 1;
            match digits[i] {
                b'.' => continue,
                b'9' => digits[i] = b'0',
                d => {
                    digits[i] = d + 1;
                    break;
                }
            }
        }
    }
    let mut s = String::from_utf8(digits).expect("ASCII digits");
    if s.ends_with('.') {
        s.pop();
    }
    s
}

/// `x.toLocaleString()` in en-US: grouped, at most `max` and at least `min` fraction digits.
pub fn locale(x: f64, min: usize, max: usize) -> String {
    if !x.is_finite() {
        return if x.is_nan() { "NaN".into() } else if x > 0.0 { "∞".into() } else { "-∞".into() };
    }
    let fixed = half_expand(x.abs(), max);
    let (int, frac) = fixed.split_once('.').unwrap_or((&fixed, ""));
    let mut frac = frac.to_string();
    while frac.len() > min && frac.ends_with('0') {
        frac.pop();
    }
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let sign = if x < 0.0 && (int != "0" || frac.chars().any(|c| c != '0')) { "-" } else { "" };
    if frac.is_empty() { format!("{sign}{grouped}") } else { format!("{sign}{grouped}.{frac}") }
}

/// `num(x, d)` for a computed number: `?` when not finite, else rounded to `d` places, grouped.
pub fn num(x: f64, d: i32) -> String {
    if !x.is_finite() {
        return "?".into();
    }
    let p = 10f64.powi(d);
    locale(js::round(x * p) / p, 0, 3)
}

/// `num(v, d)` for a document's value: `?` for `null`/absent, else `Number(v)`.
pub fn num_v(v: Option<&Value>, d: i32) -> String {
    if js::nullish(v) { "?".into() } else { num(js::to_number(v), d) }
}

/// `usd(x, d)`: `$1,234.50`.
pub fn usd(x: f64, d: usize) -> String {
    if !x.is_finite() { "?".into() } else { format!("${}", locale(x, d, d)) }
}

pub fn usd_v(v: Option<&Value>, d: usize) -> String {
    if js::nullish(v) { "?".into() } else { usd(js::to_number(v), d) }
}

/// `Number(v)`: `undefined` NaN, `null` 0.
pub fn n(v: Option<&Value>) -> f64 {
    js::to_number(v)
}

/// `meter(u)`'s test: `u == null || !Number.isFinite(Number(u))` is n/a, else `Number(u)`.
pub fn number(v: Option<&Value>) -> Option<f64> {
    match v {
        None | Some(Value::Null) => None,
        v => Some(js::to_number(v)).filter(|x| x.is_finite()),
    }
}

/// `String(x ?? "")`.
pub fn text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => js::to_string(Some(v)),
    }
}

/// `plural(n, w)`
pub fn plural(n: f64, w: &str) -> String {
    format!("{} {w}{}", js::number_to_string(n), if n == 1.0 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CENTRAL: Zone = Zone { standard: -6 };

    #[test]
    fn ties_round_away_from_zero_as_intl_does() {
        assert_eq!(usd(1246.5, 0), "$1,247");
        assert_eq!(usd(0.125, 2), "$0.13");
        assert_eq!(usd(999.5, 0), "$1,000");
        assert_eq!(locale(2.5, 0, 0), "3");
        assert_eq!(usd(1.005, 2), "$1.01"); // the digits Intl reads, not the binary 1.00499..
        assert_eq!(usd(2.675, 2), "$2.68");
        assert_eq!(locale(1234.5678, 0, 3), "1,234.568");
        assert_eq!(num(-2.5, 0), "-2");
    }

    #[test]
    fn central_time() {
        let at = |s: &str| js::date_parse(s).unwrap();
        assert_eq!(dt_ms(CENTRAL, at("2026-10-27T04:19:02Z")), "Mon, Oct 26, 11:19 PM");
        assert_eq!(dt_ms(CENTRAL, at("2026-11-02T06:08:09Z")), "Mon, Nov 2, 12:08 AM");
        // Daylight time ends 2026-11-01 at 2:00 CDT (07:00 UTC).
        assert_eq!(clock_ms(CENTRAL, at("2026-11-01T06:59:00Z")), "1:59 AM");
        assert_eq!(clock_ms(CENTRAL, at("2026-11-01T07:00:00Z")), "1:00 AM");
        // and starts 2027-03-14 at 2:00 CST (08:00 UTC).
        assert_eq!(clock_ms(CENTRAL, at("2027-03-14T07:59:00Z")), "1:59 AM");
        assert_eq!(clock_ms(CENTRAL, at("2027-03-14T08:00:00Z")), "3:00 AM");
        assert_eq!(js::iso(CENTRAL.day_start(at("2026-10-27T04:19:02Z"))), "2026-10-26T05:00:00.000Z");
        assert_eq!(tick(CENTRAL, at("2026-10-31T05:00:00Z")), ("Sa 31".to_string(), true));
    }

    #[test]
    fn numbers() {
        assert_eq!(num(14.375, 1), "14.4");
        assert_eq!(num(1234567.0, 1), "1,234,567");
        assert_eq!(num(0.03047 * 168.0, 1), "5.1");
        assert_eq!(num(f64::NAN, 1), "?");
        assert_eq!(usd(6573.95, 2), "$6,573.95");
        assert_eq!(usd(700.0, 0), "$700");
        assert_eq!(usd(0.0123, 3), "$0.012");
        assert_eq!(num_v(Some(&json!(null)), 1), "?");
        assert_eq!(num(n(Some(&json!(null))) * 168.0, 1), "0");
        assert_eq!(in_t(10.0 * H + 5.0 * 60_000.0, 0.0), "10h 5m");
        assert_eq!(in_t(3.0 * D + 4.0 * H, 0.0), "3d 4h");
    }
}
