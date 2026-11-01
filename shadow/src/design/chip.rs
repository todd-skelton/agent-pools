//! A status in a pill: `<Chip tone={Tone::Warn} dot={true} size={ChipSize::Md} title={None}>"Spend"</Chip>`.
//! The dot repeats the tone for a reader who does not see the color; `dot={false}` for a count or
//! a severity next to a heading.
use corten::prelude::*;

use super::Tone;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipSize {
    Sm,
    Md,
}

#[component(css)]
pub fn Chip<'a, C: Render>(tone: Tone, dot: bool, size: ChipSize, title: Option<&'a str>, children: C) -> impl Render {
    let tone = match tone {
        Tone::Ok => Success,
        Tone::Warn => Warning,
        Tone::Bad => Danger,
        Tone::Info => Info,
        Tone::Neutral => Neutral,
    };
    let size = match size {
        ChipSize::Sm => Sm,
        ChipSize::Md => Md,
    };
    html! { <span class={(Chip, tone, size, dot.then_some(Dot))} title={title}>{children}</span> }
}