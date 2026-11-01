// corten ui 0.1.0: table 316401ee046d241a
//! A table's look, on the `<table>` written inside it: the rows, headers and caption stay the
//! app's markup. The wrapper scrolls sideways when the table is wider than the page.
//!
//! ```rust,ignore
//! <Table>
//!     <table>
//!         <caption>"Recent orders"</caption>
//!         <thead><tr><th scope="col">"Order"</th><th scope="col">"Total"</th></tr></thead>
//!         <tbody>..</tbody>
//!     </table>
//! </Table>
//! ```
use corten::prelude::*;

/// A part without a rule does not compile:
///
/// ```compile_fail
/// use corten::prelude::*;
///
/// #[component(css)]
/// fn Table() -> impl Render {
///     html! { <div class={(Table, Striped)}></div> }
/// }
/// ```
#[component(css)]
pub fn Table<C: Render>(children: C) -> impl Render {
    html! { <div class={Table}>{children}</div> }
}
