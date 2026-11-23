// SPDX-License-Identifier: MIT OR Apache-2.0

//! A small reader for `tokens.css`, so the gallery lists the tokens straight
//! from the file and cannot drift from it.
//!
//! It is not a CSS parser. It reads the file line by line and relies on how
//! tokens.css is written: one declaration per line, a comment above each
//! group, and the size/leading notes as trailing comments. The tests below
//! pin those assumptions, so a reshuffle of the file fails here first.

use std::collections::HashSet;

/// One custom property, as first declared (the light value for colors).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The property name, with the leading `--`.
    pub name: &'static str,
    /// The declared value, as written.
    pub value: &'static str,
    /// A trailing comment on the same line, such as `34/41` or `4px`.
    pub note: Option<&'static str>,
}

/// Tokens under one comment heading, in file order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenGroup {
    /// The first sentence of the comment above the group.
    pub title: String,
    pub tokens: Vec<Token>,
}

/// Groups every token in `css` under the comment above it.
///
/// Each name is kept once, at its first declaration, so the dark and
/// Increase Contrast blocks do not repeat it.
pub fn groups(css: &'static str) -> Vec<TokenGroup> {
    let mut groups: Vec<TokenGroup> = Vec::new();
    let mut seen: HashSet<&'static str> = HashSet::new();
    let mut heading = String::new();
    let mut comment = String::new();
    let mut in_comment = false;

    for line in css.lines() {
        let trimmed = line.trim();

        if in_comment {
            if let Some(end) = trimmed.find("*/") {
                comment.push(' ');
                comment.push_str(&trimmed[..end]);
                in_comment = false;
                heading = heading_of(&comment);
            } else {
                comment.push(' ');
                comment.push_str(trimmed);
            }
            continue;
        }

        if let Some(body) = trimmed.strip_prefix("/*") {
            comment.clear();
            if let Some(end) = body.find("*/") {
                comment.push_str(&body[..end]);
                heading = heading_of(&comment);
            } else {
                comment.push_str(body);
                in_comment = true;
            }
            continue;
        }

        if let Some(token) = declaration(trimmed) {
            if !seen.insert(token.name) {
                continue;
            }
            match groups.last_mut() {
                Some(group) if group.title == heading => group.tokens.push(token),
                _ => groups.push(TokenGroup {
                    title: heading.clone(),
                    tokens: vec![token],
                }),
            }
        }
    }

    groups
}

/// Every token in `css`, flattened, in file order.
pub fn all(css: &'static str) -> Vec<Token> {
    groups(css)
        .into_iter()
        .flat_map(|group| group.tokens)
        .collect()
}

/// True for tokens that hold a color: the `--color-*` set, the glass tints
/// and borders, and the focus ring color.
pub fn is_color(name: &str) -> bool {
    if name == "--focus-ring-color" || name.starts_with("--color-") {
        return true;
    }
    name.starts_with("--material-") && !name.ends_with("-blur") && !name.ends_with("-saturate")
}

/// Parses `--name: value; /* note */` on one line.
fn declaration(line: &'static str) -> Option<Token> {
    if !line.starts_with("--") {
        return None;
    }
    let colon = line.find(':')?;
    let semicolon = line.find(';')?;
    if semicolon < colon {
        return None;
    }
    let name = line[..colon].trim();
    let value = line[colon + 1..semicolon].trim();
    let rest = line[semicolon + 1..].trim();
    let note = rest
        .strip_prefix("/*")
        .and_then(|body| body.split("*/").next())
        .map(str::trim)
        .filter(|note| !note.is_empty());
    Some(Token { name, value, note })
}

