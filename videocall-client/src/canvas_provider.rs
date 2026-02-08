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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Canvas ID provider trait for framework-agnostic canvas element resolution.
//!
//! This module provides a trait that allows different UI frameworks to specify
//! how canvas element IDs should be generated for peer video and screen share
//! rendering.

use std::rc::Rc;

/// Trait for providing canvas element IDs for peer video rendering.
///
/// Implementations of this trait are responsible for returning the DOM element
/// IDs of the canvas elements where peer video and screen share content should
/// be rendered.
///
/// # Example
///
/// ```ignore
/// use videocall_client::CanvasIdProvider;
/// use std::rc::Rc;
///
/// struct MyCanvasProvider;
///
/// impl CanvasIdProvider for MyCanvasProvider {
///     fn get_video_canvas_id(&self, peer_id: &str) -> String {
///         format!("video-{}", peer_id)
///     }
///
///     fn get_screen_canvas_id(&self, peer_id: &str) -> String {
///         format!("screen-{}", peer_id)
///     }
/// }
///
/// let provider: Rc<dyn CanvasIdProvider> = Rc::new(MyCanvasProvider);
/// ```
pub trait CanvasIdProvider: std::fmt::Debug + 'static {
    /// Get the canvas element ID for a peer's video feed.
    ///
    /// # Arguments
    /// * `peer_id` - The unique identifier of the peer
    ///
    /// # Returns
    /// The DOM element ID of the canvas where the peer's video should be rendered
    fn get_video_canvas_id(&self, peer_id: &str) -> String;

    /// Get the canvas element ID for a peer's screen share.
    ///
    /// # Arguments
    /// * `peer_id` - The unique identifier of the peer
    ///
    /// # Returns
    /// The DOM element ID of the canvas where the peer's screen share should be rendered
    fn get_screen_canvas_id(&self, peer_id: &str) -> String;
}

/// Default canvas ID provider that uses simple naming conventions.
///
/// - Video canvas: `"video-{peer_id}"`
/// - Screen canvas: `"screen-{peer_id}"`
#[derive(Clone, Debug, Default)]
pub struct DefaultCanvasIdProvider;

impl CanvasIdProvider for DefaultCanvasIdProvider {
    fn get_video_canvas_id(&self, peer_id: &str) -> String {
        format!("video-{}", peer_id)
    }

    fn get_screen_canvas_id(&self, peer_id: &str) -> String {
        format!("screen-{}", peer_id)
    }
}

/// Simple canvas ID provider that returns the peer ID directly as the video canvas ID.
///
/// This matches the current behavior in yew-ui where `get_peer_video_canvas_id`
/// returns the email/peer_id directly.
#[derive(Clone, Debug, Default)]
pub struct DirectCanvasIdProvider;

impl CanvasIdProvider for DirectCanvasIdProvider {
    fn get_video_canvas_id(&self, peer_id: &str) -> String {
        peer_id.to_string()
    }

    fn get_screen_canvas_id(&self, peer_id: &str) -> String {
        format!("screen-share-{}", peer_id)
    }
}

/// Create a boxed canvas ID provider from closures.
///
/// This is a convenience function for creating a custom canvas provider
/// without defining a new struct.
///
/// # Example
///
/// ```ignore
/// use videocall_client::create_canvas_provider;
///
/// let provider = create_canvas_provider(
///     |peer_id| format!("my-video-{}", peer_id),
///     |peer_id| format!("my-screen-{}", peer_id),
/// );
/// ```
pub fn create_canvas_provider<V, S>(
    video_fn: V,
    screen_fn: S,
) -> Rc<dyn CanvasIdProvider>
where
    V: Fn(&str) -> String + 'static,
    S: Fn(&str) -> String + 'static,
{
    Rc::new(FnCanvasIdProvider {
        video_fn: Box::new(video_fn),
        screen_fn: Box::new(screen_fn),
    })
}

/// Internal struct for closure-based canvas providers
struct FnCanvasIdProvider {
    video_fn: Box<dyn Fn(&str) -> String>,
    screen_fn: Box<dyn Fn(&str) -> String>,
}

impl std::fmt::Debug for FnCanvasIdProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FnCanvasIdProvider")
            .field("video_fn", &"<closure>")
            .field("screen_fn", &"<closure>")
            .finish()
    }
}

impl CanvasIdProvider for FnCanvasIdProvider {
    fn get_video_canvas_id(&self, peer_id: &str) -> String {
        (self.video_fn)(peer_id)
    }

    fn get_screen_canvas_id(&self, peer_id: &str) -> String {
        (self.screen_fn)(peer_id)
    }
}
