//! usage-value.mjs's `buildRoutingAttempts(sessionUsage, orchestratorRoot)`: each dispatch the
//! orchestrator logged (`dispatch-log.jsonl` under `orchestratorRoot`, outside the install) joined
//! to the session its transcript started, with that session's root totals: quota units, list-price
//! dollars and hours. Reads only; `state/routing-attempts.json` is written by [`super::report`].

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};

use pool_core::js;
use serde_json::{Map, Value, json};

const HOUR: f64 = 3_600_000.0;

/// `buildRoutingAttempts`: the report, or why a file could not be read (anything but a missing
/// one, which reads as empty, as `jsonLines` and `transcriptSession` treat it).
pub fn build(session_usage: &Value, orchestrator_root: Option<&str>) -> Result<Value, String> {
    let generated = session_usage.get("generatedAt").cloned();
    let root = orchestrator_root.filter(|r| !r.is_empty());
    let mut report = Map::new();
    report.insert("format".into(), json!("routing-attempts/v1"));
    if let Some(g) = generated.clone() {
        report.insert("generatedAt".into(), g);
    }
    report.insert("orchestratorRoot".into(), root.map_or(Value::Null, |r| json!(r)));
    report.insert("attempts".into(), json!([]));
    let Some(root) = root else {
        report.insert("note".into(), json!("state/install.json has no orchestratorRoot; dispatch join is disabled."));
        return Ok(Value::Object(report));
    };
    let list = js::values(session_usage.get("sessions"));
    // new Map(sessions.map(s => [s.sid, s])): a later session with the same id wins.
    let mut sessions: HashMap<String, &Value> = HashMap::new();
    for s in &list {
        if let Some(Value::String(sid)) = s.get("sid") {
            sessions.insert(sid.clone(), s);
        }
    }
    let key = |s: &Value| format!("{}|{}", js::to_string(s.get("provider")), js::to_string(s.get("rootSid")));
    // Per provider and root session: the earliest start, the latest end, and the sums (quota
    // units unknown when any session's are).
    let mut roots: HashMap<String, (f64, f64, Option<f64>, f64)> = HashMap::new();
    for s in &list {
        let g = roots.entry(key(s)).or_insert((f64::INFINITY, f64::NEG_INFINITY, Some(0.0), 0.0));
        g.0 = js_min(g.0, js::date_parse_value(s.get("firstAt")));
        g.1 = js_max(g.1, js::date_parse_value(s.get("lastAt")));
        g.3 += js::to_number(s.get("apiEquivalentUsd"));
        g.2 = match (g.2, s.get("quotaUnits")) {
            (None, _) | (_, Some(Value::Null)) => None,
            (Some(sum), q) => Some(sum + js::to_number(q)),
        };
    }
    let from = js::date_parse_value(session_usage.get("from"));
    let until = js::date_parse_value(generated.as_ref());
    let base = absolute(Path::new(root));
    let mut transcripts: HashMap<PathBuf, Option<Value>> = HashMap::new();
    let mut attempts = Vec::new();
    for row in json_lines(&base.join("dispatch-log.jsonl"))? {
        let when = row.get("ts").filter(|v| !v.is_null()).or(row.get("at"));
        let at = js::date_parse_value(when);
        let transcript = match row.get("transcript") {
            Some(Value::String(t)) if !t.is_empty() => t,
            _ => continue,
        };
        if row.get("kind").and_then(Value::as_str) != Some("dispatch") || !at.is_finite() || at < from || at > until {
            continue;
        }
        let file = normalize(&base.join(transcript));
        if !transcripts.contains_key(&file) {
            let id = transcript_session(&file)?;
            transcripts.insert(file.clone(), id);
        }
        let Some(Some(Value::String(id))) = transcripts.get(&file) else { continue };
        let Some(session) = sessions.get(id) else { continue };
        let (first, last, quota, usd) = roots[&key(session)];
        let or_null = |k: &str| row.get(k).filter(|v| !v.is_null()).cloned().unwrap_or(Value::Null);
        let mut a = Map::new();
        for k in ["label", "row", "model", "effort"] {
            a.insert(k.into(), or_null(k));
        }
        if let Some(r) = session.get("rootSid") {
            a.insert("rootSid".into(), r.clone());
        }
        a.insert("at".into(), json!(js::iso(at)));
        a.insert("quotaUnits".into(), quota.map_or(Value::Null, js::number_value));
        a.insert("apiEquivalentUsd".into(), js::number_value(usd));
        a.insert("hours".into(), js::number_value((last - first) / HOUR));
        attempts.push(Value::Object(a));
    }
    report.insert("attempts".into(), Value::Array(attempts));
    Ok(Value::Object(report))
}

