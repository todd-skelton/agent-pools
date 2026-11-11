//! Explicit, fail-closed routing. A new API never silently spends the control account.
use corten::axum::http::Method;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use serde_json::Value;
use std::io::Read;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Pool,
    Control,
    Unsupported,
}

const CONTROL_PREFIXES: &[&str] = &[
    "/api/oauth/",
    "/api/claude_cli/",
    "/api/event_logging/",
    "/api/eval/",
    "/api/eval-authed/",
    "/v1/code/",
    "/v1/oauth/",
];
const CONTROL_EXACT: &[&str] = &[
    "/api/hello",
    "/api/hello/",
    "/api/oauth",
    "/api/claude_cli",
    "/v1/code",
    "/v1/oauth/token",
    "/api/claude_cli_profile",
    "/api/claude_code/settings",
    "/api/claude_code/policy_limits",
    "/api/claude_code/skills",
    "/api/claude_code/notification/preferences",
    "/api/claude_code/organizations/metrics_enabled",
    "/api/organization/claude_code_first_token_date",
    "/api/web/domain_info",
    "/api/claude_code_penguin_mode",
    "/v1/mcp_servers",
];
pub const TUNNEL_DOMAINS: &[&str] = &[
    "anthropic.com",
    "claude.ai",
    "claude.com",
    "claude.app",
    "claudeusercontent.com",
    "claudemcpcontent.com",
];

fn under(path: &str, base: &str) -> bool {
    path == base || path.strip_prefix(base).is_some_and(|p| p.starts_with('/'))
}

/// First-party configuration, telemetry and local RC coordination, not managed-agent inference.
/// Returned templates are static so observability cannot disclose resource IDs.
pub fn control_endpoint(method: &Method, path: &str) -> Option<&'static str> {
    let id = |s: &str| !s.is_empty() && s.len() <= 128 && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c));
    match (method, path) {
        (&Method::GET, "/api/claude_code_grove") => return Some("/api/claude_code_grove"),
        (&Method::GET, "/api/frame/contract/latest") => return Some("/api/frame/contract/latest"),
        (&Method::GET, "/api/directory/plugins") => return Some("/api/directory/plugins"),
        (&Method::GET, "/mcp-registry/v0/servers") => return Some("/mcp-registry/v0/servers"),
        (&Method::POST, "/api/claude_code/metrics") => return Some("/api/claude_code/metrics"),
        _ => {}
    }
    if matches!(*method, Method::GET | Method::POST | Method::DELETE)
        && path.strip_prefix("/v2/ccr-sessions/").and_then(|p| p.strip_suffix("/mcp")).is_some_and(id)
    {
        return Some("/v2/ccr-sessions/:session/mcp");
    }
    if *method == Method::GET && path.strip_prefix("/v1/session_ingress/session/").is_some_and(id) {
        return Some("/v1/session_ingress/session/:session");
    }
    if *method == Method::POST && path.strip_prefix("/v1/sessions/").and_then(|p| p.strip_suffix("/archive")).is_some_and(id) {
        return Some("/v1/sessions/:session/archive");
    }
    let rest = path.strip_prefix("/v1/environments/")?;
    let parts: Vec<_> = rest.split('/').collect();
    match (method, parts.as_slice()) {
        (&Method::POST, ["bridge"]) => Some("/v1/environments/bridge"),
        (&Method::DELETE, ["bridge", env]) if id(env) => Some("/v1/environments/bridge/:environment"),
        (&Method::POST, ["bridge", env, "offline"]) if id(env) => Some("/v1/environments/bridge/:environment/offline"),
        (&Method::POST, [env, "bridge", "reconnect"]) if id(env) => Some("/v1/environments/:environment/bridge/reconnect"),
        (&Method::GET, [env, "work", "poll"]) if id(env) => Some("/v1/environments/:environment/work/poll"),
        (&Method::POST, [env, "work", work, "ack"]) if id(env) && id(work) => Some("/v1/environments/:environment/work/:work/ack"),
        (&Method::POST, [env, "work", work, "stop"]) if id(env) && id(work) => Some("/v1/environments/:environment/work/:work/stop"),
        (&Method::POST, [env, "work", work, "heartbeat"]) if id(env) && id(work) => Some("/v1/environments/:environment/work/:work/heartbeat"),
        _ => None,
    }
}

