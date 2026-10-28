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
    apart(&root, live)?;
    if root.join("state").join("secrets.json").exists() {
        return Err(Refused(format!("{} holds state/secrets.json; a copy holds no secrets", root.display())));
    }
    Ok(root)
}

/// Refused when `root` (canonical) is the live install, inside it or holding it.
fn apart(root: &Path, live: &Path) -> Result<(), Refused> {
    let lower = |p: &Path| p.to_string_lossy().to_lowercase();
    if let Ok(live) = std::fs::canonicalize(live) {
        let (r, l) = (lower(root), lower(&live));
        if r == l || r.starts_with(&(l.clone() + "\\")) || l.starts_with(&(r.clone() + "\\")) {
            return Err(Refused(format!("{} is the live install or overlaps it; the writer only runs on a copy", root.display())));
        }
    }
    Ok(())
}

/// `dest` as it will be once created: the canonical path of its nearest existing ancestor, with
/// the rest of `dest` under it.
fn planned(dest: &Path) -> std::io::Result<PathBuf> {
    let dest = std::path::absolute(dest)?;
    let mut rest = Vec::new();
    let mut at = dest.as_path();
    loop {
        match std::fs::canonicalize(at) {
            Ok(mut found) => {
                found.extend(rest.iter().rev());
                return Ok(found);
            }
            Err(_) => {
                let (Some(parent), Some(name)) = (at.parent(), at.file_name()) else {
                    return Err(std::io::Error::other(format!("{}: no existing ancestor", dest.display())));
                };
                rest.push(name.to_owned());
                at = parent;
            }
        }
    }
}

/// Copies what the write side reads from `live` into `dest` (created if missing). Returns the
/// files written. A `dest` that is or would be the live install, inside it or holding it is
/// refused before anything is created.
pub fn make_copy(live: &Install, dest: &Path) -> std::io::Result<Vec<PathBuf>> {
    apart(&planned(dest)?, live.root()).map_err(|e| std::io::Error::other(e.0))?;
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
        // The model refresh's inputs (model-refresh.mjs): the families, the proxy's live model
        // lists, the lineage, the admission evidence and the calibration preview's pointer.
        "model-families.json",
        "state/live-models.json",
        "state/model-lineage.json",
        "state/model-admission.jsonl",
        "state/calibration/latest.json",
    ]
    .map(String::from)
    .to_vec();
    for (pointer, dir) in [("state/benchmarks/latest.json", "state/benchmarks"), ("state/benchmarks/latest-arena.json", "state/benchmarks"), ("state/calibration/latest.json", "state/calibration")] {
        let file = live.json(pointer).and_then(|p| p.get("file").and_then(Value::as_str).map(String::from));
        if let Some(name) = file.as_deref().and_then(|f| Path::new(f).file_name()).and_then(|n| n.to_str()) {
            ladder_inputs.push(format!("{dir}/{name}"));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the system's temp directory.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pools-copy-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// Every file under `dir`, relative to it.
    fn files(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut todo = vec![dir.to_path_buf()];
        while let Some(d) = todo.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                if e.file_type().unwrap().is_dir() {
                    todo.push(e.path());
                } else {
                    out.push(e.path().strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/"));
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn the_guard_refuses_the_live_install_anything_inside_it_and_anything_holding_it() {
        let base = scratch("guard");
        let live = base.join("live");
        std::fs::create_dir_all(live.join("state")).unwrap();
        let sibling = base.join("copy");
        std::fs::create_dir_all(&sibling).unwrap();

        assert!(guard(&live, &live).is_err(), "the live install itself");
        assert!(guard(&live.join("state"), &live).is_err(), "inside it");
        assert!(guard(&base, &live).is_err(), "holding it");
        let shouted = PathBuf::from(live.to_string_lossy().to_uppercase());
        assert!(guard(&shouted, &live).is_err(), "the same directory in other letters");
        assert!(guard(&base.join("missing"), &live).is_err(), "a directory that is not there");
        assert_eq!(guard(&sibling, &live).unwrap(), std::fs::canonicalize(&sibling).unwrap());
    }

    #[test]
    fn the_guard_refuses_a_directory_holding_secrets() {
        let base = scratch("secrets");
        let live = base.join("live");
        std::fs::create_dir_all(&live).unwrap();
        let copy = base.join("copy");
        put(&copy.join("state/secrets.json"), "{}");
        let refused = guard(&copy, &live).unwrap_err();
        assert!(refused.0.contains("secrets.json"), "{refused}");
    }

    #[test]
    fn a_copy_carries_no_secret_auth_file_or_client_list_and_is_never_made_inside_the_live_install() {
        let base = scratch("make");
        let live = base.join("live");
        let secret = "REAL-SECRET-0123456789";
        put(&live.join("state/secrets.json"), &format!(r#"{{"poolKey":"{secret}"}}"#));
        put(&live.join("state/remote-clients.json"), &format!(r#"{{"clients":[{{"hash":"{secret}"}}]}}"#));
        put(&live.join("auth/claude-1"), &format!(r#"{{"access_token":"{secret}","organization_uuid":"real-org"}}"#));
        put(&live.join("auth/codex-1"), &format!(r#"{{"tokens":{{"access_token":"{secret}"}}}}"#));
        put(&live.join("logs/usage-ledger/raw-2026-10-27.jsonl"), "{}
");
        put(&live.join("state/install.json"), &format!(r#"{{"expectedAccounts":2,"remoteDesktopKey":"{secret}"}}"#));
        put(
            &live.join("state/pool-state.json"),
            r#"{"accounts":[{"id":"claude-1","provider":"claude"},{"id":"codex-1","provider":"codex"},{"id":"..\\escape","provider":"claude"}]}"#,
        );
        let install = Install::new(&live);

        let inside = live.join("copies/one");
        assert!(make_copy(&install, &inside).is_err());
        assert!(!live.join("copies").exists(), "nothing is created inside the live install");
        assert!(make_copy(&install, &base).is_err(), "nor in a directory holding it");

        let copy = base.join("copy");
        make_copy(&install, &copy).unwrap();
        let made = files(&copy);
        assert_eq!(made, ["auth/claude-1", "state/install.json", "state/pool-state.json"], "{made:?}");
        for rel in &made {
            let text = std::fs::read_to_string(copy.join(rel)).unwrap();
            assert!(!text.contains(secret), "{rel} carries a secret");
        }
        let stand_in: Value = serde_json::from_str(&std::fs::read_to_string(copy.join("auth/claude-1")).unwrap()).unwrap();
        assert_eq!(stand_in["organization_uuid"], FAKE_ORG);
        assert!(guard(&copy, &live).is_ok(), "the copy is one run-copy accepts");
    }
}
