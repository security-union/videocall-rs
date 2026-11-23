// SPDX-License-Identifier: MIT OR Apache-2.0

//! The typed primitives. One component per contract class in
//! `styles/components.css`, each a thin wrapper that only picks classes and
//! ARIA attributes.
//!
//! Conventions shared by all of them:
//! - `class` is joined after the system classes. Dioxus does not merge a
//!   spread `class` with the element's own, so it is a real prop.
//! - Any other global attribute (`id`, `data-testid`, `aria_*`, `title`,
//!   `style`, ...) is forwarded to the root element, and controls also take
//!   their element's own attributes (`value`, `placeholder`, `name`, ...).
//! - Event handlers are explicit `Option<EventHandler<_>>` props, because
//!   attribute extension does not carry listeners.
//! - A component that brings an ARIA role (sheet, alert, menu, menu item,
//!   tab list) steps aside when the caller passes `role`: the caller's role
//!   replaces it, together with the attributes that only make sense with
//!   that role (`aria-modal`, `aria-orientation`). `role: None::<&str>`
//!   renders no role at all.

mod buttons;
mod content;
mod forms;
mod navigation;
mod overlays;
mod status;
mod switch;

/// Whether the caller passed attribute `name`, even with a `None` value.
/// That is how a component knows to leave its own default out.
pub(crate) fn caller_sets(attributes: &[dioxus::prelude::Attribute], name: &str) -> bool {
    attributes.iter().any(|attribute| attribute.name == name)
}

pub use buttons::*;
pub use content::*;
pub use forms::*;
pub use navigation::*;
pub use overlays::*;
pub use status::*;
pub use switch::*;
