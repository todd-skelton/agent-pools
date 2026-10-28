//! The write side's memory, as pool-service.mjs holds it: the pool state (`state`, as last
//! saved), the forecast and routing ladders as last published (`capacity.current()`,
//! `capacity.ladders()`), the usage value report as last written, and the settings read at the
//! start (`install.expectedAccounts`, `poolUsageMode`) or each refresh (`resetPolicies`). The
//! routes answer from it; nothing is read from disk to answer a request. Each change bumps
//! [`Memory::version`], which open pages watch.

use std::path::Path;
use std::sync::{Arc, Mutex};

use corten::axum::body::Bytes;
use pool_core::forecast::FORECAST_FORMAT;
use pool_core::resets::{ResetPolicy, reset_policies};
use serde_json::Value;

pub struct Memory {
    pool: Mutex<Arc<Value>>,
    forecast: Mutex<Option<Arc<Value>>>,
    ladders: Mutex<Option<Arc<Value>>>,
    /// The report and the bytes written for it (`/api/value` sends the file's bytes).
    value: Mutex<Option<(Arc<Value>, Bytes)>>,
    policies: Mutex<Vec<ResetPolicy>>,
    /// install.json as read at the start (`const install`).
    pub install: Arc<Value>,
    pub version: tokio::sync::watch::Sender<u64>,
}

fn read(path: &Path) -> Option<(Value, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    Some((doc, text))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Memory {
    /// What the service starts with: the saved state as [`super::load_state`] gives it, the last
    /// published forecast and ladders (when they are those formats), the value report on disk,
    /// and install.json.
    pub fn load(root: &Path) -> Memory {
        let install = read(&root.join("state/install.json")).map_or(Value::Null, |(d, _)| d);
        let of_format = |rel: &str, format: &str| {
            read(&root.join(rel)).map(|(d, _)| d).filter(|d| d.get("format").and_then(Value::as_str) == Some(format)).map(Arc::new)
        };
        Memory {
            pool: Mutex::new(Arc::new(super::load_state(root))),
            forecast: Mutex::new(of_format("state/capacity-forecast.json", FORECAST_FORMAT)),
            ladders: Mutex::new(of_format("state/routing-ladders.json", "routing-ladders/v1")),
            value: Mutex::new(read(&root.join("state/usage-value.json")).map(|(d, t)| (Arc::new(d), Bytes::from(t)))),
            policies: Mutex::new(reset_policies(&install)),
            install: Arc::new(install),
            version: tokio::sync::watch::Sender::new(0),
        }
    }

    fn bump(&self) {
        self.version.send_modify(|v| *v = v.wrapping_add(1));
    }

    pub fn pool(&self) -> Arc<Value> {
        lock(&self.pool).clone()
    }

    /// The state as the refresh cycle holds it now (each save, and each cycle's end).
    pub fn set_pool(&self, state: &Value) {
        *lock(&self.pool) = Arc::new(state.clone());
        self.bump();
    }

    pub fn forecast(&self) -> Option<Arc<Value>> {
        lock(&self.forecast).clone()
    }

    pub fn set_forecast(&self, doc: Arc<Value>) {
        *lock(&self.forecast) = Some(doc);
        self.bump();
    }

    pub fn ladders(&self) -> Option<Arc<Value>> {
        lock(&self.ladders).clone()
    }

    pub fn set_ladders(&self, doc: Arc<Value>) {
        *lock(&self.ladders) = Some(doc);
        self.bump();
    }

    pub fn value(&self) -> Option<(Arc<Value>, Bytes)> {
        lock(&self.value).clone()
    }

    pub fn set_value(&self, doc: Value, written: String) {
        *lock(&self.value) = Some((Arc::new(doc), Bytes::from(written)));
        self.bump();
    }

    pub fn policies(&self) -> Vec<ResetPolicy> {
        lock(&self.policies).clone()
    }

    /// `resetPolicies` as the last refresh loaded them; a change only when they differ.
    pub fn set_policies(&self, policies: &[ResetPolicy]) {
        let mut p = lock(&self.policies);
        if p.as_slice() != policies {
            *p = policies.to_vec();
            drop(p);
            self.bump();
        }
    }

    /// The inputs of `/api/status`, `/api/summary` and the dashboard, with the service's
    /// memory `live`.
    pub fn inputs(&self, live: pool_core::status::Live) -> crate::served::Inputs {
        crate::served::Inputs {
            pool: self.pool(),
            forecast: self.forecast(),
            value: self.value().map(|(d, _)| d),
            expected: self.install.get("expectedAccounts").cloned(),
            policies: self.policies(),
            live,
        }
    }
}
