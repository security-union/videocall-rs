// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context providers for the application
//!
//! This module centralises shared state that needs to be accessed across
//! the component tree through Yew's `ContextProvider`.

use videocall_client::VideoCallClient;
use yew::prelude::*;

/// Type alias used throughout the app when accessing the username context.
///
/// `UseStateHandle<Option<String>>` allows both read-only access (via
/// deref) and mutation by calling `.set(Some("new_name".into()))`.
pub type UsernameCtx = UseStateHandle<Option<String>>;

/// VideoCallClient context for sharing the client instance across components.
///
/// This eliminates props drilling and provides clean access to the client
/// from any component in the tree.
pub type VideoCallClientCtx = VideoCallClient;

// -----------------------------------------------------------------------------
// Meeting Time Context
// -----------------------------------------------------------------------------

/// Holds meeting timing information shared via Yew context.
///
/// # Lifecycle
/// - Created with `Default::default()` (both fields `None`)
/// - `call_start_time` is set when WebSocket/WebTransport connection succeeds
/// - `meeting_start_time` is set when `MEETING_STARTED` packet is received from server
///
/// # Usage
/// Components access this via `use_context::<MeetingTimeCtx>()`. If context is
/// missing, `unwrap_or_default()` returns empty values and timers show "--:--".
#[derive(Clone, PartialEq, Default)]
pub struct MeetingTime {
    /// Unix timestamp (ms) when the current user joined the call.
    /// Set on successful connection. `None` before connection.
    pub call_start_time: Option<f64>,

    /// Unix timestamp (ms) when the meeting started (from server).
    /// Set when `MEETING_STARTED` packet is received. `None` if not yet received.
    pub meeting_start_time: Option<f64>,
}

/// Context type for meeting time - read-only access to timing info.
pub type MeetingTimeCtx = MeetingTime;

// -----------------------------------------------------------------------------
// Meeting Host Context
// -----------------------------------------------------------------------------

/// Holds meeting host information shared via Yew context.
///
/// Used to identify the meeting owner/host and display appropriate UI indicators.
#[derive(Clone, PartialEq, Default)]
pub struct MeetingHost {
    /// Email/ID of the meeting host. `None` if not yet known.
    pub host_email: Option<String>,
}

impl MeetingHost {
    /// Check if the given email is the meeting host
    #[allow(dead_code)]
    pub fn is_host(&self, email: &str) -> bool {
        self.host_email.as_deref() == Some(email)
    }
}

/// Context type for meeting host - read-only access to host info.
#[allow(dead_code)]
pub type MeetingHostCtx = MeetingHost;

// -----------------------------------------------------------------------------
// Local-storage helpers
// -----------------------------------------------------------------------------

const STORAGE_KEY: &str = "vc_username";

/// Read the username from `window.localStorage` (if present).
pub fn load_username_from_storage() -> Option<String> {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(STORAGE_KEY).ok().flatten())
}

/// Persist the username to `localStorage` so that it survives page reloads.
pub fn save_username_to_storage(username: &str) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(STORAGE_KEY, username);
    }
}

/// Remove the username from `localStorage` entirely (e.g. on logout).
pub fn clear_username_from_storage() {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item(STORAGE_KEY);
    }
}

// -----------------------------------------------------------------------------
// Validation helpers
// -----------------------------------------------------------------------------

/// Returns `true` iff the supplied string is non-empty and contains only
/// ASCII alphanumerics and underscores (used for meeting ID validation).
pub fn is_valid_username(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// -----------------------------------------------------------------------------
// Display name validation
// -----------------------------------------------------------------------------

pub const DISPLAY_NAME_MAX_LEN: usize = 50;

/// Trim and collapse multiple spaces into one
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
    ch.is_ascii_alphanumeric()
        || ch == ' '
        || ch == '_'
        || ch == '-'
        || ch == '\''
}

/// Convert an email address (or its local-part) into a title-cased display name.
///
/// Splits on `.`, `_`, and `-`, title-cases each word, and joins with spaces.
/// For example `"john.doe"` becomes `"John Doe"`.
pub fn email_to_display_name(email_or_local: &str) -> String {
    let local = email_or_local.split('@').next().unwrap_or(email_or_local);

    let words: Vec<String> = local
        .split(|c: char| c == '.' || c == '_' || c == '-')
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
