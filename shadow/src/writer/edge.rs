//! The copy's two other listeners, in front of the stand-in proxy: the inference relay
//! (inference-service.mjs: `pools_gateway::inference`, Agent Pools' port 8319) and the public
//! gateway (public-gateway-service.mjs: `pools_gateway`, Agent Pools' port 8330 behind the
//! Cloudflare tunnel). Both are plain HTTP on loopback; the live install's certificates, the
//! desktop's `~/.codex/auth.json`, `state/secrets.json` and `state/remote-clients.json` are never
//! read. Every key is made up for the run and written under the copy's `stand-in/`:
//!
//! | File | Holds |
//! |---|---|
//! | `stand-in/codex-auth.json` | the sign-in the relay accepts (`auth.json`'s shape, `apikey` mode) |
//! | `stand-in/pool-keys.json` | the pool's client key the relay sends (`secrets.json`'s `clientKey`) |
//! | `stand-in/keys.json` | for a test client: that sign-in, a registered machine's key, the gateway's host name |
//!
//! The relay reads the first two on every admission, through Agent Pools' own
//! `fileCredentials` port. The gateway's one registered machine is `copy-test`.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};

use super::io::FileIo;

/// The gateway's public name on a copy; requests must name it in `Host`.
pub const GATEWAY_HOST: &str = "pool-copy.localhost";

/// The made-up keys of one run.
#[derive(Clone)]
pub struct Keys {
    /// What the stand-in accepts for inference: the pool's `clientKey` / `remoteDesktopKey`.
    pub client_key: String,
    /// What the desktop presents to the relay (its `auth.json` key).
    pub desktop_token: String,
    /// The registered machine's key for the gateway.
    pub machine_key: String,
    pub session_secret: String,
}

impl Keys {
    pub fn new() -> Keys {
        let k = |prefix: &str| format!("{prefix}-{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        Keys { client_key: k("pool"), desktop_token: k("desktop"), machine_key: k("machine"), session_secret: k("session") }
    }

    /// Writes the files the relay reads and a test client uses.
    pub fn write(&self, root: &Path) -> std::io::Result<()> {
        let dir = root.join("stand-in");
        std::fs::create_dir_all(&dir)?;
        let put = |name: &str, v: Value| std::fs::write(dir.join(name), serde_json::to_string_pretty(&v).unwrap_or_default());
        put("codex-auth.json", json!({ "auth_mode": "apikey", "OPENAI_API_KEY": self.desktop_token, "note": "made up for this run of the copy" }))?;
        put("pool-keys.json", json!({ "clientKey": self.client_key }))?;
        put("keys.json", json!({ "desktopToken": self.desktop_token, "machineKey": self.machine_key, "gatewayHost": GATEWAY_HOST, "machine": "copy-test" }))
    }
}

impl Default for Keys {
    fn default() -> Keys {
        Keys::new()
    }
}

async fn bind(port: u16) -> std::io::Result<(tokio::net::TcpListener, u16)> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

/// The inference relay on `127.0.0.1:<port>` in front of the stand-in's `/v1`. Returns its port.
pub async fn start_inference(io: Arc<FileIo>, proxy: &str, port: u16) -> std::io::Result<u16> {
    let (listener, port) = bind(port).await?;
    let dir = io.root.join("stand-in");
    let authorize = pools_gateway::inference::file_credentials(dir.join("codex-auth.json"), dir.join("pool-keys.json"));
    let mut config = pools_gateway::inference::Config::new(&format!("{}/v1", proxy.trim_end_matches('/')), authorize);
    config.observe = Arc::new(move |kind: &str, details: Value| io.log_to("logs/inference-events.jsonl", kind, details));
    let (app, _) = pools_gateway::inference::app(config, port).map_err(std::io::Error::other)?;
    tokio::spawn(async move {
        if let Err(e) = corten::serve_on(vec![listener], app).await {
            eprintln!("copy: the inference relay stopped: {e}");
        }
    });
    Ok(port)
}

/// The public gateway on `127.0.0.1:<port>`: inference to the stand-in with the pool's client key,
/// the dashboard and API from the copy's own service at `service`. Returns its port.
pub async fn start_gateway(io: Arc<FileIo>, keys: &Keys, proxy: &str, service: &str, port: u16) -> std::io::Result<u16> {
    let (listener, port) = bind(port).await?;
    let mut config = pools_gateway::Config::new(GATEWAY_HOST, proxy, service);
    let machine = pools_gateway::Client { name: "copy-test".into(), key_sha256: pools_gateway::sha256(&keys.machine_key) };
    config.clients = Arc::new(move || vec![machine.clone()]);
    let (pool_key, secret) = (keys.client_key.clone(), keys.session_secret.clone());
    config.pool_key = Arc::new(move || Some(pool_key.clone()));
    config.session_secret = Arc::new(move || Ok(secret.clone()));
    config.observe = Arc::new(move |kind: &str, details: Value| io.log_to("logs/public-gateway-events.jsonl", kind, details));
    let app = pools_gateway::app(config).map_err(std::io::Error::other)?;
    tokio::spawn(async move {
        if let Err(e) = corten::serve_on(vec![listener], app).await {
            eprintln!("copy: the public gateway stopped: {e}");
        }
    });
    Ok(port)
}
