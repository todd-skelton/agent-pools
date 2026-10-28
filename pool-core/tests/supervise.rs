//! Agent Pools' own tests of health-policy.mjs (platform.test.mjs) and proxy-takeover.mjs
//! (proxy-takeover.test.mjs, Agent Pools 5c696f6), over the same simulated machine.

use std::collections::HashMap;
use std::sync::Mutex;

use pool_core::supervise::*;
use serde_json::{Value, json};

const ROOT: &str = "D:\\Pool";
const CONFIG: &str = "D:\\Pool\\proxy.yaml";
const CREATED: &str = "2026-10-16T08:58:09.406-05:00";

fn build(v: &str) -> String {
    format!("D:\\Pool\\proxy\\{v}\\cli-proxy-api.exe")
}

fn proc_(pid: u64, v: &str, created: &str) -> Value {
    json!({ "ProcessId": pid, "CreationDate": created, "ExecutablePath": build(v), "CommandLine": format!("{} -config {CONFIG}", build(v)) })
}

fn timeout() -> LookupError {
    LookupError { message: "Command failed: powershell.exe".into(), timed_out: true }
}

/// A simulated machine: processes by PID, one port holder, lookups that fail on demand. Killing
/// a process frees the port it holds.
#[derive(Default)]
struct World {
    live: Mutex<HashMap<u64, Value>>,
    holder: Mutex<Option<u64>>,
    info_failures: Mutex<HashMap<u64, u32>>,
    owner_failures: Mutex<u32>,
    /// The port lookup answers but finds no listener on 127.0.0.1.
    owner_none: bool,
    killed: Mutex<Vec<u64>>,
    events: Mutex<Vec<Value>>,
    info_calls: Mutex<u32>,
    owner_calls: Mutex<u32>,
    closing: bool,
}

impl World {
    fn new(procs: &[(u64, Value)], holder: Option<u64>) -> World {
        World { live: Mutex::new(procs.iter().cloned().collect()), holder: Mutex::new(holder), ..Default::default() }
    }
    fn events_of(&self, kind: &str) -> Vec<Value> {
        self.events.lock().unwrap().iter().filter(|e| e["type"] == kind).cloned().collect()
    }
}

impl Machine for World {
    fn alive(&self, pid: f64) -> bool {
        self.live.lock().unwrap().contains_key(&(pid as u64))
    }
    fn kill(&self, pid: f64) {
        let pid = pid as u64;
        self.killed.lock().unwrap().push(pid);
        self.live.lock().unwrap().remove(&pid);
        let mut h = self.holder.lock().unwrap();
        if *h == Some(pid) {
            *h = None;
        }
    }
    async fn port_free(&self, _: u16) -> bool {
        self.holder.lock().unwrap().is_none()
    }
    async fn process_info(&self, pid: f64) -> Result<Option<Value>, LookupError> {
        *self.info_calls.lock().unwrap() += 1;
        let pid = pid as u64;
        let mut fails = self.info_failures.lock().unwrap();
        if let Some(n) = fails.get_mut(&pid).filter(|n| **n > 0) {
            *n -= 1;
            return Err(timeout());
        }
        Ok(self.live.lock().unwrap().get(&pid).cloned())
    }
    async fn port_owner(&self, _: u16) -> Result<Option<Value>, LookupError> {
        *self.owner_calls.lock().unwrap() += 1;
        if self.owner_none {
            return Ok(None);
        }
        let mut f = self.owner_failures.lock().unwrap();
        if *f > 0 {
            *f -= 1;
            return Err(timeout());
        }
        let Some(h) = *self.holder.lock().unwrap() else { return Ok(None) };
        Ok(Some(self.live.lock().unwrap().get(&h).cloned().unwrap_or_else(|| json!({ "ProcessId": h, "ExecutablePath": "C:\\other\\app.exe", "CommandLine": "app.exe" }))))
    }
    async fn sleep(&self, _: u64) {}
    async fn event(&self, kind: &str, details: Value) {
        let mut e = json!({ "type": kind });
        e.as_object_mut().unwrap().extend(details.as_object().unwrap().clone());
        self.events.lock().unwrap().push(e);
    }
    fn closing(&self) -> bool {
        self.closing
    }
}

