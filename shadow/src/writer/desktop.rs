//! The desktop relay on the copy (pool-service.mjs's desktop relay listener): Agent Pools'
//! backend relay (`pools_gateway::backend`) with the pool-wide usage overlay over the copy's own
//! accounts, in front of the stand-in's `/backend-api` (`fake_proxy`), never chatgpt.com. Plain
//! HTTP: the TLS pair in the live install's `state/certs` is not copied. The service in live mode
//! (`live::services`) serves the same relay with that pair (`start_with`).

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

/// The copy's accounts as the service holds them (`state.accounts.map(currentAccount)`).
fn accounts(io: &FileIo, now: f64) -> Vec<Value> {
    let state = io.memory.pool();
    js::values(state.get("accounts")).into_iter().map(|a| summary::current_account(a, now)).collect()
}

/// How long a busy port waits before the relay tries it again (pool-service.mjs: 30 s).
pub const RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// Serves the relay on `127.0.0.1:<port>` and `[::1]:<port>`, as the desktop relay listens on
/// both, in front of `upstream` (the stand-in's `/backend-api`). Returns the port. As
/// pool-service.mjs's `listenDesktopRelay`: an address whose port is taken is said once
/// (`desktop-relay-error` and a toast) and tried again every `retry` until it binds
/// (`desktop-relay-listening`); `[::1]` on a system without IPv6 is said once and left. Port 0
/// (tests) binds `127.0.0.1` at once and serves that only.
pub async fn start(io: Arc<FileIo>, upstream: &str, port: u16, retry: std::time::Duration) -> std::io::Result<u16> {
    start_with(io, upstream, port, retry, None).await
}

/// One listener of the relay's, behind `tls` when there is a pair.
async fn serve_relay(listener: tokio::net::TcpListener, tls: Option<tokio_rustls::TlsAcceptor>, app: corten::App<pools_gateway::backend::Hosts>) -> std::io::Result<()> {
    match tls {
        Some(acceptor) => corten::serve_on(vec![crate::live::tls::listener(listener, acceptor)?], app).await,
        None => corten::serve_on(vec![listener], app).await,
    }
}

/// [`start`], over TLS with `tls` (the pair in `state/certs`, as pool-service.mjs serves it with
/// `https.createServer`); without it plain HTTP, said once (`desktop-relay-tls-missing`).
pub async fn start_with(io: Arc<FileIo>, upstream: &str, port: u16, retry: std::time::Duration, tls: Option<tokio_rustls::TlsAcceptor>) -> std::io::Result<u16> {
    let scheme = if tls.is_some() { "https" } else { "http" };
    if tls.is_none() {
        io.log_event("desktop-relay-tls-missing", serde_json::json!({ "certDir": "state/certs", "origin": format!("http://localhost:{port}") }));
    }
    let app = |port: u16| -> std::io::Result<corten::App<pools_gateway::backend::Hosts>> { relay_app(io.clone(), upstream, port) };
    if port == 0 {
        let v4 = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = v4.local_addr()?.port();
        let app = app(port)?;
        tokio::spawn(async move {
            if let Err(e) = serve_relay(v4, tls, app).await {
                eprintln!("copy: the desktop relay stopped: {e}");
            }
        });
        return Ok(port);
    }
    for host in ["127.0.0.1", "::1"] {
        let (io, app, tls) = (io.clone(), app(port)?, tls.clone());
        tokio::spawn(async move {
            let mut down = false;
            loop {
                match tokio::net::TcpListener::bind((host, port)).await {
                    Ok(listener) => {
                        if down {
                            io.log_event("desktop-relay-listening", serde_json::json!({ "host": host, "port": port, "tls": tls.is_some() }));
                        }
                        println!("desktop relay: {scheme}://localhost:{port} on {host}");
                        if let Err(e) = serve_relay(listener, tls, app).await {
                            eprintln!("copy: the desktop relay on {host} stopped: {e}");
                        }
                        return;
                    }
                    Err(e) if host == "::1" && matches!(e.raw_os_error(), Some(10049 | 10047 | 99 | 97)) => {
                        // WSAEADDRNOTAVAIL / WSAEAFNOSUPPORT (EADDRNOTAVAIL / EAFNOSUPPORT on Unix).
                        io.log_event("desktop-relay-ipv6-unavailable", serde_json::json!({ "code": e.raw_os_error(), "message": e.to_string() }));
                        return;
                    }
                    Err(e) => {
                        if !down {
                            down = true;
                            io.log_event("desktop-relay-error", serde_json::json!({ "host": host, "code": e.raw_os_error(), "message": e.to_string() }));
                            let body = format!("Port {port} on {host} is taken ({e}). Codex desktop usage and Remote Control will fail or slow down until it is free; retrying every 30 s.");
                            let _ = pool_core::refresh::Io::toast(&*io, "Agent Pools: desktop relay port busy", &body).await;
                        }
                        tokio::time::sleep(retry).await;
                    }
                }
            }
        });
    }
    Ok(port)
}

/// The relay's app for `port`, with the usage overlay over the copy's accounts.
fn relay_app(io: Arc<FileIo>, upstream: &str, port: u16) -> std::io::Result<corten::App<pools_gateway::backend::Hosts>> {
    let mut config = pools_gateway::backend::Config::new(upstream);
    let (rewrite_io, event_io) = (io.clone(), io);
    config.rewrite_usage = Some(Arc::new(move |usage: Value| {
        let now = crate::watch::now_ms();
        // `install.poolUsageMode` as read at the start.
        let mode = mode(Some(rewrite_io.memory.install.as_ref()));
        let accounts = accounts(&rewrite_io, now);
        Ok(pool_usage::pool_usage(&usage, &accounts, mode, now, true)?.unwrap_or(usage))
    }));
    config.observe = Arc::new(move |kind: &str, details: Value| event_io.log_event(kind, details));
    pools_gateway::backend::app(config, port).map_err(std::io::Error::other)
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
