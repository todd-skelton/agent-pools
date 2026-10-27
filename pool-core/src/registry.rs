//! The model registry (model-registry.mjs): live model lists, pool readiness and admission probes
//! turned into one authority generation; the hourly refresh's planning (model-refresh.mjs
//! `wantedEfforts`, `probePlan`, `fairBatch`, `degraded`); and the admission judges
//! (model-admission.mjs `judgeClaude`, `judgeCodex`, `poolRouting`). Held to every call Agent
//! Pools' own model-registry.test.mjs makes (`golden/registry-hook.mjs`).
//!
//! Each function takes the documents as JSON and answers as `JSON.stringify` would write the
//! JavaScript's result. A case where the JavaScript throws is an `Err` with its message where a
//! test reaches it; other malformed inputs answer what the port can, without a panic.

use serde_json::{Map, Value, json};

use crate::js;

pub const REGISTRY_FORMAT: &str = "model-registry/v2";
pub const LINEAGE_FORMAT: &str = "model-lineage/v1";
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
pub const ADMISSION_TTL_MS: f64 = 14.0 * 24.0 * 3600e3;
pub const LIST_STALE_MS: f64 = 6.0 * 3600e3;
/// Observations stamped further ahead than this are invalid, not fresh.
pub const FUTURE_SKEW_MS: f64 = 5.0 * 60e3;

/// `SAFE_ID`: `/^[a-z0-9][a-z0-9.-]{1,63}$/`.
pub fn safe_id(id: &str) -> bool {
    let b = id.as_bytes();
    (2..=64).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b[1..].iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'.' || *c == b'-')
}

/// `emptyLineage()`
pub fn empty_lineage() -> Value {
    json!({ "format": LINEAGE_FORMAT, "generation": 0, "identities": {}, "families": {}, "events": [] })
}

/// `parseVersion(rule, id)`: the family's version of an exact id, from the rule's `pattern`
/// (named groups `version`, or `major` and `minor`).
pub fn parse_version(rule: &Value, id: &str) -> Option<String> {
    if !safe_id(id) {
        return None;
    }
    let pattern = rule.get("pattern").and_then(Value::as_str)?;
    let re = regex::Regex::new(pattern).ok()?;
    let m = re.captures(id)?;
    let g = |name: &str| m.name(name).map(|x| x.as_str().to_string());
    g("version").or_else(|| match (g("major"), g("minor")) {
        (major, Some(minor)) => Some(format!("{}.{minor}", major.unwrap_or_else(|| "undefined".into()))),
        (major, None) => major,
    })
}

/// `compareVersions(a, b)`: numerically, part by part.
pub fn compare_versions(a: &str, b: &str) -> f64 {
    let parts = |s: &str| s.split('.').map(|p| js::to_number(Some(&Value::String(p.into())))).collect::<Vec<f64>>();
    let (pa, pb) = (parts(a), parts(b));
    for i in 0..pa.len().max(pb.len()) {
        let d = pa.get(i).copied().unwrap_or(0.0) - pb.get(i).copied().unwrap_or(0.0);
        if d != 0.0 && !d.is_nan() {
            return d;
        }
    }
    0.0
}

fn compare_values(a: Option<&Value>, b: Option<&Value>) -> f64 {
    compare_versions(&js::to_string(a), &js::to_string(b))
}

fn is_effort(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str).is_some_and(|e| EFFORTS.contains(&e))
}

/// `effortsOf(model)`: the efforts a listed model's `thinking.levels` offers, in effort order.
fn efforts_of(model: &Value) -> Vec<&'static str> {
    let thinking = match model.get("thinking") {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
        Some(v) => v.clone(),
        None => Value::Null,
    };
    match js::get(Some(&thinking), "levels") {
        Some(Value::Array(levels)) => EFFORTS.into_iter().filter(|e| levels.iter().any(|l| l.as_str() == Some(e))).collect(),
        _ => Vec::new(),
    }
}

