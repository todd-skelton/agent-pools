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
//! | `SHADOW_TAILNET_ORIGIN`, `SHADOW_TAILNET_OWNER` | none | the tailnet view ([`tailnet`]): the page's `https://` origin and the owner's Tailscale login |
//! | `PORT`, `HOST` | 8320, loopback | Corten's own |

mod app;
mod dash;
mod parity;
mod sources;
mod tailnet;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;
mod watch;
mod writer;

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
        Some("check-attempts") => return check_attempts_once(),
        Some("check-registry") => return check_registry_once(),
        Some("render-dash") => return render_dash(),
        Some("make-copy") => return make_copy(),
        Some("run-copy") => {
            return tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(run_copy());
        }
        Some("check-status") => {
            return tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(check_status_once());
        }
        _ => {}
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
    set_tailnet(&shadow)?;
    println!(
        "shadow: reading {} (read only), comparing with {}",
        shadow.install.root().display(),
        shadow.live_url
    );
    let assets = std::env::var_os("STATIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static"));
    // 8320 unless PORT says otherwise; the loopback pair, the drain and graceful shutdown are
    // serve's (corten#115).
    let listeners = corten::listeners(8320).await?;
    println!("shadow: listening on http://localhost:{}", listeners[0].local_addr()?.port());
    corten::serve_on(
        listeners,
        app::app(shadow, assets, secs("SHADOW_WATCH_SECS", 5), secs("SHADOW_SUMMARY_SECS", 60)),
    )
    .await
}

/// The tailnet view from the environment, when it is set.
fn set_tailnet(shadow: &watch::Shared) -> std::io::Result<()> {
    if let Some(t) = tailnet::Tailnet::from_env().map_err(std::io::Error::other)? {
        println!("tailnet: {} answered; writes there only as {}", t.origin, t.owner);
        let _ = shadow.tailnet.set(t);
    }
    Ok(())
}

/// A port from the environment, or `default`.
fn port_env(name: &str, default: u16) -> std::io::Result<u16> {
    match std::env::var(name) {
        Ok(v) => v.parse().map_err(|_| std::io::Error::other(format!("{name} is not a port number: {v}"))),
        Err(_) => Ok(default),
    }
}

const LIVE_ROOT: &str = r"D:\Users\ToddS\AgentPools";

/// `pools-shadow make-copy <dest>`: copies what the write side reads from the live install
/// (state documents, the four install.json settings, the usage samples) into `dest`. Reads the
/// live install only; refuses a `dest` that is or overlaps it.
fn make_copy() -> std::io::Result<()> {
    let dest = std::env::args().nth(2).ok_or_else(|| std::io::Error::other("usage: pools-shadow make-copy <dest>"))?;
    let live = sources::Install::new(env_or("AGENT_POOLS_ROOT", LIVE_ROOT));
    for p in writer::copy::make_copy(&live, std::path::Path::new(&dest))? {
        println!("{}", p.display());
    }
    Ok(())
}

