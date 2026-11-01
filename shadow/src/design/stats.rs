//! A row of figures, each with its label over it and a line under it:
//!
//! ```rust,ignore
//! <Stats>
//!     <Stat label="Runway" value="No shortfall" sub="next 14 days" tone={Some(Tone::Ok)} />
//! </Stats>
//! ```
use corten::prelude::*;

use super::Tone;

#[component(css)]
pub fn Stats<C: Render>(children: C) -> impl Render {
    html! { <div class={Stats}>{children}</div> }
}

#[component(css = Stats)]
pub fn Stat<'a>(label: &'a str, value: &'a str, sub: &'a str, tone: Option<Tone>) -> impl Render {
    let tone = match tone {
        Some(Tone::Ok) => Some(Success),
        Some(Tone::Warn) => Some(Warning),
        Some(Tone::Bad) => Some(Danger),
        _ => None,
    };
    html! {
        <div class={Stat}>
            <div class={Label}>{label}</div>
            <div class={(Value, tone)}>{value}</div>
            <div class={Sub}>{sub}</div>
        </div>
    }
}