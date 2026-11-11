//! Opt-in project registration and owned, windowless Claude RC servers. Configuration is
//! protected client state; consent and workspace trust are ALWAYS Claude's own decisions.
#[cfg(test)]
mod tests;
mod tree;

use super::{private, proxy_key};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::mpsc,
};

const REGISTRY: &str = "claude-projects.json";
const STATUS: &str = "claude-project-status.json";
const SCHEMA: &str = "agent-pools-claude-projects/v1";
const MAX_PROJECTS: usize = 16;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub directory: PathBuf,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "worktree")]
    pub spawn: String,
    #[serde(default = "capacity")]
    pub capacity: u8,
}
fn yes() -> bool {
    true
}
fn worktree() -> String {
    "worktree".into()
}
fn capacity() -> u8 {
    4
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Registry {
    schema: String,
    claude_binary: PathBuf,
    claude_config_dir: PathBuf,
    #[serde(default)]
    discover_roots: Vec<PathBuf>,
    #[serde(default)]
    projects: Vec<Project>,
}

fn invalid(message: &str) -> std::io::Error {
    std::io::Error::other(message)
}
fn local_absolute(p: &Path) -> bool {
    p.is_absolute()
        && !p.to_string_lossy().starts_with(r"\\")
        && !p.to_string_lossy().starts_with("//")
}
fn canonical(p: &Path) -> std::io::Result<PathBuf> {
    if !local_absolute(p) {
        return Err(invalid("Use an absolute local path, not a network path"));
    }
    // Windows' verbatim prefix is an implementation detail, not a different project ID.
    let p = std::fs::canonicalize(p)?.to_string_lossy().into_owned();
    let p = PathBuf::from(p.strip_prefix(r"\\?\").unwrap_or(&p));
    if !local_absolute(&p) || p.to_string_lossy().starts_with("UNC\\") {
        return Err(invalid("Resolved path is not local"));
    }
    Ok(p)
}
fn identity(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    if cfg!(windows) { s.to_lowercase() } else { s }
}
fn id(p: &Path) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(identity(p).as_bytes()))[..16].into()
}
fn project(path: &Path, name: Option<&str>) -> std::io::Result<Project> {
    project_mode(path, name, "worktree")
}
fn project_mode(path: &Path, name: Option<&str>, spawn: &str) -> std::io::Result<Project> {
    let directory = canonical(path)?;
    if !directory.is_dir() {
        return Err(invalid("RC projects must be directories"));
    }
    if spawn == "worktree" && !directory.join(".git").exists() {
        return Err(invalid("RC worktree projects must be Git repositories"));
    }
    Ok(Project {
        id: id(&directory),
        name: name
            .unwrap_or_else(|| {
                directory
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("project")
            })
            .into(),
        directory,
        enabled: true,
        spawn: spawn.into(),
        capacity: capacity(),
    })
}
fn validate(r: &Registry) -> std::io::Result<()> {
    if r.schema != SCHEMA
        || !local_absolute(&r.claude_binary)
        || !local_absolute(&r.claude_config_dir)
        || r.projects.len() > MAX_PROJECTS
        || r.discover_roots.len() > 16
        || r.discover_roots.iter().any(|p| !local_absolute(p))
    {
        return Err(invalid(
            "Invalid Claude project registry schema/paths/limits",
        ));
    }
    let mut seen = HashSet::new();
    for p in &r.projects {
        if !local_absolute(&p.directory)
            || p.id != id(&p.directory)
            || !seen.insert(p.id.clone())
            || p.name.is_empty()
            || p.name.len() > 80
            || p.name.chars().any(char::is_control)
            || !matches!(p.spawn.as_str(), "worktree" | "same-dir")
            || !(1..=16).contains(&p.capacity)
        {
            return Err(invalid("Invalid or duplicate Claude project entry"));
        }
    }
    Ok(())
}
fn load(profile: &Path) -> std::io::Result<Registry> {
    let r: Registry = serde_json::from_slice(&std::fs::read(profile.join(REGISTRY))?)
        .map_err(|_| invalid("Invalid Claude project registry JSON"))?;
    validate(&r)?;
    Ok(r)
}
fn save(profile: &Path, r: &Registry) -> std::io::Result<()> {
    validate(r)?;
    private::write(
        &profile.join(REGISTRY),
        &serde_json::to_vec_pretty(r)
            .map_err(|_| invalid("Could not serialize project registry"))?,
    )
}
fn desired(r: &Registry) -> std::io::Result<BTreeMap<String, Project>> {
    let mut found = BTreeMap::new();
    if !r.discover_roots.is_empty() {
        let trust: Value =
            serde_json::from_slice(&std::fs::read(r.claude_config_dir.join(".claude.json"))?)
                .map_err(|_| invalid("Could not read Claude workspace trust"))?;
        for (path, config) in trust["projects"].as_object().into_iter().flatten() {
            if config["hasTrustDialogAccepted"] != true {
                continue;
            }
            let Ok(path) = canonical(Path::new(path)) else {
                continue;
            };
            // A linked worktree's .git is a file. Only automatically register main roots.
            if !path.join(".git").is_dir()
                || !r.discover_roots.iter().any(|root| within(&path, root))
            {
                continue;
            }
            let p = project(&path, None)?;
            found.insert(p.id.clone(), p);
        }
    }
    // Explicit entries, including disabled ones, override discovered entries.
    for p in &r.projects {
        found.insert(p.id.clone(), p.clone());
    }
    found.retain(|_, p| p.enabled);
    if found.len() > MAX_PROJECTS {
        return Err(invalid(
            "Too many discovered RC projects; narrow discovery roots",
        ));
    }
    Ok(found)
}
fn within(path: &Path, root: &Path) -> bool {
    let path = identity(path);
    let root = identity(root);
    path == root || path.starts_with(&format!("{}/", root.trim_end_matches('/')))
}
fn trusted(r: &Registry, p: &Project) -> bool {
    let Ok(bytes) = std::fs::read(r.claude_config_dir.join(".claude.json")) else {
        return false;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    v["projects"]
        .as_object()
        .into_iter()
        .flatten()
        .any(|(path, v)| {
            v["hasTrustDialogAccepted"] == true
                && canonical(Path::new(path))
                    .is_ok_and(|path| identity(&path) == identity(&p.directory))
        })
}

// CLI settings can override inherited environment. Refuse conflicting routing/auth rather
// than silently sending inference outside the pool or rewriting project/user policy.
fn settings_safe(r: &Registry, p: &Project, expected: &BTreeMap<String, String>) -> bool {
    let mut paths = vec![
        r.claude_config_dir.join("settings.json"),
        r.claude_config_dir.join("settings.local.json"),
    ];
    for dir in p.directory.ancestors() {
        paths.push(dir.join(".claude/settings.json"));
        paths.push(dir.join(".claude/settings.local.json"));
    }
    #[cfg(windows)]
    if let Some(dir) = std::env::var_os("ProgramFiles") {
        paths.push(PathBuf::from(dir).join("ClaudeCode/managed-settings.json"));
    }
    paths.into_iter().all(|path| {
        if !path.exists() {
            return true;
        }
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            return false;
        };
        if v.get("apiKeyHelper").is_some_and(|v| !v.is_null()) {
            return false;
        }
        v["env"]
            .as_object()
            .into_iter()
            .flatten()
            .all(|(name, value)| {
                let upper = name.to_ascii_uppercase();
                if let Some(want) = expected.get(&upper) {
                    value.as_str().is_some_and(|value| {
                        if matches!(upper.as_str(), "CLAUDE_CONFIG_DIR" | "NODE_EXTRA_CA_CERTS") {
                            canonical(Path::new(value))
                                .ok()
                                .zip(canonical(Path::new(want)).ok())
                                .is_some_and(|(value, want)| identity(&value) == identity(&want))
                        } else {
                            value == want
                        }
                    })
                } else {
                    !removed_environment()
                        .iter()
                        .any(|n| n.eq_ignore_ascii_case(name))
                }
            })
    })
}

/// Local administration; no keys or changes to Claude's consent/trust files.
pub fn command(args: &[String]) -> std::io::Result<()> {
    if args.len() < 2 {
        return Err(invalid(
            "claude-projects <client-dir> init <claude.exe> <Claude-config-dir> | add <repo> [name] | add-dir <dir> [name] | discover-root <dir> | disable <id> | remove <id> | list | status",
        ));
    }
    let profile = Path::new(&args[0]);
    private::directory(profile)?;
    if args[1] == "status" {
        println!(
            "{}",
            serde_json::to_string_pretty(&status(profile)).unwrap()
        );
        return Ok(());
    }
    if args[1] == "init" {
        if args.len() != 4 || profile.join(REGISTRY).exists() {
            return Err(invalid(
                "init needs executable/config directory and a new registry",
            ));
        }
        let r = Registry {
            schema: SCHEMA.into(),
            claude_binary: canonical(Path::new(&args[2]))?,
            claude_config_dir: canonical(Path::new(&args[3]))?,
            discover_roots: vec![],
            projects: vec![],
        };
        save(profile, &r)?;
        println!("Initialized opt-in Claude project registry; no trust or consent changed.");
        return Ok(());
    }
    let mut r = load(profile)?;
    match args[1].as_str() {
        "add" | "add-dir" if (3..=4).contains(&args.len()) => {
            let p = project_mode(
                Path::new(&args[2]),
                args.get(3).map(String::as_str),
                if args[1] == "add-dir" { "same-dir" } else { "worktree" },
            )?;
            r.projects.retain(|old| old.id != p.id);
            r.projects.push(p);
            save(profile, &r)?;
        }
        "discover-root" if args.len() == 3 => {
            let root = canonical(Path::new(&args[2]))?;
            if !r.discover_roots.contains(&root) {
                r.discover_roots.push(root);
            }
            save(profile, &r)?;
        }
        "disable" | "remove" if args.len() == 3 => {
            if let Some(p) = r.projects.iter_mut().find(|p| p.id == args[2]) {
                p.enabled = false;
            } else if let Some(mut p) = desired(&r)?.remove(&args[2]) {
                p.enabled = false;
                r.projects.push(p);
            } else {
                return Err(invalid("Project ID not registered"));
            }
            // Keep a disabled tombstone so discovery does not silently re-enable removal.
            save(profile, &r)?;
        }
        "list" if args.len() == 2 => {}
        _ => {
            return Err(invalid(
                "Unknown Claude project operation or invalid arguments",
            ));
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"schema":SCHEMA,"projects":desired(&r)?})).unwrap()
    );
    Ok(())
}

