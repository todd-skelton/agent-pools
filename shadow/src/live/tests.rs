//! Live mode's guards: which roots open, what a rehearsal holds, and the gateway's machines.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::{Install, MARKER, rehearsal, services};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pools-live-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(dir).unwrap()
}

fn put(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A live install with the two documents every root needs.
fn live_install(base: &Path) -> PathBuf {
    let live = base.join("live");
    put(&live.join("state/install.json"), r#"{"capacityForecast":{"enabled":true},"expectedAccounts":3}"#);
    put(&live.join("state/secrets.json"), r#"{"managementKey":"live-key"}"#);
    put(&live.join("state/pool-state.json"), r#"{"accounts":[]}"#);
    live
}

fn refused(r: std::io::Result<Install>) -> String {
    r.expect_err("refused").to_string()
}

#[test]
fn the_live_install_opens_only_with_live_and_live_names_only_it() {
    let base = scratch("open");
    let live = live_install(&base);
    assert!(refused(Install::open(&live, &live, false)).contains("pass --live"));
    let shouted = PathBuf::from(live.to_string_lossy().to_uppercase());
    assert!(refused(Install::open(&shouted, &live, false)).contains("pass --live"), "the same directory in other letters");
    let opened = Install::open(&live, &live, true).unwrap();
    assert!(!opened.rehearsal);
    assert_eq!(opened.args().last().map(String::as_str), Some("--live"));

    let other = base.join("other");
    put(&other.join(MARKER), "{}");
    put(&other.join("state/install.json"), "{}");
    put(&other.join("state/secrets.json"), "{}");
    assert!(refused(Install::open(&other, &live, true)).contains("is not it"));
    let rehearsal = Install::open(&other, &live, false).unwrap();
    assert!(rehearsal.rehearsal);
    assert_eq!(rehearsal.args(), vec![super::plain(other.clone()).to_string_lossy().into_owned()]);
    assert!(!rehearsal.root.to_string_lossy().starts_with(r"\\?\"), "the root is named as a process names it");
}

#[test]
fn a_root_that_is_not_a_rehearsal_or_overlaps_the_live_install_is_refused() {
    let base = scratch("refuse");
    let live = live_install(&base);
    let plain = base.join("plain");
    put(&plain.join("state/install.json"), "{}");
    put(&plain.join("state/secrets.json"), "{}");
    assert!(refused(Install::open(&plain, &live, false)).contains("not a rehearsal"));

    // A marker inside the live install does not make part of it a rehearsal.
    let inside = live.join("state");
    put(&inside.join(MARKER), "{}");
    put(&inside.join("state/install.json"), "{}");
    put(&inside.join("state/secrets.json"), "{}");
    assert!(refused(Install::open(&inside, &live, false)).contains("overlaps"));
    // Nor one holding it.
    put(&base.join(MARKER), "{}");
    put(&base.join("state/install.json"), "{}");
    put(&base.join("state/secrets.json"), "{}");
    assert!(refused(Install::open(&base, &live, false)).contains("overlaps"));

    let broken = base.join("broken");
    put(&broken.join(MARKER), "{}");
    put(&broken.join("state/install.json"), "[]");
    put(&broken.join("state/secrets.json"), "{}");
    assert!(refused(Install::open(&broken, &live, false)).contains("not a JSON object"));
    assert!(Install::open(&base.join("missing"), &live, false).is_err());
}

#[test]
fn a_rehearsal_holds_made_up_secrets_its_own_ports_and_the_stand_in() {
    let base = scratch("make");
    let live = live_install(&base);
    let exe = base.join("pools-shadow.exe");
    put(&exe, "not really a program");
    let dest = base.join("rehearsal");
    rehearsal::make(&crate::sources::Install::new(&live), &dest, &exe).unwrap();

    let install = Install::open(&dest, &live, false).unwrap();
    assert!(install.rehearsal);
    let secrets = install.secrets().unwrap();
    for k in ["managementKey", "clientKey", "remoteDesktopKey", "publicGatewaySessionSecret"] {
        let v = secrets[k].as_str().unwrap();
        assert!(v.starts_with("rehearsal-") && v.len() >= 32, "{k}");
    }
    assert_ne!(secrets["managementKey"], json!("live-key"));
    let settings = install.settings();
    assert_eq!(settings["capacityForecast"], json!({ "enabled": true }), "the copy's settings are kept");
    let [proxy, service, inference, desktop, gateway] = rehearsal::PORTS;
    assert_eq!((install.proxy_port(), install.supervisor_port()), (proxy, service));
    assert_eq!((install.port("inferenceRelayPort", 0), install.port("desktopRelayPort", 0)), (inference, desktop));
    assert_eq!(settings["publicGateway"]["port"], json!(gateway));
    for p in rehearsal::PORTS {
        assert!(![8000, 8317, 8318, 8319, 8327, 8330].contains(&p) && !(8320..=8324).contains(&p), "{p} is the live install's or the copy's");
    }

    // The stand-in is where an owned proxy is, running the rehearsal's config.
    let binary = install.proxy_binary();
    assert_eq!(std::fs::read_to_string(&binary).unwrap(), "not really a program");
    let actual = json!({ "ExecutablePath": binary, "CommandLine": format!("\"{}\" -config {}", binary.display(), install.proxy_config().display()) });
    assert!(pool_core::supervise::owned_proxy(Some(&actual), &install.root.to_string_lossy(), &install.proxy_config().to_string_lossy()));
    assert!(std::fs::read_to_string(install.proxy_config()).unwrap().contains(&format!("port: {proxy}")));

    // The relays' pair loads as theirs does; the desktop sign-in and the machine are made up.
    assert!(super::tls::load(&install.certs()).unwrap().is_some());
    let auth = super::read_json(&install.codex_auth().unwrap()).unwrap();
    assert_eq!(auth["auth_mode"], json!("chatgpt"));
    let keys = super::read_json(&dest.join("stand-in/keys.json")).unwrap();
    let machines = services::clients(Some(&super::read_json(&dest.join("state/remote-clients.json")).unwrap()));
    assert_eq!(machines.len(), 1);
    assert_eq!(machines[0].key_sha256, pools_gateway::sha256(keys["machineKey"].as_str().unwrap()));
    assert_eq!(install.backend_upstream(), format!("http://127.0.0.1:{proxy}/backend-api"));

    // It is not made over, and nothing of the live install changed.
    assert!(rehearsal::make(&crate::sources::Install::new(&live), &dest, &exe).is_err());
    assert_eq!(std::fs::read_to_string(live.join("state/secrets.json")).unwrap(), r#"{"managementKey":"live-key"}"#);
    assert!(!live.join(MARKER).exists());
}

#[test]
fn a_rehearsal_is_not_made_in_or_around_the_live_install() {
    let base = scratch("where");
    let live = live_install(&base);
    let exe = base.join("x.exe");
    put(&exe, "x");
    let source = crate::sources::Install::new(&live);
    assert!(rehearsal::make(&source, &live.join("rehearsal"), &exe).is_err());
    assert!(!live.join("rehearsal").exists(), "refused before anything was created");
    assert!(rehearsal::make(&source, &live, &exe).is_err());
}

#[test]
fn the_stand_in_runs_only_on_a_rehearsals_config() {
    let base = scratch("stand-in");
    let live = live_install(&base);
    put(&live.join("proxy.yaml"), "port: 8317\n");
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let err = rt.block_on(rehearsal::stand_in(&live.join("proxy.yaml"), &live)).unwrap_err();
    assert!(err.to_string().contains("pass --live"), "{err}");
    let other = base.join("other");
    put(&other.join("proxy.yaml"), "port: 1\n");
    assert!(rt.block_on(rehearsal::stand_in(&other.join("proxy.yaml"), &live)).is_err());
}

/// The lookup the takeover and the record rest on, against this process.
#[cfg(windows)]
#[test]
fn a_running_process_reads_with_its_identity() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let pid = std::process::id();
    let info = rt.block_on(super::machine::process_info(pid)).unwrap().expect("this process is running");
    assert_eq!(info["ProcessId"], json!(pid));
    assert!(pool_core::js::date_parse_value(info.get("CreationDate")).is_finite(), "{info}");
    assert!(info["ExecutablePath"].as_str().is_some_and(|p| p.ends_with(".exe")), "{info}");
    assert!(super::machine::alive(pid));
    // The same record CIM gives, field for field: the proxy's record (pid and creationDate) is
    // compared against it, so a millisecond or an offset apart would retire an adopted proxy.
    let cim = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &format!("Get-CimInstance Win32_Process -Filter \"ProcessId={pid}\" | Select-Object ProcessId, @{{n='CreationDate';e={{$_.CreationDate.ToString('yyyy-MM-ddTHH:mm:ss.fffK')}}}}, ExecutablePath, CommandLine | ConvertTo-Json -Compress")])
        .output()
        .unwrap();
    let cim: Value = serde_json::from_slice(&cim.stdout).unwrap();
    assert_eq!(info, cim);
    // No such process: none; process 4 (System) is another user's: its id, nothing else.
    assert_eq!(rt.block_on(super::machine::process_info(u32::MAX - 3)).unwrap(), None);
    assert_eq!(rt.block_on(super::machine::process_info(4)).unwrap().unwrap()["ProcessId"], json!(4));

    // A listener of this process's is found by its port, with the same details.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let owner = rt.block_on(super::machine::port_owner(port)).unwrap().expect("the port has a listener");
    assert_eq!(owner["ProcessId"], json!(pid));
    drop(listener);
    assert_eq!(rt.block_on(super::machine::port_owner(port)).unwrap(), None);
}

#[test]
fn the_tailnet_view_comes_from_the_ports_own_settings_both_fields_or_none() {
    let base = scratch("tailnet");
    let live = live_install(&base);
    let install = Install::open(&live, &live, true).unwrap();
    assert!(install.tailnet().unwrap().is_none(), "no pool-rs.json");
    put(&live.join(super::PORT_SETTINGS), r#"{"current":"0.1.0-x"}"#);
    assert!(install.tailnet().unwrap().is_none(), "no tailnet in it");
    put(&live.join(super::PORT_SETTINGS), r#"{"current":"0.1.0-x","tailnet":{"origin":"https://box.example.ts.net:8443","owner":"me@example.com"}}"#);
    let t = install.tailnet().unwrap().expect("named");
    assert_eq!((t.origin.as_str(), t.owner.as_str()), ("https://box.example.ts.net:8443", "me@example.com"));
    put(&live.join(super::PORT_SETTINGS), r#"{"tailnet":{"origin":"https://box.example.ts.net:8443"}}"#);
    assert!(install.tailnet().is_err(), "one field without the other");
}

#[test]
fn the_gateway_takes_enabled_named_machines_with_a_full_hash() {
    let hash = "a".repeat(64);
    let doc: Value = json!({ "clients": [
        { "name": "one", "keySha256": hash },
        { "name": "off", "keySha256": hash, "disabled": true },
        { "name": "short", "keySha256": "abc" },
        { "name": "upper", "keySha256": "A".repeat(64) },
        { "keySha256": hash },
    ] });
    let names: Vec<String> = services::clients(Some(&doc)).into_iter().map(|c| c.name).collect();
    assert_eq!(names, vec!["one"]);
    assert!(services::clients(None).is_empty());
}