/// `Math.min`: NaN when either is.
fn js_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() { f64::NAN } else { a.min(b) }
}

fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() { f64::NAN } else { a.max(b) }
}

/// `path.resolve(p)`'s base: a relative root is taken from the working directory.
fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf()) }
}

/// `path.resolve`'s normalizing: `.` dropped, `..` taking the part before it, without touching
/// the disk.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// readline's lines: split on `\n`, `\r\n` and a lone `\r`.
fn lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(['\r', '\n']) {
        out.push(&rest[..i]);
        let skip = if rest[i..].starts_with("\r\n") { 2 } else { 1 };
        rest = &rest[i + skip..];
    }
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// `jsonLines(file)`: each non-empty line that parses as an object or array; a missing file is
/// empty.
fn json_lines(file: &Path) -> Result<Vec<Value>, String> {
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    let text = String::from_utf8_lossy(&bytes);
    Ok(lines(&text)
        .into_iter()
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.is_object() || v.is_array())
        .collect())
}

/// `transcriptSession(file)`: the session a transcript's first line names (Codex:
/// `thread.started`'s `thread_id`; Claude: `session_id`), or `None` when it is missing, empty
/// or not JSON. The value is as written (only a string can name a session).
fn transcript_session(file: &Path) -> Result<Option<Value>, String> {
    let f = match std::fs::File::open(file) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    let mut first = Vec::new();
    BufReader::new(f).read_until(b'\n', &mut first).map_err(|e| format!("{}: {e}", file.display()))?;
    if first.is_empty() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&first);
    let line = lines(&text).into_iter().next().unwrap_or("");
    let Ok(event) = serde_json::from_str::<Value>(line.strip_prefix('\u{feff}').unwrap_or(line)) else { return Ok(None) };
    let id = if event.get("type").and_then(Value::as_str) == Some("thread.started") { event.get("thread_id") } else { event.get("session_id") };
    Ok(id.cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case Agent Pools' own `buildRoutingAttempts` answered (golden/routing-attempts.mjs):
    /// no root, an empty one, a fixture orchestrator with dispatches in and out of the window,
    /// unreadable and unnamed transcripts, and a directory with no dispatch log.
    #[test]
    fn each_case_is_agent_pools_report() {
        let golden = crate::testing::golden();
        let dir = golden.join("inputs/routing-attempts");
        let session_usage: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("session-usage.json")).unwrap()).unwrap();
        let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(golden.join("outputs/attempts/cases.json")).unwrap()).unwrap();
        assert_eq!(cases.len(), 4);
        let shown = dir.to_string_lossy().replace('/', "\\");
        for c in &cases {
            let root = c["root"].as_str().map(|r| if r.is_empty() { String::new() } else { format!("{shown}\\{r}") });
            let out = build(&session_usage, root.as_deref()).unwrap();
            let out: Value = serde_json::from_str(&js::stringify(&out).replace(&js::stringify(&json!(shown)).trim_matches('"').to_string(), "<dir>")).unwrap();
            assert_eq!(js::stringify(&out), js::stringify(&c["output"]), "{}", c["name"]);
        }
    }
}
