//! The provider facts the decisions read (providers.mjs `PROVIDERS`, provider-codex.mjs and
//! provider-claude.mjs): redemption kind, reset defaults and capacity facts. Endpoints,
//! request identity and everything else that talks to a provider stay out of the port.

/// What spending a banked credit does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Redemption {
    /// Codex: restarts the weekly window from now.
    Reset,
    /// Claude: clears usage but keeps the account's reset date.
    Refill,
}

impl Redemption {
    pub fn as_str(self) -> &'static str {
        match self {
            Redemption::Reset => "reset",
            Redemption::Refill => "refill",
        }
    }
}

/// `resetDefaults`.
#[derive(Clone, Copy, Debug)]
pub struct ResetDefaults {
    pub enabled: bool,
    pub trigger: &'static str,
    pub headroom_percent: f64,
    pub redeem_before_lapse: bool,
    /// Refill providers only.
    pub max_runway_percent: Option<f64>,
}

/// `capacity`: facts for the capacity forecast.
#[derive(Clone, Copy, Debug)]
pub struct Capacity {
    pub weekly_window_id: &'static str,
    pub window_starts_on_first_use: bool,
    pub plan_weights: &'static [(&'static str, f64)],
}

#[derive(Clone, Copy, Debug)]
pub struct Provider {
    pub id: &'static str,
    pub name: &'static str,
    /// The proxy's OAuth provider id (`loginId`).
    pub login_id: &'static str,
    pub redemption: Redemption,
    pub reset_defaults: ResetDefaults,
    pub capacity: Capacity,
    pub max_headroom_percent: f64,
}

pub const CODEX: Provider = Provider {
    id: "codex",
    name: "Codex",
    login_id: "codex",
    redemption: Redemption::Reset,
    reset_defaults: ResetDefaults {
        enabled: true,
        trigger: "any",
        headroom_percent: 5.0,
        redeem_before_lapse: true,
        max_runway_percent: None,
    },
    capacity: Capacity {
        weekly_window_id: "codex:primary_window",
        window_starts_on_first_use: true,
        plan_weights: &[("prolite", 0.5)],
    },
    max_headroom_percent: 10.0,
};

pub const CLAUDE: Provider = Provider {
    id: "claude",
    name: "Claude",
    login_id: "anthropic",
    redemption: Redemption::Refill,
    reset_defaults: ResetDefaults {
        enabled: true,
        trigger: "weekly",
        headroom_percent: 0.0,
        redeem_before_lapse: true,
        max_runway_percent: Some(50.0),
    },
    capacity: Capacity {
        weekly_window_id: "limit:1",
        window_starts_on_first_use: false,
        plan_weights: &[],
    },
    max_headroom_percent: 100.0,
};

/// `PROVIDER_IDS` order.
pub const PROVIDERS: [Provider; 2] = [CODEX, CLAUDE];

/// `PROVIDERS[id]` (a null-prototype object, so only the real ids resolve).
pub fn provider(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.id == id)
}