fn record() -> Value {
    json!({ "pid": 13724, "creationDate": CREATED })
}

async fn plan(w: &World, record: Option<&Value>) -> Plan {
    let exe = build("pool.25");
    let child = Child { name: "proxy", exe: &exe, config: CONFIG, root: ROOT, port: Some(8317) };
    plan_takeover(&child, record, w).await
}

#[tokio::test]
async fn deploy_a_recorded_old_build_is_retired_even_when_its_first_identity_reads_time_out() {
    let w = World::new(&[(13724, proc_(13724, "pool.24", CREATED))], Some(13724));
    w.info_failures.lock().unwrap().insert(13724, 2);
    assert_eq!(plan(&w, Some(&record())).await, Plan::Spawn);
    assert_eq!(*w.killed.lock().unwrap(), [13724]);
    let errors = w.events_of("process-lookup-error");
    assert_eq!(errors.len(), 2);
    assert!(w.events_of("port-busy").is_empty());
    assert_eq!(errors[0]["timedOut"], true);
}

#[tokio::test]
async fn deploy_when_the_recorded_read_keeps_failing_the_port_holder_is_identified_and_retired_instead() {
    let w = World::new(&[(13724, proc_(13724, "pool.24", CREATED))], Some(13724));
    w.info_failures.lock().unwrap().insert(13724, 3);
    assert_eq!(plan(&w, Some(&record())).await, Plan::Spawn);
    assert_eq!(*w.killed.lock().unwrap(), [13724]);
    assert!(w.events_of("process-retired").iter().any(|e| e["unrecorded"] == true));
}

