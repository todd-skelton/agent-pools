//! Live mode: the pool's own processes on Corten, over an install of Agent Pools, as its
//! scheduled tasks start them. Each Node process has its own command:
//!
//! | Command | Agent Pools | Listens on (install.json, default) |
//! |---|---|---|
//! | `guardian <root>` | pool-guardian.mjs: keeps `serve-service` running | none |
//! | `serve-service <root>` | pool-service.mjs: dashboard, API, refresh, ledger, desktop relay, the proxy child | `supervisorPort` 8318, `desktopRelayPort` 8000 |
//! | `serve-inference <root>` | inference-service.mjs | `inferenceRelayPort` 8319 (TLS) |
//! | `serve-gateway <root>` | public-gateway-service.mjs | `publicGateway.port` 8330 |
//!
//! The root is always given; nothing defaults to the live install. It is either the live
//! install with `--live` (only at cutover, from its scheduled tasks), or a rehearsal: a
//! directory `make-rehearsal` built, apart from the live install and marked by
//! `state/rehearsal.json`, whose secrets are made up and whose proxy is the stand-in
//! (`fake_proxy`, as its own process at `proxy/0.0.0-standin/cli-proxy-api.exe`). A rehearsal
//! differs from the live install in three places only: toasts go to `logs/toasts.jsonl`, the
//! inference relay checks the sign-in in `stand-in/codex-auth.json` instead of the desktop's
//! `~/.codex/auth.json`, and the desktop relay's upstream is the stand-in's `/backend-api`
//! instead of chatgpt.com.

pub mod admission;
pub mod guardian;
pub mod machine;
pub mod notify;
pub mod ops;
pub mod proxy;
pub mod rehearsal;
pub mod services;
pub mod tls;

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The rehearsal's marker, under the root.
pub const MARKER: &str = "state/rehearsal.json";

/// An install the live commands run on.
#[derive(Clone, Debug)]
pub struct Install {
    pub root: PathBuf,
    /// A rehearsal (`state/rehearsal.json`), not the live install.
    pub rehearsal: bool,
}

/// A JSON file (a byte-order mark allowed, as `readJSON` allows), or why not.
pub fn read_json(path: &Path) -> std::io::Result<Value> {
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| std::io::Error::other(format!("{}: {e}", path.display())))
}

impl Install {
    /// `root` as an install to run on: the live install (`live_root`) only with `live` set, else
    /// a rehearsal apart from it. Either way it must hold `state/install.json` and
    /// `state/secrets.json`.
    pub fn open(root: &Path, live_root: &Path, live: bool) -> std::io::Result<Install> {
        let root = std::fs::canonicalize(root).map_err(|e| std::io::Error::other(format!("{}: {e}", root.display())))?;
        let is_live = std::fs::canonicalize(live_root).is_ok_and(|l| l.to_string_lossy().eq_ignore_ascii_case(&root.to_string_lossy()));
        let rehearsal = root.join(MARKER).exists();
        if is_live && !live {
            return Err(std::io::Error::other(format!("{} is the live install: pass --live (cutover only)", root.display())));
        }
        if !is_live {
            if live {
                return Err(std::io::Error::other(format!("--live names the live install, and {} is not it", root.display())));
            }
            if !rehearsal {
                return Err(std::io::Error::other(format!("{} is not a rehearsal ({MARKER} is missing)", root.display())));
            }
            crate::writer::copy::apart(&root, live_root).map_err(|e| std::io::Error::other(e.0))?;
        }
        for rel in ["state/install.json", "state/secrets.json"] {
            if !read_json(&root.join(rel))?.is_object() {
                return Err(std::io::Error::other(format!("{rel} is not a JSON object")));
            }
        }
        Ok(Install { root: plain(root), rehearsal: !is_live })
    }

    /// `state/install.json`, read now (`{}` when it does not read).
    pub fn settings(&self) -> Value {
        read_json(&self.root.join("state/install.json")).unwrap_or_else(|_| Value::Object(Default::default()))
    }

    /// `state/secrets.json`, read now.
    pub fn secrets(&self) -> std::io::Result<Value> {
        read_json(&self.root.join("state/secrets.json"))
    }

