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
            "Name is too long (max {} characters).",
            DISPLAY_NAME_MAX_LEN
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
            "Invalid character(s): {:?}. Allowed: ASCII letters, numbers, spaces, '_', '-', and apostrophe (').",
            invalid_chars
        ));
    }

    Ok(value)
}

/// Returns `true` if the string matches the standard UUID/GUID format
/// (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` — 8-4-4-4-12 hex digits).
pub fn is_guid_like(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    s.bytes().enumerate().all(|(i, b)| match i {
        8 | 13 | 18 | 23 => b == b'-',
        _ => b.is_ascii_hexdigit(),
    })
}

/// Maximum length of a meeting ID, in bytes.
pub const MEETING_ID_MAX_LEN: usize = 255;

/// Human-readable description of the characters [`is_allowed_meeting_id_char`] accepts.
pub const MEETING_ID_ALLOWED_CHARS: &str =
    "letters (a-z, A-Z), numbers (0-9), underscores (_), hyphens (-) and tildes (~)";

/// Returns `true` for the characters a meeting ID may contain: ASCII letters,
/// ASCII digits, `_`, `-` and `~`.
pub fn is_allowed_meeting_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '~')
}

/// Why [`validate_meeting_id`] rejected a meeting ID.
///
/// `Display` renders a predicate meant to follow a subject: `"Meeting ID {err}"`
/// reads "Meeting ID cannot be empty".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeetingIdError {
    /// The ID is empty.
    Empty,
    /// Every character is allowed, but there are more than [`MEETING_ID_MAX_LEN`] of them.
    TooLong,
    /// The disallowed characters, each listed once, in order of first appearance.
    InvalidChars(Vec<char>),
}

impl std::fmt::Display for MeetingIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MeetingIdError::Empty => write!(f, "cannot be empty"),
            MeetingIdError::TooLong => write!(f, "cannot exceed {MEETING_ID_MAX_LEN} characters"),
            MeetingIdError::InvalidChars(chars) => {
                let listed: Vec<String> = chars.iter().map(|c| format!("{c:?}")).collect();
                write!(
                    f,
                    "contains characters that are not allowed ({}); use only {MEETING_ID_ALLOWED_CHARS}",
                    listed.join(", ")
                )
            }
        }
    }
}

impl std::error::Error for MeetingIdError {}

/// Validates a meeting ID exactly as given; it is never trimmed or rewritten.
///
/// A valid ID is 1 to [`MEETING_ID_MAX_LEN`] bytes, every character of which
/// satisfies [`is_allowed_meeting_id_char`]. Disallowed characters are reported
/// ahead of the length.
pub fn validate_meeting_id(id: &str) -> Result<(), MeetingIdError> {
    if id.is_empty() {
        return Err(MeetingIdError::Empty);
    }
    let mut invalid: Vec<char> = Vec::new();
    // `take` bounds the dedup on over-long input; an ID within the limit has
    // at most MEETING_ID_MAX_LEN characters, so none are dropped.
    for c in id
        .chars()
        .filter(|c| !is_allowed_meeting_id_char(*c))
        .take(MEETING_ID_MAX_LEN)
    {
        if !invalid.contains(&c) {
            invalid.push(c);
        }
    }
    if !invalid.is_empty() {
        return Err(MeetingIdError::InvalidChars(invalid));
    }
    if id.len() > MEETING_ID_MAX_LEN {
        return Err(MeetingIdError::TooLong);
    }
    Ok(())
}

