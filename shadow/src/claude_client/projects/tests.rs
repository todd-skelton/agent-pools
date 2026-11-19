use super::*;
use tokio::io::AsyncWriteExt;

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("pools-rc-test-{}", uuid::Uuid::new_v4()));
        private::directory(&path).unwrap();
        Self(canonical(&path).unwrap())
    }
    fn repo(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir_all(path.join(".git")).unwrap();
        path
    }
    fn registry(&self) -> Registry {
        Registry {
            schema: SCHEMA.into(),
            claude_binary: std::env::current_exe().unwrap(),
            claude_config_dir: self.0.clone(),
            discover_roots: vec![],
            projects: vec![],
        }
    }
    fn trust(&self, paths: &[(&Path, bool)]) {
        let mut projects = serde_json::Map::new();
        for (p, trusted) in paths {
            projects.insert(
                p.to_string_lossy().into_owned(),
                json!({"hasTrustDialogAccepted":trusted}),
            );
        }
        std::fs::write(
            self.0.join(".claude.json"),
            json!({"projects":projects}).to_string(),
        )
        .unwrap();
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        // A newly generated test-only absolute directory, never user state.
        assert!(
            self.0
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("pools-rc-test-")
        );
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn discovery_requires_trust_main_repository_and_allowed_root() {
    let t = Temp::new();
    let a = t.repo("allowed/a");
    let b = t.repo("allowed/b");
    let outside = t.repo("outside");
    let linked = t.0.join("allowed/worktree");
    std::fs::create_dir_all(&linked).unwrap();
    std::fs::write(linked.join(".git"), "gitdir: elsewhere").unwrap();
    t.trust(&[(&a, true), (&b, false), (&outside, true), (&linked, true)]);
    let mut r = t.registry();
    r.discover_roots.push(t.0.join("allowed"));
    let found = desired(&r).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found.values().next().unwrap().directory, a);
    assert!(trusted(&r, found.values().next().unwrap()));
    assert!(!trusted(&r, &project(&b, None).unwrap()));
}

#[test]
fn trust_of_a_parent_folder_covers_projects_beneath_it() {
    let t = Temp::new();
    let parent = t.0.join("repos");
    let inside = parent.join("controller");
    let sibling = t.0.join("repos-other");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    let r = t.registry();
    let p = project_mode(&inside, None, "same-dir").unwrap();
    let s = project_mode(&sibling, None, "same-dir").unwrap();
    t.trust(&[(&parent, false)]);
    assert!(!trusted(&r, &p));
    t.trust(&[(&parent, true)]);
    assert!(trusted(&r, &p));
    assert!(!trusted(&r, &s), "A shared name prefix is not an ancestor");
}

#[test]
fn explicit_shared_containers_do_not_require_git_or_grant_trust() {
    let t = Temp::new();
    let path = t.0.join("container");
    std::fs::create_dir_all(&path).unwrap();
    let p = project_mode(&path, Some("Controller"), "same-dir").unwrap();
    assert_eq!(p.spawn, "same-dir");
    assert!(project(&path, None).is_err());
    let mut r = t.registry();
    t.trust(&[(&path, false)]);
    let trust_before = std::fs::read(t.0.join(".claude.json")).unwrap();
    r.discover_roots.push(t.0.clone());
    r.projects.push(p.clone());
    save(&t.0, &r).unwrap();
    assert_eq!(load(&t.0).unwrap(), r);
    assert_eq!(desired(&r).unwrap().get(&p.id), Some(&p));
    assert!(!trusted(&r, &p));
    assert_eq!(std::fs::read(t.0.join(".claude.json")).unwrap(), trust_before);
    r.projects.clear();
    t.trust(&[(&path, true)]);
    assert!(desired(&r).unwrap().is_empty(), "Non-Git folders require explicit opt-in even when trusted");
    let file = t.0.join("not-a-directory");
    std::fs::write(&file, "fixture").unwrap();
    assert!(project_mode(&file, None, "same-dir").is_err());
}

#[test]
fn disabled_explicit_entry_overrides_discovery_and_round_trips() {
    let t = Temp::new();
    let a = t.repo("a");
    t.trust(&[(&a, true)]);
    let mut r = t.registry();
    r.discover_roots.push(t.0.clone());
    let mut p = project(&a, None).unwrap();
    p.enabled = false;
    r.projects.push(p);
    save(&t.0, &r).unwrap();
    assert_eq!(load(&t.0).unwrap(), r);
    assert!(desired(&r).unwrap().is_empty());
}