/// `admittedEfforts(probes, id, harnessVersion, now)`: the efforts whose latest probe is ok,
/// fresh and on the installed harness. `harness` `None` is `undefined`.
pub fn admitted_efforts(probes: &Value, id: &str, harness: Option<&Value>, now: f64) -> Vec<&'static str> {
    let mut latest: Vec<(&'static str, &Value)> = Vec::new();
    for p in js::values(Some(probes)) {
        if p.get("model").and_then(Value::as_str) != Some(id) || !is_effort(p.get("effort")) {
            continue;
        }
        let effort = EFFORTS.into_iter().find(|e| p.get("effort").and_then(Value::as_str) == Some(e)).unwrap_or_default();
        // An unreadable or future timestamp is quarantined, so it can never outrank later probes.
        let t = js::date_parse_value(Some(js::coalesce([p.get("at")]).unwrap_or(&Value::String(String::new()))));
        if !t.is_finite() || t - now > FUTURE_SKEW_MS {
            continue;
        }
        match latest.iter_mut().find(|(e, _)| *e == effort) {
            Some(slot) => {
                if js::date_parse_value(slot.1.get("at")) < t {
                    slot.1 = p;
                }
            }
            None => latest.push((effort, p)),
        }
    }
    EFFORTS
        .into_iter()
        .filter(|e| {
            let Some((_, p)) = latest.iter().find(|(x, _)| x == e) else { return false };
            let age = now - js::date_parse_value(Some(js::coalesce([p.get("at")]).unwrap_or(&Value::String(String::new()))));
            p.get("ok") == Some(&Value::Bool(true))
                && p.get("harnessVersion") == harness
                && age.is_finite()
                && (-FUTURE_SKEW_MS..=ADMISSION_TTL_MS).contains(&age)
        })
        .collect()
}

fn efforts_value(es: &[&str]) -> Value {
    Value::Array(es.iter().map(|e| Value::String((*e).into())).collect())
}

fn array_len(v: Option<&Value>) -> usize {
    v.and_then(Value::as_array).map_or(0, Vec::len)
}

fn includes(list: Option<&Value>, x: &Value) -> bool {
    list.and_then(Value::as_array).is_some_and(|l| l.contains(x))
}

/// One exact model as listed, with each account that lists it.
struct Observed {
    id: String,
    provider: Value,
    accounts: Vec<Value>,
}