/// Returns `true` iff [`validate_meeting_id`] accepts `id`.
pub fn is_valid_meeting_id(id: &str) -> bool {
    validate_meeting_id(id).is_ok()
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
    fn test_is_guid_like() {
        assert!(is_guid_like("a1b2c3d4-e5f6-7890-abcd-ef1234567890"));
        assert!(is_guid_like("00000000-0000-0000-0000-000000000000"));
        assert!(is_guid_like("ABCDEF01-2345-6789-ABCD-EF0123456789"));
        assert!(!is_guid_like("not-a-guid"));
        assert!(!is_guid_like(""));
        assert!(!is_guid_like("a1b2c3d4e5f67890abcdef1234567890"));
        assert!(!is_guid_like("a1b2c3d4-e5f6-7890-abcd-ef123456789"));
        assert!(!is_guid_like("a1b2c3d4-e5f6-7890-abcd-ef12345678901"));
        assert!(!is_guid_like("g1b2c3d4-e5f6-7890-abcd-ef1234567890"));
        assert!(!is_guid_like("John Doe"));
        assert!(!is_guid_like("alice@example.com"));
    }

    #[test]
    fn test_is_valid_meeting_id() {
        for ok in [
            "abc123",
            "meeting_1",
            "A",
            "meeting-1",
            "my-meeting",
            "a~b",
            "~",
            "-",
            "_",
            "Mixed_Case-9~x",
        ] {
            assert!(is_valid_meeting_id(ok), "{ok:?} should be valid");
        }
        for bad in [
            "",
            ".",
            "..",
            "a.b",
            "meeting id",
            " a",
            "a ",
            "a\tb",
            "a\nb",
            "a\rb",
            "a%20b",
            "a/b",
            "a?b",
            "a#b",
            "a*b",
            "a>b",
            "a+b",
            "a&b",
            "a=b",
            "a;b",
            "a$b",
            "a'b",
            "a(b)",
            "a!b",
            "a,b",
            "a:b",
            "user@name",
            "caf\u{e9}",
            "\u{455}ecret",
            "a\u{202e}b",
            "\u{ff41}",
        ] {
            assert!(!is_valid_meeting_id(bad), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn test_is_allowed_meeting_id_char() {
        for c in ['a', 'z', 'A', 'Z', '0', '9', '_', '-', '~'] {
            assert!(is_allowed_meeting_id_char(c), "{c:?} should be allowed");
        }
        for c in ['.', ' ', '%', '/', '*', '>', '+', '@', '\u{e9}', '\u{663}'] {
            assert!(!is_allowed_meeting_id_char(c), "{c:?} should be rejected");
        }
    }

    #[test]
    fn test_validate_meeting_id_length_limit() {
        assert_eq!(validate_meeting_id(""), Err(MeetingIdError::Empty));
        assert_eq!(validate_meeting_id(&"a".repeat(255)), Ok(()));
        assert_eq!(
            validate_meeting_id(&"a".repeat(256)),
            Err(MeetingIdError::TooLong)
        );
    }

    #[test]
    fn test_validate_meeting_id_reports_invalid_chars_ahead_of_length() {
        assert_eq!(
            validate_meeting_id(&"\u{e9}".repeat(130)),
            Err(MeetingIdError::InvalidChars(vec!['\u{e9}']))
        );
        assert_eq!(
            validate_meeting_id(&format!("{}.", "a".repeat(300))),
            Err(MeetingIdError::InvalidChars(vec!['.']))
        );
    }

    #[test]
    fn test_validate_meeting_id_bounds_the_listed_chars_on_overlong_input() {
        let distinct: String = ('\u{4e00}'..).take(1000).collect();
        let Err(MeetingIdError::InvalidChars(chars)) = validate_meeting_id(&distinct) else {
            panic!("1000 distinct CJK characters must be InvalidChars");
        };
        assert_eq!(chars.len(), MEETING_ID_MAX_LEN);
        assert_eq!(chars[0], '\u{4e00}');
    }

    #[test]
    fn test_validate_meeting_id_lists_invalid_chars_once_in_order() {
        assert_eq!(
            validate_meeting_id("a.b c.d e"),
            Err(MeetingIdError::InvalidChars(vec!['.', ' ']))
        );
        assert_eq!(
            validate_meeting_id(".."),
            Err(MeetingIdError::InvalidChars(vec!['.']))
        );
    }

    #[test]
    fn test_meeting_id_error_display() {
        assert_eq!(MeetingIdError::Empty.to_string(), "cannot be empty");
        assert_eq!(
            MeetingIdError::TooLong.to_string(),
            "cannot exceed 255 characters"
        );
        assert_eq!(
            validate_meeting_id("a.b\nc").unwrap_err().to_string(),
            "contains characters that are not allowed ('.', '\\n'); \
             use only letters (a-z, A-Z), numbers (0-9), underscores (_), \
             hyphens (-) and tildes (~)"
        );
    }
}
