//! The Agent Pools shadow: the dashboard and `/api/summary` on Corten, built by `pool-core`
//! from the live install's files, beside the live service and never writing to it, with a
//! running parity check against what the live service publishes.
//!
//! Settings (environment):
//!
//! | Variable | Default | |
//! |---|---|---|
//! | `AGENT_POOLS_ROOT` | `D:\Users\ToddS\AgentPools` | the install, read only |
//! | `AGENT_POOLS_URL` | `http://127.0.0.1:8318` | the live service, for `GET /api/summary` |
//! | `SHADOW_STATE` | `shadow-state` next to the workspace | where `parity.jsonl` is appended |
//! | `SHADOW_WATCH_SECS` | 5 | how often the install's files are looked at |
//! | `SHADOW_SUMMARY_SECS` | 60 | how often the live summary is compared |
//! | `STATIC_DIR` | `shadow/static` | the stylesheet |
//! | `PORT`, `HOST` | 8320, loopback | Corten's own |

mod app;
mod parity;
mod sources;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;
mod watch;

use std::path::PathBuf;
use std::time::Duration;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.into())
}

fn secs(key: &str, default: u64) -> Duration {
    Duration::from_secs(
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|s| *s > 0)
            .unwrap_or(default),
    )
}

fn main() -> std::io::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("check-value") => return check_value_once(),
        Some("check-status") => {
            return tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(check_status_once());
        }
        _ => {}
    }
    if std::env::var_os("PORT").is_none() {
        // corten::serve takes its port from PORT only (corten#115: no listener of the app's own).
        // SAFETY: the process has one thread here; the runtime is built after.
        unsafe { std::env::set_var("PORT", "8320") };
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve())
}

async fn serve() -> std::io::Result<()> {
    let install = sources::Install::new(env_or("AGENT_POOLS_ROOT", r"D:\Users\ToddS\AgentPools"));
    let state_dir = std::env::var_os("SHADOW_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../shadow-state"));
    let shadow = watch::Shadow::new(
        install,
        env_or("AGENT_POOLS_URL", "http://127.0.0.1:8318"),
        state_dir.join("parity.jsonl"),
    );
    println!(
        "shadow: reading {} (read only), comparing with {}",
        shadow.install.root().display(),
        shadow.live_url
    );
    let assets = std::env::var_os("STATIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static"));
    corten::serve(app::app(
        shadow,
        assets,
        secs("SHADOW_WATCH_SECS", 5),
        secs("SHADOW_SUMMARY_SECS", 60),
    ))
    .await
}

/// `pools-shadow check-value`: rebuilds the live `state/usage-value.json` once at its
/// `generatedAt`, with the pool state and forecast on disk now (the ones it read are gone, so
/// account rows may differ), and prints the check. Reads only.
fn check_value_once() -> std::io::Result<()> {
    let install = sources::Install::new(env_or("AGENT_POOLS_ROOT", r"D:\Users\ToddS\AgentPools"));
    let live = install
        .json("state/usage-value.json")
        .ok_or_else(|| std::io::Error::other("state/usage-value.json did not read"))?;
    let pool = install.json("state/pool-state.json").unwrap_or_default();
    let forecast = install.json("state/capacity-forecast.json");
    let written = install
        .modified("state/usage-value.json")
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(f64::INFINITY, |d| d.as_millis() as f64);
    let at = pool_core::js::date_parse_value(live.get("generatedAt"));
    // SHADOW_LEDGER_UNTIL (ISO time): another cut-off for ledger rows, to bracket the moment the
    // live report stopped reading the ledger.
    let written = std::env::var("SHADOW_LEDGER_UNTIL")
        .ok()
        .and_then(|s| pool_core::js::date_parse(&s))
        .unwrap_or(written);
    let inp = parity::ValueInputs {
        install: &install,
        pool: &pool,
        forecast: forecast.as_ref(),
        ledger_until: written,
        samples_until: at + 1000.0,
        ledger_rows: None,
    };
    let check = parity::check_value(&live, &inp, watch::now_ms());
    println!(
        "{}",
        serde_json::to_string_pretty(&check).unwrap_or_default()
    );
    Ok(())
}

/// `pools-shadow check-status`: asks the live service for `/api/status` once and checks it.
async fn check_status_once() -> std::io::Result<()> {
    let install = sources::Install::new(env_or("AGENT_POOLS_ROOT", r"D:\Users\ToddS\AgentPools"));
    let url = format!(
        "{}/api/status",
        env_or("AGENT_POOLS_URL", "http://127.0.0.1:8318")
    );
    let live: serde_json::Value = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(std::io::Error::other)?
        .get(url)
        .send()
        .await
        .map_err(std::io::Error::other)?
        .json()
        .await
        .map_err(std::io::Error::other)?;
    let at = watch::now_ms();
    let check = parity::check_status(&live, &install, at, watch::now_ms());
    println!(
        "{}",
        serde_json::to_string_pretty(&check).unwrap_or_default()
    );
    Ok(())
}