/// `buildRegistry(o)`: `o` is the JavaScript's argument (`liveModels`, `status`,
/// `families`, `probes`, `lineage`, `harnessVersions`, `now`). Answers `{ registry, lineage }`.
pub fn build_registry(o: &Value) -> Result<Value, String> {
    let live = o.get("liveModels");
    let lists = js::get(live, "lists");
    if js::get(live, "format").and_then(Value::as_f64) != Some(1.0) || !js::is_object(lists) {
        return Err("live-models format 1 required".into());
    }
    let status = js::coalesce([o.get("status")]);
    let families = o.get("families");
    let empty = Value::Array(Vec::new());
    let probes = js::coalesce([o.get("probes")]).unwrap_or(&empty);
    let mut lineage = js::coalesce([o.get("lineage")]).cloned().unwrap_or_else(empty_lineage);
    let harness_versions = js::coalesce([o.get("harnessVersions")]).cloned().unwrap_or_else(|| json!({}));
    let now = js::to_number(o.get("now"));
    let iso = js::iso(now);
    let iso_v = Value::String(iso.clone());

    let account_status: Vec<(&str, &Value)> = js::values(js::get(status, "accounts")).into_iter().filter_map(|a| a.get("id").and_then(Value::as_str).map(|id| (id, a))).collect();
    // A later account with the same id wins, as `new Map(...)` keeps the last.
    let status_of = |id: &str| account_status.iter().rev().find(|(x, _)| *x == id).map(|(_, a)| *a);

    let mut observed: Vec<Observed> = Vec::new();
    for (account, list) in js::entries(lists) {
        let acct = status_of(&account);
        let file_at = js::date_parse_value(Some(js::coalesce([js::get(Some(list), "fetched_at")]).unwrap_or(&Value::String(String::new()))));
        let ml = js::get(acct, "modelList");
        let status_at = if js::get(ml, "source").and_then(Value::as_str) == Some("live") {
            js::date_parse_value(Some(js::coalesce([js::get(ml, "fetchedAt")]).unwrap_or(&Value::String(String::new()))))
        } else {
            f64::NAN
        };
        let fin = |x: f64| if x.is_finite() { x } else { f64::NEG_INFINITY };
        let fetched_at = fin(file_at).max(fin(status_at));
        let stale = !fetched_at.is_finite() || now - fetched_at > LIST_STALE_MS || fetched_at - now > FUTURE_SKEW_MS;
        let provider = js::get(Some(list), "provider");
        for model in js::values(js::get(Some(list), "models")) {
            let Some(id) = js::coalesce([model.get("id"), model.get("slug")]).and_then(Value::as_str) else { continue };
            if !safe_id(id) {
                continue;
            }
            let routing = js::coalesce([js::get(js::get(js::get(acct, "routingModels"), id), "status")]).cloned().unwrap_or(Value::Null);
            let disabled = js::get(acct, "disabled") == Some(&Value::Bool(true));
            let usable = !stale && acct.is_some() && !disabled && routing.as_str() == Some("ready");
            let row = json!({
                "account": account,
                "efforts": efforts_value(&efforts_of(model)),
                "fetchedAt": if fetched_at.is_finite() { Value::String(js::iso(fetched_at)) } else { Value::Null },
                "stale": stale,
                "disabled": disabled,
                "accountStatus": js::coalesce([js::get(acct, "status")]).cloned().unwrap_or(Value::Null),
                "routing": routing,
                "usable": usable,
            });
            let key = (js::to_string(provider), id.to_string());
            match observed.iter_mut().find(|e| (js::to_string(Some(&e.provider)), e.id.clone()) == key) {
                Some(e) => e.accounts.push(row),
                None => observed.push(Observed { id: id.into(), provider: provider.cloned().unwrap_or(Value::Null), accounts: vec![row] }),
            }
        }
    }

    let mut registry = Map::new();
    registry.insert("format".into(), REGISTRY_FORMAT.into());
    registry.insert("generatedAt".into(), iso_v.clone());
    registry.insert("harnessVersions".into(), harness_versions.clone());
    registry.insert(
        "source".into(),
        json!({ "accountLists": js::entries(lists).len(), "statusCheckedAt": js::coalesce([js::get(status, "checkedAt")]).cloned().unwrap_or(Value::Null) }),
    );
    let mut reg_families = Map::new();
    let mut models = Map::new();
    let mut notices: Vec<Value> = Vec::new();

    for rule in js::values(js::get(families, "families")) {
        let family = js::to_string(rule.get("family"));
        let provider = rule.get("provider").cloned().unwrap_or(Value::Null);
        let min_version = rule.get("minVersion");
        let required: Vec<Value> = js::values(rule.get("requiredEfforts")).into_iter().cloned().collect();
        let lin = lineage.as_object_mut().ok_or("lineage is not an object")?;
        let lin_families = lin.get_mut("families").and_then(Value::as_object_mut).ok_or("lineage.families is not an object")?;
        if js::nullish(lin_families.get(&family)) {
            lin_families.insert(family.clone(), json!({ "current": null, "promotedAt": null }));
        }
        // Members: each listed model of this provider whose id the family's pattern reads.
        let mut members: Vec<(usize, String)> = Vec::new();
        for (i, entry) in observed.iter().enumerate() {
            if entry.provider != provider {
                continue;
            }
            let Some(version) = parse_version(rule, &entry.id) else { continue };
            let identities = lineage["identities"].as_object_mut().ok_or("lineage.identities is not an object")?;
            if js::nullish(identities.get(&entry.id)) {
                identities.insert(entry.id.clone(), json!({ "family": family, "provider": provider, "version": version, "firstSeen": iso }));
            }
            if let Some(ident) = identities.get_mut(&entry.id).and_then(Value::as_object_mut) {
                ident.insert("lastListed".into(), iso_v.clone());
            }
            members.push((i, version));
        }
        js::sort_by(&mut members, |a, b| {
            js::or_else(compare_versions(&b.1, &a.1), || js::locale_compare(&observed[a.0].id, &observed[b.0].id) as i8 as f64)
        });
        let harness = js::coalesce([harness_versions.get(js::to_string(Some(&provider)).as_str())]).cloned().unwrap_or(Value::Null);
        for (i, version) in &members {
            let m = &mut observed[*i];
            let live_efforts: Vec<&str> = EFFORTS.into_iter().filter(|e| m.accounts.iter().any(|a| includes(a.get("efforts"), &Value::String((*e).into())))).collect();
            let admitted: Vec<&str> = admitted_efforts(probes, &m.id, Some(&harness), now).into_iter().filter(|e| live_efforts.contains(e)).collect();
            let mut by_effort = Map::new();
            for e in &live_efforts {
                let mut usable: Vec<String> = m
                    .accounts
                    .iter()
                    .filter(|a| a.get("usable") == Some(&Value::Bool(true)) && includes(a.get("efforts"), &Value::String((*e).into())))
                    .map(|a| js::to_string(a.get("account")))
                    .collect();
                usable.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
                by_effort.insert((*e).into(), json!(usable));
            }
            js::sort_by(&mut m.accounts, |a, b| js::locale_compare(&js::to_string(a.get("account")), &js::to_string(b.get("account"))) as i8 as f64);
            models.insert(
                m.id.clone(),
                json!({
                    "family": family, "provider": provider, "version": version, "role": null,
                    "efforts": efforts_value(&live_efforts), "admittedEfforts": efforts_value(&admitted),
                    "usableAccountsByEffort": by_effort, "accounts": m.accounts,
                }),
            );
        }
        // Promotion: the highest version at or above the floor and the current one, admitted at
        // every required effort and usable on an account at each.
        let fam = lineage["families"][&family].clone();
        let current_id = fam.get("current").filter(|c| js::truthy(Some(c))).map(|c| js::to_string(Some(c)));
        let cur_version = current_id.as_ref().and_then(|c| lineage["identities"].get(c)).filter(|i| js::truthy(Some(i))).map(|i| i.get("version").cloned());
        let eligible = members.iter().find(|(i, version)| {
            if compare_values(Some(&Value::String(version.clone())), min_version) < 0.0 {
                return false;
            }
            if let Some(cv) = &cur_version {
                if compare_values(Some(&Value::String(version.clone())), cv.as_ref()) <= 0.0 {
                    return false;
                }
            }
            let r = &models[&observed[*i].id];
            required.iter().all(|e| includes(r.get("admittedEfforts"), e) && array_len(js::get(r.get("usableAccountsByEffort"), &js::to_string(Some(e)))) > 0)
        });
        if let Some((i, _)) = eligible {
            let to = observed[*i].id.clone();
            let from = fam.get("current").cloned().unwrap_or(Value::Null);
            {
                let f = lineage["families"][&family].as_object_mut().ok_or("lineage family is not an object")?;
                f.insert("current".into(), Value::String(to.clone()));
                f.insert("promotedAt".into(), iso_v.clone());
            }
            if js::truthy(Some(&from)) {
                let ident = lineage["identities"].get_mut(js::to_string(Some(&from))).and_then(Value::as_object_mut).ok_or_else(|| "Cannot read properties of undefined (reading 'retiredAt')".to_string())?;
                if js::nullish(ident.get("retiredAt")) {
                    ident.insert("retiredAt".into(), iso_v.clone());
                }
            }
            let generation = js::to_number(lineage.get("generation")) + 1.0;
            lineage["generation"] = js::number_value(generation);
            let event = json!({ "at": iso, "generation": js::number_value(generation), "family": family, "event": "promoted", "from": from, "to": to });
            match lineage.get_mut("events").and_then(Value::as_array_mut) {
                Some(events) => events.push(event),
                None => return Err("lineage.events is not an array".into()),
            }
        }
        let fam = lineage["families"][&family].clone();
        let current = fam.get("current").cloned().unwrap_or(Value::Null);
        let has_current = js::truthy(Some(&current));
        let newest = members.iter().find(|(_, v)| compare_values(Some(&Value::String(v.clone())), min_version) >= 0.0);
        let candidate: Value = match newest {
            Some((i, v)) if has_current => {
                let cur_v = lineage["identities"].get(js::to_string(Some(&current))).and_then(|x| x.get("version")).cloned();
                if compare_values(Some(&Value::String(v.clone())), cur_v.as_ref()) > 0.0 { Value::String(observed[*i].id.clone()) } else { Value::Null }
            }
            Some((i, _)) => Value::String(observed[*i].id.clone()),
            None => Value::Null,
        };
        for (id, m) in models.iter_mut() {
            if m.get("family").and_then(Value::as_str) != Some(family.as_str()) {
                continue;
            }
            let role = if current.as_str() == Some(id) {
                "current"
            } else if candidate.as_str() == Some(id) {
                "candidate"
            } else {
                "historical"
            };
            m["role"] = role.into();
        }
        let unavailable: Vec<Value> = if has_current {
            let cur_model = models.get(&js::to_string(Some(&current)));
            required.iter().filter(|e| array_len(js::get(js::get(cur_model, "usableAccountsByEffort"), &js::to_string(Some(e)))) == 0).cloned().collect()
        } else {
            Vec::new()
        };
        if has_current && !unavailable.is_empty() {
            notices.push(json!({ "family": family, "notice": "current-unavailable", "model": current, "efforts": unavailable }));
        }
        if let Some(c) = candidate.as_str() {
            let admitted = models.get(c).and_then(|m| m.get("admittedEfforts"));
            let missing: Vec<&Value> = required.iter().filter(|e| !includes(admitted, e)).collect();
            notices.push(json!({ "family": family, "notice": "candidate-awaiting-admission", "model": c, "missingEfforts": missing }));
        }
        let mut historical: Vec<String> = js::entries(lineage.get("identities"))
            .into_iter()
            .filter(|(id, i)| i.get("family").and_then(Value::as_str) == Some(family.as_str()) && current.as_str() != Some(id.as_str()))
            .map(|(id, _)| id)
            .collect();
        historical.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        let mut f = Map::new();
        f.insert("provider".into(), provider.clone());
        f.insert("current".into(), current.clone());
        f.insert("candidate".into(), candidate.clone());
        f.insert("requiredEfforts".into(), Value::Array(required.clone()));
        // `promotedAt: fam.promotedAt`: left out when the lineage never set it (undefined).
        if let Some(p) = fam.get("promotedAt") {
            f.insert("promotedAt".into(), p.clone());
        }
        f.insert("unavailableEfforts".into(), Value::Array(unavailable));
        f.insert("historical".into(), json!(historical));
        reg_families.insert(family, Value::Object(f));
    }
    let digest_input = json!({
        "g": lineage.get("generation").cloned().unwrap_or(Value::Null),
        "f": Value::Object(reg_families.iter().map(|(k, v)| (k.clone(), v.get("current").cloned().unwrap_or(Value::Null))).collect()),
    });
    registry.insert("families".into(), Value::Object(reg_families));
    registry.insert("models".into(), Value::Object(models));
    registry.insert("notices".into(), Value::Array(notices));
    registry.insert("identities".into(), lineage.get("identities").cloned().unwrap_or(Value::Null));
    registry.insert("generation".into(), lineage.get("generation").cloned().unwrap_or(Value::Null));
    registry.insert("authorityDigest".into(), js::digest(&digest_input).into());
    Ok(json!({ "registry": Value::Object(registry), "lineage": lineage }))
}

