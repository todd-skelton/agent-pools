//! The desktop relay on the copy (pool-service.mjs's desktop relay listener): Agent Pools'
//! backend relay (`pools_gateway::backend`) with the pool-wide usage overlay over the copy's own
//! accounts, in front of the stand-in's `/backend-api` (`fake_proxy`), never chatgpt.com. Plain
//! HTTP: the TLS pair in the live install's `state/certs` is not copied.

use std::sync::Arc;

use pool_core::{js, pool_usage, summary};
use serde_json::Value;

use super::io::FileIo;

/// `poolUsageMode` from the copy's install.json when it is one of the modes, else the first
/// (`capped`), as pool-service.mjs chooses it. make-copy does not copy that key, so a copy runs
/// `capped` unless its install.json is edited.
pub fn mode(install: Option<&Value>) -> &'static str {
    let named = install.and_then(|i| i.get("poolUsageMode")).and_then(Value::as_str);
    pool_usage::POOL_MODES.iter().copied().find(|m| Some(*m) == named).unwrap_or(pool_usage::POOL_MODES[0])
}

/// The copy's accounts as the service holds them, read from its latest save.
fn accounts(io: &FileIo, now: f64) -> Result<Vec<Value>, String> {
    let text = std::fs::read_to_string(io.root.join("state/pool-state.json")).map_err(|e| e.to_string())?;
    let state: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())?;
    Ok(js::values(state.get("accounts")).into_iter().map(|a| summary::current_account(a, now)).collect())
}

/// Serves the relay on `127.0.0.1:<port>` (and `[::1]` when the system has it, as the desktop
/// relay listens on both), in front of `upstream` (the stand-in's `/backend-api`). Returns the
/// port it got.
pub async fn start(io: Arc<FileIo>, upstream: &str, port: u16) -> std::io::Result<u16> {
    let v4 = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let port = v4.local_addr()?.port();
    let mut listeners = vec![v4];
    if let Ok(v6) = tokio::net::TcpListener::bind(("::1", port)).await {
        listeners.push(v6);
    }
    let mut config = pools_gateway::backend::Config::new(upstream);
    let (rewrite_io, event_io) = (io.clone(), io);
    config.rewrite_usage = Some(Arc::new(move |usage: Value| {
        let now = crate::watch::now_ms();
        let mode = mode(rewrite_io.install().as_ref());
        let accounts = accounts(&rewrite_io, now)?;
        Ok(pool_usage::pool_usage(&usage, &accounts, mode, now, true)?.unwrap_or(usage))
    }));
    config.observe = Arc::new(move |kind: &str, details: Value| event_io.log_event(kind, details));
    let app = pools_gateway::backend::app(config, port).map_err(std::io::Error::other)?;
    tokio::spawn(async move {
        if let Err(e) = corten::serve_on(listeners, app).await {
            eprintln!("copy: the desktop relay stopped: {e}");
        }
    });
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_mode_is_the_installs_when_known_else_capped() {
        assert_eq!(mode(None), "capped");
        assert_eq!(mode(Some(&json!({ "poolUsageMode": "average" }))), "average");
        assert_eq!(mode(Some(&json!({ "poolUsageMode": "max" }))), "capped");
        assert_eq!(mode(Some(&json!({ "poolUsageMode": 3 }))), "capped");
    }
}
