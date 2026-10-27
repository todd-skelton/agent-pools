//! An install made from the golden inputs, in a directory of its own under the system's temp
//! directory, for the tests: the files the shadow reads, laid out as the live install has them.

use std::io::Read;
use std::path::PathBuf;

use serde_json::Value;

use crate::sources::Install;

pub fn golden() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../golden")
}

pub fn read(rel: &str) -> Value {
    let path = golden().join(rel);
    serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
    .unwrap()
}

fn gunzip(rel: &str) -> String {
    let bytes = std::fs::read(golden().join(rel)).unwrap();
    let mut text = String::new();
    flate2::read::GzDecoder::new(bytes.as_slice())
        .read_to_string(&mut text)
        .unwrap();
    text
}

/// A temp install, removed (its files, then its directories) when dropped.
pub struct TempInstall {
    pub dir: PathBuf,
    files: Vec<PathBuf>,
}

impl TempInstall {
    pub fn install(&self) -> Install {
        Install::new(&self.dir)
    }

    fn put(&mut self, rel: &str, text: &str) {
        let path = self.dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        self.files.push(path);
    }
}

impl Drop for TempInstall {
    fn drop(&mut self) {
        for f in &self.files {
            let _ = std::fs::remove_file(f);
        }
        for d in ["state", "logs/usage-ledger", "logs", ""] {
            let _ = std::fs::remove_dir(self.dir.join(d));
        }
    }
}

/// The snapshot's install: `state/` from the inputs (the live forecast and value report from the
/// golden outputs), with the logs ungzipped. `ledger` writes the usage ledger too (60 MB).
pub fn snapshot(name: &str, ledger: bool) -> TempInstall {
    let dir = std::env::temp_dir().join(format!("pools-shadow-{name}-{}", std::process::id()));
    let mut t = TempInstall {
        dir,
        files: Vec::new(),
    };
    let copy = |t: &mut TempInstall, from: &str, to: &str| {
        let text = std::fs::read_to_string(golden().join(from)).unwrap();
        t.put(to, &text);
    };
    copy(&mut t, "inputs/pool-state.json", "state/pool-state.json");
    copy(&mut t, "inputs/install.json", "state/install.json");
    copy(
        &mut t,
        "inputs/model-registry.json",
        "state/model-registry.json",
    );
    copy(&mut t, "inputs/model-prices.json", "model-prices.json");
    copy(
        &mut t,
        "outputs/forecast/now.json",
        "state/capacity-forecast.json",
    );
    copy(
        &mut t,
        "outputs/value/deployed.json",
        "state/usage-value.json",
    );
    // Split across the two logs, as the live install rotates them.
    let samples = gunzip("inputs/usage-samples.jsonl.gz");
    let lines: Vec<&str> = samples.lines().collect();
    let half = lines.len() / 2;
    t.put(
        "logs/usage-samples.jsonl.1",
        &(lines[..half].join("\n") + "\n"),
    );
    t.put(
        "logs/usage-samples.jsonl",
        &(lines[half..].join("\n") + "\n"),
    );
    if ledger {
        // One file in the analysis window: the rows in their recorded order.
        t.put(
            "logs/usage-ledger/raw-2026-10-20.jsonl",
            &gunzip("inputs/usage-ledger.jsonl.gz"),
        );
    }
    t
}
