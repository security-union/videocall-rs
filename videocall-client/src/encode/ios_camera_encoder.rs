// videocall-client/src/encode/ios_camera_encoder.rs
use crate::client::VideoCallClient;
use crate::crypto::aes::Aes128State;
use crate::encode::encoder_state::EncoderState;
// use crate::encode::transform::transform_video_chunk; // Use existing transform function - Removed as unused
use crate::utils::is_ios; // Use the actual utility
use gloo_utils::window;
use log::{error, info};
use protobuf::Message;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{MediaPacket, VideoMetadata};
use videocall_types::protos::packet_wrapper::{packet_wrapper::PacketType, PacketWrapper};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    CanvasRenderingContext2d,
    HtmlCanvasElement,
    HtmlVideoElement,
    MediaStream,
    MediaStreamConstraints,
    MessageEvent,
    Worker, // Removed WorkerOptions
};

// Structure matching the output from the Worker/WASM encoder
#[derive(Debug)]
struct EncodedChunkPayload {
    data: Vec<u8>, // Assuming WASM returns Vec<u8> directly or via serialization
    timestamp: f64,
    duration: Option<f64>,
    is_keyframe: bool,
}

// The main orchestrator struct for iOS
#[derive(Clone)] // Clone might be needed if used like CameraEncoder
pub struct IosCameraEncoder {
    client: VideoCallClient,
    video_elem_id: String,
    state: EncoderState, // Reuse state management
    // --- Internal state for iOS path ---
    // Held in Rc<RefCell<>> for interior mutability needed in callbacks/async blocks
    inner: Rc<RefCell<IosEncoderInner>>,
}

// Inner mutable state
struct IosEncoderInner {
    worker: Option<Worker>,
    on_message_closure: Option<Closure<dyn FnMut(MessageEvent)>>,
    raf_handle: Option<i32>, // requestAnimationFrame handle
    local_stream: Option<MediaStream>,
    offscreen_canvas: Option<HtmlCanvasElement>,
    canvas_ctx: Option<CanvasRenderingContext2d>,
    video_element_ref: Option<HtmlVideoElement>, // Ref to draw from
    sequence_number: u64,
    aes: Rc<Aes128State>, // Clone AES state for transform function
    userid: String,       // Clone user ID for transform function
    // Buffer for transform function (avoids reallocation)
    // Size might need adjustment based on max expected encoded frame size
    transform_buffer: Vec<u8>,
}

impl IosEncoderInner {
    // Helper to stop the animation frame loop
    fn cancel_animation_frame(&mut self) {
        if let Some(handle) = self.raf_handle.take() {
            let win = window();
            let _ = win.cancel_animation_frame(handle);
            info!("iOS Encoder: Cancelled animation frame");
        }
    }

    // Helper to terminate worker
    fn terminate_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.terminate();
            info!("iOS Encoder: Terminated worker");
        }
        // Also drop the closure reference if held separately
        self.on_message_closure = None;
    }

    // Helper to stop local media stream tracks
    fn stop_local_stream(&mut self) {
        if let Some(stream) = self.local_stream.take() {
            let tracks = stream.get_tracks();
            let tracks_len = tracks.length();
            for i in 0..tracks_len {
                let track = tracks.get(i);
                track.unchecked_into::<web_sys::MediaStreamTrack>().stop();
            }
            info!("iOS Encoder: Stopped local stream tracks");
        }
    }

    // Cleanup method
    fn cleanup(&mut self) {
        info!("iOS Encoder: Cleaning up...");
        self.cancel_animation_frame();
        self.terminate_worker();
        self.stop_local_stream();
        self.offscreen_canvas = None;
        self.canvas_ctx = None;
        self.video_element_ref = None;
    }
}

// Implement the public API mirroring CameraEncoder
impl IosCameraEncoder {
    pub fn new(client: VideoCallClient, video_elem_id: &str) -> Self {
        // Assert we are actually on iOS, otherwise this shouldn't be called
        debug_assert!(is_ios(), "IosCameraEncoder created on non-iOS platform!");

        let inner = Rc::new(RefCell::new(IosEncoderInner {
            worker: None,
            on_message_closure: None,
            raf_handle: None,
            local_stream: None,
            offscreen_canvas: None,
            canvas_ctx: None,
            video_element_ref: None,
            sequence_number: 0,
            aes: client.aes(),               // Clone from client
            userid: client.userid().clone(), // Clone from client
            // Allocate a reasonably large buffer once.
            // Typical encoded frames are much smaller, but keyframes can be larger.
            // Adjust size based on codec, resolution, and quality settings.
            transform_buffer: vec![0u8; 150_000], // Example size: 150KB
        }));

        Self {
            client,
            video_elem_id: video_elem_id.to_string(),
            state: EncoderState::new(), // Independent state
            inner,
        }
    }

    pub fn set_enabled(&mut self, value: bool) -> bool {
        let changed = self.state.set_enabled(value);
        if !value {
            // If disabling, ensure cleanup happens
            self.inner.borrow_mut().cleanup();
        }
        changed
    }

    pub fn select(&mut self, device_id: String) -> bool {
        let changed = self.state.select(device_id);
        if changed {
            // If selection changes while running, signal a restart is needed
            self.state.switching.store(true, Ordering::SeqCst);
            // Cleanup existing resources before restart (will happen in start again)
            self.inner.borrow_mut().cleanup();
            info!("iOS Encoder: Device selection changed, will restart stream.");
        }
        changed
    }