#[test]
fn validates_paths_bounds_ids_duplicates_and_unknown_fields() {
    let t = Temp::new();
    let mut r = t.registry();
    r.projects.push(project(&t.repo("a"), None).unwrap());
    assert!(validate(&r).is_ok());
    let mut bad = r.clone();
    bad.projects[0].capacity = 0;
    assert!(validate(&bad).is_err());
    let mut bad = r.clone();
    bad.projects[0].name = "bad\nname".into();
    assert!(validate(&bad).is_err());
    let mut bad = r.clone();
    bad.projects[0].id = "wrong".into();
    assert!(validate(&bad).is_err());
    let mut bad = r.clone();
    bad.projects.push(bad.projects[0].clone());
    assert!(validate(&bad).is_err());
    let mut bad = r.clone();
    bad.claude_binary = "relative".into();
    assert!(validate(&bad).is_err());
    let mut bad = r.clone();
    bad.projects[0].spawn = "session".into();
    assert!(validate(&bad).is_err());
    let mut bad = r.clone();
    bad.discover_roots = vec![t.0.clone(); 17];
    assert!(validate(&bad).is_err());
    let mut value = serde_json::to_value(r).unwrap();
    value["unknown"] = json!(true);
    assert!(serde_json::from_value::<Registry>(value).is_err());
    assert!(!local_absolute(Path::new(r"\\server\share")));
    assert!(!within(&t.0.join("allowed-other"), &t.0.join("allowed")));
}

#[test]
fn configuration_conflicts_fail_closed_without_mutating_settings() {
    let t = Temp::new();
    let r = t.registry();
    let p = project(&t.repo("a"), None).unwrap();
    let expected = BTreeMap::from([("HTTPS_PROXY".into(), "http://local".into())]);
    assert!(settings_safe(&r, &p, &expected));
    let path = t.0.join("settings.json");
    for value in [
        json!({"env":{"ANTHROPIC_API_KEY":"test-only"}}),
        json!({"env":{"https_proxy":"other"}}),
        json!({"apiKeyHelper":"echo test-only"}),
    ] {
        let bytes = value.to_string();
        std::fs::write(&path, &bytes).unwrap();
        assert!(!settings_safe(&r, &p, &expected));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
    }
    std::fs::write(
        &path,
        json!({"env":{"HTTPS_PROXY":"http://local"},"permissions":{"defaultMode":"default"}})
            .to_string(),
    )
    .unwrap();
    assert!(settings_safe(&r, &p, &expected));
}

#[cfg(windows)]
#[test]
fn equivalent_windows_certificate_paths_are_not_configuration_conflicts() {
    let t = Temp::new();
    let r = t.registry();
    let p = project(&t.repo("a"), None).unwrap();
    let ca = t.0.join("ca.pem");
    std::fs::write(&ca, "fixture").unwrap();
    let expected = BTreeMap::from([(
        "NODE_EXTRA_CA_CERTS".into(),
        ca.to_string_lossy().into_owned(),
    )]);
    let alternate = ca.to_string_lossy().replace('\\', "/").to_uppercase();
    std::fs::write(
        t.0.join("settings.json"),
        json!({"env":{"NODE_EXTRA_CA_CERTS":alternate}}).to_string(),
    )
    .unwrap();
    assert!(settings_safe(&r, &p, &expected));
    std::fs::write(
        t.0.join("settings.json"),
        json!({"env":{"NODE_EXTRA_CA_CERTS":t.0.join("other.pem")}}).to_string(),
    )
    .unwrap();
    assert!(!settings_safe(&r, &p, &expected));
}

#[tokio::test]
async fn detects_split_unterminated_consent_and_never_emits_raw_output() {
    let (mut write, read) = tokio::io::duplex(64);
    let (tx, mut rx) = mpsc::channel(8);
    let task = tokio::spawn(observe(read, tx));
    write
        .write_all(b"secret-value Enable Remote ")
        .await
        .unwrap();
    write.write_all(b"Control? (y/n)").await.unwrap();
    drop(write);
    task.await.unwrap();
    assert_eq!(rx.recv().await, Some(State::NeedsConsent));
    assert_eq!(
        observation("https://claude.ai/code/session_test"),
        Some(State::Ready)
    );
    assert_eq!(observation("Go to claude.ai/code"), None);
    assert_eq!(
        observation("Workspace not trusted"),
        Some(State::NeedsTrust)
    );
    assert_eq!(observation("Please run /login"), Some(State::NeedsLogin));
    assert!(
        !serde_json::to_string(&State::NeedsConsent)
            .unwrap()
            .contains("secret")
    );
}

#[test]
fn retries_are_bounded_and_setup_failures_are_slow() {
    assert_eq!(retry_delay(1, false), Duration::from_secs(10));
    assert_eq!(retry_delay(u32::MAX, false), Duration::from_secs(300));
    assert_eq!(retry_delay(1, true), Duration::from_secs(60));
}