/// The first sentence of a comment, without section dashes.
fn heading_of(comment: &str) -> String {
    let text = comment.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.trim_matches(|c: char| c == '-' || c == '=' || c.is_whitespace());
    let end = [text.find(". "), text.find(':')]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(text.len());
    text[..end].trim_end_matches('.').trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TOKENS_CSS;

    fn group<'a>(groups: &'a [TokenGroup], title: &str) -> &'a TokenGroup {
        groups
            .iter()
            .find(|group| group.title == title)
            .unwrap_or_else(|| panic!("no group {title:?}"))
    }

    #[test]
    fn system_colors_come_first_with_their_light_values() {
        let groups = groups(TOKENS_CSS);
        let system = &groups[0];
        assert_eq!(system.title, "System colors");
        assert_eq!(system.tokens.len(), 12);
        assert_eq!(system.tokens[0].name, "--color-red");
        assert_eq!(system.tokens[0].value, "#FF383C");
    }

    #[test]
    fn groups_follow_the_file() {
        let groups = groups(TOKENS_CSS);
        for title in [
            "Grays",
            "Labels, fills, separators, link",
            "Backgrounds",
            "Glass",
            "Fixed colors",
            "Semantic colors",
            "Elevation",
            "Stacking",
            "Motion",
        ] {
            assert!(!group(&groups, title).tokens.is_empty(), "{title}");
        }
        assert_eq!(group(&groups, "Grays").tokens.len(), 6);
        assert_eq!(
            group(&groups, "Semantic colors").tokens[0].name,
            "--color-accent"
        );
    }

    #[test]
    fn names_are_unique_and_the_legacy_aliases_are_gone() {
        let tokens = all(TOKENS_CSS);
        let mut names = HashSet::new();
        for token in &tokens {
            assert!(names.insert(token.name), "duplicate {}", token.name);
        }
        for legacy in ["--bg", "--accent", "--glass-bg", "--radius-card"] {
            assert!(!names.contains(legacy), "{legacy} is back");
        }
        assert!(!TOKENS_CSS.contains("LEGACY"));
    }

    /// The custom properties declared directly in each rule, keyed by the
    /// chain of block headers around it, such as
    /// `@layer tokens { > .theme-dark {`. Relies on tokens.css putting every
    /// `{` and `}` at the end or start of its own line, which it does.
    fn blocks(css: &str) -> Vec<(String, Vec<(String, String)>)> {
        let mut out: Vec<(String, Vec<(String, String)>)> = Vec::new();
        let mut stack: Vec<String> = Vec::new();
        let mut pending_selector = String::new();
        let mut in_comment = false;
        for line in css.lines() {
            let line = line.trim();
            if in_comment {
                in_comment = !line.contains("*/");
                continue;
            }
            if line.starts_with("/*") {
                in_comment = !line.contains("*/");
                continue;
            }
            if let Some(selector) = line.strip_suffix(',') {
                // The first lines of a selector list (`:root,`).
                pending_selector.push_str(selector);
                pending_selector.push_str(", ");
            } else if let Some(header) = line.strip_suffix('{') {
                stack.push(format!("{pending_selector}{}", header.trim()));
                pending_selector.clear();
                out.push((stack.join(" > "), Vec::new()));
            } else if line == "}" {
                stack.pop();
            } else if let Some(token) = declaration_owned(line) {
                let path = stack.join(" > ");
                let block = out
                    .iter_mut()
                    .rev()
                    .find(|(key, _)| *key == path)
                    .expect("a declaration outside any block");
                block.1.push(token);
            }
        }
        out
    }

    fn declaration_owned(line: &str) -> Option<(String, String)> {
        let (name, rest) = line.split_once(':')?;
        if !name.starts_with("--") {
            return None;
        }
        let value = rest.split(';').next()?.trim();
        Some((name.trim().to_string(), value.to_string()))
    }

    fn block<'a>(
        blocks: &'a [(String, Vec<(String, String)>)],
        path: &str,
    ) -> &'a [(String, String)] {
        let found: Vec<_> = blocks.iter().filter(|(key, _)| key == path).collect();
        assert_eq!(found.len(), 1, "expected exactly one block {path:?}");
        &found[0].1
    }

    const LIGHT: &str = "@layer tokens > :root, .theme-light";
    const DARK_OS: &str =
        "@layer tokens > @media (prefers-color-scheme: dark) > :root:not(.theme-light)";
    const DARK_PIN: &str = "@layer tokens > .theme-dark";
    const CONTRAST_LIGHT: &str =
        "@layer tokens > @media (prefers-contrast: more) > :root, .theme-light";
    const CONTRAST_DARK_PIN: &str = "@layer tokens > @media (prefers-contrast: more) > .theme-dark";
    const CONTRAST_DARK_OS: &str = "@layer tokens > @media (prefers-contrast: more) and (prefers-color-scheme: dark) > :root:not(.theme-light)";

    fn names(block: &[(String, String)]) -> Vec<&str> {
        block.iter().map(|(name, _)| name.as_str()).collect()
    }

    // The dark values live in two places each: the OS query and the
    // .theme-dark pin. Nothing generates them, so this is what keeps the
    // copies equal. Same names, same order, same values.
    #[test]
    fn dark_blocks_match() {
        let blocks = blocks(TOKENS_CSS);
        assert_eq!(block(&blocks, DARK_OS), block(&blocks, DARK_PIN));
        assert_eq!(
            block(&blocks, CONTRAST_DARK_OS),
            block(&blocks, CONTRAST_DARK_PIN)
        );
    }

    // Every appearance token has a value in every appearance. The dark
    // Increase Contrast block lists everything because it has to undo the
    // light Increase Contrast overrides; light Increase Contrast lists only
    // what changes.
    #[test]
    fn every_appearance_declares_every_appearance_token() {
        let blocks = blocks(TOKENS_CSS);
        let light = names(block(&blocks, LIGHT));
        assert!(
            light.len() > 50,
            "light block parsed as {} tokens",
            light.len()
        );
        assert_eq!(names(block(&blocks, DARK_PIN)), light);
        assert_eq!(names(block(&blocks, CONTRAST_DARK_PIN)), light);
        for name in names(block(&blocks, CONTRAST_LIGHT)) {
            assert!(light.contains(&name), "{name} is only in light contrast");
        }
    }

    #[test]
    fn block_reader_keeps_media_and_selector_lists_apart() {
        let css = "@layer a {\n  :root,\n  .x {\n    --one: 1;\n  }\n  @media (m) {\n    .x {\n      --one: 2; /* note */\n    }\n  }\n}\n";
        let blocks = blocks(css);
        assert_eq!(
            block(&blocks, "@layer a > :root, .x"),
            &[("--one".to_string(), "1".to_string())]
        );
        assert_eq!(
            block(&blocks, "@layer a > @media (m) > .x"),
            &[("--one".to_string(), "2".to_string())]
        );
    }

    #[test]
    fn eleven_text_styles_and_eleven_emphasized_with_notes() {
        let tokens = all(TOKENS_CSS);
        let styles: Vec<_> = tokens
            .iter()
            .filter(|t| t.name.starts_with("--text-") && !t.name.ends_with("-emphasized"))
            .collect();
        let emphasized = tokens
            .iter()
            .filter(|t| t.name.starts_with("--text-") && t.name.ends_with("-emphasized"))
            .count();
        assert_eq!(styles.len(), 11);
        assert_eq!(emphasized, 11);
        assert_eq!(styles[0].name, "--text-large-title");
        assert_eq!(styles[0].note, Some("34/41"));
    }

    #[test]
    fn color_detection() {
        assert!(is_color("--color-red"));
        assert!(is_color("--material-regular-tint"));
        assert!(is_color("--focus-ring-color"));
        assert!(!is_color("--material-regular-blur"));
        assert!(!is_color("--material-clear-saturate"));
        assert!(!is_color("--space-4"));
    }

    #[test]
    fn declaration_parsing() {
        let token = declaration("--space-1: 0.25rem; /* 4px */").unwrap();
        assert_eq!(token.name, "--space-1");
        assert_eq!(token.value, "0.25rem");
        assert_eq!(token.note, Some("4px"));
        assert!(declaration("color-scheme: light dark;").is_none());
    }

    #[test]
    fn headings_take_the_first_sentence() {
        assert_eq!(
            heading_of(" Glass. Approximation, Apple publishes no value. "),
            "Glass"
        );
        assert_eq!(heading_of(" ---- Dark ---- "), "Dark");
        assert_eq!(
            heading_of(" Tinted fills: the semantic color at low alpha"),
            "Tinted fills"
        );
        assert_eq!(heading_of(" Semantic colors "), "Semantic colors");
    }
}
