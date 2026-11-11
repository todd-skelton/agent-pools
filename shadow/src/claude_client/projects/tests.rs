use super::*;

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

#[cfg(windows)]
#[tokio::test]
async fn job_close_kills_owned_worker_and_descendants() {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };
    let mut cmd = Command::new("powershell.exe");
    cmd.args(["-NoProfile","-NonInteractive","-Command", "$null=[Console]::Read(); $p=Start-Process powershell.exe -WindowStyle Hidden -ArgumentList '-NoProfile -NonInteractive -Command Start-Sleep -Seconds 60' -PassThru; [Console]::WriteLine($p.Id); Start-Sleep -Seconds 60"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
    crate::live::machine::hide_window(&mut cmd);
    let mut child = cmd.spawn().unwrap();
    let tree = tree::Tree::attach(&child).unwrap();
    child.stdin.take().unwrap().write_all(b"R").await.unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let pid: u32 = tokio::time::timeout(Duration::from_secs(15), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .parse()
        .unwrap();
    // SAFETY: this handle refers only to the live descendant just reported by our test.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    assert!(!handle.is_null());
    drop(tree);
    tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    // SAFETY: the live process handle above is waited on and closed exactly once.
    let result = unsafe {
        let r = WaitForSingleObject(handle, 5000);
        CloseHandle(handle);
        r
    };
    assert_eq!(result, 0, "owned descendant survived job close");
}

#[cfg(windows)]
#[tokio::test]
async fn owned_stop_and_drop_reap_workers() {
    for explicit_stop in [true, false] {
        let mut cmd = Command::new("powershell.exe");
        cmd.args(["-NoProfile","-NonInteractive","-Command", "$null=[Console]::Read(); [Console]::WriteLine('{\"rcState\":\"ready\",\"pid\":123}'); Start-Sleep -Seconds 60"]);
        let mut owned = Owned::start(cmd).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(15), owned.events.recv())
                .await
                .unwrap(),
            Some((State::Ready, Some(123)))
        );
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
        };
        // SAFETY: the process is our own live test worker.
        let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, owned.child.id().unwrap()) };
        assert!(!h.is_null());
        if explicit_stop {
            owned.stop().await;
        }
        drop(owned);
        // SAFETY: the handle above is waited on and closed exactly once.
        assert_eq!(
            unsafe {
                let r = WaitForSingleObject(h, 5000);
                CloseHandle(h);
                r
            },
            0
        );
    }
}

#[cfg(windows)]
#[tokio::test]
async fn supervisor_restarts_crashes_and_stops_removed_projects() {
    let t = Temp::new();
    let mut r = t.registry();
    r.projects.push(project(&t.repo("a"), None).unwrap());
    save(&t.0, &r).unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let profile = t.0.clone();
    let task = tokio::spawn(supervise_with(
        profile,
        async {
            let _ = rx.await;
        },
        |_, _| {
            let mut cmd = Command::new("powershell.exe");
            cmd.args(["-NoProfile","-NonInteractive","-Command","$null=[Console]::Read(); [Console]::WriteLine(('{\"rcState\":\"ready\",\"pid\":'+$PID+'}')); Start-Sleep -Seconds 60"]);
            Ok(cmd)
        },
    ));
    async fn wait_for(profile: &Path, starts: u64) -> u32 {
        tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                let s = status(profile);
                if s["projects"][0]["state"] == "ready"
                    && s["projects"][0]["starts"].as_u64().unwrap_or(0) >= starts
                {
                    return s["projects"][0]["workerPid"].as_u64().unwrap() as u32;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap()
    }
    let pid = wait_for(&t.0, 1).await;
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess,
            WaitForSingleObject,
        },
    };
    // SAFETY: this is the test's own live fake worker reported by its supervisor.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        assert!(!h.is_null());
        assert_ne!(TerminateProcess(h, 1), 0);
        CloseHandle(h);
    }
    let next = wait_for(&t.0, 2).await;
    assert_ne!(pid, next);
    // SAFETY: own live replacement worker; the handle survives process exit for the check.
    let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, next) };
    assert!(!h.is_null());
    r.projects[0].enabled = false;
    save(&t.0, &r).unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        while status(&t.0)["projects"]
            .as_array()
            .is_none_or(|p| !p.is_empty())
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    // SAFETY: handle above is waited on and closed exactly once.
    assert_eq!(
        unsafe {
            let result = WaitForSingleObject(h, 5000);
            CloseHandle(h);
            result
        },
        0
    );
    tx.send(()).unwrap();
    task.await.unwrap();
    assert_eq!(status(&t.0)["state"], "stopped");
}
