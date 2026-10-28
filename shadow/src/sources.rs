//! Read-only access to the live Agent Pools install: its `state/` documents, the usage sample
//! logs and the usage ledger. Nothing here writes. Files are opened as `std` opens them on
//! Windows (sharing read, write and delete), so a read in progress never makes the service's
//! write-then-rename fail.

use pool_core::{js, value::LedgerRow};
use serde_json::{Map, Value};
use std::io::{BufRead, BufReader};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// The install.json settings the port reads. Nothing else in that file (keys, paths) is read
/// into the shadow.
pub const INSTALL_KEYS: [&str; 4] = [
    "capacityForecast",
    "resetPolicy",
    "expectedAccounts",
    "usageLedger",
];
pub const DAY: f64 = 86_400_000.0;

/// A parsed document and the file it was parsed from (its modified time and length).
type Parsed = (SystemTime, u64, Arc<Value>);

#[derive(Clone, Debug)]
pub struct Install {
    root: PathBuf,
    /// Each JSON document as last parsed, shared by every clone: a document is parsed again only
    /// when its file changes (a save is a write and a rename, so a new modified time).
    parsed: Arc<Mutex<HashMap<String, Parsed>>>,
}

impl Install {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Install { root: root.into(), parsed: Arc::default() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// A JSON document (a leading byte-order mark allowed, as the service's `readJSON` allows).
    pub fn json(&self, rel: &str) -> Option<Value> {
        self.doc(rel).map(|d| (*d).clone())
    }

    /// The same, shared: from memory while the file is unchanged since it was last parsed.
    pub fn doc(&self, rel: &str) -> Option<Arc<Value>> {
        let path = self.path(rel);
        let stamp = std::fs::metadata(&path).ok().and_then(|m| Some((m.modified().ok()?, m.len())));
        if let Some((at, len)) = stamp
            && let Some((a, l, d)) = self.parsed.lock().unwrap_or_else(|e| e.into_inner()).get(rel)
            && *a == at
            && *l == len
        {
            return Some(d.clone());
        }
        let text = std::fs::read_to_string(&path).ok()?;
        let doc: Value = serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(&text)).ok()?;
        let doc = Arc::new(doc);
        let mut parsed = self.parsed.lock().unwrap_or_else(|e| e.into_inner());
        match stamp {
            // Kept only when the file did not change while it was read.
            Some((at, len)) if std::fs::metadata(&path).ok().and_then(|m| Some((m.modified().ok()?, m.len()))) == Some((at, len)) => {
                parsed.insert(rel.to_string(), (at, len, doc.clone()));
            }
            _ => {
                parsed.remove(rel);
            }
        }
        Some(doc)
    }

    pub fn modified(&self, rel: &str) -> Option<SystemTime> {
        std::fs::metadata(self.path(rel))
            .and_then(|m| m.modified())
            .ok()
    }

    /// install.json reduced to [`INSTALL_KEYS`].
    pub fn settings(&self) -> Value {
        let mut out = Map::new();
        if let Some(Value::Object(m)) = self.doc("state/install.json").as_deref() {
            for k in INSTALL_KEYS {
                if let Some(v) = m.get(k) {
                    out.insert(k.into(), v.clone());
                }
            }
        }
        Value::Object(out)
    }

    /// Usage samples with `from <= at <= to`, from `logs/usage-samples.jsonl.1` then
    /// `logs/usage-samples.jsonl`, in time order (stable, as the publisher sorts them).
    pub fn samples(&self, from: f64, to: f64) -> Vec<Value> {
        let mut out: Vec<(f64, Value)> = Vec::new();
        for rel in ["logs/usage-samples.jsonl.1", "logs/usage-samples.jsonl"] {
            for v in json_lines(&self.path(rel)) {
                let at = js::date_parse_value(v.get("at"));
                if at >= from && at <= to {
                    out.push((at, v));
                }
            }
        }
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        out.into_iter().map(|(_, v)| v).collect()
    }

    /// Ledger rows from `logs/usage-ledger/raw-YYYY-MM-DD.jsonl` for days from `first_day` on, in
    /// file-name order, as `writeValueReport` streams them. Rows dated after `until` are left out
    /// (they were written after the report being reproduced).
    pub fn ledger(&self, first_day: &str, until: f64) -> impl Iterator<Item = LedgerRow> + use<> {
        let dir = self.path("logs/usage-ledger");
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .map(|d| {
                d.filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .filter(|n| ledger_day(n).is_some_and(|day| day >= first_day))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
            .into_iter()
            .flat_map(move |n| json_lines(&dir.join(n)))
            .map(|v| LedgerRow::from_value(&v))
            // A row with no readable time is kept: the report counts and skips it itself.
            .filter(move |r| r.at.partial_cmp(&until) != Some(std::cmp::Ordering::Greater))
    }
}

/// `LEDGER_FILE` (`/^raw-(\d{4}-\d{2}-\d{2})\.jsonl$/`): the day a ledger file holds.
fn ledger_day(name: &str) -> Option<&str> {
    let day = name.strip_prefix("raw-")?.strip_suffix(".jsonl")?;
    let b = day.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    (b.len() == 10 && digits(0..4) && b[4] == b'-' && digits(5..7) && b[7] == b'-' && digits(8..10))
        .then_some(day)
}

/// The objects in a JSON-lines file, skipping lines that do not parse (a line being appended).
fn json_lines(path: &Path) -> impl Iterator<Item = Value> + use<> {
    let lines = std::fs::File::open(path)
        .ok()
        .map(|f| BufReader::new(f).lines());
    lines
        .into_iter()
        .flatten()
        .map_while(Result::ok)
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .filter(|v| v.is_object() || v.is_array())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_file_names() {
        assert_eq!(ledger_day("raw-2026-10-27.jsonl"), Some("2026-10-27"));
        assert_eq!(ledger_day("raw-2026-10-27.jsonl.tmp"), None);
        assert_eq!(ledger_day("raw-2026-1-27.jsonl"), None);
        assert_eq!(ledger_day("summary-2026-10-27.jsonl"), None);
    }
}
