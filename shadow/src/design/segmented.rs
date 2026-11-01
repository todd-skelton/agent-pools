//! Links that each show one view of the page, the current one marked: the `<a>`s are the app's
//! (typed routes, `aria-current="page"` on the one shown), inside a `<nav>` named by `label`.
//!
//! ```rust,ignore
//! <Segmented label="Filter accounts">
//!     <a href={url::Accounts { filter: None }} aria-current="page">"All"</a>
//!     <a href={url::Accounts { filter: Some("credits") }}>"Has credits"</a>
//! </Segmented>
//! ```
use corten::prelude::*;

#[component(css)]
pub fn Segmented<'a, C: Render>(label: &'a str, children: C) -> impl Render {
    html! { <nav class={Segmented} aria-label={label}>{children}</nav> }
}