//! Agent Pools' design system: the kit's theme (`tokens/`, through [`crate::ui`]) and the
//! components the pages share, each a `#[component(css)]` on the theme's tokens. The kit's own
//! components (`crate::ui`: `Button`, `Card`, `Table`, `Alert`, `Badge`, `Input`) are used as they
//! are where they fit; these are what an operations dashboard needs that the kit does not have.
//!
//! | Component | What it draws |
//! |---|---|
//! | [`Chip`] | a status in a pill, in a [`Tone`], with or without its dot |
//! | [`ProviderDot`], [`StatusDot`] | a provider's color (Claude, Codex); an account's state |
//! | [`Meter`] | a share used, its fill toned by level, with an optional mark (where time is) |
//! | [`Stats`], [`Stat`] | a row of figures with a label and a line under each |
//! | [`Kpi`] | one large figure in a card |
//! | [`Eyebrow`] | the small uppercase label over a group |
//! | [`Segmented`] | links that choose one view of a page, the current one marked |
//!
//! Tokens (`tokens/ui.tokens.json`, light, and `ui-dark.tokens.json`): the kit's names
//! (`--background`, `--card`, `--primary`, `--muted-foreground`, `--border`, `--input`,
//! `--ring`, `--radius`, ...) in Agent Pools' colors, and Agent Pools' own: `--success`,
//! `--warning`, `--info` (with `--destructive`, the four tones), `--subtle-foreground` (the
//! quietest text that still meets WCAG AA), `--claude` and `--codex`, `--font-sans` and
//! `--font-mono`. Every text color meets 4.5:1 on the surfaces it is drawn on, and `--input` 3:1,
//! in both themes. The dashboard is dark (`data-theme="dark"` on `<html>`); the light theme is
//! one attribute away.
//!
//! Sizes from data (a meter's fill, a mark's place) are custom properties each component
//! registers with `@property` and sets with `style={..}` (`FILL.pct(42.0)`), so no page needs
//! style attributes and the CSP stays whole.

mod chip;
mod eyebrow;
mod kpi;
mod meter;
mod provider_dot;
mod segmented;
mod stats;
mod status_dot;

pub use chip::{Chip, ChipSize};
pub use eyebrow::Eyebrow;
pub use kpi::Kpi;
pub use meter::Meter;
pub use provider_dot::{DotSize, ProviderDot};
pub use segmented::Segmented;
pub use stats::{Stat, Stats};
pub use status_dot::StatusDot;

/// How good or bad a thing is: a chip's color, a meter's fill, a figure's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Warn,
    Bad,
    Info,
    Neutral,
}

/// Each component with a stylesheet, after the kit's (`ui::COMPONENTS`), so a rule here wins a
/// tie with the kit's.
pub const COMPONENTS: &[corten::Component] = &[Chip, Eyebrow, Kpi, Meter, ProviderDot, Segmented, Stats, StatusDot];