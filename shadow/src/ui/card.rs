// corten ui 0.1.0: card 3433de8de2526243
//! A surface that groups related content. The heading inside `CardHeader` is yours, so the page
//! keeps its outline (`<h2>` or `<h3>` as the page needs), and a `<p>` after it is the
//! description:
//!
//! ```rust,ignore
//! <Card>
//!     <CardHeader><h2>"Payouts"</h2><p>"Sent every Friday."</p></CardHeader>
//!     <CardContent>..</CardContent>
//!     <CardFooter>..</CardFooter>
//! </Card>
//! ```
//!
//! Agent Pools' edits: the header is one row (a provider dot, the heading, a note, a chip) with a
//! divider under it, and `CardAction` holds its controls at the right end; the parts are at the
//! dashboard's density; a `Table` placed straight in the card, outside `CardContent`, runs edge
//! to edge.
use corten::prelude::*;

/// A part without a rule in `card.css` does not compile:
///
/// ```compile_fail
/// use corten::prelude::*;
///
/// #[component(css)]
/// fn Card() -> impl Render {
///     html! { <div class={Banner}></div> }
/// }
/// ```
#[component(css)]
pub fn Card<C: Render>(children: C) -> impl Render {
    html! { <div class={Card}>{children}</div> }
}

#[component(css = Card)]
pub fn CardHeader<C: Render>(children: C) -> impl Render {
    html! { <div class={Header}>{children}</div> }
}

/// The header's controls (buttons, a legend), at its right end.
#[component(css = Card)]
pub fn CardAction<C: Render>(children: C) -> impl Render {
    html! { <div class={Action}>{children}</div> }
}

#[component(css = Card)]
pub fn CardContent<C: Render>(children: C) -> impl Render {
    html! { <div class={Content}>{children}</div> }
}

#[component(css = Card)]
pub fn CardFooter<C: Render>(children: C) -> impl Render {
    html! { <div class={Footer}>{children}</div> }
}