pub fn status(profile: &Path) -> Value {
    let mut s = crate::live::read_json(&profile.join(STATUS))
        .unwrap_or_else(|_| json!({"schema":SCHEMA,"state":"not-running","projects":[]}));
    let fresh = s["updatedAtMs"]
        .as_f64()
        .is_some_and(|t| crate::writer::io::now_ms() - t < 20_000.0);
    s["stale"] = json!(!fresh);
    s
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum State {
    Starting,
    Ready,
    NeedsConsent,
    NeedsTrust,
    NeedsLogin,
    NeedsConfiguration,
    NeedsGit,
    Exited,
    StartupTimeout,
    SpawnFailed,
}
impl State {
    fn blocked(self) -> bool {
        matches!(
            self,
            Self::NeedsConsent
                | Self::NeedsTrust
                | Self::NeedsLogin
                | Self::NeedsConfiguration
                | Self::NeedsGit
        )
    }
}
fn observation(text: &str) -> Option<State> {
    let t = text.to_ascii_lowercase();
    if t.contains("worktree mode requires a git repository") {
        Some(State::NeedsGit)
    } else if t.contains("enable remote control?") {
        Some(State::NeedsConsent)
    } else if t.contains("workspace not trusted")
        || t.contains("do you trust")
        || t.contains("trust this")
        || (t.contains("trust ") && t.contains("[y/n]"))
    {
        Some(State::NeedsTrust)
    } else if t.contains("not logged in")
        || t.contains("please run /login")
        || t.contains("requires a claude.ai subscription")
        || t.contains("full-scope login")
    {
        Some(State::NeedsLogin)
    } else if t.contains("https://claude.ai/code/") || t.contains("https://claude.ai/code?") {
        Some(State::Ready)
    } else {
        None
    }
}
async fn observe(mut pipe: impl AsyncRead + Unpin, tx: mpsc::Sender<State>) {
    let mut buf = [0u8; 1024];
    let mut tail = Vec::new();
    let mut last = None;
    while let Ok(n) = pipe.read(&mut buf).await {
        if n == 0 {
            break;
        }
        tail.extend_from_slice(&buf[..n]);
        if tail.len() > 16_384 {
            tail.drain(..tail.len() - 16_384);
        }
        if let Some(s) = observation(&String::from_utf8_lossy(&tail))
            && last != Some(s)
        {
            let _ = tx.try_send(s);
            last = Some(s);
        }
    }
}
fn emit(state: State, pid: Option<u32>) {
    println!("{}", json!({"rcState":state,"pid":pid}));
}

fn removed_environment() -> &'static [&'static str] {
    &[
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_CUSTOM_HEADERS",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
        "AGENT_POOLS_API_KEY",
        "CLAUDE_SPLIT_PROXY_KEY",
        "NODE_TLS_REJECT_UNAUTHORIZED",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
        "DISABLE_GROWTHBOOK",
        "ALL_PROXY",
        "all_proxy",
        "http_proxy",
        "https_proxy",
        "no_proxy",
    ]
}
fn routing_environment(profile: &Path, config: &Path) -> std::io::Result<BTreeMap<String, String>> {
    let c = super::config(profile)?;
    let port = c["port"].as_u64().unwrap();
    let key = proxy_key(profile, false)?;
    let proxy = format!("http://probe:{key}@127.0.0.1:{port}");
    Ok(BTreeMap::from([
        (
            "CLAUDE_CONFIG_DIR".into(),
            config.to_string_lossy().into_owned(),
        ),
        ("HTTP_PROXY".into(), proxy.clone()),
        ("HTTPS_PROXY".into(), proxy),
        ("NO_PROXY".into(), "localhost,127.0.0.1,::1".into()),
        (
            "NODE_EXTRA_CA_CERTS".into(),
            profile
                .join("probe-certs")
                .join("ca.pem")
                .to_string_lossy()
                .into_owned(),
        ),
    ]))
}

