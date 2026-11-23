// SPDX-License-Identifier: MIT OR Apache-2.0

//! The videocall-rs design system.
//!
//! Three stylesheets ([`TOKENS_CSS`], [`BASE_CSS`], [`COMPONENTS_CSS`]) and
//! thin Dioxus components that emit exactly the classes those sheets define.
//! The header comment of `styles/components.css` is the contract: if a class
//! is not listed there, it is not part of the system.
//!
//! An app links the sheets itself (with Trunk, `rel="css"`), tokens first,
//! because tokens.css declares the cascade layer order. The constants here
//! are for consumers that cannot do that, such as a browser test that mounts
//! a component and wants the real styles.
//!
//! The components are presentational. They hold no timers and no global
//! state. Every one takes `class` (joined after the system classes) and
//! forwards any other global attribute (`id`, `data-testid`, `aria-*`, ...)
//! to its root element.

pub mod classes;
pub mod components;
pub mod icons;
/// Gallery helper: simulates the accessibility media queries. Not part of
/// the app-facing API, so it only exists with the `gallery` feature (and in
/// this crate's own tests, which pin its behavior).
#[cfg(any(feature = "gallery", test))]
pub mod preview;
pub mod tokens;

pub use classes::*;
pub use components::*;

/// `styles/tokens.css`: every value, the layer order and the appearances.
pub const TOKENS_CSS: &str = include_str!("../styles/tokens.css");

/// `styles/base.css`: the element reset and the page defaults and helpers.
pub const BASE_CSS: &str = include_str!("../styles/base.css");

/// `styles/components.css`: the shared primitives.
pub const COMPONENTS_CSS: &str = include_str!("../styles/components.css");

/// The three sheets in the order they must load: `(file name, contents)`.
pub const STYLESHEETS: [(&str, &str); 3] = [
    ("tokens.css", TOKENS_CSS),
    ("base.css", BASE_CSS),
    ("components.css", COMPONENTS_CSS),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_come_first_and_declare_the_layer_order() {
        assert_eq!(STYLESHEETS[0].0, "tokens.css");
        assert!(TOKENS_CSS.contains("@layer reset, tokens, base, components, features, overrides;"));
    }

    #[test]
    fn each_sheet_lives_in_its_layer() {
        assert!(TOKENS_CSS.contains("@layer tokens {"));
        assert!(BASE_CSS.contains("@layer reset {"));
        assert!(BASE_CSS.contains("@layer base {"));
        assert!(COMPONENTS_CSS.contains("@layer components {"));
    }
}
