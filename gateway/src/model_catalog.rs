//! Public model capabilities only. Account identities, credentials and probe output stay local.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub const SCHEMA: &str = "agent-pools-model-catalog/v1";
const EFFORTS: &[&str] = &[
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];
const CLAUDE_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const SELECTION_PREFIX: &str = "pool--";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Model {
    pub id: String,
    pub provider: String,
    pub efforts: Vec<String>,
    pub admitted_efforts: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Catalog {
    pub schema: String,
    pub generated_at: Option<String>,
    pub models: Vec<Model>,
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

impl Catalog {
    pub fn empty() -> Self {
        Self {
            schema: SCHEMA.into(),
            generated_at: None,
            models: vec![],
        }
    }

    pub fn from_registry(registry: &Value) -> Self {
        let mut catalog = Self::empty();
        catalog.generated_at = registry["generatedAt"]
            .as_str()
            .filter(|s| valid_timestamp(s))
            .map(str::to_string);
        if let Some(models) = registry["models"].as_object() {
            for (id, row) in models.iter().take(256) {
                let provider = match row["provider"].as_str() {
                    Some("codex") => "openai",
                    Some("claude") => "anthropic",
                    _ => continue,
                };
                if !valid_id(id) {
                    continue;
                }
                let levels = |key: &str| -> Vec<String> {
                    let values: BTreeSet<&str> = row[key]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    EFFORTS
                        .iter()
                        .filter(|e| values.contains(**e))
                        .map(|e| (*e).into())
                        .collect()
                };
                let efforts = levels("efforts");
                let admitted_efforts = levels("admittedEfforts")
                    .into_iter()
                    .filter(|e| efforts.contains(e))
                    .collect();
                catalog.models.push(Model {
                    id: id.clone(),
                    provider: provider.into(),
                    efforts,
                    admitted_efforts,
                });
            }
        }
        catalog
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        let mut ids = BTreeSet::new();
        if self.schema != SCHEMA
            || self.models.is_empty()
            || self.models.len() > 256
            || self
                .generated_at
                .as_ref()
                .is_some_and(|s| !valid_timestamp(s))
        {
            return Err("Invalid or empty model catalog");
        }
        for model in &self.models {
            if !valid_id(&model.id)
                || model.id.starts_with(SELECTION_PREFIX)
                || !ids.insert(&model.id)
                || !matches!(model.provider.as_str(), "openai" | "anthropic")
                || model.efforts.len() > EFFORTS.len()
                || model.efforts.iter().collect::<BTreeSet<_>>().len() != model.efforts.len()
                || model.admitted_efforts.iter().collect::<BTreeSet<_>>().len()
                    != model.admitted_efforts.len()
                || model.efforts.iter().any(|e| !EFFORTS.contains(&e.as_str()))
                || model
                    .admitted_efforts
                    .iter()
                    .any(|e| !model.efforts.contains(e))
            {
                return Err("Invalid model capabilities");
            }
        }
        Ok(())
    }

    pub fn picker(&self) -> Value {
        // Explicit pairs also work when the first-party UI does not expose effort for a custom ID.
        json!({"replaceBuiltInOptions":false,"options":self.models.iter().flat_map(|m| {
            m.admitted_efforts.iter().filter(|e| CLAUDE_EFFORTS.contains(&e.as_str())).map(|e| {
                json!({"model":format!("{SELECTION_PREFIX}{}--{e}",m.id),
                    "label":format!("{} / {} / {e}", if m.provider == "openai" {"OpenAI"} else {"Anthropic"}, m.id),
                    "description":"Agent Pools: selects this provider, model and verified reasoning effort together"})
            })
        }).collect::<Vec<_>>()})
    }

    /// Add current request identity, not a stale startup persona. No account identifiers.
    /// Unknown IDs remain byte-for-byte pass-through, preserving normal Claude compatibility.
    pub fn annotate(&self, body: &mut Value) -> Result<Option<Value>, &'static str> {
        let Some(id) = body["model"].as_str().map(str::to_owned) else {
            return Ok(None);
        };
        if let Some(selection) = id.strip_prefix(SELECTION_PREFIX) {
            let (id, effort) = selection
                .rsplit_once("--")
                .ok_or("Invalid pooled model selection")?;
            let model = self
                .models
                .iter()
                .find(|m| m.id == id)
                .ok_or("Unknown pooled model selection; refresh the catalog")?;
            if !model.admitted_efforts.iter().any(|e| e == effort)
                || !CLAUDE_EFFORTS.contains(&effort)
            {
                return Err("Pooled model selection has not been verified; refresh the catalog");
            }
            let object = body.as_object_mut().ok_or("Messages must be an object")?;
            object.insert("model".into(), json!(id));
            let output = object.entry("output_config").or_insert_with(|| json!({}));
            output
                .as_object_mut()
                .ok_or("Invalid Messages output_config field")?
                .insert("effort".into(), json!(effort));
        }
        let id = body["model"].as_str().unwrap();
        let Some(model) = self.models.iter().find(|m| m.id == id) else {
            return Ok(None);
        };
        let effort = body
            .pointer("/output_config/effort")
            .map(|e| e.as_str().ok_or("Reasoning effort must be a string"))
            .transpose()?;
        if effort.is_some_and(|e| !model.efforts.contains(&e.to_string())) {
            return Err("Selected model does not support the requested effort");
        }
        let identity = json!({"provider":model.provider,"model":model.id,"requestedEffort":effort.unwrap_or("provider-default")});
        // Message Threads inherit system/tools from creation. Model and output_config
        // are fixed for that thread; changing them requires a new full-history create.
        if body.pointer("/thread/type").and_then(Value::as_str) == Some("continue") {
            return Ok(Some(identity));
        }
        let instruction = format!(
            "Runtime identity supplied by Agent Pools for this request: provider={}, model={}, requested reasoning effort={}. The host/tool runtime is Claude Code, not the model provider. Report these values when asked; do not infer your identity from earlier turns. Account selection remains private. A requested effort of provider-default does not establish the effective effort.",
            model.provider,
            model.id,
            effort.unwrap_or("provider-default")
        );
        let block = json!({"type":"text","text":instruction});
        let system = body
            .as_object_mut()
            .ok_or("Messages must be an object")?
            .entry("system")
            .or_insert_with(|| json!([]));
        match system {
            Value::String(text) => *system = json!([{"type":"text","text":text}, block]),
            Value::Array(blocks) => blocks.push(block),
            _ => return Err("Invalid Messages system field"),
        }
        Ok(Some(identity))
    }
}

fn valid_timestamp(value: &str) -> bool {
    value.len() <= 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || b"TtZz:+-.".contains(&b))
        && pool_core::js::date_parse_value(Some(&json!(value))).is_finite()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn registry() -> Value {
        json!({"models":{"gpt-fixture":{"provider":"codex","efforts":["low","high"],"admittedEfforts":["low","ultra"],"accounts":["private@example.test"],"token":"secret"},
            "claude-fixture":{"provider":"claude","efforts":["medium"],"admittedEfforts":["medium"]},
            "../unsafe":{"provider":"codex","efforts":["low"]}}})
    }
    #[test]
    fn projection_is_bounded_and_excludes_private_registry_fields() {
        let c = Catalog::from_registry(&registry());
        c.validate().unwrap();
        let text = serde_json::to_string(&c).unwrap();
        assert!(!text.contains("secret") && !text.contains("private@") && !text.contains("unsafe"));
        assert_eq!(c.models.len(), 2);
        assert_eq!(
            c.models
                .iter()
                .find(|m| m.id == "gpt-fixture")
                .unwrap()
                .admitted_efforts,
            ["low"]
        );
        assert_eq!(c.picker()["options"].as_array().unwrap().len(), 2);
        let mut duplicate = c.clone();
        duplicate.models.push(c.models[0].clone());
        assert!(duplicate.validate().is_err());
        let mut malformed = c;
        malformed.generated_at = Some("private registry text".into());
        assert!(malformed.validate().is_err());
        assert!(
            Catalog::from_registry(&json!({"generatedAt":"private registry text"}))
                .generated_at
                .is_none()
        );
    }
    #[test]
    fn identity_tracks_each_request_and_preserves_tools_and_reasoning() {
        let c = Catalog::from_registry(&registry());
        let mut body = json!({"model":"gpt-fixture","system":"original","messages":[{"role":"assistant","content":[{"type":"thinking","signature":"opaque"}]}],"output_config":{"effort":"high"}});
        let before = body["messages"].clone();
        assert_eq!(
            c.annotate(&mut body).unwrap().unwrap()["provider"],
            "openai"
        );
        assert_eq!(body["messages"], before);
        assert_eq!(body["system"][0]["text"], "original");
        body["output_config"]["effort"] = json!("ultra");
        assert!(c.annotate(&mut body).is_err());
        body["output_config"]["effort"] = json!(true);
        assert!(c.annotate(&mut body).is_err());
        let mut unknown = json!({"model":"unknown","system":"untouched"});
        let before = unknown.clone();
        assert!(c.annotate(&mut unknown).unwrap().is_none());
        assert_eq!(unknown, before);
    }

    #[test]
    fn thread_continuations_inherit_identity_without_mutating_create_only_fields() {
        let c = Catalog::from_registry(&registry());
        let mut body = json!({"model":"pool--claude-fixture--medium",
            "thread":{"type":"continue","previous_message_id":"msg_fixture"},
            "cache_control":{"type":"ephemeral"},
            "messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool_fixture","content":"done"}]}]});
        let before = body.clone();
        let identity = c.annotate(&mut body).unwrap().unwrap();
        assert_eq!(identity["requestedEffort"], "medium");
        assert_eq!(body["model"], "claude-fixture");
        assert_eq!(body["output_config"]["effort"], "medium");
        assert!(body.get("system").is_none());
        assert!(body.get("tools").is_none());
        for key in ["thread", "cache_control", "messages"] {
            assert_eq!(body[key], before[key]);
        }
        let mut create = json!({"model":"pool--claude-fixture--medium","thread":{"type":"create"}});
        c.annotate(&mut create).unwrap();
        assert!(create["system"][0]["text"].as_str().unwrap().contains("claude-fixture"));
    }

    #[test]
    fn picker_pairs_resolve_to_real_models_and_efforts_without_faking_capabilities() {
        let c = Catalog::from_registry(&registry());
        for option in c.picker()["options"].as_array().unwrap() {
            assert!(option.get("behavesAs").is_none());
            let mut body = json!({"model":option["model"],"output_config":{"effort":"ultra","format":{"type":"json_schema"}},"messages":[]});
            let identity = c.annotate(&mut body).unwrap().unwrap();
            assert_eq!(body["model"], identity["model"]);
            assert_eq!(body["output_config"]["effort"], identity["requestedEffort"]);
            assert_eq!(body["output_config"]["format"]["type"], "json_schema");
            assert!(
                !body["model"]
                    .as_str()
                    .unwrap()
                    .starts_with(SELECTION_PREFIX)
            );
        }
        for id in [
            "pool--gpt-fixture--high",
            "pool--unknown--low",
            "pool--gpt-fixture--ultra",
            "pool--malformed",
        ] {
            assert!(c.annotate(&mut json!({"model":id})).is_err());
        }
    }
}