fn child_command(r: &Registry, p: &Project, profile: &Path) -> std::io::Result<Command> {
    if canonical(&p.directory)? != p.directory
        || !p.directory.is_dir()
        || (p.spawn == "worktree" && !p.directory.join(".git").exists())
    {
        return Err(invalid("Project directory moved, is not a directory, or worktree mode has no repository"));
    }
    let env = routing_environment(profile, &r.claude_config_dir)?;
    if !settings_safe(r, p, &env) {
        return Err(invalid("Conflicting Claude routing settings"));
    }
    let mut cmd = Command::new(&r.claude_binary);
    cmd.args([
        "remote-control",
        "--spawn",
        &p.spawn,
        "--capacity",
        &p.capacity.to_string(),
        "--permission-mode",
        "default",
        "--no-chrome",
        "--name",
        &format!("{} (Agent Pools)", p.name),
    ])
    .current_dir(&p.directory)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    for name in removed_environment() {
        cmd.env_remove(name);
    }
    cmd.envs(env);
    crate::live::machine::hide_window(&mut cmd);
    Ok(cmd)
}

/// Private process entry point. It cannot launch Claude unless its owning supervisor has
/// first assigned its entire future process tree and released the launch handshake.
pub async fn worker(args: &[String]) -> std::io::Result<()> {
    if args.len() != 2 {
        return Err(invalid("RC worker requires profile and project ID"));
    }
    let mut byte = [0u8; 1];
    tokio::io::stdin().read_exact(&mut byte).await?;
    if byte != *b"R" {
        return Err(invalid("Invalid RC worker handshake"));
    }
    let profile = Path::new(&args[0]);
    let r = load(profile)?;
    let p = desired(&r)?
        .remove(&args[1])
        .ok_or_else(|| invalid("RC project no longer enabled"))?;
    if !trusted(&r, &p) {
        emit(State::NeedsTrust, None);
        return Ok(());
    }
    if !settings_safe(&r, &p, &routing_environment(profile, &r.claude_config_dir)?) {
        emit(State::NeedsConfiguration, None);
        return Ok(());
    }
    let mut child = child_command(&r, &p, profile)?.spawn()?;
    let pid = child.id();
    emit(State::Starting, pid);
    let (tx, mut rx) = mpsc::channel(8);
    let a = tokio::spawn(observe(child.stdout.take().unwrap(), tx.clone()));
    let b = tokio::spawn(observe(child.stderr.take().unwrap(), tx));
    let started = Instant::now();
    let mut ready = false;
    let outcome = loop {
        tokio::select! {
            code = child.wait() => { break code.and_then(|code| if code.success() { Ok(()) } else { Err(invalid("Claude RC process exited unsuccessfully")) }); },
            Some(state) = rx.recv() => {
                emit(state, pid);
                if state.blocked() { break Ok(()); }
                ready |= state == State::Ready;
            },
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                if !ready && started.elapsed() > Duration::from_secs(90) { emit(State::StartupTimeout, pid); break Ok(()); }
            }
        }
    };
    // An exit and a final consent/login line can arrive together. Drain both readers before
    // leaving, so the supervisor reports actionable setup rather than a generic restart.
    if child.try_wait()?.is_some() {
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(state) = rx.recv().await {
                emit(state, pid);
            }
        })
        .await;
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
    a.abort();
    b.abort();
    outcome
}