#[tokio::test]
async fn an_unidentifiable_holder_is_never_killed_and_is_reported_as_lookup_failed_not_port_busy() {
    for owner_failures in [3, 0] {
        let mut w = World::new(&[(13724, proc_(13724, "pool.24", CREATED))], Some(13724));
        w.info_failures.lock().unwrap().insert(13724, 99);
        *w.owner_failures.lock().unwrap() = owner_failures;
        w.owner_none = owner_failures == 0;
        let p = plan(&w, Some(&record())).await;
        assert!(matches!(p, Plan::Wait { reason: "lookup-failed", .. }), "{p:?}");
        assert!(w.killed.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_port_held_by_something_that_is_not_this_pools_proxy_is_port_busy_and_left_alone() {
    let w = World::new(&[], Some(999));
    match plan(&w, None).await {
        Plan::Wait { reason: "port-busy", owner: Some(o) } => assert_eq!(o["ProcessId"], 999),
        p => panic!("{p:?}"),
    }
    assert!(w.killed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_selected_build_is_adopted_from_the_record_or_from_the_port() {
    let recorded = World::new(&[(13724, proc_(13724, "pool.25", CREATED))], Some(13724));
    assert_eq!(plan(&recorded, Some(&record())).await, Plan::Adopt { pid: 13724.0, record: None, owner: None });
    let unrecorded = World::new(&[(13724, proc_(13724, "pool.25", CREATED))], Some(13724));
    match plan(&unrecorded, None).await {
        Plan::Adopt { record: Some(r), .. } => assert_eq!(r, record()),
        p => panic!("{p:?}"),
    }
    assert!(recorded.killed.lock().unwrap().is_empty() && unrecorded.killed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_reused_pid_is_not_killed_through_the_record() {
    let mut p = proc_(13724, "pool.24", "2026-10-18T19:00:00.000-05:00");
    p["ExecutablePath"] = json!("C:\\other\\app.exe");
    p["CommandLine"] = json!("app.exe");
    let w = World::new(&[(13724, p)], Some(13724));
    assert!(matches!(plan(&w, Some(&record())).await, Plan::Wait { reason: "port-busy", .. }));
    assert!(w.killed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_dead_recorded_pid_is_not_looked_up_and_a_free_port_spawns_at_once() {
    let w = World::new(&[], None);
    assert_eq!(plan(&w, Some(&record())).await, Plan::Spawn);
    assert_eq!((*w.info_calls.lock().unwrap(), *w.owner_calls.lock().unwrap()), (0, 0));
}

#[tokio::test]
async fn nothing_is_retired_once_the_supervisor_is_closing() {
    let mut w = World::new(&[(13724, proc_(13724, "pool.24", CREATED))], Some(13724));
    w.closing = true;
    assert!(matches!(plan(&w, Some(&record())).await, Plan::Wait { .. }));
    assert!(w.killed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn retry_lookup_returns_the_first_success_and_rethrows_the_last_failure() {
    let calls = Mutex::new(0);
    let ok = retry_lookup(
        || async {
            let mut c = calls.lock().unwrap();
            *c += 1;
            if *c < 3 { Err(timeout()) } else { Ok("ok") }
        },
        LOOKUP_ATTEMPTS,
        |_| async {},
        |_, _| async {},
    )
    .await;
    assert_eq!(ok, Ok("ok"));
    *calls.lock().unwrap() = 0;
    let failed: Result<(), _> = retry_lookup(
        || async {
            let mut c = calls.lock().unwrap();
            *c += 1;
            Err(LookupError { message: format!("fail {c}"), timed_out: false })
        },
        LOOKUP_ATTEMPTS,
        |_| async {},
        |_, _| async {},
    )
    .await;
    assert_eq!(failed.unwrap_err().message, "fail 3");
}

const NOW: &str = "2026-09-13T02:00:00.000Z";

/// platform.test.mjs: the health policy and process identity.
#[test]
fn consecutive_failures_restart_and_identity_is_pid_and_creation_time() {
    let mut restart = HealthPolicy::default();
    assert!(!restart.check(false));
    assert!(!restart.check(true));
    assert!(!restart.check(false));
    assert!(!restart.check(false));
    assert!(restart.check(false));
    let record = json!({ "pid": 42, "creationDate": NOW });
    let actual = json!({ "ProcessId": 42, "CreationDate": NOW, "ExecutablePath": "C:/pool/proxy.exe", "CommandLine": "proxy.exe -config C:/pool/proxy.yaml" });
    let with = |k: &str, v: Value| {
        let mut a = actual.clone();
        a[k] = v;
        a
    };
    assert!(same_process(&record, Some(&actual), "C:/pool/proxy.exe", "C:/pool/proxy.yaml"));
    assert!(!same_process(&record, Some(&with("CreationDate", json!("2026-09-13T02:00:00.001Z"))), "C:/pool/proxy.exe", "C:/pool/proxy.yaml"));
    assert!(!same_process(&record, Some(&actual), "C:/pool/proxy.exe", "C:/other/proxy.yaml"));
    assert!(same_identity(&record, Some(&with("ExecutablePath", json!("C:/pool/old.exe")))), "a live recorded process on a superseded binary keeps its identity");
    assert!(!same_identity(&record, None));
    assert!(!same_identity(&record, Some(&with("ProcessId", json!(43)))));
}

/// platform.test.mjs: "proxy restarts need a dead proxy, not slow management, and spare a
/// starting one".
#[test]
fn proxy_restarts_need_a_dead_proxy_not_slow_management_and_spare_a_starting_one() {
    let mut clock = 0.0;
    let mut policy = ProxyRestartPolicy::new(3, 6, 100.0);
    let slow: Vec<Decision> = (0..5).map(|_| policy.check(false, true, clock)).collect();
    assert_eq!(slow.iter().map(|d| d.restart).collect::<Vec<_>>(), [false; 5]);
    assert_eq!(slow.iter().map(|d| d.degraded).collect::<Vec<_>>(), [false, false, true, false, false]);
    assert!(policy.check(false, true, clock).restart, "wedged past the degraded limit");
    assert!(!policy.check(false, false, clock).restart);
    assert!(!policy.check(true, true, clock).restart);
    assert!(!policy.check(false, false, clock).restart);
    assert!(!policy.check(false, false, clock).restart);
    assert!(policy.check(false, false, clock).restart);
    policy.started(clock);
    for _ in 0..10 {
        clock += 9.0;
        assert!(!policy.check(false, false, clock).restart);
    }
    clock += 20.0;
    assert!(!policy.check(false, false, clock).restart);
    assert!(!policy.check(false, false, clock).restart);
    assert!(policy.check(false, false, clock).restart);
    policy.started(clock);
    assert!(!policy.check(true, true, clock).restart);
    for _ in 0..2 {
        assert!(!policy.check(false, false, clock).restart);
    }
    assert!(policy.check(false, false, clock).restart);
}

/// platform.test.mjs: "an unrecorded port holder is ours only when it is a pool proxy build on
/// the pool config".
#[test]
fn an_unrecorded_port_holder_is_ours_only_when_it_is_a_pool_proxy_build_on_the_pool_config() {
    let exe = "D:\\Pool\\proxy\\7.2.158-pool.16\\cli-proxy-api.exe";
    let holder = json!({ "ProcessId": 7, "CreationDate": NOW, "ExecutablePath": exe, "CommandLine": format!("{exe} -config D:\\Pool\\proxy.yaml") });
    let with = |k: &str, v: &str| {
        let mut a = holder.clone();
        a[k] = json!(v);
        a
    };
    assert!(owned_proxy(Some(&holder), "D:/Pool/", "D:/Pool/proxy.yaml"));
    assert!(!owned_proxy(Some(&holder), "D:/Pool", "D:/Other/proxy.yaml"));
    assert!(!owned_proxy(Some(&with("ExecutablePath", "D:\\Pool\\tools\\cli-proxy-api.exe")), "D:/Pool", "D:/Pool/proxy.yaml"));
    assert!(!owned_proxy(Some(&with("ExecutablePath", "D:\\Pool\\proxy\\x\\node.exe")), "D:/Pool", "D:/Pool/proxy.yaml"));
    assert!(!owned_proxy(Some(&with("ExecutablePath", "D:\\PoolTwo\\proxy\\x\\cli-proxy-api.exe")), "D:/Pool", "D:/Pool/proxy.yaml"));
    assert!(!owned_proxy(None, "D:/Pool", "D:/Pool/proxy.yaml"));
    assert!(!owned_proxy(Some(&with("CommandLine", "x.exe -config D:\\Pool\\proxy.yaml.other")), "D:/Pool", "D:/Pool/proxy.yaml"));
    assert!(!owned_proxy(Some(&with("CommandLine", "x.exe -config D:\\Other.yaml --note D:\\Pool\\proxy.yaml")), "D:/Pool", "D:/Pool/proxy.yaml"));
    assert!(!owned_proxy(Some(&with("ExecutablePath", "D:\\Pool\\proxy\\..\\..\\x\\cli-proxy-api.exe")), "D:/Pool", "D:/Pool/proxy.yaml"));
    let cl = |s: &str| json!(s);
    assert!(uses_config(Some(&cl("x.exe -config \"D:\\Pool\\proxy.yaml\"")), "D:/Pool/proxy.yaml"));
    assert!(uses_config(Some(&cl("x.exe --config=D:/Pool/proxy.yaml")), "D:\\Pool\\proxy.yaml"));
    assert!(!uses_config(Some(&cl("x.exe")), "D:/Pool/proxy.yaml"));
    assert!(!uses_config(Some(&cl("x.exe -config D:/Pool/proxy.yaml -config D:/Other/proxy.yaml")), "D:/Pool/proxy.yaml"));
    assert!(uses_config(Some(&cl("x.exe -config D:/Pool/proxy.yaml -config=D:/Pool/proxy.yaml")), "D:/Pool/proxy.yaml"));
    let record = json!({ "pid": 7, "creationDate": NOW });
    assert!(same_process(&record, Some(&holder), "D:/Pool/proxy/7.2.158-pool.16/cli-proxy-api.exe", "D:/Pool/proxy.yaml"));
    assert!(!same_process(&record, Some(&holder), "D:/Pool/proxy/7.2.158-pool.17/cli-proxy-api.exe", "D:/Pool/proxy.yaml"));
}
