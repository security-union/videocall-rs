// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure validation and normalisation helpers for display names and meeting IDs.
//!
//! These functions contain **no web/wasm dependencies** and can be used from any
//! target (server, CLI, wasm).  Both the Yew and Dioxus UIs re-export them from
//! their respective `context` modules so that existing call-sites keep working.

/// Maximum allowed length (in Unicode scalar values) for a display name.
pub const DISPLAY_NAME_MAX_LEN: usize = 50;

/// Trim and collapse multiple spaces into one.
pub fn normalize_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;

    for ch in s.trim().chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }

    out
}

/// Allowed characters for display names.
/// Only ASCII alphanumerics are permitted (not full Unicode) to prevent
/// homoglyph / spoofing attacks.
pub fn is_allowed_display_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == ' ' || ch == '_' || ch == '-' || ch == '\''
}

/// Convert an email address (or its local-part) into a title-cased display name.
///
/// Splits on `.`, `_`, and `-`, title-cases each word, and joins with spaces.
/// For example `"john.doe"` becomes `"John Doe"`.
pub fn email_to_display_name(email_or_local: &str) -> String {
    let local = email_or_local.split('@').next().unwrap_or(email_or_local);

    let words: Vec<String> = local
        .split(['.', '_', '-'])
        .filter(|part| !part.trim().is_empty())
        .map(|part| {
            let mut chars = part.trim().chars();
            match chars.next() {
                None => String::new(),
                Some(first) => {
                    let mut word = String::new();
                    word.extend(first.to_uppercase());
                    word.push_str(&chars.as_str().to_lowercase());
                    word
                }
            }
        })
        .collect();

    normalize_spaces(&words.join(" "))
}

/// Validate and normalize a display name.
/// Returns normalized value on success, otherwise a clear error message.
///
/// NOTE: Server-side validation should mirror these rules. Client-side
/// validation is a UX convenience; the backend is the authoritative boundary.
pub fn validate_display_name(raw: &str) -> Result<String, String> {
    let value = normalize_spaces(raw);

    if value.is_empty() {
        return Err("Name cannot be empty.".to_string());
    }

    if value.chars().count() > DISPLAY_NAME_MAX_LEN {
        return Err(format!(
            "Name is too long (max {DISPLAY_NAME_MAX_LEN} characters).",
        ));
    }

    let mut invalid_chars: Vec<char> = value
        .chars()
        .filter(|ch| !is_allowed_display_name_char(*ch))
        .collect();
    invalid_chars.sort();
    invalid_chars.dedup();

    if !invalid_chars.is_empty() {
        return Err(format!(
            "Invalid character(s): {invalid_chars:?}. Allowed: ASCII letters, numbers, spaces, '_', '-', and apostrophe (').",
        ));
    }

    Ok(value)
}

/// Returns `true` iff the supplied string is non-empty and contains only
/// ASCII alphanumerics and underscores. Used for meeting ID validation.
pub fn is_valid_meeting_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_spaces() {
        assert_eq!(normalize_spaces("  a   b  "), "a b");
        assert_eq!(normalize_spaces("hello"), "hello");
        assert_eq!(normalize_spaces("   "), "");
    }

    #[test]
    fn test_is_allowed_display_name_char() {
        assert!(is_allowed_display_name_char('a'));
        assert!(is_allowed_display_name_char('Z'));
        assert!(is_allowed_display_name_char('0'));
        assert!(is_allowed_display_name_char(' '));
        assert!(is_allowed_display_name_char('_'));
        assert!(is_allowed_display_name_char('-'));
        assert!(is_allowed_display_name_char('\''));
        assert!(!is_allowed_display_name_char('@'));
        assert!(!is_allowed_display_name_char('.'));
        assert!(!is_allowed_display_name_char('!'));
    }

    #[test]
    fn test_email_to_display_name() {
        assert_eq!(email_to_display_name("john.doe"), "John Doe");
        assert_eq!(email_to_display_name("john.doe@example.com"), "John Doe");
        assert_eq!(email_to_display_name("jane_smith"), "Jane Smith");
        assert_eq!(email_to_display_name("bob-jones"), "Bob Jones");
        assert_eq!(email_to_display_name("alice"), "Alice");
    }

    #[test]
    fn test_validate_display_name_valid() {
        assert!(validate_display_name("alice").is_ok());
        assert!(validate_display_name("Bob 123").is_ok());
        assert!(validate_display_name("O'Brien").is_ok());
        assert!(validate_display_name("Mary-Jane").is_ok());
    }

    #[test]
    fn test_validate_display_name_invalid() {
        assert!(validate_display_name("").is_err());
        assert!(validate_display_name("   ").is_err());
        assert!(validate_display_name("user@name").is_err());
        let long = "a".repeat(DISPLAY_NAME_MAX_LEN + 1);
        assert!(validate_display_name(&long).is_err());
    }

    #[test]
    fn test_validate_display_name_normalizes() {
        assert_eq!(
            validate_display_name("  hello   world  ").unwrap(),
            "hello world"
        );
    }

    #[test]
    fn test_is_valid_meeting_id() {
        assert!(is_valid_meeting_id("abc123"));
        assert!(is_valid_meeting_id("meeting_1"));
        assert!(is_valid_meeting_id("A"));
        assert!(!is_valid_meeting_id(""));
        assert!(!is_valid_meeting_id("meeting-1"));
        assert!(!is_valid_meeting_id("meeting id"));
        assert!(!is_valid_meeting_id("user@name"));
    }
}
