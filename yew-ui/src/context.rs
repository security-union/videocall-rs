// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context providers for the application
//!
//! This module centralises shared state that needs to be accessed across
//! the component tree through Yew's `ContextProvider`.

use std::cell::RefCell;
use std::rc::Rc;
use videocall_client::{CameraEncoder, MicrophoneEncoderTrait, ScreenEncoder, VideoCallClient};
use web_sys::MediaStream;
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
// Media Encoder Context
// -----------------------------------------------------------------------------

/// Inner state for media encoders - wrapped in RefCell for interior mutability.
pub struct MediaEncoderInner {
    pub camera: Option<CameraEncoder>,
    pub microphone: Option<Box<dyn MicrophoneEncoderTrait>>,
    pub screen: Option<ScreenEncoder>,
    /// Current camera stream for UI attachment
    pub camera_stream: Option<MediaStream>,
    /// Subscribers for camera stream changes
    camera_stream_subscribers: Vec<(usize, Callback<Option<MediaStream>>)>,
    next_subscriber_id: usize,
}

impl MediaEncoderInner {
    pub fn new() -> Self {
        Self {
            camera: None,
            microphone: None,
            screen: None,
            camera_stream: None,
            camera_stream_subscribers: Vec::new(),
            next_subscriber_id: 0,
        }
    }
}

impl Default for MediaEncoderInner {
    fn default() -> Self {
        Self::new()
    }
}

/// Context for media encoders that survives component recreation.
///
/// This context is created in AttendantsComponent and provided to Host.
/// The encoders live here instead of in Host, so they survive Host recreation.
#[derive(Clone)]
pub struct MediaEncoderCtx {
    inner: Rc<RefCell<MediaEncoderInner>>,
}

impl PartialEq for MediaEncoderCtx {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }
}

impl MediaEncoderCtx {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(MediaEncoderInner::new())),
        }
    }

    /// Initialize the camera encoder
    pub fn set_camera(&self, camera: CameraEncoder) {
        self.inner.borrow_mut().camera = Some(camera);
    }

    /// Initialize the microphone encoder
    pub fn set_microphone(&self, microphone: Box<dyn MicrophoneEncoderTrait>) {
        self.inner.borrow_mut().microphone = Some(microphone);
    }

    /// Initialize the screen encoder
    pub fn set_screen(&self, screen: ScreenEncoder) {
        self.inner.borrow_mut().screen = Some(screen);
    }

    /// Access camera encoder mutably
    pub fn with_camera<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut CameraEncoder) -> R,
    {
        self.inner.borrow_mut().camera.as_mut().map(f)
    }

    /// Access microphone encoder mutably
    pub fn with_microphone<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut Box<dyn MicrophoneEncoderTrait>) -> R,
    {
        self.inner.borrow_mut().microphone.as_mut().map(f)
    }

    /// Access screen encoder mutably
    pub fn with_screen<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut ScreenEncoder) -> R,
    {
        self.inner.borrow_mut().screen.as_mut().map(f)
    }

    /// Subscribe to camera stream changes. Returns subscription ID for unsubscribing.
    pub fn subscribe_camera_stream(&self, callback: Callback<Option<MediaStream>>) -> usize {
        let mut inner = self.inner.borrow_mut();
        let id = inner.next_subscriber_id;
        inner.next_subscriber_id += 1;
        inner.camera_stream_subscribers.push((id, callback.clone()));

        // Immediately emit current state
        let current_stream = inner.camera_stream.clone();
        drop(inner); // Release borrow before callback
        callback.emit(current_stream);

        id
    }

    /// Unsubscribe from camera stream changes
    pub fn unsubscribe_camera_stream(&self, id: usize) {
        self.inner
            .borrow_mut()
            .camera_stream_subscribers
            .retain(|(sub_id, _)| *sub_id != id);
    }

    /// Set the camera stream and notify all subscribers
    pub fn set_camera_stream(&self, stream: Option<MediaStream>) {
        let subscribers: Vec<_> = {
            let mut inner = self.inner.borrow_mut();
            inner.camera_stream = stream.clone();
            inner.camera_stream_subscribers.clone()
        };

        // Notify subscribers outside of borrow
        for (_, callback) in subscribers {
            callback.emit(stream.clone());
        }
    }

    /// Get current camera stream
    pub fn get_camera_stream(&self) -> Option<MediaStream> {
        self.inner.borrow().camera_stream.clone()
    }

    /// Check if encoders are initialized
    pub fn is_initialized(&self) -> bool {
        let inner = self.inner.borrow();
        inner.camera.is_some() && inner.microphone.is_some() && inner.screen.is_some()
    }

    /// Stop all encoders
    pub fn stop_all(&self) {
        let mut inner = self.inner.borrow_mut();
        if let Some(ref mut camera) = inner.camera {
            camera.stop();
        }
        if let Some(ref mut microphone) = inner.microphone {
            microphone.stop();
        }
        if let Some(ref mut screen) = inner.screen {
            screen.stop();
        }
    }
}

impl Default for MediaEncoderCtx {
    fn default() -> Self {
        Self::new()
    }
}

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
// Local-storage helpers
// -----------------------------------------------------------------------------

const STORAGE_KEY: &str = "vc_username";
const SELF_VIDEO_POSITION_KEY: &str = "vc_self_video_floating";

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

/// Read the self-video position preference from `window.localStorage`.
/// Returns `true` if floating (corner position), `false` if grid position.
pub fn load_self_video_position_from_storage() -> bool {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(SELF_VIDEO_POSITION_KEY).ok().flatten())
        .map(|v| v == "true")
        .unwrap_or(false) // Default to grid position
}

/// Persist the self-video position preference to `localStorage`.
pub fn save_self_video_position_to_storage(is_floating: bool) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(
            SELF_VIDEO_POSITION_KEY,
            if is_floating { "true" } else { "false" },
        );
    }
}

// -----------------------------------------------------------------------------
// Validation helpers
// -----------------------------------------------------------------------------

use once_cell::sync::Lazy;

static USERNAME_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"^[A-Za-z0-9_]+$").unwrap());

/// Returns `true` iff the supplied username is non-empty and matches the
/// allowed pattern.
pub fn is_valid_username(name: &str) -> bool {
    !name.is_empty() && USERNAME_RE.is_match(name)
}