/// `wantedEfforts(policy, preview)`: per family, every effort the routing policy uses or the
/// latest calibration preview offers, in first-seen order.
pub fn wanted_efforts(policy: Option<&Value>, preview: Option<&Value>) -> Value {
    let mut out = Map::new();
    let mut add = |family: Option<&Value>, effort: Option<&Value>| {
        if let (Some(Value::String(f)), Some(Value::String(e))) = (family, effort) {
            let list = out.entry(f.clone()).or_insert_with(|| Value::Array(Vec::new()));
            if let Some(l) = list.as_array_mut() {
                if !l.iter().any(|x| x.as_str() == Some(e)) {
                    l.push(Value::String(e.clone()));
                }
            }
        }
    };
    for row in js::values(js::get(policy, "rows")) {
        for s in js::values(row.get("slots")) {
            add(s.get("family"), s.get("effort"));
        }
    }
    for s in js::values(js::get(preview, "slots")) {
        for o in std::iter::once(s.get("proposal")).chain(js::values(s.get("consider")).into_iter().map(Some)) {
            if js::truthy(o) {
                add(js::get(o, "family"), js::get(o, "effort"));
            }
        }
    }
    Value::Object(out)
}

/// `probePlan(registry, families, probes, harnessVersions, now, extra)`: for the current model
/// and any newer candidate of each family, the wanted efforts it lists but lacks fresh ok
/// evidence for. Historical models are never probed.
pub fn probe_plan(registry: &Value, families: &Value, probes: &Value, harness_versions: &Value, now: f64, extra: &Value) -> Vec<Value> {
    let mut plan = Vec::new();
    for rule in js::values(families.get("families")) {
        let family = js::to_string(rule.get("family"));
        let f = js::get(registry.get("families"), &family);
        let mut wanted: Vec<Value> = Vec::new();
        for e in js::values(rule.get("requiredEfforts")).into_iter().chain(js::values(js::get(Some(extra), &family))) {
            if !wanted.contains(e) {
                wanted.push(e.clone());
            }
        }
        for id in [js::get(f, "candidate"), js::get(f, "current")].into_iter().filter(|v| js::truthy(*v)) {
            let id = js::to_string(id);
            let Some(m) = js::get(registry.get("models"), &id) else { continue };
            let provider = js::to_string(rule.get("provider"));
            let fresh = admitted_efforts(probes, &id, harness_versions.get(&provider), now);
            let need: Vec<&Value> = wanted.iter().filter(|e| includes(m.get("efforts"), e) && !e.as_str().is_some_and(|e| fresh.contains(&e))).collect();
            if !need.is_empty() {
                plan.push(json!({ "id": id, "provider": rule.get("provider"), "efforts": need }));
            }
        }
    }
    plan
}

