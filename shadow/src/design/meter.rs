//! A share used, as a bar and its percent: `<Meter used={42.0} tone={None} tick={Some(60.0)} />`.
//! `tone` colors the fill (`None` is healthy); `tick` marks a place on the track, such as how far
//! through its window an account is. The fill and the mark are custom properties set from data
//! (`--meter__width`, `--meter__left`), so the page needs no style attribute.
use corten::prelude::*;

use super::Tone;

#[component(css)]
pub fn Meter(used: f64, tone: Option<Tone>, tick: Option<f64>) -> impl Render {
    let fill = match tone {
        Some(Tone::Bad) => Some(Danger),
        Some(Tone::Warn) => Some(Warning),
        _ => None,
    };
    html! {
        <div class={Meter}>
            <div class={Track}>
                <div class={(Fill, fill)} style={WIDTH.pct(used.clamp(0.0, 100.0))}></div>
                if let Some(t) = tick { <span class={Tick} style={LEFT.pct(t)}></span> }
            </div>
            <span class={Pct}>{pool_core::js::round(used)}"%"</span>
        </div>
    }
}