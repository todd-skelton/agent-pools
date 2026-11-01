//! The small uppercase label over a group of figures or rows.
use corten::prelude::*;

#[component(css)]
pub fn Eyebrow<'a, C: Render>(title: Option<&'a str>, children: C) -> impl Render {
    html! { <div class={Eyebrow} title={title}>{children}</div> }
}