/// `fairBatch(plan, probes, limit, now)`: at most `limit` (model, effort) probes, least
/// recently attempted first, so a target that keeps failing cannot starve the ones after it.
pub fn fair_batch(plan: &Value, probes: &Value, limit: f64, now: f64) -> Vec<Value> {
    let mut last: Vec<(String, f64)> = Vec::new();
    for p in js::values(Some(probes)) {
        let k = format!("{}|{}", js::to_string(p.get("model")), js::to_string(p.get("effort")));
        let t = js::date_parse_value(Some(js::coalesce([p.get("at")]).unwrap_or(&Value::String(String::new()))));
        let prev = last.iter().find(|(x, _)| *x == k).map_or(f64::NEG_INFINITY, |(_, t)| *t);
        // A future row cannot park a target.
        if t.is_finite() && t <= now + 5.0 * 60e3 && t > prev {
            match last.iter_mut().find(|(x, _)| *x == k) {
                Some(slot) => slot.1 = t,
                None => last.push((k, t)),
            }
        }
    }
    struct Item<'a> {
        t: &'a Value,
        effort: &'a Value,
        order: f64,
        at: f64,
    }
    let mut items: Vec<Item> = Vec::new();
    for (i, t) in js::values(Some(plan)).into_iter().enumerate() {
        for (j, effort) in js::values(t.get("efforts")).into_iter().enumerate() {
            let k = format!("{}|{}", js::to_string(t.get("id")), js::to_string(Some(effort)));
            let at = last.iter().find(|(x, _)| *x == k).map_or(f64::NEG_INFINITY, |(_, t)| *t);
            items.push(Item { t, effort, order: (i * 100 + j) as f64, at });
        }
    }
    js::sort_by(&mut items, |a, b| js::or_else(a.at - b.at, || a.order - b.order));
    let take = if limit.is_nan() || limit <= 0.0 { 0 } else { limit.trunc().min(items.len() as f64) as usize };
    let mut out: Vec<(Value, Value)> = Vec::new();
    for x in items.into_iter().take(take) {
        let id = x.t.get("id").cloned().unwrap_or(Value::Null);
        let pos = match out.iter().position(|(k, _)| *k == id) {
            Some(p) => p,
            None => {
                let mut e = x.t.clone();
                if let Some(o) = e.as_object_mut() {
                    o.insert("efforts".into(), Value::Array(Vec::new()));
                }
                out.push((id, e));
                out.len() - 1
            }
        };
        if let Some(list) = out[pos].1.get_mut("efforts").and_then(Value::as_array_mut) {
            list.push(x.effort.clone());
        }
    }
    out.into_iter().map(|(_, v)| v).collect()
}

