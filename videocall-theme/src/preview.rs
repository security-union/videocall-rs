// SPDX-License-Identifier: MIT OR Apache-2.0

//! Simulating the accessibility settings without touching the stylesheets.
//!
//! Light and dark need no help: tokens.css has a pin for each, `.theme-light`
//! and `.theme-dark`, so a page forces an appearance by putting the class on
//! `<html>`. Increase Contrast, Reduce Transparency and Reduce Motion have no
//! class. They can only come from the OS. To preview them, [`apply`]
//! rewrites the media feature in an in-memory copy of a sheet into
//! `(min-width: 0px)`, which always matches. The files on disk stay exactly
//! as they are.
//!
//! The gallery uses this, which is why the module only exists with the
//! `gallery` feature. A browser test that mounts components with
//! [`crate::STYLESHEETS`] can turn the feature on and use it the same way.

use std::borrow::Cow;

/// The OS settings to simulate. `Default` changes nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Preview {
    /// Force `prefers-contrast: more` on.
    pub increase_contrast: bool,
    /// Force `prefers-reduced-transparency: reduce` on.
    pub reduce_transparency: bool,
    /// Force `prefers-reduced-motion: reduce` on.
    pub reduce_motion: bool,
}

const CONTRAST: &str = "(prefers-contrast: more)";
const TRANSPARENCY: &str = "(prefers-reduced-transparency: reduce)";
const MOTION: &str = "(prefers-reduced-motion: reduce)";

/// Always true. Valid inside `and` and in a comma list, unlike `all`.
const ALWAYS: &str = "(min-width: 0px)";

/// The class that pins an appearance, for `<html>` or any subtree.
pub const THEME_LIGHT: &str = "theme-light";
/// See [`THEME_LIGHT`].
pub const THEME_DARK: &str = "theme-dark";

/// Returns `css` with the media features of `preview` forced on.
///
/// Borrows when nothing needs to change, so the default preview costs
/// nothing.
pub fn apply<'a>(css: &'a str, preview: &Preview) -> Cow<'a, str> {
    let mut replacements: Vec<(&str, &str)> = Vec::new();
    if preview.increase_contrast {
        replacements.push((CONTRAST, ALWAYS));
    }
    if preview.reduce_transparency {
        replacements.push((TRANSPARENCY, ALWAYS));
    }
    if preview.reduce_motion {
        replacements.push((MOTION, ALWAYS));
    }

    if replacements.iter().all(|(from, _)| !css.contains(from)) {
        return Cow::Borrowed(css);
    }
    let mut out = css.to_string();
    for (from, to) in replacements {
        out = out.replace(from, to);
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BASE_CSS, COMPONENTS_CSS, TOKENS_CSS};

    /// Counts the queries that actually use a feature, not comment mentions.
    fn media_uses(css: &str, feature: &str) -> usize {
        css.lines()
            .filter(|line| line.trim_start().starts_with("@media") && line.contains(feature))
            .count()
    }

    // If one of these fails, a sheet changed how it writes a media query and
    // the gallery's toggles would silently stop working. Update the constants
    // above to match.
    #[test]
    fn the_sheets_use_the_queries_this_module_rewrites() {
        assert_eq!(media_uses(TOKENS_CSS, CONTRAST), 2);
        assert_eq!(media_uses(COMPONENTS_CSS, CONTRAST), 2);
        assert_eq!(media_uses(COMPONENTS_CSS, TRANSPARENCY), 1);
        assert_eq!(media_uses(COMPONENTS_CSS, MOTION), 1);
        assert_eq!(media_uses(BASE_CSS, MOTION), 1);
    }

    // The pins are plain classes in tokens.css, so the gallery can set them
    // on <html> instead of rewriting the dark queries.
    #[test]
    fn both_pins_exist_in_tokens() {
        for class in [THEME_LIGHT, THEME_DARK] {
            let selector = format!(".{class} {{");
            assert!(TOKENS_CSS.contains(&selector), "{selector}");
        }
        // The OS dark queries step aside for a light pin on <html>.
        assert_eq!(
            TOKENS_CSS.matches(":root:not(.theme-light) {").count(),
            media_uses(TOKENS_CSS, "(prefers-color-scheme: dark)")
        );
    }

    #[test]
    fn default_preview_borrows() {
        for css in [TOKENS_CSS, BASE_CSS, COMPONENTS_CSS] {
            assert!(matches!(apply(css, &Preview::default()), Cow::Borrowed(_)));
        }
    }

    #[test]
    fn contrast_forces_both_queries_on() {
        let preview = Preview {
            increase_contrast: true,
            ..Preview::default()
        };
        let out = apply(TOKENS_CSS, &preview);
        assert_eq!(media_uses(&out, CONTRAST), 0);
        assert!(out.contains("@media (min-width: 0px) {"));
        assert!(out.contains("@media (min-width: 0px) and (prefers-color-scheme: dark) {"));
    }

    #[test]
    fn transparency_and_motion() {
        let preview = Preview {
            reduce_transparency: true,
            reduce_motion: true,
            ..Preview::default()
        };
        let out = apply(COMPONENTS_CSS, &preview);
        assert!(out.contains("@media (min-width: 0px), (prefers-contrast: more) {"));
        assert_eq!(media_uses(&out, MOTION), 0);
        let base = apply(BASE_CSS, &preview);
        assert_eq!(media_uses(&base, MOTION), 0);
    }
}