/// `pools-shadow run-copy <root>`: the dashboard and the write side on a copy made by
/// `make-copy`, with a stand-in proxy on loopback. Refuses the live install. Settings:
/// `SHADOW_COPY_REFRESH_SECS` (120) and `SHADOW_COPY_DEGRADED_SECS` (30), `PORT` (8321),
/// `SHADOW_COPY_RELAY_PORT` (8322, the desktop relay; Agent Pools' is 8000),
/// `SHADOW_COPY_INFERENCE_PORT` (8323; Agent Pools' 8319) and `SHADOW_COPY_GATEWAY_PORT` (8324;
/// Agent Pools' 8330).
async fn run_copy() -> std::io::Result<()> {
    let root = std::env::args().nth(2).ok_or_else(|| std::io::Error::other("usage: pools-shadow run-copy <root>"))?;
    let live = env_or("AGENT_POOLS_ROOT", LIVE_ROOT);
    let root = writer::copy::guard(std::path::Path::new(&root), std::path::Path::new(&live)).map_err(|e| std::io::Error::other(e.0))?;
    // A key for this run only, known to the stand-in and the writer; never written down.
    let key = uuid::Uuid::new_v4().to_string();
    // The run's made-up inference keys (writer::edge), written under the copy's stand-in/.
    let keys = writer::edge::Keys::new();
    keys.write(&root)?;
    let proxy = writer::fake_proxy::start(root.clone(), key.clone(), Some(keys.client_key.clone())).await?;
    println!("copy: writing {} only; stand-in proxy at {proxy}", root.display());
    let io = std::sync::Arc::new(writer::io::FileIo::new(root.clone(), proxy.clone(), key));
    let relay = writer::desktop::start(io.clone(), &format!("{proxy}/backend-api"), port_env("SHADOW_COPY_RELAY_PORT", 8322)?, writer::desktop::RETRY).await?;
    println!("copy: desktop relay at http://127.0.0.1:{relay}/backend-api (in front of the stand-in)");
    let inference = writer::edge::start_inference(io.clone(), &proxy, port_env("SHADOW_COPY_INFERENCE_PORT", 8323)?).await?;
    println!("copy: inference relay at http://127.0.0.1:{inference}/v1 (in front of the stand-in)");
    let service = format!("http://127.0.0.1:{}", port_env("PORT", 8321)?);
    let gateway = writer::edge::start_gateway(io.clone(), &keys, &proxy, &service, port_env("SHADOW_COPY_GATEWAY_PORT", 8324)?).await?;
    println!("copy: public gateway at http://127.0.0.1:{gateway} for Host {} (keys in stand-in/keys.json)", writer::edge::GATEWAY_HOST);
    let shadow = watch::Shadow::new(sources::Install::new(root.clone()), "(a copy: not compared)".into(), root.join("logs/parity.jsonl"));
    set_tailnet(&shadow)?;
    let assets = std::env::var_os("STATIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static"));
    let delays = writer::Delays { every: secs("SHADOW_COPY_REFRESH_SECS", 120), degraded: secs("SHADOW_COPY_DEGRADED_SECS", 30), budget: 3 };
    let options = pool_core::refresh::Options { read_plans: true, ..Default::default() };
    let listeners = corten::listeners(8321).await?;
    println!("copy: listening on http://localhost:{}", listeners[0].local_addr()?.port());
    let copy = std::sync::Arc::new(writer::Copy::new(io).with_management_key(keys.management_key.clone()));
    corten::serve_on(listeners, app::app_copy(shadow, assets, copy, delays, options)).await
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

/// `pools-shadow check-attempts`: rebuilds the live `state/routing-attempts.json` from the live
/// `state/session-usage.json` (written with it) and install.json's `orchestratorRoot`, and says
/// whether it is the same document. Reads only; dispatches logged since are outside the window.
fn check_attempts_once() -> std::io::Result<()> {
    let install = sources::Install::new(env_or("AGENT_POOLS_ROOT", LIVE_ROOT));
    let read = |rel: &str| -> std::io::Result<serde_json::Value> {
        let text = std::fs::read_to_string(install.root().join(rel))?;
        serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(&text)).map_err(|e| std::io::Error::other(format!("{rel}: {e}")))
    };
    let (live, sessions, config) = (read("state/routing-attempts.json")?, read("state/session-usage.json")?, read("state/install.json")?);
    let root = config.get("orchestratorRoot").and_then(serde_json::Value::as_str);
    let port = writer::attempts::build(&sessions, root).map_err(std::io::Error::other)?;
    let (a, b) = (pool_core::js::stringify(&port), pool_core::js::stringify(&live));
    let count = |v: &serde_json::Value| v.get("attempts").and_then(serde_json::Value::as_array).map_or(0, Vec::len);
    println!("live {} attempts, port {} attempts, same document: {}", count(&live), count(&port), a == b);
    if a != b {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        let at = a.iter().zip(&b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
        let around = |s: &[char]| s[at.saturating_sub(120).min(s.len())..(at + 120).min(s.len())].iter().collect::<String>();
        println!("port: {}\nlive: {}", around(&a), around(&b));
    }
    Ok(())
}

