//! Native provisioning of the shared catalog and named, explicitly configured Claude agents.
//! No login, workspace trust, tool permission or selected/default model is changed here.
use super::private;
use futures_util::StreamExt;
use pools_gateway::model_catalog::Catalog;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

const STATE: &str = "model-picker-state.json";
const LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

fn invalid(message: &str) -> std::io::Error {
    std::io::Error::other(message)
}

fn agents(catalog: &Catalog) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    for model in &catalog.models {
        for effort in model
            .admitted_efforts
            .iter()
            .filter(|e| LEVELS.contains(&e.as_str()))
        {
            let name = format!("pool-{}-{effort}", model.id);
            let text = format!(
                "---\nname: {name}\ndescription: Use when explicitly requested to delegate to {} / {} at {effort} effort through Agent Pools.\nmodel: {}\neffort: {effort}\n---\n\nYou are a delegated coding agent hosted by Claude Code. Your configured inference provider is {}, model {}, reasoning effort {effort}. Follow the runtime identity supplied with each request if the caller explicitly overrides this selection. Preserve inherited permission rules. Inspect and verify the assigned work, stay within its scope, and report results and changed files to the parent. Do not infer your provider from the Claude Code host name.\n",
                model.provider, model.id, model.id, model.provider, model.id
            );
            files.insert(format!("{name}.md"), text);
        }
    }
    files
}

fn owned_file(name: &str) -> bool {
    name.len() <= 200
        && name
            .strip_prefix("pool-")
            .and_then(|s| s.strip_suffix(".md"))
            .is_some_and(|s| {
                !s.is_empty()
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            })
}

