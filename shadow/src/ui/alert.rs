// corten ui 0.1.0: alert 428d34de5f2eeeee
//! A message the page calls out. Its heading and text are yours (`<h2>` to `<h6>` or `<strong>`,
//! then `<p>`). `AlertRole::Alert` is `role="alert"`, so a screen reader reads it at once when a
//! patch brings it in, as a form's error does; `AlertRole::Status` waits its turn (`role="status"`),
//! for news that is not a problem.
//!
//! ```rust,ignore
//! <Alert variant={AlertVariant::Destructive} role={AlertRole::Alert}>
//!     <strong>"Payment failed"</strong>
//!     <p>"Your card was declined."</p>
//!     <AlertActions><Button ..><a href={url::Billing}>"Update card"</a></Button></AlertActions>
//! </Alert>
//! ```
//!
//! Agent Pools' edits: the status tones (`Info`, `Success`, `Warning`) beside the kit's two, the
//! role chosen where it is used, and `AlertActions` at the right end.
use corten::prelude::*;

/// Each variant is a class in `alert.css`, so one without a rule does not compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertVariant {
    Neutral,
    Info,
    Success,
    Warning,
    Destructive,
}

/// How assistive technology hears the message when it appears.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertRole {
    /// At once, interrupting: a failure the user must act on.
    Alert,
    /// When the reader is free: a confirmation, a step to finish.
    Status,
}

/// A variant without a rule in `alert.css` does not compile:
///
/// ```compile_fail
/// use corten::prelude::*;
///
/// #[component(css)]
/// fn Alert() -> impl Render {
///     html! { <div class={(Alert, Urgent)}></div> }
/// }
/// ```
#[component(css)]
pub fn Alert<C: Render>(variant: AlertVariant, role: AlertRole, children: C) -> impl Render {
    let variant = match variant {
        AlertVariant::Neutral => Neutral,
        AlertVariant::Info => Info,
        AlertVariant::Success => Success,
        AlertVariant::Warning => Warning,
        AlertVariant::Destructive => Destructive,
    };
    let role = match role {
        AlertRole::Alert => "alert",
        AlertRole::Status => "status",
    };
    html! { <div class={(Alert, variant)} role={role}>{children}</div> }
}

/// The message's controls, at its right end (under it on a phone).
#[component(css = Alert)]
pub fn AlertActions<C: Render>(children: C) -> impl Render {
    html! { <div class={Actions}>{children}</div> }
}