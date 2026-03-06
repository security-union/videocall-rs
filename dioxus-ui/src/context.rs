// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context providers for the application
//!
//! This module centralises shared state that needs to be accessed across
//! the component tree through Dioxus context providers.

use dioxus::prelude::*;
use videocall_client::VideoCallClient;

/// Wrapper for the username signal used as context.
#[derive(Clone, Copy)]
pub struct UsernameCtx(pub Signal<Option<String>>);

/// VideoCallClient context for sharing the client instance across components.
pub type VideoCallClientCtx = VideoCallClient;

/// Holds meeting timing information shared via context.
#[derive(Clone, PartialEq, Default)]
pub struct MeetingTime {
    pub call_start_time: Option<f64>,
    pub meeting_start_time: Option<f64>,
}

pub type MeetingTimeCtx = Signal<MeetingTime>;

/// Per-peer media state tracked by the shared diagnostics subscriber.
#[derive(Clone, Default, PartialEq)]
pub struct PeerMediaState {
    pub audio_enabled: bool,
    pub video_enabled: bool,
    pub screen_enabled: bool,
}

/// Shared map of per-peer media state signals, provided as a Dioxus context.
///
/// A single async task subscribes to the diagnostics broadcast channel and
/// updates per-peer signals.  Each `PeerTile` reads only its own
/// `Signal<PeerMediaState>`, so a state change for peer A does not cause
/// peer B's tile to re-render.
pub type PeerStatusMap = Signal<std::collections::HashMap<String, Signal<PeerMediaState>>>;

/// Holds meeting host information shared via context.
#[derive(Clone, PartialEq, Default)]
#[allow(dead_code)]
pub struct MeetingHost {
    pub host_email: Option<String>,
}

impl MeetingHost {
    #[allow(dead_code)]
    pub fn is_host(&self, email: &str) -> bool {
        self.host_email.as_deref() == Some(email)
    }
}

#[allow(dead_code)]
pub type MeetingHostCtx = Signal<MeetingHost>;

// ---------------------------------------------------------------------------
// Local-storage helpers
// ---------------------------------------------------------------------------

const STORAGE_KEY: &str = "vc_username";

pub fn load_username_from_storage() -> Option<String> {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(STORAGE_KEY).ok().flatten())
}

pub fn save_username_to_storage(username: &str) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(STORAGE_KEY, username);
    }
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

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
