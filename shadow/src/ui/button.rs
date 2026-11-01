// corten ui 0.1.0: button 71f206bc167066a1
//! A button's look, on the `<button>` or `<a>` written inside it. The element stays yours, so
//! `html!` still checks everything it takes (`type`, `disabled`, `href={url::Home}`,
//! `on:click`, `commandfor`), and the wrapper adds no box (`display: contents`):
//!
//! ```rust,ignore
//! <Button variant={ButtonVariant::Destructive} size={ButtonSize::Sm}>
//!     <button type="submit">"Delete"</button>
//! </Button>
//! <Button variant={ButtonVariant::Link} size={ButtonSize::Md}>
//!     <a href={url::Home}>"Home"</a>
//! </Button>
//! ```
use corten::prelude::*;

/// Each variant is a class in `button.css`, so one without a rule does not compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonVariant {
    Primary,
    Secondary,
    Outline,
    Ghost,
    Destructive,
    Link,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonSize {
    Sm,
    Md,
    Lg,
    /// A square button holding an icon; give the element an `aria-label`.
    Icon,
}

/// A variant or size without a rule in `button.css` does not compile:
///
/// ```compile_fail
/// use corten::prelude::*;
///
/// #[component(css)]
/// fn Button() -> impl Render {
///     html! { <span class={(Button, Huge)}></span> }
/// }
/// ```
#[component(css)]
pub fn Button<C: Render>(variant: ButtonVariant, size: ButtonSize, children: C) -> impl Render {
    let variant = match variant {
        ButtonVariant::Primary => Primary,
        ButtonVariant::Secondary => Secondary,
        ButtonVariant::Outline => Outline,
        ButtonVariant::Ghost => Ghost,
        ButtonVariant::Destructive => Destructive,
        ButtonVariant::Link => Link,
    };
    let size = match size {
        ButtonSize::Sm => Sm,
        ButtonSize::Md => Md,
        ButtonSize::Lg => Lg,
        ButtonSize::Icon => Icon,
    };
    html! { <span class={(Button, variant, size)}>{children}</span> }
}