/// Bounded static vocabulary only: unknown route components may be private handles.
pub fn diagnostic_endpoint(path: &str) -> String {
    const WORDS: &[&str] = &[
        "api", "v1", "claude_code", "claude_cli", "claude_code_grove", "claude_code_shared_session_transcripts",
        "environments", "environment_providers", "sessions", "session_ingress", "session", "code", "bridge",
        "work", "poll", "ack", "stop", "heartbeat", "offline", "reconnect", "archive", "metrics", "oauth",
        "messages", "compact", "count_tokens", "files", "file_upload", "agent-proxy", "settings", "profile",
    ];
    let mut parts = path.split('/').skip(1);
    let mut result = String::new();
    for part in parts.by_ref().take(8) {
        result.push('/');
        result.push_str(if WORDS.contains(&part) { part } else { ":resource" });
    }
    if parts.next().is_some() {
        result.push_str("/:truncated");
    }
    result
}

/// Local operators can compare API vocabulary without putting raw handles in logs.
pub fn diagnostic_tags(path: &str, local_key: &str) -> Vec<String> {
    path.split('/').skip(1).take(8).map(|part| {
        let mut mac = Hmac::<Sha256>::new_from_slice(local_key.as_bytes()).expect("HMAC accepts any key length");
        mac.update(b"claude-split-route-v1\0");
        mac.update(part.as_bytes());
        mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
    }).collect()
}

pub fn classify(method: &Method, path: &str) -> Route {
    if under(path, "/api/oauth/file_upload") || under(path, "/api/oauth/files") || under(path, "/v1/code/agent-proxy") {
        return Route::Unsupported;
    }
    if *method == Method::POST && matches!(path, "/v1/messages" | "/v1/messages/count_tokens") {
        return Route::Pool;
    }
    if *method == Method::GET {
        if path == "/v1/models"
            || path
                .strip_prefix("/v1/models/")
                .is_some_and(|id| !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)))
        {
            return Route::Pool;
        }
        let selector = path.strip_prefix("/api/").and_then(|p| {
            if let Some(p) = p.strip_prefix("organizations/") {
                let (id, rest) = p.split_once('/')?;
                (!id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))).then_some(rest)
            } else {
                Some(p)
            }
        });
        if selector.is_some_and(|p| {
            let p = p.strip_suffix('/').unwrap_or(p);
            p == "model_selector"
                || p.strip_prefix("model_selector/")
                    .is_some_and(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_lowercase() || c == b'_'))
        }) || path == "/v1/ultrareview/quota"
        {
            return Route::Control;
        }
    }
    if control_endpoint(method, path).is_some() || CONTROL_EXACT.contains(&path) || CONTROL_PREFIXES.iter().any(|p| path.starts_with(p)) {
        Route::Control
    } else {
        Route::Unsupported
    }
}

pub fn canonical_path(path: &str) -> bool {
    let low = path.to_ascii_lowercase();
    path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains(['\\', '#'])
        && !["%2f", "%5c", "%2e"].iter().any(|p| low.contains(p))
        && !path.split('/').any(|p| matches!(p, "." | ".."))
}

pub fn dns_name(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.parse::<std::net::IpAddr>().is_err()
        && host.split('.').all(|p| {
            !p.is_empty()
                && p.len() <= 63
                && !p.starts_with('-')
                && !p.ends_with('-')
                && p.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        })
}

pub fn tunnel_host(host: &str, extra: &[String]) -> bool {
    dns_name(host)
        && (TUNNEL_DOMAINS
            .iter()
            .any(|d| host == *d || host.strip_suffix(d).is_some_and(|p| p.ends_with('.')))
            || extra.iter().any(|h| h == host))
}

pub fn resource_reference(v: &Value) -> bool {
    match v {
        Value::Array(a) => a.iter().any(resource_reference),
        Value::Object(o) => o.iter().any(|(k, v)| {
            if (matches!(k.as_str(), "input_schema" | "input_examples") && o.get("name").is_some_and(Value::is_string))
                || (k == "input" && o.get("type").and_then(Value::as_str) == Some("tool_use"))
            {
                return false;
            }
            (matches!(k.as_str(), "file_id" | "container" | "thread_id") && !v.is_null())
                || (k == "type"
                    && v.as_str().is_some_and(|s| {
                        ["container_upload", "code_execution_", "bash_code_execution", "text_editor_code_execution"]
                            .iter()
                            .any(|p| s.starts_with(p))
                    }))
                || resource_reference(v)
        }),
        _ => false,
    }
}

