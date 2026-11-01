//! One large figure in a card, its label over it and a line under it.
use corten::prelude::*;

#[component(css)]
pub fn Kpi<'a>(label: &'a str, value: &'a str, sub: &'a str) -> impl Render {
    html! {
        <div class={Kpi}>
            <div class={Label}>{label}</div>
            <div class={Value}>{value}</div>
            <div class={Sub}>{sub}</div>
        </div>
    }
}