/// `degraded(registry)`: a missing registry, or a family reporting an unavailable effort, is
/// rebuilt by `--recheck`.
pub fn degraded(registry: Option<&Value>) -> bool {
    if !js::truthy(registry) || !js::is_object(js::get(registry, "families")) {
        return true;
    }
    js::values(js::get(registry, "families")).into_iter().any(|f| array_len(f.get("unavailableEfforts")) > 0)
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l))
}

/// `judgeClaude(id, stdout)`: `claude --print --output-format json`'s last line replied
/// exactly OK, with only `id` in its model usage.
pub fn judge_claude(id: &str, stdout: &str) -> Result<Value, String> {
    let last = lines(stdout.trim()).last().unwrap_or_default();
    let Ok(result) = serde_json::from_str::<Value>(last) else {
        return Ok(json!({ "ok": false, "reason": "unparseable" }));
    };
    if result.is_null() {
        return Err("Cannot read properties of null (reading 'modelUsage')".into());
    }
    let used: Vec<String> = js::entries(js::coalesce([js::get(Some(&result), "modelUsage")])).into_iter().map(|(k, _)| k).collect();
    if js::truthy(js::get(Some(&result), "is_error")) {
        return Ok(json!({ "ok": false, "reason": "is_error" }));
    }
    let reply = js::coalesce([js::get(Some(&result), "result")]).map_or_else(String::new, |r| js::to_string(Some(r)));
    if reply.trim() != "OK" {
        return Ok(json!({ "ok": false, "reason": "reply-not-OK" }));
    }
    if used.len() != 1 || used[0] != id {
        let names = used.join(",");
        return Ok(json!({ "ok": false, "reason": format!("modelUsage {}", if names.is_empty() { "empty" } else { &names }) }));
    }
    Ok(json!({ "ok": true, "servedModel": used[0] }))
}