    /// A string from the secrets, or an error naming it (never its value).
    pub fn secret(&self, key: &str) -> std::io::Result<String> {
        self.secrets()?
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| std::io::Error::other(format!("state/secrets.json has no {key}")))
    }

    /// `install[key] ?? default`.
    pub fn port(&self, key: &str, default: u16) -> u16 {
        self.settings().get(key).and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()).unwrap_or(default)
    }

    pub fn proxy_port(&self) -> u16 {
        self.port("proxyPort", 8317)
    }

    pub fn supervisor_port(&self) -> u16 {
        self.port("supervisorPort", 8318)
    }

    /// The pool proxy's origin.
    pub fn proxy_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.proxy_port())
    }

    /// The service's origin.
    pub fn service_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.supervisor_port())
    }

    /// `path.resolve(root, install.proxyBinary ?? "proxy/7.2.158/cli-proxy-api.exe")`.
    pub fn proxy_binary(&self) -> PathBuf {
        let rel = self.settings().get("proxyBinary").and_then(Value::as_str).unwrap_or("proxy/7.2.158/cli-proxy-api.exe").replace('/', "\\");
        let p = PathBuf::from(rel);
        if p.is_absolute() { p } else { self.root.join(p) }
    }

    pub fn proxy_config(&self) -> PathBuf {
        self.root.join("proxy.yaml")
    }

    pub fn certs(&self) -> PathBuf {
        self.root.join("state").join("certs")
    }

    /// The sign-in the inference relay accepts: the desktop's own (`~/.codex/auth.json`), or on
    /// a rehearsal the made-up one in `stand-in/codex-auth.json`.
    pub fn codex_auth(&self) -> std::io::Result<PathBuf> {
        if self.rehearsal {
            return Ok(self.root.join("stand-in").join("codex-auth.json"));
        }
        let home = std::env::var_os("USERPROFILE").ok_or_else(|| std::io::Error::other("USERPROFILE is not set"))?;
        Ok(PathBuf::from(home).join(".codex").join("auth.json"))
    }

    /// The desktop relay's upstream: chatgpt.com, or on a rehearsal the stand-in's.
    pub fn backend_upstream(&self) -> String {
        if self.rehearsal { format!("{}/backend-api", self.proxy_url()) } else { "https://chatgpt.com/backend-api".into() }
    }

    /// The arguments that name this install to another of its commands (the guardian's child).
    pub fn args(&self) -> Vec<String> {
        let mut a = vec![self.root.to_string_lossy().into_owned()];
        if !self.rehearsal {
            a.push("--live".into());
        }
        a
    }

    /// One line in `logs/<file>`.
    pub fn event(&self, file: &str, kind: &str, details: Value) {
        let _ = crate::writer::io::append_event(&self.root.join("logs").join(file), kind, details);
    }
}

/// The port's own settings in an install, `pool-rs/pool-rs.json` (`Install-Pool-Rs.ps1` writes it):
/// `current`, the version the wrappers start, and `tailnet` (`origin`, `owner`), the tailnet view
/// tailnet-dashboard-proxy.mjs serves today. Agent Pools' own files are not changed for the port.
pub const PORT_SETTINGS: &str = "pool-rs/pool-rs.json";

impl Install {
    /// Whether this service runs the model refresh: `--models` on its command line, or
    /// `pool-rs/pool-rs.json`'s `models: true` (so the cutover is a settings change and a restart).
    pub fn models(&self) -> bool {
        std::env::args().any(|a| a == "--models") || read_json(&self.root.join(PORT_SETTINGS)).is_ok_and(|d| d.get("models") == Some(&Value::Bool(true)))
    }

    /// The tailnet view from `pool-rs/pool-rs.json`, when it names one.
    pub fn tailnet(&self) -> Result<Option<crate::tailnet::Tailnet>, String> {
        let Ok(doc) = read_json(&self.root.join(PORT_SETTINGS)) else { return Ok(None) };
        let field = |k: &str| doc.get("tailnet").and_then(|t| t.get(k)).and_then(Value::as_str).map(str::to_string);
        match (field("origin"), field("owner")) {
            (Some(origin), Some(owner)) => crate::tailnet::Tailnet::new(&origin, &owner).map(Some),
            (None, None) => Ok(None),
            _ => Err(format!("{PORT_SETTINGS}: tailnet needs both origin and owner")),
        }
    }
}

