//! The supervisor's decisions about its child processes: health-policy.mjs (when a failing
//! service or proxy is restarted, and whether a process is the one the pool started) and
//! proxy-takeover.mjs (what to do with whatever already runs before a child is (re)spawned).
//! Processes are `Get-CimInstance Win32_Process` records as JSON: `ProcessId`, `CreationDate`,
//! `ExecutablePath`, `CommandLine`. Records the pool keeps are `{ pid, creationDate }`.

use std::future::Future;
use std::sync::LazyLock;

use serde_json::{Value, json};

use crate::js;

pub const PROXY_HEALTH_TIMEOUT_MS: f64 = 8000.0;
pub const PROXY_HEALTH_FAILURES: u32 = 5;
pub const PROXY_LIVENESS_TIMEOUT_MS: f64 = 3000.0;
pub const PROXY_DEGRADED_FAILURES: u32 = 30;
pub const PROXY_STARTUP_GRACE_MS: f64 = 120_000.0;
pub const LOOKUP_ATTEMPTS: u32 = 3;

/// `createHealthPolicy(threshold)`: true on the `threshold`th consecutive failure, then counts
/// again from zero; a success resets it.
pub struct HealthPolicy {
    threshold: u32,
    failures: u32,
}

impl HealthPolicy {
    pub fn new(threshold: u32) -> HealthPolicy {
        HealthPolicy { threshold, failures: 0 }
    }

    pub fn check(&mut self, healthy: bool) -> bool {
        self.failures = if healthy { 0 } else { self.failures + 1 };
        if self.failures < self.threshold {
            return false;
        }
        self.failures = 0;
        true
    }
}

impl Default for HealthPolicy {
    fn default() -> HealthPolicy {
        HealthPolicy::new(3)
    }
}

/// One reading's decision: restart now, and whether this reading is the one that marks the proxy
/// degraded (management failing while it is alive), to be logged once per episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Decision {
    pub restart: bool,
    pub degraded: bool,
}

/// `createProxyRestartPolicy(...)`: a dead proxy is restarted after `failures` readings, one whose
/// management stalls while it is alive only after `degraded_failures`; a just-started proxy is
/// not judged until it first answers or `startup_grace_ms` passes.
pub struct ProxyRestartPolicy {
    pub failures: u32,
    pub degraded_failures: u32,
    pub startup_grace_ms: f64,
    dead: u32,
    slow: u32,
    started_at: Option<f64>,
}

impl Default for ProxyRestartPolicy {
    fn default() -> ProxyRestartPolicy {
        ProxyRestartPolicy::new(PROXY_HEALTH_FAILURES, PROXY_DEGRADED_FAILURES, PROXY_STARTUP_GRACE_MS)
    }
}

impl ProxyRestartPolicy {
    pub fn new(failures: u32, degraded_failures: u32, startup_grace_ms: f64) -> ProxyRestartPolicy {
        ProxyRestartPolicy { failures, degraded_failures, startup_grace_ms, dead: 0, slow: 0, started_at: None }
    }

    /// `started()`: the proxy was (re)started at `now`.
    pub fn started(&mut self, now: f64) {
        self.started_at = Some(now);
        self.dead = 0;
        self.slow = 0;
    }

    /// `check({ managed, alive })` at `now`.
    pub fn check(&mut self, managed: bool, alive: bool, now: f64) -> Decision {
        if managed {
            self.started_at = None;
            self.dead = 0;
            self.slow = 0;
            return Decision::default();
        }
        if self.started_at.is_some_and(|s| now - s < self.startup_grace_ms) {
            return Decision::default();
        }
        if alive {
            self.dead = 0;
            self.slow += 1;
            if self.slow >= self.degraded_failures {
                self.slow = 0;
                return Decision { restart: true, degraded: false };
            }
            return Decision { restart: false, degraded: self.slow == self.failures };
        }
        self.slow = 0;
        self.dead += 1;
        if self.dead >= self.failures {
            self.dead = 0;
            return Decision { restart: true, degraded: false };
        }
        Decision::default()
    }
}

fn number(v: Option<&Value>) -> f64 {
    js::to_number(v)
}

