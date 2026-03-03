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

/// Shared map of peer media states, provided as a Dioxus context.
///
/// A single async task subscribes to the diagnostics broadcast channel and
/// updates this map.  Individual `PeerTile` components read from it instead
/// of each spawning their own subscriber.
pub type PeerStatusMap = Signal<std::collections::HashMap<String, PeerMediaState>>;

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

use once_cell::sync::Lazy;

static USERNAME_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"^[A-Za-z0-9_]+$").unwrap());

pub fn is_valid_username(name: &str) -> bool {
    !name.is_empty() && USERNAME_RE.is_match(name)
}