/// `judgeCodex(stdout)`: `codex exec --json`'s events end the turn, and its last agent message
/// is exactly OK.
pub fn judge_codex(stdout: &str) -> Value {
    let events: Vec<Value> = lines(stdout).filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter(|e| js::truthy(Some(e))).collect();
    let msg = events
        .iter()
        .rfind(|e| js::get(Some(e), "type").and_then(Value::as_str) == Some("item.completed") && js::get(js::get(Some(e), "item"), "type").and_then(Value::as_str) == Some("agent_message"))
        .and_then(|e| js::get(js::get(Some(e), "item"), "text"));
    if !events.iter().any(|e| js::get(Some(e), "type").and_then(Value::as_str) == Some("turn.completed")) {
        return json!({ "ok": false, "reason": "no-turn-completed" });
    }
    let reply = js::coalesce([msg]).map_or_else(String::new, |m| js::to_string(Some(m)));
    if reply.trim() != "OK" {
        return json!({ "ok": false, "reason": "reply-not-OK" });
    }
    json!({ "ok": true })
}

/// `poolRouting(logText, id, startMs, endMs)`: the pool's log shows a 2xx request inside the
/// probe window (two seconds either side) whose selector line bound `id` to an account.
/// `local_ms` reads a log line's `YYYY-MM-DDTHH:mm:ss` as the pool wrote it (local time), the
/// way `Date.parse` does; NaN when it cannot.
pub fn pool_routing(log: &str, id: &str, start_ms: f64, end_ms: f64, local_ms: impl Fn(&str) -> f64) -> Value {
    use std::sync::LazyLock;
    static OK: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^\[[^\]]+\] \[([0-9a-f]{8})\] .*gin_logger.* (2\d\d) \|").unwrap());
    static BOUND: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^\[[^\]]+\] \[([0-9a-f]{8})\] .*auth=(\S+) provider=\S+ model=(\S+)").unwrap());
    let ok: Vec<&str> = lines(log).filter_map(|l| OK.captures(l).and_then(|m| m.get(1)).map(|m| m.as_str())).collect();
    for l in lines(log) {
        let stamp: String = l.chars().skip(1).take(19).collect::<String>().replacen(' ', "T", 1);
        let t = local_ms(&stamp);
        if !(t >= start_ms - 2000.0 && t <= end_ms + 2000.0) {
            continue;
        }
        if let Some(m) = BOUND.captures(l) {
            if &m[3] == id && ok.contains(&&m[1]) {
                return json!({ "account": &m[2], "requestId": &m[1] });
            }
        }
    }
    Value::Null
}
