//! Native client-side lifecycle for the Claude splitter. It uses its own protected client
//! directory on a pool host or a remote workstation, not the pool host's shared auth/state.
//! The PowerShell installer is provisioning only; no PowerShell or Node runs in service.
mod certificates;
mod private;
pub mod projects;
pub mod models;

use base64::Engine;
use pools_gateway::claude_split;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};

struct Shutdown(claude_split::Handle);
impl Drop for Shutdown {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

fn config(profile: &Path) -> std::io::Result<Value> {
    let c = crate::live::read_json(&profile.join("splitter.json"))?;
    if c["schema"] != "claude-splitter/v1" || c["poolOrigin"].as_str().is_none() || c["port"].as_u64().is_none_or(|p| !(1024..=65535).contains(&p)) {
        return Err(std::io::Error::other("Invalid splitter.json schema, poolOrigin or port"));
    }
    Ok(c)
}

fn proxy_key(profile: &Path, create: bool) -> std::io::Result<String> {
    let p = profile.join("proxy-key.txt");
    if create && !p.exists() {
        private::write(&p, format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple()).as_bytes())?;
    }
    let key = std::fs::read_to_string(p)?;
    if key.len() < 32 || key.contains(['\r', '\n']) {
        return Err(std::io::Error::other("Invalid local proxy key"));
    }
    Ok(key)
}

fn log(profile: &Path, details: Value) {
    let path = profile.join("splitter-events.jsonl");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
        // Metadata only; at most two bounded files, as the prototype kept.
        let old = profile.join("splitter-events.jsonl.1");
        if old.exists() {
            let _ = std::fs::remove_file(&old);
        }
        let _ = std::fs::rename(&path, old);
    }
    let _ = crate::writer::io::append_event(&path, "claude-split", details);
}

pub async fn run(command: &str, profile: &Path) -> std::io::Result<()> {
    if command == "init-claude-split" {
        private::directory(profile)?;
        config(profile)?;
        proxy_key(profile, true)?;
        certificates::ensure(profile)?;
        println!("Initialized native Agent Pools Claude client; no system proxy or trust store changed.");
        return Ok(());
    }
    let c = config(profile)?;
    let key = proxy_key(profile, false)?;
    let port = c["port"].as_u64().unwrap() as u16;
    if command == "check-claude-split" {
        return check(profile, port, &key).await;
    }
    // The DPAPI blob has no plaintext key or command-line/environment counterpart.
    let sealed = std::fs::read(profile.join("pool-key.dpapi"))?;
    let pool_key = String::from_utf8(crate::live::ops::dpapi::unprotect(&sealed)?).map_err(|_| std::io::Error::other("Invalid DPAPI pool key"))?;
    // Bind before touching certificates/state. An existing proxy is never adopted or killed.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    private::directory(profile)?;
    let (tls, exp) = certificates::ensure(profile)?;
    let mut cfg = claude_split::Config::new(c["poolOrigin"].as_str().unwrap(), &pool_key, &key, tls, exp);
    let catalog_path = profile.join("model-catalog.json");
    if catalog_path.exists() {
        let catalog: pools_gateway::model_catalog::Catalog = serde_json::from_value(crate::live::read_json(&catalog_path)?)
            .map_err(|_| std::io::Error::other("Invalid Claude pool model catalog"))?;
        catalog.validate().map_err(std::io::Error::other)?;
        cfg.model_catalog = Some(catalog);
    }
    cfg.additional_tunnel_hosts = match c.get("additionalTunnelHosts") {
        None => Vec::new(),
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => a.iter().map(|v| v.as_str().unwrap().to_string()).collect(),
        _ => return Err(std::io::Error::other("additionalTunnelHosts must be an array of exact DNS names")),
    };
    let log_profile = profile.to_path_buf();
    cfg.observe = Arc::new(move |event| log(&log_profile, event));
    let (app, handle) = claude_split::app(cfg).map_err(std::io::Error::other)?;
    let project_profile = profile.to_path_buf();
    let app = app.task("claude-projects", move |signal| async move { projects::supervise(project_profile, async { signal.aborted().await; }).await });
    let _shutdown = Shutdown(handle.clone());
    let state_path = profile.join("probe-proxy.json");
    private::write(&state_path, json!({"schema":"claude-splitter/v1","runtime":"pools-rs","port":port,"pid":std::process::id(),"instanceId":handle.instance_id(),"proxyKey":key,"poolOrigin":c["poolOrigin"]}).to_string().as_bytes())?;
    println!("Agent Pools Claude splitter on 127.0.0.1:{port}");
    let renew = async {
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            match certificates::ensure(profile) {
                Ok((tls, exp)) => handle.replace_tls(tls, exp),
                Err(_) => log(profile, json!({"route":"certificate-renewal","error":"RENEWAL_FAILED"})),
            }
        }
    };
    let result = tokio::select! {
        r = corten::serve_on(vec![listener], app) => r,
        _ = handle.stopped() => Ok(()),
        _ = renew => Ok(()),
    };
    handle.shutdown();
    if crate::live::read_json(&state_path).is_ok_and(|s| s["instanceId"] == handle.instance_id()) {
        let _ = std::fs::remove_file(state_path);
    }
    result
}

async fn check(profile: &Path, port: u16, key: &str) -> std::io::Result<()> {
    let health_client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|_| std::io::Error::other("Could not initialize local health check"))?;
    let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("probe:{key}")));
    let health: Value = health_client
        .get(format!("http://127.0.0.1:{port}/health"))
        .header("proxy-authorization", auth)
        .send()
        .await
        .map_err(|_| std::io::Error::other("Native splitter health unavailable"))?
        .json()
        .await
        .map_err(|_| std::io::Error::other("Invalid native splitter health"))?;
    if health["service"] != "agent-pools-claude-split" || health["certificateValid"] != true {
        return Err(std::io::Error::other("Not a healthy native Agent Pools Claude splitter"));
    }
    let ca = reqwest::Certificate::from_pem(&std::fs::read(profile.join("probe-certs/ca.pem"))?).map_err(|_| std::io::Error::other("Invalid profile CA"))?;
    let proxy = reqwest::Proxy::all(format!("http://127.0.0.1:{port}"))
        .map_err(|_| std::io::Error::other("Invalid proxy endpoint"))?
        .basic_auth("probe", key);
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .add_root_certificate(ca)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| std::io::Error::other("Could not initialize verified client"))?;
    let r = client
        .get("https://api.anthropic.com/v1/models")
        .send()
        .await
        .map_err(|_| std::io::Error::other("Native splitter TLS/model check failed"))?;
    if !r.status().is_success() {
        return Err(std::io::Error::other(format!(
            "Pool Models returned HTTP {}; check pool connectivity/key",
            r.status().as_u16()
        )));
    }
    let models: Value = r.json().await.map_err(|_| std::io::Error::other("Invalid pool Models response"))?;
    let count = models["data"]
        .as_array()
        .ok_or_else(|| std::io::Error::other("Invalid pool Models response"))?
        .len();
    println!("PASS: native authenticated proxy, verified TLS, pool model discovery ({count} models). No inference or control-login credentials used.");
    Ok(())
}
