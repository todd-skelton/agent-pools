//! The three services the scheduled tasks start, each as Agent Pools' own script starts it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde_json::{Value, json};

use super::{Install, tls};
use crate::writer;

/// `127.0.0.1:<port>`, and `[::1]:<port>` when the system has IPv6 loopback, so `localhost`
/// answers at once however it resolves.
async fn loopback(port: u16) -> std::io::Result<Vec<tokio::net::TcpListener>> {
    let v4 = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| in_use(port, e))?;
    let mut out = vec![v4];
    if let Ok(v6) = tokio::net::TcpListener::bind(("::1", port)).await {
        out.push(v6);
    }
    Ok(out)
}

fn in_use(port: u16, e: std::io::Error) -> std::io::Error {
    if e.kind() == std::io::ErrorKind::AddrInUse {
        std::io::Error::other(format!("port {port}: already running or port in use"))
    } else {
        e
    }
}

/// `pools-shadow serve-service <root> [--live]`: pool-service.mjs. The dashboard and API on
/// `supervisorPort` (the same pages and routes as a copy), the refresh loop, the proxy watch with
/// its restarts, the usage ledger, the value report, the desktop relay on `desktopRelayPort` (TLS
/// from `state/certs` when the pair is there), and the proxy child. With `--models` (or
/// `pool-rs.json`'s `models: true`) also model-refresh.mjs's two scheduled tasks: the refresh
/// hourly at :15 (admission probes with the real tools, the benchmark snapshots) and the recheck
/// every five minutes ([`writer::models::run`]). A rehearsal never probes: its proxy is the stand-in.
pub async fn serve_service(install: Install) -> std::io::Result<()> {
    super::normal_priority();
    let root = install.root.clone();
    let key = install.secret("managementKey")?;
    let listeners = loopback(install.supervisor_port()).await?;
    let io = writer::io::FileIo::new(root.clone(), install.proxy_url(), key.clone());
    let io = Arc::new(if install.rehearsal { io } else { io.with_desktop_toasts() });
    let relay_tls = tls::load(&install.certs())?;
    let relay = writer::desktop::start_with(io.clone(), &install.backend_upstream(), install.port("desktopRelayPort", 8000), writer::desktop::RETRY, relay_tls).await?;
    let shadow = crate::watch::Shadow::new(crate::sources::Install::new(root.clone()), "(the service: not compared)".into(), root.join("logs/parity.jsonl"));
    let _ = shadow.service.set(());
    // The environment's (as the shadow takes it), else the install's pool-rs.json.
    crate::set_tailnet(&shadow)?;
    if shadow.tailnet.get().is_none()
        && let Some(t) = install.tailnet().map_err(std::io::Error::other)?
    {
        println!("tailnet: {} answered; writes there only as {}", t.origin, t.owner);
        let _ = shadow.tailnet.set(t);
    }
    let copy = writer::Copy::new(io).with_management_key(key);
    let supervisor = super::proxy::Supervisor::new(install.clone(), copy.closing.clone());
    let copy = Arc::new(copy.with_supervisor(supervisor.clone()));
    println!(
        "service: {} on http://127.0.0.1:{} (desktop relay {relay}, proxy {}){}",
        root.display(),
        install.supervisor_port(),
        install.proxy_url(),
        if install.rehearsal { ", a rehearsal" } else { "" }
    );
    tokio::spawn(supervisor.start());
    let options = pool_core::refresh::Options { read_plans: true, ..Default::default() };
    let models = install.models() && !install.rehearsal;
    let s = shadow.clone();
    let app = crate::app::app_copy(shadow, copy, writer::Delays::default(), options);
    let app = if models {
        println!("models: refresh hourly at :15, recheck every {} s", writer::models::RECHECK_EVERY.as_secs());
        app.task("model-refresh", move |signal| {
            let (status, changed) = (s.clone(), s);
            writer::models::run(
                root,
                Arc::new(super::admission::Native::new(install.root.clone())),
                Some(Arc::new(writer::benchmarks::Fetcher::default())),
                move || {
                    let now = crate::watch::now_ms();
                    status.built(now).map(|b| (*b.status(now)).clone())
                },
                move || changed.changed_inputs(),
                (writer::models::until_minute(15), writer::models::EVERY, writer::models::RECHECK_EVERY),
                signal,
            )
        })
    } else {
        app
    };
    corten::serve_on(listeners, app).await
}