#[test]
fn recognizes_environment_urls_and_actionable_git_errors() {
    assert_eq!(
        observation("Connect: https://claude.ai/code?environment_id=fixture"),
        Some(State::Ready)
    );
    assert_eq!(
        observation("Open https://claude.ai/code to learn more"),
        None
    );
    assert_eq!(
        observation(
            "Error: Worktree mode requires a git repository or WorktreeCreate hooks configured."
        ),
        Some(State::NeedsGit)
    );
    assert!(State::NeedsGit.blocked());
}

#[test]
fn child_uses_normal_permissions_and_removes_gateway_auth() {
    let t = Temp::new();
    let r = t.registry();
    let p = project(&t.repo("a"), None).unwrap();
    std::fs::write(
        t.0.join("splitter.json"),
        json!({"schema":"claude-splitter/v1","poolOrigin":"http://localhost:8330","port":8326})
            .to_string(),
    )
    .unwrap();
    std::fs::write(
        t.0.join("proxy-key.txt"),
        "test-key-with-at-least-thirty-two-characters",
    )
    .unwrap();
    let cmd = child_command(&r, &p, &t.0).unwrap();
    let args: Vec<_> = cmd
        .as_std()
        .get_args()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    assert!(
        args.windows(2)
            .any(|w| w == ["--permission-mode", "default"])
    );
    assert!(args.windows(2).any(|w| w == ["--spawn", "worktree"]));
    assert!(
        !args
            .iter()
            .any(|s| s.contains("bypass") || s.contains("test-key"))
    );
    let env: BTreeMap<_, _> = cmd.as_std().get_envs().collect();
    assert_eq!(
        env.get(std::ffi::OsStr::new("ANTHROPIC_API_KEY")),
        Some(&None)
    );
    assert!(env.iter().any(|(name, value)| {
        name.to_string_lossy().eq_ignore_ascii_case("HTTPS_PROXY") && value.is_some()
    }));
    let directory = t.0.join("non-git-container");
    std::fs::create_dir_all(&directory).unwrap();
    let mut shared = project_mode(&directory, None, "same-dir").unwrap();
    let cmd = child_command(&r, &shared, &t.0).unwrap();
    assert_eq!(cmd.as_std().get_current_dir(), Some(directory.as_path()));
    let args: Vec<_> = cmd.as_std().get_args().map(|s| s.to_string_lossy().into_owned()).collect();
    assert!(args.windows(2).any(|w| w == ["--spawn", "same-dir"]));
    assert!(args.windows(2).any(|w| w == ["--permission-mode", "default"]));
    shared.spawn = "worktree".into();
    assert!(child_command(&r, &shared, &t.0).is_err());
}


/// Run only by `worker_job_ends_its_descendants`, in its own process: the job it takes would
/// otherwise end the test runner with it.
#[cfg(windows)]
#[test]
#[ignore = "helper process for worker_job_ends_its_descendants"]
fn helper_tree_owner() {
    let _tree = tree::Tree::current().unwrap();
    let mut cmd = std::process::Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 60"]);
    let mut child = cmd.spawn().unwrap();
    println!("descendant={}", child.id());
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    std::thread::sleep(Duration::from_secs(60));
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(windows)]
#[tokio::test]
async fn worker_job_ends_its_descendants() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--ignored", "--exact", "claude_client::projects::tests::helper_tree_owner", "--nocapture", "--test-threads=1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    crate::live::machine::hide_window(&mut cmd);
    let mut helper = cmd.spawn().unwrap();
    let mut lines = BufReader::new(helper.stdout.take().unwrap()).lines();
    let pid: u32 = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let line = lines.next_line().await.unwrap().expect("helper ended before its descendant");
            if let Some(pid) = line.split("descendant=").nth(1) {
                return pid.trim().parse().unwrap();
            }
        }
    })
    .await
    .unwrap();
    // SAFETY: this handle refers only to the live descendant just reported by our helper.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    assert!(!handle.is_null());
    helper.start_kill().unwrap();
    helper.wait().await.unwrap();
    // SAFETY: the handle above is waited on and closed exactly once.
    let result = unsafe {
        let r = WaitForSingleObject(handle, 5000);
        CloseHandle(handle);
        r
    };
    assert_eq!(result, 0, "descendant survived its worker");
}

