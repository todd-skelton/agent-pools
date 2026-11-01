//! An account's state as a dot, its words in `title` and, when it is not healthy, in a chip
//! beside it.
use corten::prelude::*;

use super::Tone;

#[component(css)]
pub fn StatusDot<'a>(tone: Tone, title: &'a str) -> impl Render {
    let tone = match tone {
        Tone::Ok => Success,
        Tone::Warn => Warning,
        Tone::Bad => Danger,
        Tone::Info => Info,
        Tone::Neutral => Neutral,
    };
    html! { <span class={(StatusDot, tone)} title={title}></span> }
}