/// `pools-shadow serve-inference <root> [--live]`: inference-service.mjs. The relay over TLS on
/// `inferenceRelayPort`, refusing to start without the pair (Codex sends its own sign-in here),
/// checking the desktop's sign-in and sending the pool's `clientKey`.
pub async fn serve_inference(install: Install) -> std::io::Result<()> {
    super::normal_priority();
    let acceptor = tls::load(&install.certs())?
        .ok_or_else(|| std::io::Error::other("Existing trusted localhost relay certificate required; refusing plaintext login credentials"))?;
    let port = install.port("inferenceRelayPort", 8319);
    let tcp = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| in_use(port, e))?;
    let authorize = pools_gateway::inference::file_credentials(install.codex_auth()?, install.root.join("state").join("secrets.json"));
    let mut config = pools_gateway::inference::Config::new(&format!("{}/v1", install.proxy_url()), authorize);
    config.tls = true;
    let log = install.clone();
    config.observe = Arc::new(move |kind: &str, details: Value| log.event("inference-events.jsonl", kind, details));
    let (app, _) = pools_gateway::inference::app(config, port).map_err(std::io::Error::other)?;
    let base = format!("https://localhost:{port}/v1");
    install.event("inference-events.jsonl", "inference-listening", json!({ "pid": std::process::id(), "baseUrl": base }));
    println!("inference relay: {base}");
    corten::serve_on(vec![tls::listener(tcp, acceptor)?], app).await
}

/// A JSON file read again only when it changes (public-gateway-service.mjs `cachedJSON`): a
/// missing or broken file gives `pick(None)` and is said once.
struct Cached<T> {
    file: PathBuf,
    pick: fn(Option<&Value>) -> T,
    seen: Mutex<(Option<Result<SystemTime, ()>>, T)>,
    install: Install,
}

impl<T: Clone> Cached<T> {
    fn new(install: Install, rel: &str, pick: fn(Option<&Value>) -> T) -> Cached<T> {
        Cached { file: install.root.join(rel), pick, seen: Mutex::new((None, pick(None))), install }
    }

    fn get(&self) -> T {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let read = std::fs::metadata(&self.file).and_then(|m| m.modified()).and_then(|m| {
            if seen.0 == Some(Ok(m)) {
                return Ok(None);
            }
            super::read_json(&self.file).map(|v| Some((m, v)))
        });
        match read {
            Ok(None) => {}
            Ok(Some((m, v))) => *seen = (Some(Ok(m)), (self.pick)(Some(&v))),
            Err(e) => {
                if seen.0 != Some(Err(())) {
                    let name = self.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let message = if e.kind() == std::io::ErrorKind::NotFound { "ENOENT".to_string() } else { e.to_string() };
                    self.install.event("public-gateway-events.jsonl", "public-gateway-problem", json!({ "file": name, "message": message }));
                }
                *seen = (Some(Err(())), (self.pick)(None));
            }
        }
        seen.1.clone()
    }
}

/// `state/remote-clients.json`'s enabled machines with a name and a 64-hex key hash.
pub fn clients(doc: Option<&Value>) -> Vec<pools_gateway::Client> {
    pool_core::js::values(doc.and_then(|d| d.get("clients")))
        .into_iter()
        .filter(|c| !pool_core::js::truthy(c.get("disabled")))
        .filter_map(|c| {
            let name = c.get("name")?.as_str()?;
            let hash = c.get("keySha256")?.as_str()?;
            (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
                .then(|| pools_gateway::Client { name: name.into(), key_sha256: hash.into() })
        })
        .collect()
}

fn secrets_doc(doc: Option<&Value>) -> Value {
    doc.cloned().unwrap_or_else(|| json!({}))
}

/// `pools-shadow serve-gateway <root> [--live]`: public-gateway-service.mjs, behind the tunnel on
/// `publicGateway.port` for `publicGateway.hostname`: inference to the proxy with the pool's
/// `remoteDesktopKey`, the dashboard from the service, machines from
/// `state/remote-clients.json` and sessions signed with `publicGatewaySessionSecret`, each file
/// read again when it changes.
pub async fn serve_gateway(install: Install) -> std::io::Result<()> {
    let settings = install.settings();
    let publish = settings.get("publicGateway");
    let hostname = publish.and_then(|p| p.get("hostname")).and_then(Value::as_str).unwrap_or("pool.example.com").to_string();
    let port = publish.and_then(|p| p.get("port")).and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()).unwrap_or(8330);
    let tcp = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| in_use(port, e))?;
    let mut config = pools_gateway::Config::new(&hostname, &install.proxy_url(), &install.service_url());
    let machines = Arc::new(Cached::new(install.clone(), "state/remote-clients.json", clients));
    let secrets = Arc::new(Cached::new(install.clone(), "state/secrets.json", secrets_doc));
    config.clients = Arc::new(move || machines.get());
    let s = secrets.clone();
    config.pool_key = Arc::new(move || s.get().get("remoteDesktopKey").and_then(Value::as_str).map(str::to_string));
    config.session_secret = Arc::new(move || match secrets.get().get("publicGatewaySessionSecret").and_then(Value::as_str) {
        Some(s) if s.chars().count() >= 32 => Ok(s.to_string()),
        _ => Err("publicGatewaySessionSecret missing".into()),
    });
    let log = install.clone();
    config.observe = Arc::new(move |kind: &str, details: Value| log.event("public-gateway-events.jsonl", kind, details));
    let app = pools_gateway::app(config).map_err(std::io::Error::other)?;
    install.event("public-gateway-events.jsonl", "public-gateway-listening", json!({ "pid": std::process::id(), "port": port, "hostname": hostname }));
    println!("Public gateway for https://{hostname} on 127.0.0.1:{port}");
    corten::serve_on(vec![tcp], app).await
}
