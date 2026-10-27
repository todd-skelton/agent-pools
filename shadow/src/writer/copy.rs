//! The copy the write side runs against, and the guard that keeps it off the live install.
//!
//! `make-copy` reads the live install (its state documents, the four install.json settings the
//! port reads, and the usage-sample history the forecast learns from) and writes them under a
//! directory of the port's own. No secret, auth file, proxy config or ledger is copied. Claude
//! accounts get a stand-in auth file holding only a made-up organization UUID, so the claim path
//! runs against the fake proxy.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::sources::Install;

/// A made-up organization for the stand-in Claude auth files (no real organization has it).
pub const FAKE_ORG: &str = "00000000-0000-4000-8000-0000c0ffee00";

/// Why a directory cannot be written as a copy.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `root` as a copy the writer may change: an existing directory that is not the live install,
/// not inside it and not holding it, and that has no `state/secrets.json`.
pub fn guard(root: &Path, live: &Path) -> Result<PathBuf, Refused> {
    let root = std::fs::canonicalize(root).map_err(|e| Refused(format!("{}: {e}", root.display())))?;
    let lower = |p: &Path| p.to_string_lossy().to_lowercase();
    if let Ok(live) = std::fs::canonicalize(live) {
        let (r, l) = (lower(&root), lower(&live));
        if r == l || r.starts_with(&(l.clone() + "\\")) || l.starts_with(&(r.clone() + "\\")) {
            return Err(Refused(format!("{} is the live install or overlaps it; the writer only runs on a copy", root.display())));
        }
    }
    if root.join("state").join("secrets.json").exists() {
        return Err(Refused(format!("{} holds state/secrets.json; a copy holds no secrets", root.display())));
    }
    Ok(root)
}

/// Copies what the write side reads from `live` into `dest` (created if missing). Returns the
/// files written.
pub fn make_copy(live: &Install, dest: &Path) -> std::io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dest)?;
    let dest = guard(dest, live.root()).map_err(|e| std::io::Error::other(e.0))?;
    let mut written = Vec::new();
    let mut put = |rel: &str, text: &str| -> std::io::Result<()> {
        let p = dest.join(rel);
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&p, text)?;
        written.push(p);
        Ok(())
    };
    // The price table is public list prices (the value report reads it), not a secret.
    for rel in ["state/pool-state.json", "state/capacity-forecast.json", "state/usage-value.json", "model-prices.json"] {
        if let Ok(text) = std::fs::read_to_string(live.root().join(rel)) {
            put(rel, &text)?;
        }
    }
    put("state/install.json", &serde_json::to_string_pretty(&live.settings()).unwrap_or_default())?;
    for rel in ["logs/usage-samples.jsonl.1", "logs/usage-samples.jsonl"] {
        if let Ok(text) = std::fs::read_to_string(live.root().join(rel)) {
            put(rel, &text)?;
        }
    }
    // The routing ladders' inputs (capacity-publisher.mjs readLadderInputs, model-calibrate.mjs
    // loadInputs) and the last ladders published.
    let mut ladder_inputs: Vec<String> = [
        "calibration-rules.json",
        "state/routing-policy.json",
        "state/model-registry.json",
        "state/benchmarks/latest.json",
        "state/benchmarks/latest-arena.json",
        "state/benchmarks/manual-scores.json",
        "state/routing-attempts.json",
        "state/session-usage.json",
        "state/routing-ladders.json",
    ]
    .map(String::from)
    .to_vec();
    for pointer in ["state/benchmarks/latest.json", "state/benchmarks/latest-arena.json"] {
        let file = live.json(pointer).and_then(|p| p.get("file").and_then(Value::as_str).map(String::from));
        if let Some(name) = file.as_deref().and_then(|f| Path::new(f).file_name()).and_then(|n| n.to_str()) {
            ladder_inputs.push(format!("state/benchmarks/{name}"));
        }
    }
    for rel in &ladder_inputs {
        if let Ok(text) = std::fs::read_to_string(live.root().join(rel)) {
            put(rel, &text)?;
        }
    }
    let state = live.json("state/pool-state.json").unwrap_or(Value::Null);
    for a in pool_core::js::values(state.get("accounts")) {
        let id = a.get("id").and_then(Value::as_str).unwrap_or_default();
        if a.get("provider").and_then(Value::as_str) == Some("claude") && is_file_name(id) {
            put(&format!("auth/{id}"), &json!({ "organization_uuid": FAKE_ORG, "note": "stand-in for the port's copy; not a credential" }).to_string())?;
        }
    }
    Ok(written)
}

/// A plain file name (no separators or parent references).
fn is_file_name(s: &str) -> bool {
    !s.is_empty() && !s.contains(['/', '\\', ':']) && s != "." && s != ".."
}