/// Inspect a bounded decompressed copy; the original (possibly compressed) bytes go upstream.
pub fn inspect(body: &[u8], encoding: &str, limit: usize) -> Result<(), (u16, &'static str)> {
    decode(body, encoding, limit).map(|_| ())
}

pub fn decode(body: &[u8], encoding: &str, limit: usize) -> Result<Value, (u16, &'static str)> {
    let mut reader: Box<dyn Read + '_> = match encoding.to_ascii_lowercase().as_str() {
        "identity" => Box::new(body),
        "gzip" => Box::new(flate2::read::GzDecoder::new(body)),
        "deflate" => Box::new(flate2::read::ZlibDecoder::new(body)),
        "br" => Box::new(brotli::Decompressor::new(body, 4096)),
        _ => return Err((415, "Unsupported request Content-Encoding")),
    };
    let mut decoded = Vec::new();
    reader
        .by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut decoded)
        .map_err(|_| (400, "Invalid compressed JSON"))?;
    if decoded.len() > limit {
        return Err((413, "Decoded request exceeds splitter body limit"));
    }
    let parsed: Value = serde_json::from_slice(&decoded).map_err(|_| (400, "Invalid Messages JSON"))?;
    if !parsed.is_object() {
        return Err((400, "Expected a Messages JSON object"));
    }
    if resource_reference(&parsed) {
        return Err((
            422,
            "Account-bound files, server containers and thread IDs are unsupported with rotating accounts; use inline attachments and local tools",
        ));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn remote_control_is_method_and_shape_scoped() {
        for (method, path) in [
            (Method::GET, "/api/directory/plugins"),
            (Method::GET, "/api/frame/contract/latest"),
            (Method::GET, "/mcp-registry/v0/servers"),
            (Method::GET, "/v2/ccr-sessions/session_1/mcp"),
            (Method::POST, "/v2/ccr-sessions/session_1/mcp"),
            (Method::DELETE, "/v2/ccr-sessions/session_1/mcp"),
            (Method::GET, "/api/claude_code_grove"),
            (Method::POST, "/api/claude_code/metrics"),
            (Method::GET, "/v1/session_ingress/session/session_1"),
            (Method::POST, "/v1/sessions/session_1/archive"),
            (Method::POST, "/v1/environments/bridge"),
            (Method::DELETE, "/v1/environments/bridge/env_1"),
            (Method::POST, "/v1/environments/bridge/env_1/offline"),
            (Method::POST, "/v1/environments/env_1/bridge/reconnect"),
            (Method::GET, "/v1/environments/env_1/work/poll"),
            (Method::POST, "/v1/environments/env_1/work/work_1/ack"),
            (Method::POST, "/v1/environments/env_1/work/work_1/stop"),
            (Method::POST, "/v1/environments/env_1/work/work_1/heartbeat"),
        ] {
            assert_eq!(classify(&method, path), Route::Control, "{path}");
            assert_eq!(classify(&Method::PUT, path), Route::Unsupported, "{path}");
        }
        for path in [
            "/v1/environments", "/v1/environments/", "/v1/environments/bridge/",
            "/v1/environments/env_1/messages", "/v1/environments/env_1/work/work_1/inference",
            "/v1/environments/env_1/work/work_1/heartbeat/extra", "/v1/environments//work/poll",
            "/v1/environments/env%2F1/work/poll", "/v1/environments/../work/poll",
            "/v1/sessions", "/v1/sessions/session_1/events", "/v1/session_ingress/session/session_1/messages",
            "/v1/messages/compact", "/v1/code/agent-proxy/env_1",
            "/v2/ccr-sessions/session_1/agent-proxy", "/v2/ccr-sessions/session_1/mcp/messages",
            "/v2/ccr-sessions/session_1/teleport/conversations/conversation_1/completion",
            "/v2/ccr-sessions//mcp", "/v2/ccr-sessions/session%2f1/mcp",
            "/api/frame/contract/unknown", "/mcp-registry/v0/servers/unknown",
            "/api/directory/plugins/unknown",
        ] {
            for method in [Method::GET, Method::POST, Method::DELETE] {
                assert_eq!(classify(&method, path), Route::Unsupported, "{method} {path}");
            }
        }
        assert!(control_endpoint(&Method::GET, &format!("/v1/environments/{}/work/poll", "x".repeat(129))).is_none());
        assert_eq!(classify(&Method::POST, "/api/claude_code_grove"), Route::Unsupported);
        assert_eq!(classify(&Method::GET, "/api/claude_code/metrics"), Route::Unsupported);
        assert_eq!(classify(&Method::POST, "/v1/session_ingress/session/session_1"), Route::Unsupported);
        assert_eq!(classify(&Method::GET, "/v1/sessions/session_1/archive"), Route::Unsupported);
        assert_eq!(classify(&Method::POST, "/api/frame/contract/latest"), Route::Unsupported);
        assert_eq!(classify(&Method::POST, "/mcp-registry/v0/servers"), Route::Unsupported);
        assert_eq!(classify(&Method::POST, "/api/directory/plugins"), Route::Unsupported);
        assert_eq!(classify(&Method::DELETE, "/api/directory/plugins"), Route::Unsupported);
    }

    #[test]
    fn rejected_route_diagnostics_are_static_bounded_and_redact_handles() {
        assert_eq!(diagnostic_endpoint("/v1/environments/private-env/work/private-work/new-secret-api"), "/v1/environments/:resource/work/:resource/:resource");
        assert_eq!(diagnostic_endpoint("/api/claude_code/metrics"), "/api/claude_code/metrics");
        let long = diagnostic_endpoint(&format!("/{}", "secret/".repeat(50)));
        assert!(!long.contains("secret"));
        assert!(long.ends_with("/:truncated"));
        assert!(long.len() < 100);
        let tags = diagnostic_tags("/v1/environments/private-env", "private-key");
        assert_eq!(tags.len(), 3);
        assert!(tags.iter().all(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())));
        assert_eq!(tags, diagnostic_tags("/v1/environments/private-env", "private-key"));
        assert_ne!(tags, diagnostic_tags("/v1/environments/private-env", "different-key"));
        assert_eq!(diagnostic_tags(&format!("/{}", "secret/".repeat(50)), "private-key").len(), 8);
    }

    #[test]
    fn routes_and_canonical_paths_fail_closed() {
        for p in ["/v1/messages", "/v1/messages/count_tokens"] {
            assert_eq!(classify(&Method::POST, p), Route::Pool);
        }
        for p in ["/v1/models", "/v1/models/claude-sonnet-5-5"] {
            assert_eq!(classify(&Method::GET, p), Route::Pool);
        }
        for p in [
            "/v1/files",
            "/v1/messages/compact",
            "/v1/code/agent-proxy",
            "/v1/code/agent-proxy/x",
            "/api/oauth/files/x",
            "/api/oauth/file_upload",
            "/api/unknown",
        ] {
            assert_eq!(classify(&Method::POST, p), Route::Unsupported, "{p}");
        }
        for p in [
            "/api/oauth/profile",
            "/v1/code/sessions/x",
            "/api/organizations/org_1/model_selector/available",
            "/v1/ultrareview/quota",
        ] {
            assert_eq!(classify(&Method::GET, p), Route::Control);
        }
        for p in [
            "//v1/messages",
            "/v1/../messages",
            "/v1/%2e%2e/messages",
            "/v1%2fmessages",
            "/v1%5cmessages",
            "/v1\\messages",
        ] {
            assert!(!canonical_path(p));
        }
        assert!(canonical_path("/v1/messages"));
        assert!(tunnel_host("api.anthropic.com", &[]));
        for h in ["anthropic.com.evil.test", "evil-anthropic.com", "127.0.0.1", "api..anthropic.com"] {
            assert!(!tunnel_host(h, &[]));
        }
        assert!(tunnel_host("github.com", &["github.com".into()]));
    }
    #[test]
    fn schemas_are_data_but_api_handles_are_not() {
        assert!(!resource_reference(
            &json!({"tools":[{"name":"local","input_schema":{"properties":{"file_id":{"type":"string"}}},"input_examples":[{"container":"local"}]}],"messages":[{"content":[{"type":"tool_use","input":{"thread_id":"local"}}]}]})
        ));
        for v in [
            json!({"file_id":"x"}),
            json!({"messages":[{"content":[{"type":"tool_result","content":[{"source":{"file_id":"x"}}]}]}]}),
            json!({"tools":[{"type":"code_execution_20260101"}]}),
        ] {
            assert!(resource_reference(&v));
        }
        assert_eq!(inspect(b"[]", "identity", 1024).unwrap_err().0, 400);
        assert_eq!(inspect(b"{}", "zstd", 1024).unwrap_err().0, 415);
        assert_eq!(inspect(b"{\"file_id\":\"x\"}", "identity", 1024).unwrap_err().0, 422);
        assert_eq!(inspect(b"{}", "identity", 1).unwrap_err().0, 413);
    }
}
