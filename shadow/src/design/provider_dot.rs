//! A provider's color, before its name: `<ProviderDot provider="claude" size={DotSize::Sm} title={None} />`.
//! The name is always written beside it (or in `title`), so the color is never the only sign.
use corten::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DotSize {
    Sm,
    Lg,
}

#[component(css)]
pub fn ProviderDot<'a>(provider: &'a str, size: DotSize, title: Option<&'a str>) -> impl Render {
    let color = match provider {
        "claude" => Some(Claude),
        "codex" => Some(Codex),
        _ => None,
    };
    let size = match size {
        DotSize::Sm => Sm,
        DotSize::Lg => Lg,
    };
    html! { <span class={(ProviderDot, size, color)} title={title}></span> }
}