struct Owned {
    child: Child,
    tree: Option<tree::Tree>,
    events: mpsc::Receiver<(State, Option<u32>)>,
    reader: tokio::task::JoinHandle<()>,
}
impl Owned {
    async fn start(mut cmd: Command) -> std::io::Result<Self> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        crate::live::machine::hide_window(&mut cmd);
        let mut child = cmd.spawn()?;
        let tree = tree::Tree::attach(&child)?;
        child.stdin.take().unwrap().write_all(b"R").await?;
        let (tx, events) = mpsc::channel(8);
        let out = child.stdout.take().unwrap();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // The worker emits only this bounded metadata protocol, never CLI output.
                if line.len() > 2048 {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && let Ok(s) = serde_json::from_value::<State>(v["rcState"].clone())
                {
                    let _ = tx
                        .send((s, v["pid"].as_u64().and_then(|n| u32::try_from(n).ok())))
                        .await;
                }
            }
        });
        Ok(Self {
            child,
            tree: Some(tree),
            events,
            reader,
        })
    }
    async fn stop(&mut self) {
        // Closing the job stops exactly this worker and its descendants, not other CLI use.
        self.tree.take();
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
        self.reader.abort();
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        self.reader.abort();
        self.tree.take();
    }
}
struct Slot {
    project: Project,
    launch: (PathBuf, PathBuf),
    owned: Option<Owned>,
    state: State,
    pid: Option<u32>,
    restarts: u32,
    failures: u32,
    retry: Instant,
    started: Instant,
    last_exit: Option<i32>,
}
fn retry_delay(failures: u32, blocked: bool) -> Duration {
    Duration::from_secs(if blocked {
        60
    } else {
        (5u64 * 2u64.pow(failures.min(6))).min(300)
    })
}