/// A stand-in worker: reports `ready` the way `worker` does and keeps running.
#[cfg(windows)]
fn fake_worker(profile: &Path, id: &str) -> std::io::Result<Command> {
    let state = state_path(profile, id);
    let mut cmd = Command::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &format!(
            "$null='claude-rc-worker {id}'; [IO.File]::WriteAllText('{}', (@{{rcState='ready';pid=$PID;workerPid=$PID}} | ConvertTo-Json -Compress)); Start-Sleep -Seconds 120",
            state.display()
        ),
    ]);
    Ok(cmd)
}

#[cfg(windows)]
async fn wait_for_row(profile: &Path, what: &str, ok: impl Fn(&Value) -> bool) -> Value {
    // A replacement waits out its 10 s backoff, then a PowerShell start, which a busy machine
    // running the whole suite can make slow.
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let s = status(profile);
            if s["state"] == "running" && ok(&s["projects"]) {
                return s["projects"].clone();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}: {}", status(profile)))
}

#[cfg(windows)]
#[tokio::test]
async fn supervisor_restarts_crashes_hands_workers_over_and_stops_removed_projects() {
    use crate::live::machine::{alive, kill};
    let t = Temp::new();
    let mut r = t.registry();
    r.projects.push(project(&t.repo("a"), None).unwrap());
    save(&t.0, &r).unwrap();
    let start = |profile: PathBuf| {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(supervise_with(profile, async { let _ = rx.await; }, fake_worker));
        (tx, task)
    };
    let ready = |starts: u64| move |p: &Value| p[0]["state"] == "ready" && p[0]["starts"].as_u64() >= Some(starts) && p[0]["workerPid"].is_u64();
    let (tx, task) = start(t.0.clone());
    let first = wait_for_row(&t.0, "first worker", ready(1)).await[0]["workerPid"].as_u64().unwrap() as u32;

    // A crashed worker is started again.
    assert!(kill(first));
    let rows = wait_for_row(&t.0, "replacement", ready(2)).await;
    let second = rows[0]["workerPid"].as_u64().unwrap() as u32;
    assert_ne!(first, second);

    // Stopping the supervisor (a service restart) leaves the worker and its sessions running.
    tx.send(()).unwrap();
    task.await.unwrap();
    assert_eq!(status(&t.0)["state"], "stopped");
    assert!(alive(second), "a supervisor stop ended its worker");

    // The next supervisor adopts it instead of starting another.
    let (tx, task) = start(t.0.clone());
    let rows = wait_for_row(&t.0, "adoption", |p| p[0]["adopted"] == true && p[0]["state"] == "ready").await;
    assert_eq!(rows[0]["workerPid"].as_u64(), Some(u64::from(second)));
    assert_eq!(rows[0]["starts"], 0);

    // Removing the project ends the adopted worker.
    r.projects[0].enabled = false;
    save(&t.0, &r).unwrap();
    wait_for_row(&t.0, "removal", |p| p.as_array().is_some_and(Vec::is_empty)).await;
    assert!(!alive(second), "removed project's worker still running");
    tx.send(()).unwrap();
    task.await.unwrap();
}

/// A record for a worker started for another configuration, or a process that is not that
/// worker any more, is never adopted.
#[cfg(windows)]
#[tokio::test]
async fn reconfigured_or_reused_workers_are_not_adopted() {
    use crate::live::machine::alive;
    let t = Temp::new();
    let mut r = t.registry();
    let p = project(&t.repo("a"), None).unwrap();
    r.projects.push(p.clone());
    save(&t.0, &r).unwrap();
    let mut stale = spawn(fake_worker(&t.0, &p.id).unwrap(), "another-launch".into()).unwrap();
    let stale_pid = stale.pid;
    // Someone else's process under a recorded pid: wrong creation date, so never ended.
    let bystander = std::process::id();
    let records = json!({"schema":SCHEMA,"workers":{
        p.id.clone(): stale.record(),
        "0123456789abcdef": {"pid":bystander,"creationDate":"20000101000000.000000+000","launch":"x"},
    }});
    std::fs::write(t.0.join(WORKERS), records.to_string()).unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(supervise_with(t.0.clone(), async { let _ = rx.await; }, fake_worker));
    let rows = wait_for_row(&t.0, "a fresh worker", |p| p[0]["state"] == "ready" && p[0]["starts"] == 1).await;
    assert_eq!(rows[0]["adopted"], false);
    assert_ne!(rows[0]["workerPid"].as_u64(), Some(u64::from(stale_pid)));
    assert!(!alive(stale_pid), "a worker for another configuration was left running");
    let _ = stale.child.take();
    r.projects[0].enabled = false;
    save(&t.0, &r).unwrap();
    wait_for_row(&t.0, "removal", |p| p.as_array().is_some_and(Vec::is_empty)).await;
    tx.send(()).unwrap();
    task.await.unwrap();
}
