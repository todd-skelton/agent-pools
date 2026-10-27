//! The tailnet view (tailnet-dashboard-proxy.mjs): the dashboard on the owner's tailnet, through
//! `tailscale serve` (HTTPS) to this app, with its writes only for the owner.
//!
//! Agent Pools puts a proxy in front of the service that rewrites `Host` and `Origin`, forwards
//! the dashboard's reads, and forwards `login`, `callback` and `refresh` only from the tailnet
//! page's origin and the owner's Tailscale identity (`Tailscale-User-Login`, which Tailscale Serve
//! sets). Here the app answers the tailnet name itself (`Policy::host`), takes writes from the
//! tailnet page's origin (`Policy::allow_origin`), and each dashboard write takes a
//! `Caller<Owner>`: anyone on this PC, and on the tailnet only the owner.
//!
//! Set by `SHADOW_TAILNET_ORIGIN` (`https://<machine>.<tailnet>.ts.net:8443`) and
//! `SHADOW_TAILNET_OWNER` (the owner's Tailscale login), both or neither. Without them a request
//! that carries a Tailscale identity is still refused a write.
//!
//! Differences: every read the dashboard's pages use is served on the tailnet (the pages, their
//! live updates and assets, and the JSON reads), where the proxy forwards a fixed list; the
//! service's JSON writes (`/api/refresh`, `/api/login`, `/api/callback`) stay on this PC, since
//! the pages' own forms do those writes.

use corten::axum::http::{HeaderMap, header, request::Parts};
use corten::{Identify, Refused};

use crate::watch::Shared;

/// The tailnet the dashboard is served on.
#[derive(Clone, Debug, PartialEq)]
pub struct Tailnet {
    /// The page's origin, as a browser sends it: `https://name:8443`.
    pub origin: String,
    /// The machine's tailnet name, `Host` without a port.
    pub name: String,
    /// The owner's Tailscale login.
    pub owner: String,
}

impl Tailnet {
    /// From `origin` (`https://name[:port]`) and `owner`.
    pub fn new(origin: &str, owner: &str) -> Result<Tailnet, String> {
        let origin = origin.trim().trim_end_matches('/').to_ascii_lowercase();
        let url = reqwest::Url::parse(&origin).map_err(|e| format!("SHADOW_TAILNET_ORIGIN {origin}: {e}"))?;
        let name = match (url.scheme(), url.host_str(), url.path()) {
            ("https", Some(name), "/" | "") => name.to_string(),
            _ => return Err(format!("SHADOW_TAILNET_ORIGIN {origin}: expected https://<name>[:port]")),
        };
        let owner = owner.trim().to_string();
        if owner.is_empty() {
            return Err("SHADOW_TAILNET_OWNER is empty".into());
        }
        Ok(Tailnet { origin, name, owner })
    }

    /// From `SHADOW_TAILNET_ORIGIN` and `SHADOW_TAILNET_OWNER`: both, or neither (none).
    pub fn from_env() -> Result<Option<Tailnet>, String> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        match (var("SHADOW_TAILNET_ORIGIN"), var("SHADOW_TAILNET_OWNER")) {
            (None, None) => Ok(None),
            (Some(o), Some(w)) => Tailnet::new(&o, &w).map(Some),
            _ => Err("set both SHADOW_TAILNET_ORIGIN and SHADOW_TAILNET_OWNER, or neither".into()),
        }
    }

    /// The app's policy with the tailnet name answered and its page's writes taken.
    pub fn policy(&self, policy: corten::security::Policy) -> corten::security::Policy {
        policy.host(&self.name).allow_origin(&self.origin)
    }

    fn is_name(&self, host: &str) -> bool {
        let name = host.rsplit_once(':').filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(n, _)| n);
        name.eq_ignore_ascii_case(&self.name)
    }
}

const LOGIN: &str = "tailscale-user-login";

/// Whether a request came through the tailnet: it carries a Tailscale identity, or names the
/// tailnet's machine or page.
fn via_tailnet(headers: &HeaderMap, tailnet: Option<&Tailnet>) -> bool {
    let get = |h: &str| headers.get(h).and_then(|v| v.to_str().ok());
    if headers.contains_key(LOGIN) {
        return true;
    }
    let Some(t) = tailnet else { return false };
    get(header::HOST.as_str()).is_some_and(|h| t.is_name(h)) || get(header::ORIGIN.as_str()).is_some_and(|o| o.trim_end_matches('/').eq_ignore_ascii_case(&t.origin))
}

/// Who may change the pool from the dashboard: anyone on this PC, and on the tailnet only the
/// owner's Tailscale login (tailnet-dashboard-proxy.mjs's `ownerLogin`).
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Owner {
    Local,
    Tailnet,
}

impl Identify<Shared> for Owner {
    async fn identify(parts: &Parts, shadow: &Shared) -> Result<Owner, Refused> {
        let tailnet = shadow.tailnet.get();
        if !via_tailnet(&parts.headers, tailnet) {
            return Ok(Owner::Local);
        }
        let login = parts.headers.get(LOGIN).and_then(|v| v.to_str().ok());
        match tailnet {
            Some(t) if login == Some(t.owner.as_str()) => Ok(Owner::Tailnet),
            Some(_) => Err(Refused::forbidden().message("Owner tailnet identity required")),
            None => Err(Refused::forbidden().message("Not available on the tailnet view")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_origin_names_the_machine_and_a_request_through_it_is_the_tailnets() {
        let t = Tailnet::new("HTTPS://Box.example.ts.net:8443/", "owner@example.com").unwrap();
        assert_eq!((t.origin.as_str(), t.name.as_str()), ("https://box.example.ts.net:8443", "box.example.ts.net"));
        assert!(Tailnet::new("http://box.example.ts.net", "o").is_err(), "the tailnet page is HTTPS");
        assert!(Tailnet::new("https://box.example.ts.net/x", "o").is_err());
        assert!(Tailnet::new("https://box.example.ts.net", " ").is_err());

        let h = |pairs: &[(&'static str, &str)]| {
            let mut m = HeaderMap::new();
            for (k, v) in pairs {
                m.insert(*k, v.parse().unwrap());
            }
            m
        };
        assert!(!via_tailnet(&h(&[("host", "localhost:8321")]), Some(&t)));
        assert!(via_tailnet(&h(&[("host", "box.example.ts.net:8443")]), Some(&t)));
        assert!(via_tailnet(&h(&[("host", "BOX.example.ts.net")]), Some(&t)));
        assert!(via_tailnet(&h(&[("host", "127.0.0.1:8321"), ("origin", "https://box.example.ts.net:8443")]), Some(&t)), "a Host rewritten by the proxy");
        assert!(via_tailnet(&h(&[("host", "localhost"), ("tailscale-user-login", "x")]), None), "a Tailscale identity with no tailnet set");
    }
}
