/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 */

//! `static/auth.css` copies its custom properties from the design system
//! (`videocall-theme/styles/tokens.css`) because the pages ship one plain
//! stylesheet with no build step. This test keeps the copy honest: in each
//! appearance mode (light, dark, Increase Contrast light and dark) every
//! property auth.css declares must resolve to the theme's value for the
//! same mode. Properties the theme does not have must be listed in
//! [`AUTH_ONLY`].
//!
//! The comparison resolves the cascade per mode for `:root` only: a block
//! applies when its selector list contains `:root` (or `:root:not(...)`,
//! which matches an unpinned root) and every condition of its enclosing
//! `@media` is a colour-scheme or contrast condition that holds in that
//! mode; the last applicable declaration in source order wins. `@layer` is
//! transparent. Blocks under `@supports` or other media features (hover,
//! width) are context tweaks, not modes, and are skipped on both sides.
//! The theme's `.theme-light` / `.theme-dark` subtree pins do not apply to
//! a page root and are ignored.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Properties declared only in auth.css, with why.
const AUTH_ONLY: &[(&str, &str)] = &[];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mode {
    dark: bool,
    contrast: bool,
}

const MODES: [(Mode, &str); 4] = [
    (
        Mode {
            dark: false,
            contrast: false,
        },
        "light",
    ),
    (
        Mode {
            dark: true,
            contrast: false,
        },
        "dark",
    ),
    (
        Mode {
            dark: false,
            contrast: true,
        },
        "increased contrast (light)",
    ),
    (
        Mode {
            dark: true,
            contrast: true,
        },
        "increased contrast (dark)",
    ),
];

/// One custom-property declaration and the context it sits in.
#[derive(Debug)]
struct Declaration {
    name: String,
    value: String,
    /// Enclosing at-rule and selector preludes, outermost first.
    context: Vec<String>,
}

fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Every `--name: value` declaration with its block context. Good enough
/// for these two hand-written files: no strings contain braces or `;`.
fn custom_properties(css: &str) -> Vec<Declaration> {
    let css = strip_comments(css);
    let mut stack: Vec<String> = Vec::new();
    let mut buffer = String::new();
    let mut out = Vec::new();
    let take = |buffer: &mut String, stack: &[String], out: &mut Vec<Declaration>| {
        let text = buffer.trim();
        if let Some((name, value)) = text.split_once(':') {
            let name = name.trim();
            if name.starts_with("--") {
                out.push(Declaration {
                    name: name.to_string(),
                    value: value.split_whitespace().collect::<Vec<_>>().join(" "),
                    context: stack.to_vec(),
                });
            }
        }
        buffer.clear();
    };
    for ch in css.chars() {
        match ch {
            '{' => {
                stack.push(buffer.split_whitespace().collect::<Vec<_>>().join(" "));
                buffer.clear();
            }
            '}' => {
                take(&mut buffer, &stack, &mut out);
                stack.pop();
            }
            ';' => take(&mut buffer, &stack, &mut out),
            _ => buffer.push(ch),
        }
    }
    out
}

/// Does a block with this context apply to the page root in `mode`?
fn applies(context: &[String], mode: Mode) -> bool {
    let Some((selector, at_rules)) = context.split_last() else {
        return false;
    };
    let root = selector.split(',').map(str::trim).any(|item| {
        item == ":root" || (item.starts_with(":root:not(") && !item.contains(".theme-dark"))
    });
    if !root {
        return false;
    }
    for at_rule in at_rules {
        if at_rule.starts_with("@layer") {
            continue;
        }
        let Some(query) = at_rule.strip_prefix("@media") else {
            return false; // @supports and anything else: not a mode.
        };
        for condition in query.split(" and ") {
            let condition = condition
                .trim()
                .trim_start_matches('(')
                .trim_end_matches(')')
                .replace(' ', "");
            let holds = match condition.as_str() {
                "prefers-color-scheme:dark" => mode.dark,
                "prefers-color-scheme:light" => !mode.dark,
                "prefers-contrast:more" => mode.contrast,
                _ => return false,
            };
            if !holds {
                return false;
            }
        }
    }
    true
}

/// The cascaded value of each custom property on the page root in `mode`.
fn resolve(declarations: &[Declaration], mode: Mode) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for declaration in declarations {
        if applies(&declaration.context, mode) {
            values.insert(declaration.name.clone(), declaration.value.clone());
        }
    }
    values
}

fn read(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

#[test]
fn auth_css_tokens_match_the_theme_in_every_mode() {
    let auth = custom_properties(&read("static/auth.css"));
    let theme = custom_properties(&read("../videocall-theme/styles/tokens.css"));
    assert!(!auth.is_empty() && !theme.is_empty(), "parsed no tokens");

    let theme_names: std::collections::BTreeSet<&str> =
        theme.iter().map(|d| d.name.as_str()).collect();
    let mut problems = Vec::new();

    for (name, _why) in AUTH_ONLY {
        if theme_names.contains(name) {
            problems.push(format!(
                "{name} is in AUTH_ONLY but the theme declares it; remove it from the allowlist"
            ));
        }
        if !auth.iter().any(|d| d.name == *name) {
            problems.push(format!(
                "{name} is in AUTH_ONLY but auth.css no longer declares it"
            ));
        }
    }
    for declaration in &auth {
        let allowed = AUTH_ONLY.iter().any(|(name, _)| *name == declaration.name);
        if !theme_names.contains(declaration.name.as_str()) && !allowed {
            problems.push(format!(
                "{} is declared in auth.css but not in the theme; add it to AUTH_ONLY with a reason or use a theme token",
                declaration.name
            ));
        }
    }

    for (mode, label) in MODES {
        let ours = resolve(&auth, mode);
        let theirs = resolve(&theme, mode);
        assert!(!ours.is_empty(), "no auth.css tokens apply in {label}");
        for (name, value) in &ours {
            let Some(expected) = theirs.get(name) else {
                continue; // Only in auth.css (checked above) or not set for :root in this mode.
            };
            if !value.eq_ignore_ascii_case(expected) {
                problems.push(format!(
                    "{name} in {label}: auth.css has `{value}`, the theme has `{expected}`"
                ));
            }
        }
    }

    assert!(
        problems.is_empty(),
        "auth.css has drifted from videocall-theme/styles/tokens.css:\n  {}",
        problems.join("\n  ")
    );
}

#[test]
fn the_parser_sees_modes() {
    let css = "@layer t { :root { --a: 1; --b: x } \
               @media (prefers-color-scheme: dark) { :root:not(.theme-light) { --a: 2; } } \
               @media (prefers-contrast: more) { :root, .theme-light { --a: 3; } } \
               @supports (height: 100dvh) { :root { --b: y; } } \
               .theme-dark { --a: 9; } }";
    let declarations = custom_properties(css);
    let get = |dark, contrast, name: &str| {
        resolve(&declarations, Mode { dark, contrast })
            .get(name)
            .cloned()
    };
    assert_eq!(get(false, false, "--a").as_deref(), Some("1"));
    assert_eq!(get(true, false, "--a").as_deref(), Some("2"));
    assert_eq!(get(false, true, "--a").as_deref(), Some("3"));
    // Contrast comes later in source order, so it wins in dark + contrast.
    assert_eq!(get(true, true, "--a").as_deref(), Some("3"));
    assert_eq!(get(false, false, "--b").as_deref(), Some("x"));
}
