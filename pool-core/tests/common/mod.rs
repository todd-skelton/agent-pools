//! Shared by the golden tests: the recorded inputs, the golden outputs, and an exact diff.
#![allow(dead_code)]

use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;

pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../golden")
}

fn read_json(path: PathBuf) -> Value {
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

/// A file under `golden/outputs`.
pub fn golden(rel: &str) -> Value {
    read_json(root().join("outputs").join(rel))
}

/// A file under `golden/inputs`.
pub fn input(rel: &str) -> Value {
    read_json(root().join("inputs").join(rel))
}

pub fn input_exists(rel: &str) -> bool {
    root().join("inputs").join(rel).exists()
}

/// A gzipped JSON-lines file under `golden/inputs`.
pub fn input_lines(rel: &str) -> Vec<Value> {
    let path = root().join("inputs").join(rel);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut text = String::new();
    flate2::read::GzDecoder::new(bytes.as_slice())
        .read_to_string(&mut text)
        .unwrap();
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Exact equality of parsed JSON, numbers as f64. Returns the first difference's path.
pub fn diff(path: &str, expected: &Value, actual: &Value) -> Option<String> {
    match (expected, actual) {
        (Value::Number(e), Value::Number(a)) => {
            (e.as_f64() != a.as_f64()).then(|| format!("{path}: expected {e}, got {a}"))
        }
        (Value::Object(e), Value::Object(a)) => {
            if e.contains_key("$number") || e.contains_key("$set") || e.contains_key("$map") {
                return Some(format!(
                    "{path}: tagged value {expected} needs explicit handling"
                ));
            }
            for k in e.keys().chain(a.keys()) {
                match (e.get(k), a.get(k)) {
                    (Some(x), Some(y)) => {
                        if let Some(d) = diff(&format!("{path}.{k}"), x, y) {
                            return Some(d);
                        }
                    }
                    (x, y) => return Some(format!("{path}.{k}: expected {x:?}, got {y:?}")),
                }
            }
            None
        }
        (Value::Array(e), Value::Array(a)) => {
            if e.len() != a.len() {
                return Some(format!(
                    "{path}: expected {} items, got {}",
                    e.len(),
                    a.len()
                ));
            }
            e.iter()
                .zip(a)
                .enumerate()
                .find_map(|(i, (x, y))| diff(&format!("{path}[{i}]"), x, y))
        }
        _ => (expected != actual).then(|| format!("{path}: expected {expected}, got {actual}")),
    }
}