/// \`pools-shadow check-registry\`: rebuilds the live \`state/model-registry.json\` with
/// \`pool_core::registry::build_registry\` from the live inputs model-refresh.mjs read for it
/// (\`model-families.json\`, \`state/live-models.json\`, \`state/model-admission.jsonl\` up to its
/// \`generatedAt\`, \`state/model-lineage.json\`, its \`harnessVersions\`, at its \`generatedAt\`,
/// then \`registry.benchmarks\` from \`state/benchmarks/latest.json\`) and says whether it is the
/// same document. Reads only. The pool's \`/api/status\` at that moment was not kept, so it is
/// taken from what the registry recorded of it: each account's routing, status, disabled flag and
/// list time. The saved lineage is the build's output, which is its input again unless that run
/// promoted a model (then the check says so).
fn check_registry_once() -> std::io::Result<()> {
    use serde_json::{Value, json};
    let install = sources::Install::new(env_or("AGENT_POOLS_ROOT", LIVE_ROOT));
    let read = |rel: &str| -> std::io::Result<Value> {
        let text = std::fs::read_to_string(install.root().join(rel))?;
        serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(&text)).map_err(|e| std::io::Error::other(format!("{rel}: {e}")))
    };
    let live = read("state/model-registry.json")?;
    let lineage = read("state/model-lineage.json")?;
    let generated = live["generatedAt"].as_str().unwrap_or_default().to_string();
    let now = pool_core::js::date_parse(&generated).ok_or_else(|| std::io::Error::other("model-registry.json has no generatedAt"))?;
    if pool_core::js::values(lineage.get("events")).iter().any(|e| e.get("at").and_then(Value::as_str) == Some(generated.as_str())) {
        println!("the run at {generated} promoted a model: the saved lineage is not its input; not compared");
        return Ok(());
    }
    let probes: Vec<Value> = std::fs::read_to_string(install.root().join("state/model-admission.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|p| pool_core::js::date_parse_value(p.get("at")) <= now)
        .collect();
    // The pool's status as the registry recorded it, per account.
    let mut accounts: Vec<(String, Value)> = Vec::new();
    for (id, m) in pool_core::js::entries(live.get("models")) {
        for row in pool_core::js::values(m.get("accounts")) {
            let account = pool_core::js::to_string(row.get("account"));
            let pos = accounts.iter().position(|(a, _)| *a == account).unwrap_or_else(|| {
                let mut a = json!({ "id": account, "disabled": row["disabled"], "status": row["accountStatus"], "routingModels": {} });
                if let Some(at) = row.get("fetchedAt").filter(|v| v.is_string()) {
                    a["modelList"] = json!({ "source": "live", "fetchedAt": at });
                }
                accounts.push((account.clone(), a));
                accounts.len() - 1
            });
            if !row["routing"].is_null() {
                accounts[pos].1["routingModels"][&id] = json!({ "status": row["routing"] });
            }
        }
    }
    let status = json!({ "checkedAt": live["source"]["statusCheckedAt"], "accounts": accounts.into_iter().map(|(_, a)| a).collect::<Vec<_>>() });
    let input = json!({
        "liveModels": read("state/live-models.json")?,
        "status": status,
        "families": read("model-families.json")?,
        "probes": probes,
        "lineage": lineage,
        "harnessVersions": live["harnessVersions"],
        "now": now,
    });
    let built = pool_core::registry::build_registry(&input).map_err(std::io::Error::other)?;
    let mut port = built["registry"].clone();
    port["benchmarks"] = read("state/benchmarks/latest.json").unwrap_or(Value::Null);
    let (a, b) = (pool_core::js::stringify(&port), pool_core::js::stringify(&live));
    let count = |v: &Value| pool_core::js::entries(v.get("models")).len();
    println!(
        "live {} models at {generated} (generation {}, digest {}), port {} models (generation {}, digest {}), same document: {}",
        count(&live), live["generation"], live["authorityDigest"], count(&port), port["generation"], port["authorityDigest"], a == b
    );
    println!("same lineage: {}", pool_core::js::stringify(&built["lineage"]) == pool_core::js::stringify(&input["lineage"]));
    if a != b {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        let at = a.iter().zip(&b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
        let around = |s: &[char]| s[at.saturating_sub(160).min(s.len())..(at + 160).min(s.len())].iter().collect::<String>();
        println!("port: {}\nlive: {}", around(&a), around(&b));
    }
    Ok(())
}

/// \`pools-shadow check-status\`: asks the live service for \`/api/status\` once and checks it.
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

/// `pools-shadow render-dash <status.json> <value.json|-> <now ms> [filter] [open]`: the
/// dashboard's parts drawn from those documents at that moment, as JSON (`golden/dash-text.mjs`
/// draws the JavaScript's from the same ones and compares them).
fn render_dash() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let read = |p: &str| -> std::io::Result<serde_json::Value> {
        let text = std::fs::read_to_string(p)?;
        serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(&text)).map_err(std::io::Error::other)
    };
    let (Some(status), Some(value), Some(now)) = (args.first(), args.get(1), args.get(2)) else {
        return Err(std::io::Error::other("usage: pools-shadow render-dash <status.json> <value.json|-> <now ms> [filter] [open]"));
    };
    let data = dash::Data {
        s: read(status)?,
        v: if value == "-" { None } else { Some(read(value)?) },
        now: now.parse().map_err(std::io::Error::other)?,
        z: dash::fmt::Zone::from_env(),
        writable: false,
        signin: None,
    };
    let parts = dash::fragments(&data, args.get(3).map(String::as_str).filter(|f| !f.is_empty()), args.get(4).map(String::as_str));
    println!("{}", serde_json::Value::Object(parts));
    Ok(())
}