/// `sameIdentity(record, actual)`: the recorded process is still the one started: same PID and
/// creation time.
pub fn same_identity(record: &Value, actual: Option<&Value>) -> bool {
    let Some(actual) = actual.filter(|a| js::truthy(Some(*a))) else { return false };
    let created = js::date_parse_value(record.get("creationDate"));
    number(record.get("pid")) == number(actual.get("ProcessId")) && created.is_finite() && created == js::date_parse_value(actual.get("CreationDate"))
}

fn normalize_path(v: Option<&Value>) -> String {
    let s = match v {
        None | Some(Value::Null) => String::new(),
        Some(v) => js::to_string(Some(v)),
    };
    s.replace('/', "\\").to_lowercase()
}

/// `ownedProxy(actual, poolRoot, configPath)`: one of the pool's own proxy builds
/// (`<root>\proxy\<version>\cli-proxy-api.exe`) running the pool's config.
pub fn owned_proxy(actual: Option<&Value>, pool_root: &str, config_path: &str) -> bool {
    let Some(actual) = actual.filter(|a| js::truthy(Some(*a))) else { return false };
    let exe = normalize_path(actual.get("ExecutablePath"));
    let builds = normalize_path(Some(&json!(pool_root))).trim_end_matches('\\').to_string() + "\\proxy\\";
    exe.starts_with(&builds)
        && exe.ends_with("\\cli-proxy-api.exe")
        && !exe[builds.len()..].contains("..")
        && uses_config(actual.get("CommandLine"), config_path)
}

/// `usesConfig(commandLine, configPath)`: every `-config` / `--config` argument (quoted or bare,
/// `=` or a space) is `configPath`, and there is at least one.
pub fn uses_config(command_line: Option<&Value>, config_path: &str) -> bool {
    static CONFIG: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r#"(?:^|\s)--?[cC][oO][nN][fF][iI][gG](?:=|\s+)(?:"([^"]*)"|(\S+))"#).unwrap());
    let text = match command_line {
        None | Some(Value::Null) => String::new(),
        Some(v) => js::to_string(Some(v)),
    };
    let want = normalize_path(Some(&json!(config_path)));
    let values: Vec<String> = CONFIG.captures_iter(&text).map(|m| m.get(1).or(m.get(2)).map_or("", |v| v.as_str()).to_string()).collect();
    !values.is_empty() && values.iter().all(|v| normalize_path(Some(&json!(v))) == want)
}

/// `sameProcess(record, actual, executable, configPath)`: the same identity, and running the
/// executable and config the install selects now.
pub fn same_process(record: &Value, actual: Option<&Value>, executable: &str, config_path: &str) -> bool {
    same_identity(record, actual)
        && actual.is_some_and(|a| normalize_path(a.get("ExecutablePath")) == normalize_path(Some(&json!(executable))) && uses_config(a.get("CommandLine"), config_path))
}

/// What the takeover may do on the machine. Lookups fail with a message; `timed_out` says
/// whether the failure was the lookup's timeout (`e.killed`).
pub trait Machine: Sync {
    fn alive(&self, pid: f64) -> bool;
    fn kill(&self, pid: f64);
    fn port_free(&self, port: u16) -> impl Future<Output = bool> + Send;
    fn process_info(&self, pid: f64) -> impl Future<Output = Result<Option<Value>, LookupError>> + Send;
    fn port_owner(&self, port: u16) -> impl Future<Output = Result<Option<Value>, LookupError>> + Send;
    fn sleep(&self, ms: u64) -> impl Future<Output = ()> + Send;
    fn event(&self, kind: &str, details: Value) -> impl Future<Output = ()> + Send;
    fn closing(&self) -> bool;
}

#[derive(Debug, Clone, PartialEq)]
pub struct LookupError {
    pub message: String,
    pub timed_out: bool,
}

/// `retryLookup(lookup, { attempts, sleep, onError })`: the first success, or the last failure
/// after `attempts` tries a second apart; each failure is reported with its attempt number.
pub async fn retry_lookup<T, F, Fut, S, SFut, E, EFut>(mut lookup: F, attempts: u32, sleep: S, mut on_error: E) -> Result<T, LookupError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, LookupError>>,
    S: Fn(u64) -> SFut,
    SFut: Future<Output = ()>,
    E: FnMut(LookupError, u32) -> EFut,
    EFut: Future<Output = ()>,
{
    let mut last = LookupError { message: "no attempt".into(), timed_out: false };
    for attempt in 1..=attempts {
        match lookup().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                last = e.clone();
                on_error(e, attempt).await;
                if attempt < attempts {
                    sleep(1000).await;
                }
            }
        }
    }
    Err(last)
}

