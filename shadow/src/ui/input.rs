// corten ui 0.1.0: input bfd68bde76bf7a71
//! A text field's look, on the `<input>` written inside it. The element stays yours, so its
//! `name={f.email}`, `type`, `required` and the rest are checked as always, and the wrapper adds
//! no box:
//!
//! ```rust,ignore
//! <Input><input type="email" name={f.email} required></Input>
//! ```
//!
//! A field the browser finds invalid after the user edits it (`:user-invalid`), or one the
//! server marked (`aria-invalid="true"`, which `e.aria(field)` writes), gets the destructive
//! border.
use corten::prelude::*;

/// A part without a rule in `input.css` does not compile:
///
/// ```compile_fail
/// use corten::prelude::*;
///
/// #[component(css)]
/// fn Input() -> impl Render {
///     html! { <span class={(Input, Large)}></span> }
/// }
/// ```
#[component(css)]
pub fn Input<C: Render>(children: C) -> impl Render {
    html! { <span class={Input}>{children}</span> }
}