pub async fn supervise(profile: PathBuf, stop: impl Future<Output = ()>) {
    supervise_with(profile, stop, |profile, id| {
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.arg("claude-rc-worker").arg(profile).arg(id);
        Ok(cmd)
    })
    .await;
}

async fn supervise_with(
    profile: PathBuf,
    stop: impl Future<Output = ()>,
    worker: impl Fn(&Path, &str) -> std::io::Result<Command>,
) {
    tokio::pin!(stop);
    let mut slots: BTreeMap<String, Slot> = BTreeMap::new();
    loop {
        let registry = if profile.join(REGISTRY).exists() {
            load(&profile).and_then(|r| desired(&r).map(|p| (r, p)))
        } else {
            Err(invalid("Registry disabled"))
        };
        let (r, desired) = match registry {
            Ok((r, p)) => (Some(r), p),
            Err(_) => (None, BTreeMap::new()),
        };
        let removed: Vec<_> = slots
            .iter()
            .filter(|(id, s)| {
                desired.get(*id) != Some(&s.project)
                    || r.as_ref().is_none_or(|r| {
                        s.launch != (r.claude_binary.clone(), r.claude_config_dir.clone())
                    })
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in removed {
            if let Some(mut s) = slots.remove(&id)
                && let Some(ref mut c) = s.owned
            {
                c.stop().await;
            }
        }
        if let Some(ref r) = r {
            for (id, p) in desired {
                slots.entry(id).or_insert_with(|| Slot {
                    project: p,
                    launch: (r.claude_binary.clone(), r.claude_config_dir.clone()),
                    owned: None,
                    state: State::Starting,
                    pid: None,
                    restarts: 0,
                    failures: 0,
                    retry: Instant::now(),
                    started: Instant::now(),
                    last_exit: None,
                });
            }
        }
        for (id, s) in &mut slots {
            if let Some(ref mut c) = s.owned {
                while let Ok((state, pid)) = c.events.try_recv() {
                    s.state = state;
                    s.pid = pid;
                }
                match c.child.try_wait() {
                    Ok(None) => continue,
                    Ok(Some(code)) => s.last_exit = code.code(),
                    Err(_) => s.last_exit = None,
                }
                let _ = tokio::time::timeout(Duration::from_secs(1), &mut c.reader).await;
                while let Ok((state, pid)) = c.events.try_recv() {
                    s.state = state;
                    s.pid = pid;
                }
                let blocked = s.state.blocked();
                if !blocked && s.state != State::StartupTimeout {
                    s.state = State::Exited;
                }
                if s.started.elapsed() > Duration::from_secs(300) {
                    s.failures = 0;
                }
                s.failures = s.failures.saturating_add(1);
                s.retry = Instant::now() + retry_delay(s.failures, blocked);
                s.owned.take();
                s.pid = None;
            }
            if Instant::now() < s.retry {
                continue;
            }
            s.restarts += 1;
            let result = match worker(&profile, id) {
                Ok(cmd) => Owned::start(cmd).await,
                Err(e) => Err(e),
            };
            match result {
                Ok(c) => {
                    s.owned = Some(c);
                    s.state = State::Starting;
                    s.started = Instant::now();
                }
                Err(_) => {
                    s.state = State::SpawnFailed;
                    s.failures = s.failures.saturating_add(1);
                    s.retry = Instant::now() + retry_delay(s.failures, false);
                }
            }
        }
        let rows: Vec<_> = slots.iter().map(|(id,s)| json!({"id":id,"name":s.project.name,"directory":s.project.directory,"spawn":s.project.spawn,"capacity":s.project.capacity,"state":s.state,"pid":s.pid,"workerPid":s.owned.as_ref().and_then(|c|c.child.id()),"starts":s.restarts,"lastExitCode":s.last_exit})).collect();
        if profile.join(REGISTRY).exists() {
            let doc = json!({"schema":SCHEMA,"ownerPid":std::process::id(),"updatedAtMs":crate::writer::io::now_ms(),"state":if r.is_some(){"running"}else{"registry-invalid"},"projects":rows});
            let _ = private::write(&profile.join(STATUS), doc.to_string().as_bytes());
        }
        tokio::select! { _ = &mut stop => break, _ = tokio::time::sleep(Duration::from_secs(3)) => {} }
    }
    for s in slots.values_mut() {
        if let Some(ref mut c) = s.owned {
            c.stop().await;
        }
    }
    let _ = private::write(&profile.join(STATUS), json!({"schema":SCHEMA,"state":"stopped","updatedAtMs":crate::writer::io::now_ms(),"projects":[]}).to_string().as_bytes());
}