async fn fetch(profile: &Path) -> std::io::Result<Catalog> {
    let config = super::config(profile)?;
    let origin = config["poolOrigin"].as_str().unwrap();
    let url = reqwest::Url::parse(origin).map_err(|_| invalid("Invalid pool origin"))?;
    let local = url.host_str().is_some_and(|h| {
        h == "localhost" || h.parse::<std::net::IpAddr>().is_ok_and(|a| a.is_loopback())
    });
    if !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && local))
    {
        return Err(invalid("Catalog requires HTTPS or loopback HTTP origin"));
    }
    let sealed = std::fs::read(profile.join("pool-key.dpapi"))?;
    let key = String::from_utf8(crate::live::ops::dpapi::unprotect(&sealed)?)
        .map_err(|_| invalid("Invalid pool credential"))?;
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| invalid("Catalog client unavailable"))?;
    let response = http
        .get(format!(
            "{}/v1/agent-pools/models",
            origin.trim_end_matches('/')
        ))
        .bearer_auth(key)
        .send()
        .await
        .map_err(|_| invalid("Pool model catalog unavailable"))?;
    if !response.status().is_success() {
        return Err(invalid("Pool model catalog rejected; no settings changed"));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| invalid("Catalog transfer failed"))?;
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err(invalid("Catalog exceeds size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid("Invalid pool model catalog"))
}

fn parse(path: &Path) -> std::io::Result<Catalog> {
    if std::fs::metadata(path)?.len() > 1024 * 1024 {
        return Err(invalid("Catalog exceeds size limit"));
    }
    let value = crate::live::read_json(path)?;
    if value["schema"] == pools_gateway::model_catalog::SCHEMA {
        serde_json::from_value(value).map_err(|_| invalid("Invalid model catalog"))
    } else {
        Ok(Catalog::from_registry(&value))
    }
}

pub async fn command(args: &[String]) -> std::io::Result<()> {
    if !(3..=4).contains(&args.len())
        || !matches!(args[0].as_str(), "configure" | "preview" | "disable")
    {
        return Err(invalid(
            "Usage: claude-models configure|preview|disable <client-dir> <claude-config-dir> [catalog-or-registry.json]",
        ));
    }
    let profile = Path::new(&args[1]);
    let config = std::fs::canonicalize(&args[2])?;
    if !config.is_dir() {
        return Err(invalid("Claude config must be an existing directory"));
    }
    if args[0] == "disable" {
        if args.len() != 3 {
            return Err(invalid("Disable does not accept a catalog argument"));
        }
        return install(profile, &config, None);
    }
    let catalog = match args.get(3) {
        Some(path) => parse(Path::new(path))?,
        None => fetch(profile).await?,
    };
    catalog.validate().map_err(invalid)?;
    if args[0] == "preview" {
        println!(
            "{}",
            json!({"schema":catalog.schema,"models":catalog.picker()["options"].as_array().map_or(0, Vec::len),"agents":agents(&catalog).len(),"changesApplied":false})
        );
        return Ok(());
    }
    install(profile, &config, Some(catalog))
}

fn install(profile: &Path, config: &Path, catalog: Option<Catalog>) -> std::io::Result<()> {
    let settings_path = config.join("settings.json");
    let original = std::fs::read(&settings_path)?;
    let mut settings: Value =
        serde_json::from_slice(original.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&original))
            .map_err(|_| invalid("Invalid Claude settings"))?;
    if !settings.is_object() {
        return Err(invalid("Claude settings must be an object"));
    }
    let state_path = profile.join(STATE);
    let previous = if state_path.exists() {
        Some(crate::live::read_json(&state_path)?)
    } else {
        None
    };
    if let Some(state) = &previous
        && (state["schema"] != "agent-pools-claude-model-settings/v1"
            || state["configDirectory"] != config.to_string_lossy().as_ref()
            || settings.get("modelPicker") != state.get("appliedPicker")
            || !state["appliedPicker"].is_object()
            || !state["hadPicker"].is_boolean()
            || state.get("previousPicker").is_none()
            || !state["files"].is_object())
    {
        return Err(invalid(
            "Managed model picker changed or belongs to another profile; left untouched",
        ));
    }
    if previous.is_none() && profile.join("model-catalog.json").exists() {
        return Err(invalid(
            "Unmanaged model catalog already exists; left untouched",
        ));
    }
    let directory = config.join("agents");
    let old_files = previous
        .as_ref()
        .and_then(|s| s["files"].as_object())
        .cloned()
        .unwrap_or_default();
    for (name, expected) in &old_files {
        if !owned_file(name)
            || !expected.is_string()
            || std::fs::read_to_string(directory.join(name))
                .ok()
                .as_deref()
                != expected.as_str()
        {
            return Err(invalid(
                "Generated agent changed or has an unsafe name; left untouched",
            ));
        }
    }
    let desired = catalog.as_ref().map(agents).unwrap_or_default();
    for name in desired.keys() {
        if directory.join(name).exists() && !old_files.contains_key(name) {
            return Err(invalid(
                "Agent filename collision; existing agent left untouched",
            ));
        }
    }
    let (had_picker, old_picker) = previous.as_ref().map_or_else(
        || {
            (
                settings.get("modelPicker").is_some(),
                settings["modelPicker"].clone(),
            )
        },
        |s| (s["hadPicker"] == true, s["previousPicker"].clone()),
    );
    let next_state = if let Some(catalog) = &catalog {
        settings["modelPicker"] = catalog.picker();
        Some(
            json!({"schema":"agent-pools-claude-model-settings/v1","configDirectory":config.to_string_lossy(),"hadPicker":had_picker,"previousPicker":old_picker,"appliedPicker":settings["modelPicker"],"files":desired}),
        )
    } else {
        if previous.is_none() {
            return Err(invalid("No managed model settings to disable"));
        }
        if had_picker {
            settings["modelPicker"] = old_picker;
        } else {
            settings.as_object_mut().unwrap().remove("modelPicker");
        }
        None
    };
    private::directory(profile)?;
    std::fs::create_dir_all(&directory)?;
    let backup = profile
        .join("backups")
        .join(format!("models-{}", uuid::Uuid::new_v4().simple()));
    private::directory(&backup)?;
    private::write(&backup.join("settings.json"), &original)?;
    let mut writes: BTreeMap<PathBuf, Option<Vec<u8>>> = BTreeMap::new();
    writes.insert(
        settings_path.clone(),
        Some(serde_json::to_vec_pretty(&settings)?),
    );
    for name in old_files.keys() {
        writes.insert(directory.join(name), None);
    }
    for (name, text) in &desired {
        writes.insert(directory.join(name), Some(text.as_bytes().to_vec()));
    }
    writes.insert(
        state_path,
        next_state
            .map(|s| serde_json::to_vec_pretty(&s))
            .transpose()?,
    );
    writes.insert(
        profile.join("model-catalog.json"),
        catalog
            .as_ref()
            .map(serde_json::to_vec_pretty)
            .transpose()?,
    );
    let before: BTreeMap<_, _> = writes
        .keys()
        .map(|p| match std::fs::symlink_metadata(p) {
            Ok(m) if m.is_file() && !m.file_type().is_symlink() => {
                Ok((p.clone(), Some(std::fs::read(p)?)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((p.clone(), None)),
            _ => Err(invalid(
                "Model provisioning target is not a regular writable file; left untouched",
            )),
        })
        .collect::<std::io::Result<_>>()?;
    if std::fs::read(&settings_path)? != original {
        return Err(invalid(
            "Claude settings changed during provisioning; left untouched",
        ));
    }
    let apply = |path: &Path, bytes: &Option<Vec<u8>>| -> std::io::Result<()> {
        match bytes {
            Some(b) => private::write(path, b),
            None if path.exists() => std::fs::remove_file(path),
            None => Ok(()),
        }
    };
    for (path, bytes) in &writes {
        if let Err(error) = apply(path, bytes) {
            let mut rollback_failed = false;
            for (path, bytes) in &before {
                rollback_failed |= apply(path, bytes).is_err();
            }
            return Err(if rollback_failed {
                invalid("Model provisioning and rollback failed; protected backup retained")
            } else {
                error
            });
        }
    }
    println!(
        "{}",
        json!({"models":catalog.as_ref().map_or(0, |c| c.picker()["options"].as_array().map_or(0, Vec::len)),"agents":desired.len(),"permissionsChanged":false,"loginChanged":false,"trustChanged":false,"restartRequired":true})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_agents_are_explicit_and_do_not_grant_permissions() {
        let c = Catalog::from_registry(
            &json!({"models":{"gpt-test":{"provider":"codex","efforts":["low","high"],"admittedEfforts":["high"]}}}),
        );
        let files = agents(&c);
        assert_eq!(files.len(), 1);
        let text = &files["pool-gpt-test-high.md"];
        assert!(text.contains("model: gpt-test\neffort: high"));
        assert!(!text.contains("permissionMode:") && !text.contains("allowedTools:"));
        assert!(!owned_file("pool-../escape.md"));
        assert!(!owned_file("unowned.md"));
    }
    #[test]
    fn settings_install_is_idempotent_preserves_auth_and_detects_user_edits() {
        let root = std::env::temp_dir().join(format!(
            "claude-model-settings-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let profile = root.join("client");
        let config = root.join("claude");
        std::fs::create_dir_all(&config).unwrap();
        let config = std::fs::canonicalize(config).unwrap();
        let settings = json!({"env":{"HTTPS_PROXY":"private-local-proxy"},"permissions":{"deny":["Read(.env)"]},"model":"original","effortLevel":"high"});
        std::fs::write(
            config.join("settings.json"),
            serde_json::to_vec(&settings).unwrap(),
        )
        .unwrap();
        std::fs::write(config.join(".claude.json"), "trust-untouched").unwrap();
        let c = Catalog::from_registry(
            &json!({"models":{"gpt-test":{"provider":"codex","efforts":["high"],"admittedEfforts":["high"]}}}),
        );
        install(&profile, &config, Some(c.clone())).unwrap();
        install(&profile, &config, Some(c.clone())).unwrap();
        let mut actual = crate::live::read_json(&config.join("settings.json")).unwrap();
        actual.as_object_mut().unwrap().remove("modelPicker");
        assert_eq!(actual, settings);
        assert_eq!(
            std::fs::read_to_string(config.join(".claude.json")).unwrap(),
            "trust-untouched"
        );
        let agent = config.join("agents/pool-gpt-test-high.md");
        let generated = std::fs::read(&agent).unwrap();
        std::fs::write(&agent, "user edit").unwrap();
        assert!(install(&profile, &config, Some(c.clone())).is_err());
        assert_eq!(std::fs::read_to_string(&agent).unwrap(), "user edit");
        std::fs::write(&agent, generated).unwrap();
        install(&profile, &config, None).unwrap();
        assert_eq!(
            crate::live::read_json(&config.join("settings.json")).unwrap(),
            settings
        );
        assert!(!agent.exists() && !profile.join("model-catalog.json").exists());
        let before = std::fs::read(config.join("settings.json")).unwrap();
        std::fs::create_dir(profile.join("model-catalog.json")).unwrap();
        assert!(install(&profile, &config, Some(c.clone())).is_err());
        assert_eq!(std::fs::read(config.join("settings.json")).unwrap(), before);
        std::fs::remove_dir(profile.join("model-catalog.json")).unwrap();
        std::fs::write(&agent, "existing user agent").unwrap();
        assert!(install(&profile, &config, Some(c.clone())).is_err());
        assert_eq!(
            std::fs::read_to_string(&agent).unwrap(),
            "existing user agent"
        );
        assert_eq!(std::fs::read(config.join("settings.json")).unwrap(), before);
        std::fs::remove_file(&agent).unwrap();
        install(&profile, &config, Some(c.clone())).unwrap();
        let state = std::fs::read(profile.join(STATE)).unwrap();
        let mut invalid_state: Value = serde_json::from_slice(&state).unwrap();
        invalid_state["files"] = json!(null);
        std::fs::write(profile.join(STATE), invalid_state.to_string()).unwrap();
        assert!(install(&profile, &config, Some(c)).is_err());
        std::fs::write(profile.join(STATE), state).unwrap();
        install(&profile, &config, None).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
