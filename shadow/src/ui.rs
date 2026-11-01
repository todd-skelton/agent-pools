// corten ui 0.1.0
//! The UI kit: the theme tokens (`tokens/ui.resolver.json`, light and dark) and the components
//! `corten ui add` copied into `src/ui/`. They are the app's to change; `corten ui list` names
//! the others.

use corten::Component;

pub mod alert;
pub mod badge;
pub mod button;
pub mod card;
pub mod input;
pub mod table;

corten::style::tokens!("tokens/ui.resolver.json");

/// The tokens, then each component with a stylesheet: `App::components(ui::COMPONENTS)`.
pub const COMPONENTS: &[Component] = &[
    TOKENS,
    alert::Alert,
    badge::Badge,
    button::Button,
    card::Card,
    input::Input,
    table::Table,
];