/// The takeover's plan: adopt a running process, spawn a new one, or wait (`port-busy`,
/// `lookup-failed`, `closing`).
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// `record` and `owner` are set when the process was found on the port, not from the record.
    Adopt { pid: f64, record: Option<Value>, owner: Option<Value> },
    Spawn,
    Wait { reason: &'static str, owner: Option<Value> },
}

/// What is being (re)started: its name, executable, config, the pool root and its port.
pub struct Child<'a> {
    pub name: &'a str,
    pub exe: &'a str,
    pub config: &'a str,
    pub root: &'a str,
    pub port: Option<u16>,
}

/// `planTakeover(...)`: a process is killed only after its identity was read; a failed read is
/// retried and never taken to mean "not ours".
pub async fn plan_takeover<M: Machine>(c: &Child<'_>, record: Option<&Value>, m: &M) -> Plan {
    let log = |kind: &'static str, mut details: Value| async move {
        let mut d = serde_json::Map::new();
        d.insert("name".into(), json!(c.name));
        if let Value::Object(o) = details.take() {
            d.extend(o);
        }
        m.event(kind, Value::Object(d)).await;
    };
    let lookup = |what: &'static str, pid_or_port: f64, by_port: bool| async move {
        retry_lookup(
            || async move { if by_port { m.port_owner(pid_or_port as u16).await } else { m.process_info(pid_or_port).await } },
            LOOKUP_ATTEMPTS,
            |ms| m.sleep(ms),
            |e: LookupError, attempt| async move {
                let message: String = e.message.chars().take(200).collect();
                log("process-lookup-error", json!({ "lookup": what, "attempt": attempt, "timedOut": e.timed_out, "message": message })).await;
            },
        )
        .await
    };
    let mut record_unread = false;
    if let Some(record) = record.filter(|r| js::truthy(Some(*r)) && m.alive(number(r.get("pid")))) {
        let pid = number(record.get("pid"));
        let actual = match lookup("recorded", pid, false).await {
            Ok(a) => a,
            Err(_) => {
                record_unread = true;
                None
            }
        };
        let ours = same_identity(record, actual.as_ref());
        if ours && same_process(record, actual.as_ref(), c.exe, c.config) {
            return Plan::Adopt { pid, record: None, owner: None };
        }
        if ours {
            if m.closing() {
                return Plan::Wait { reason: "closing", owner: None };
            }
            let exe = actual.as_ref().and_then(|a| a.get("ExecutablePath")).cloned().unwrap_or(Value::Null);
            log("process-retired", json!({ "pid": record.get("pid"), "executable": exe })).await;
            m.kill(pid);
            for _ in 0..40 {
                if !m.alive(pid) {
                    break;
                }
                m.sleep(250).await;
            }
        }
    }
    let Some(port) = c.port else { return Plan::Spawn };
    for _ in 0..40 {
        if m.port_free(port).await {
            break;
        }
        m.sleep(250).await;
    }
    if m.port_free(port).await {
        return Plan::Spawn;
    }
    let owner = match lookup("port", f64::from(port), true).await {
        Ok(o) => o,
        Err(_) => return Plan::Wait { reason: "lookup-failed", owner: None },
    };
    if let Some(o) = owner.as_ref().filter(|o| owned_proxy(Some(*o), c.root, c.config)) {
        let owner_record = json!({ "pid": o.get("ProcessId"), "creationDate": o.get("CreationDate") });
        if same_process(&owner_record, Some(o), c.exe, c.config) {
            return Plan::Adopt { pid: number(o.get("ProcessId")), record: Some(owner_record), owner: Some(o.clone()) };
        }
        if m.closing() {
            return Plan::Wait { reason: "closing", owner: Some(o.clone()) };
        }
        log("process-retired", json!({ "pid": o.get("ProcessId"), "executable": o.get("ExecutablePath"), "unrecorded": true })).await;
        m.kill(number(o.get("ProcessId")));
        for _ in 0..40 {
            if m.port_free(port).await {
                break;
            }
            m.sleep(250).await;
        }
    }
    if m.port_free(port).await {
        return Plan::Spawn;
    }
    Plan::Wait { reason: if owner.is_none() && record_unread { "lookup-failed" } else { "port-busy" }, owner }
}
