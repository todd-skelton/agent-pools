//! pool-service.mjs's value report schedule on the copy: `writeValueReport` (usage-value.mjs) two
//! minutes after the start, then every `reportIntervalMinutes` after the last one ends. Agent
//! Pools runs it as a child process so parsing weeks of rows never blocks its event loop; here it
//! is a blocking task. It writes `state/session-usage.json`, `state/routing-attempts.json` (the
//! join with the orchestrator's dispatch log, [`super::attempts`], from install.json's
//! `orchestratorRoot`) and `state/usage-value.json` atomically, in that order (one retry after
//! 250 ms, for a reader holding the file on Windows), and logs `usage-value-problem` when the
//! problem changes.

use std::path::Path;
use std::time::Duration;

use pool_core::js;
use pool_core::value::value_config;
use serde_json::Value;

use crate::parity::{ValueInputs, reports_at};
use crate::sources::Install;

pub const FIRST: Duration = Duration::from_secs(120);

/// `writeValueReport(root, now)`: builds the reports from the copy's files and writes them.
/// Returns the report's ledger row count, or why it could not be built or written.
#[cfg(test)]
pub fn write_value_report(root: &Path, now: f64) -> Result<f64, String> {
    write_report(root, now).map(|(rows, _, _)| rows)
}

/// The same, with the value report and the text written for it.
fn write_report(root: &Path, now: f64) -> Result<(f64, Value, String), String> {
    let install = Install::new(root);
    // usage-value.mjs reads the price table without a fallback: no table, no report.
    install.json("model-prices.json").ok_or("model-prices.json did not read")?;
    let pool = install.json("state/pool-state.json").unwrap_or(Value::Null);
    let forecast = install.json("state/capacity-forecast.json");
    let inp = ValueInputs {
        install: &install,
        pool: &pool,
        forecast: forecast.as_ref(),
        ledger_until: f64::INFINITY,
        samples_until: f64::INFINITY,
        ledger_rows: None,
    };
    let reports = reports_at(&inp, now);
    let orchestrator = install.json("state/install.json").and_then(|i| i.get("orchestratorRoot").and_then(Value::as_str).map(str::to_string));
    let attempts = super::attempts::build(&reports.session_usage, orchestrator.as_deref())?;
    write_atomic(&root.join("state/session-usage.json"), &reports.session_usage)?;
    write_atomic(&root.join("state/routing-attempts.json"), &attempts)?;
    let written = write_atomic(&root.join("state/usage-value.json"), &reports.value_report)?;
    let rows = js::finite(reports.value_report.pointer("/coverage/ledgerRows")).unwrap_or(0.0);
    Ok((rows, reports.value_report, written))
}

/// `writeAtomic(file, report)`: `JSON.stringify(report, null, 2)` to a temporary file, renamed
/// over the file; one retry. Returns the text written.
fn write_atomic(file: &Path, doc: &Value) -> Result<String, String> {
    let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
    let text = js::stringify_pretty(doc, 2);
    let attempt = || std::fs::write(&tmp, &text).and_then(|_| std::fs::rename(&tmp, file));
    attempt()
        .or_else(|_| {
            std::thread::sleep(Duration::from_millis(250));
            attempt()
        })
        .map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(text)
}

/// `scheduleValueReport`: the first report after `first`, then one `reportIntervalMinutes` (read
/// from install.json each time) after the last ends, until the app stops.
pub async fn run(io: std::sync::Arc<super::io::FileIo>, first: Duration, signal: corten::Signal) {
    let mut wait = first;
    let mut last_problem = String::new();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = signal.aborted() => return,
        }
        let root = io.root.clone();
        let built = tokio::task::spawn_blocking(move || write_report(&root, super::io::now_ms())).await;
        let problem = match built {
            Ok(Ok((rows, doc, written))) => {
                // `/api/value` and `/api/summary` answer from the report as written.
                io.memory.set_value(doc, written);
                println!("usage-value: {rows} ledger rows");
                String::new()
            }
            Ok(Err(e)) => format!("usage-value failed: {e}"),
            Err(e) => format!("usage-value failed: {e}"),
        };
        if problem != last_problem {
            if !problem.is_empty() {
                io.log_event("usage-value-problem", serde_json::json!({ "problem": problem }));
            }
            last_problem = problem;
        }
        let install = io.install();
        let (config, _) = value_config(install.as_ref().and_then(|i| i.get("usageLedger")));
        wait = Duration::from_secs_f64(config.report_interval_minutes * 60.0);
    }
}
