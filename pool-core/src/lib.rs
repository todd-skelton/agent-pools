//! Agent Pools' decisions, ported from the JavaScript at Agent Pools commit `5c696f6`
//! (`reference/agent-pools/`), held to the golden outputs in `golden/outputs/`.
//!
//! Every function takes the time as `now` (milliseconds since the epoch) and does no I/O.
//! Provider payloads stay `serde_json::Value`, because the JavaScript reads them loosely and
//! the port has to read them the same way.
//!
//! Ported: quota windows and normalized usage ([`quota`]), routing order and the banked-credit
//! choice ([`routing`]), Codex credits and the Claude bank ([`banks`]), reset policies
//! ([`resets`]), the capacity forecast ([`forecast`]), the usage value report and session usage
//! ([`value`]) `/api/summary` with account freshness ([`summary`]) and `/api/status` ([`status`]), over the provider facts
//! in [`providers`].
//!
//! # Where the port knowingly differs
//!
//! These are inputs no provider has sent and the golden set does not hold; each is listed so
//! a change in a provider's format shows up as a decision, not a surprise.
//!
//! 1. A date-time with no offset (`2026-10-28T00:00:00`) is local time in JavaScript; here it
//!    is unparsed (`None`). Every timestamp shape in the recorded inputs has an offset or `Z`.
//! 2. Date strings only V8's legacy parser reads (`Oct 28 2026`) are unparsed.
//! 3. `localeCompare` is ICU's root collation for printable ASCII; other characters order by
//!    code point after it. Account ids are ASCII.
//! 4. Sorts follow V8's `Array.prototype.sort` step for step for arrays under 64 elements
//!    (every sort here); longer ones use a stable merge, which agrees whenever the comparator
//!    is consistent.
//! 5. Where the JavaScript would throw a `TypeError` on a malformed payload (`null` in a list
//!    of limits, `"percent" in 5`, a non-string scope lower-cased), the port skips the item or
//!    treats the value as a string. Agent Pools catches such a throw as a failed read.
//! 6. A numeric string where a number is expected (`limit_window_seconds: "3600"`,
//!    `starts_at: 5`) is not coerced the way JavaScript would; no provider sends one.
//! 7. A ledger row with neither an account nor a `prov` is filed under the provider
//!    `"undefined"`, where the JavaScript keeps `undefined` and so drops `provider` from that
//!    model's row. The proxy writes `prov` on every row (all 250,238 recorded rows have it).

pub mod banks;
pub mod forecast;
pub mod js;
pub mod providers;
pub mod quota;
pub mod resets;
pub mod routing;
pub mod status;
pub mod summary;
pub mod value;