/// `p` without Windows' verbatim prefix (`\\?\D:\...` as `D:\...`), as a process's
/// `ExecutablePath` and command line name it: the proxy's identity is compared on those.
pub fn plain(p: PathBuf) -> PathBuf {
    let stripped = p.to_string_lossy().strip_prefix(r"\\?\").filter(|rest| rest.as_bytes().get(1) == Some(&b':')).map(PathBuf::from);
    stripped.unwrap_or(p)
}

/// `<command> <root> [--live]` from the process's arguments.
pub fn open_from_args(command: &str) -> std::io::Result<Install> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let live = args.iter().any(|a| a == "--live");
    let root = args.iter().find(|a| !a.starts_with("--")).ok_or_else(|| std::io::Error::other(format!("usage: pools-shadow {command} <root> [--live]")))?;
    let live_root = crate::env_or("AGENT_POOLS_ROOT", crate::LIVE_ROOT);
    Install::open(Path::new(root), Path::new(&live_root), live)
}

/// `model-refresh <root> --live [--no-probe] [--recheck]`: one run of model-refresh.mjs by hand
/// (the pool's status from the running service's `/api/status`, as it read it), its lines printed.
/// `model-refresh <root> --live --probe <provider> <id> <effort>`: one probe with the real tool, its
/// verdict printed and not recorded.
/// `fetch-benchmarks <root> --live --out <dir>`: both benchmark snapshots taken now with the
/// registry's identities, published under `<dir>/state/benchmarks` (nothing under `<root>` is
/// written; the Artificial Analysis key is read from it).
pub async fn models_once(install: Install, command: &str) -> std::io::Result<()> {
    use crate::writer::{benchmarks, models};
    let args: Vec<String> = std::env::args().collect();
    let has = |f: &str| args.iter().any(|a| a == f);
    let log = |l: &str| println!("{l}");
    let fail = |e: String| std::io::Error::other(e);
    if command == "fetch-benchmarks" {
        let out = args.iter().position(|a| a == "--out").and_then(|i| args.get(i + 1)).ok_or_else(|| fail("usage: pools-shadow fetch-benchmarks <root> --live --out <dir>".into()))?;
        let out = PathBuf::from(out);
        if std::fs::canonicalize(&out).is_ok_and(|o| plain(o) == install.root) {
            return Err(fail("--out must not be the install".into()));
        }
        let registry = read_json(&install.root.join("state/model-registry.json"))?;
        let fetcher = benchmarks::Fetcher::default();
        let deadline = crate::watch::now_ms() + models::BENCH_DEADLINE_MS;
        for feed in benchmarks::Feed::ALL {
            match fetcher.fetch_to(feed, &install.root, &out, &registry["identities"], deadline).await {
                Ok(r) => log(&format!("{}: {}", feed.name(), pool_core::js::stringify(&r))),
                Err(e) => log(&format!("{}: failed: {e}", feed.name())),
            }
        }
        return Ok(());
    }
    let url = format!("{}/api/status", install.service_url());
    let status = async {
        let r = reqwest::Client::builder().no_proxy().build().ok()?.get(&url).timeout(std::time::Duration::from_secs(15)).send().await.ok()?;
        if r.status().is_success() { r.json::<Value>().await.ok() } else { None }
    }
    .await;
    let prober = admission::Native::new(install.root.clone());
    if let Some(i) = args.iter().position(|a| a == "--probe") {
        use crate::writer::models::Prober;
        let [provider, id, effort] = [1, 2, 3].map(|k| args.get(i + k).cloned().unwrap_or_default());
        if !pool_core::registry::safe_id(&id) || !["claude", "codex"].contains(&provider.as_str()) || !pool_core::registry::EFFORTS.contains(&effort.as_str()) {
            return Err(fail("usage: --probe <claude|codex> <id> <effort>".into()));
        }
        log(&format!("harness {}", pool_core::js::stringify(&prober.harness_versions().await.map_err(fail)?)));
        log(&format!("battery running: {}", prober.deferred()));
        log(&pool_core::js::stringify(&prober.probe(&provider, &id, &effort).await));
        return Ok(());
    }
    if has("--recheck") {
        let rebuilt = models::recheck(&install.root, &prober, || status, &log).await.map_err(fail)?;
        log(&format!("rebuilt: {rebuilt}"));
    } else {
        log(&format!("--- model-refresh {} pid={}", pool_core::js::iso(crate::watch::now_ms()), std::process::id()));
        let fetcher = benchmarks::Fetcher::default();
        models::refresh(&install.root, &prober, || status, !has("--no-probe"), Some(&fetcher), &log).await.map_err(fail)?;
    }
    Ok(())
}

/// Raises this process to normal priority: the scheduled tasks start below normal, which starved
/// the supervisor on a busy machine (pool-guardian.mjs, 2026-10-19).
pub fn normal_priority() {
    machine::normal_priority(std::process::id());
}

#[cfg(test)]
mod tests;
