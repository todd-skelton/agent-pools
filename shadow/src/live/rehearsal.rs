//! `pools-shadow make-rehearsal <dest>`: an install the live commands may run on before cutover.
//! It is a copy (`make-copy`: the state documents, the four install.json settings, the usage
//! samples, the routing inputs) plus what the live commands need and the copy leaves out, every
//! piece of it made up here:
//!
//! | File | |
//! |---|---|
//! | `state/rehearsal.json` | the marker `Install::open` requires of any root but the live one |
//! | `state/secrets.json` | `managementKey`, `clientKey` (also `remoteDesktopKey`: the stand-in takes one key), `publicGatewaySessionSecret` |
//! | `state/install.json` | the copy's settings, ports clear of the live install's and the copy's ([`PORTS`]), `proxyBinary` the stand-in, `publicGateway` an `.invalid` host |
//! | `proxy.yaml` | the stand-in's port |
//! | `proxy/0.0.0-standin/cli-proxy-api.exe` | this program, which runs as the stand-in proxy when started as the proxy is (`-config <root>\proxy.yaml`) |
//! | `state/certs/desktop-relay.{crt,key}` | a self-signed pair for `localhost` and `127.0.0.1`, in no certificate store |
//! | `state/remote-clients.json` | one machine, `rehearsal`, by its key's hash |
//! | `stand-in/codex-auth.json` | the desktop sign-in the inference relay checks |
//! | `stand-in/keys.json` | the plain keys, for a smoke test to present |
//!
//! Nothing is read from the live install but what make-copy reads; no secret of it is read.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::MARKER;

/// The rehearsal's ports: proxy, service, inference relay, desktop relay, gateway. The live
/// install's are 8317, 8318, 8319, 8000 and 8330 (the tailnet proxy 8327); the copy's 8320-8324.
pub const PORTS: [u16; 5] = [8340, 8341, 8342, 8343, 8344];
/// The stand-in's place, as `<root>\proxy\<version>\cli-proxy-api.exe` (`ownedProxy`).
pub const STAND_IN: &str = "proxy/0.0.0-standin/cli-proxy-api.exe";
pub const GATEWAY_HOST: &str = "pool.rehearsal.invalid";

fn made_up(kind: &str) -> String {
    format!("rehearsal-{kind}-{}", uuid::Uuid::new_v4().simple())
}

/// A self-signed pair for the relays (PEM certificate, PEM key).
pub fn pair() -> std::io::Result<(String, String)> {
    let names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let cert = rcgen::generate_simple_self_signed(names).map_err(|e| std::io::Error::other(format!("rehearsal certificate: {e}")))?;
    Ok((cert.cert.pem(), cert.key_pair.serialize_pem()))
}

/// Makes the rehearsal at `dest` from `live` (read as make-copy reads it), with the stand-in
/// executable `exe`. Returns the files written.
pub fn make(live: &crate::sources::Install, dest: &Path, exe: &Path) -> std::io::Result<Vec<PathBuf>> {
    // make_copy refuses a dest that is or overlaps the live install, or holds secrets (so an
    // existing rehearsal is not made over).
    let mut written = crate::writer::copy::make_copy(live, dest)?;
    let dest = std::fs::canonicalize(dest)?;
    let mut put = |rel: &str, bytes: &[u8]| -> std::io::Result<()> {
        let p = dest.join(rel);
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&p, bytes)?;
        written.push(p);
        Ok(())
    };
    let [proxy, service, inference, desktop, gateway] = PORTS;
    let mut install = live.settings();
    for (k, v) in [
        ("proxyPort", json!(proxy)),
        ("supervisorPort", json!(service)),
        ("inferenceRelayPort", json!(inference)),
        ("desktopRelayPort", json!(desktop)),
        ("proxyBinary", json!(STAND_IN)),
        ("publicGateway", json!({ "hostname": GATEWAY_HOST, "port": gateway, "tunnel": null })),
    ] {
        install[k] = v;
    }
    put("state/install.json", serde_json::to_string_pretty(&install).unwrap_or_default().as_bytes())?;
    let (client, management, session, machine, desktop_token) = (made_up("client"), made_up("management"), made_up("session"), made_up("machine"), made_up("desktop"));
    put(
        "state/secrets.json",
        serde_json::to_string_pretty(&json!({
            "managementKey": management,
            "clientKey": client,
            "remoteDesktopKey": client,
            "publicGatewaySessionSecret": session,
        }))
        .unwrap_or_default()
        .as_bytes(),
    )?;
    put("state/remote-clients.json", json!({ "clients": [{ "name": "rehearsal", "keySha256": pools_gateway::sha256(&machine) }] }).to_string().as_bytes())?;
    put("stand-in/codex-auth.json", json!({ "auth_mode": "chatgpt", "tokens": { "access_token": desktop_token } }).to_string().as_bytes())?;
    put(
        "stand-in/keys.json",
        serde_json::to_string_pretty(&json!({
            "note": "made up for this rehearsal; none is a credential",
            "managementKey": management,
            "clientKey": client,
            "machineName": "rehearsal",
            "machineKey": machine,
            "desktopToken": desktop_token,
            "gatewayHost": GATEWAY_HOST,
        }))
        .unwrap_or_default()
        .as_bytes(),
    )?;
    put("proxy.yaml", format!("# The stand-in proxy of a rehearsal (pools-shadow).\nhost: 127.0.0.1\nport: {proxy}\n").as_bytes())?;
    let (cert, key) = pair()?;
    put("state/certs/desktop-relay.crt", cert.as_bytes())?;
    put("state/certs/desktop-relay.key", key.as_bytes())?;
    put(STAND_IN, &std::fs::read(exe)?)?;
    std::fs::create_dir_all(dest.join("logs"))?;
    // The marker last: a rehearsal half made does not open.
    put(MARKER, serde_json::to_string_pretty(&json!({ "createdAt": pool_core::js::iso(crate::watch::now_ms()), "from": live.root() })).unwrap_or_default().as_bytes())?;
    Ok(written)
}

/// The stand-in proxy, as the service starts the proxy (`cli-proxy-api.exe -config <root>\proxy.yaml`):
/// only for a rehearsal's config (its directory opens as one), on its `proxyPort`, with its
/// management and client keys, until it is ended.
pub async fn stand_in(config: &Path, live_root: &Path) -> std::io::Result<()> {
    let root = config.parent().ok_or_else(|| std::io::Error::other("-config names no directory"))?;
    let install = super::Install::open(root, live_root, false)?;
    if !install.rehearsal || std::fs::canonicalize(config)? != std::fs::canonicalize(install.proxy_config())? {
        return Err(std::io::Error::other("the stand-in runs only on a rehearsal's proxy.yaml"));
    }
    let (key, client) = (install.secret("managementKey")?, install.secret("clientKey")?);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", install.proxy_port())).await?;
    println!("stand-in proxy: {} on http://{}", install.root.display(), listener.local_addr()?);
    crate::writer::fake_proxy::serve(listener, install.root.clone(), key, Some(client)).await
}