    pub fn stop(&mut self) {
        self.state.stop(); // Sets destroy flag
        self.inner.borrow_mut().cleanup(); // Perform cleanup immediately
    }

    pub fn start(&mut self) {
        // Keep device_id, video_elem_id, inner_rc, client_clone, even if unused for now
        // Their presence might be necessary when the commented-out code is re-enabled
        let device_id = match &self.state.selected {
            Some(id) if self.state.enabled.load(Ordering::Acquire) => id.clone(),
            _ => {
                info!("iOS Encoder: Not starting - disabled or no device selected.");
                return; // Don't start if not enabled or no device selected
            }
        };

        let video_elem_id = self.video_elem_id.clone();
        let inner_rc = self.inner.clone();
        let client_clone = self.client.clone(); // Clone for async block

        // Ensure previous resources are cleaned before starting anew
        // (Handles cases like device switching)
        self.inner.borrow_mut().cleanup();
        // Reset sequence number on start
        self.inner.borrow_mut().sequence_number = 0;

        // Spawn the main async task for setting up and running the loop
        wasm_bindgen_futures::spawn_local(async move {
            // The implementation of the encoder is commented out, to be completed later
            // Prevent unused variable warnings for now
            let _ = (device_id, video_elem_id, inner_rc, client_clone);
        });
    }

    // --- Helper: Get MediaStream ---
    async fn get_media_stream(device_id: &str) -> Result<MediaStream, JsValue> {
        let win = window(); // Directly use the returned Window

        let navigator = win.navigator();
        let media_devices = navigator
            .media_devices()
            .map_err(|_| JsValue::from_str("MediaDevices not available"))?;

        let constraints = MediaStreamConstraints::new();
        let media_info = web_sys::MediaTrackConstraints::new();
        media_info.set_device_id(&JsValue::from_str(device_id));
        // Request specific resolution for capture if desired
        media_info.set_width(&JsValue::from(640)); // Example: Request 640 width
        media_info.set_height(&JsValue::from(480)); // Example: Request 480 height

        constraints.set_video(&media_info.into());
        constraints.set_audio(&JsValue::FALSE); // No audio track needed here

        let promise = media_devices.get_user_media_with_constraints(&constraints)?;
        JsFuture::from(promise).await?.dyn_into::<MediaStream>()
    }

    // --- Helper: Setup Local Video Element ---
    fn setup_local_video(
        video_elem_id: &str,
        stream: &MediaStream,
    ) -> Result<HtmlVideoElement, JsValue> {
        let win = window(); // Directly use the returned Window

        let document = win
            .document()
            .ok_or_else(|| JsValue::from_str("Document not available"))?; // Document can be None

        let video_element = document
            .get_element_by_id(video_elem_id)
            .ok_or_else(|| {
                JsValue::from_str(&format!("No element found with ID: {}", video_elem_id))
            })?
            .dyn_into::<HtmlVideoElement>()?;

        video_element.set_src_object(Some(stream));
        video_element.set_muted(true);
        // ESSENTIAL for iOS inline playback
        let _ = video_element.set_attribute("playsinline", "true");
        // Autoplay might require user interaction policies to be met
        video_element.set_autoplay(true);

        Ok(video_element)
    }
}

// Implement Drop for automatic cleanup if IosCameraEncoder goes out of scope
impl Drop for IosCameraEncoder {
    fn drop(&mut self) {
        self.stop(); // Use existing stop logic which calls cleanup
    }
}

// --- Helper function to transform video bytes for iOS code path ---
pub fn transform_video_bytes(
    encoded_data: &[u8],
    timestamp: f64,
    duration: Option<f64>,
    is_keyframe: bool,
    sequence: u64,
    _buffer: &mut [u8], // Mark buffer as unused for now
    email: &str,
    aes: Rc<Aes128State>,
) -> PacketWrapper {
    let frame_type = if is_keyframe { "key" } else { "delta" }.to_string();

    let media_packet = MediaPacket {
        data: encoded_data.to_vec(), // Copy encoded data directly
        frame_type,
        email: email.to_owned(),
        media_type: MediaType::VIDEO.into(),
        timestamp,
        duration: duration.unwrap_or(0.0), // Use 0.0 if duration is None
        video_metadata: Some(VideoMetadata {
            sequence,
            ..Default::default()
        })
        .into(),
        ..Default::default()
    };

    // Serialize MediaPacket
    // Use the provided buffer if helpful, otherwise allocate dynamically.
    // write_to_bytes might be more convenient here.
    let serialized_media = match media_packet.write_to_bytes() {
        Ok(bytes) => bytes,
        Err(e) => {
            error!("Failed to serialize MediaPacket: {}", e);
            // Return a default/empty packet on error
            return PacketWrapper::default();
        }
    };

    // Encrypt the serialized MediaPacket
    let encrypted_data = match aes.encrypt(&serialized_media) {
        Ok(data) => data,
        Err(e) => {
            error!("Failed to encrypt MediaPacket: {}", e);
            // Return a default/empty packet on error
            return PacketWrapper::default();
        }
    };

    // Create the final PacketWrapper
    PacketWrapper {
        data: encrypted_data,
        email: media_packet.email, // Use email from MediaPacket
        packet_type: PacketType::MEDIA.into(),
        ..Default::default()
    }
}
