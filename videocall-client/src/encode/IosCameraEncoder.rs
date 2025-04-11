// videocall-client/src/encode/ios_camera_encoder.rs
use crate::client::VideoCallClient;
use crate::constants::{VIDEO_HEIGHT, VIDEO_WIDTH}; // Use configured dimensions
use crate::crypto::aes::Aes128State;
use crate::encode::encoder_state::EncoderState;
// Import the modified transform function (or keep original name if compatible)
use crate::encode::transform::transform_video_bytes; // Assuming modification
use crate::utils::is_ios; // Use the utility
use gloo_console::log;
use gloo_utils::window;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    HtmlCanvasElement, HtmlVideoElement, ImageBitmap, MediaStream, MediaStreamConstraints,
    MessageEvent, Worker, WorkerOptions, CanvasRenderingContext2d,
};
use videocall_types::protos::packet_wrapper::PacketWrapper;


// Structure matching the output from the Worker/WASM encoder
#[derive(serde::Deserialize, Debug)] // Use serde if message passing involves JSON
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
            window().unwrap().cancel_animation_frame(handle).unwrap();
            log!("iOS Encoder: Cancelled animation frame");
        }
    }

    // Helper to terminate worker
    fn terminate_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.terminate();
            log!("iOS Encoder: Terminated worker");
        }
        // Also drop the closure reference if held separately
         self.on_message_closure = None;
    }

     // Helper to stop local media stream tracks
     fn stop_local_stream(&mut self) {
         if let Some(stream) = self.local_stream.take() {
             stream.get_tracks().for_each(|track| {
                 track.unchecked_into::<web_sys::MediaStreamTrack>().stop();
             });
             log!("iOS Encoder: Stopped local stream tracks");
         }
     }

    // Cleanup method
     fn cleanup(&mut self) {
        log!("iOS Encoder: Cleaning up...");
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
            aes: client.aes(), // Clone from client
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
        if !value { // If disabling, ensure cleanup happens
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
             log!("iOS Encoder: Device selection changed, will restart stream.");
        }
        changed
    }

    pub fn stop(&mut self) {
        self.state.stop(); // Sets destroy flag
        self.inner.borrow_mut().cleanup(); // Perform cleanup immediately
    }

    pub fn start(&mut self) {
        let device_id = match &self.state.selected {
            Some(id) if self.state.enabled.load(Ordering::Acquire) => id.clone(),
            _ => {
                log!("iOS Encoder: Not starting - disabled or no device selected.");
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
            // --- 1. Get Media Stream ---
            let stream = match Self::get_media_stream(&device_id).await {
                Ok(s) => s,
                Err(e) => {
                    error!("iOS Encoder: Failed to get media stream: {:?}", e);
                    // Optionally: Trigger a UI callback indicating the error
                    return;
                }
            };

             // Store stream and video element ref in inner state
             let video_element = match Self::setup_local_video(&video_elem_id, &stream) {
                 Ok(el) => el,
                 Err(e) => {
                     error!("iOS Encoder: Failed to setup local video element: {:?}", e);
                     stream.get_tracks().for_each(|track| track.unchecked_into::<web_sys::MediaStreamTrack>().stop());
                     return;
                 }
             };
             inner_rc.borrow_mut().local_stream = Some(stream);
             inner_rc.borrow_mut().video_element_ref = Some(video_element.clone()); // Keep ref needed for drawing

            // --- 2. Setup Offscreen Canvas ---
             // Use target dimensions, e.g., 640x480 for initial iOS optimisation
             let capture_width = 640; // TODO: Make configurable or dynamic
             let capture_height = 480;
             let canvas = window().unwrap().document().unwrap().create_element("canvas").unwrap().unchecked_into::<HtmlCanvasElement>();
             canvas.set_width(capture_width);
             canvas.set_height(capture_height);
             let ctx = canvas.get_context("2d").unwrap().unwrap().unchecked_into::<CanvasRenderingContext2d>();
             // Improve performance: disable alpha
              ctx.set_alpha(false);
              // Potentially other context attributes: 'desynchronized', 'powerPreference' if needed/supported

             inner_rc.borrow_mut().offscreen_canvas = Some(canvas);
             inner_rc.borrow_mut().canvas_ctx = Some(ctx.clone()); // Clone context for raf loop

            // --- 3. Setup Web Worker & WASM ---
            // Assume worker JS is at '/workers/ios_camera_worker.js'
            let worker = match Worker::new_with_options("/workers/ios_camera_worker.js", WorkerOptions::new().type_(web_sys::WorkerType::Module)) {
                 Ok(w) => w,
                 Err(e) => {
                     error!("iOS Encoder: Failed to create worker: {:?}", e);
                     inner_rc.borrow_mut().cleanup(); // Cleanup media stream etc.
                     return;
                 }
            };

            // --- 3a. Initialize WASM in Worker ---
            // This requires obtaining your compiled WASM bytes (e.g., via fetch)
            // and potentially the JS bindings module from wasm-pack/bindgen.
            // How you do this depends on your build setup. Example:
            // let wasm_module_url = "/path/to/your_wasm_bg.wasm";
            // let wasm_js_bindings_url = "/path/to/your_wasm.js";
            // TODO: Fetch WASM bytes and JS bindings, then post to worker
            // worker.postMessage({ type: 'init', payload: { wasmBytes: ..., wasm_bindgen_module: ... } });
            // For now, we assume initialization happens and worker posts back 'init_complete' or 'init_failed'

             inner_rc.borrow_mut().worker = Some(worker.clone()); // Store worker reference


             // --- 4. Setup Worker Message Listener ---
            let inner_handler_rc = inner_rc.clone();
            let client_handler_clone = client_clone.clone();
            let on_message_closure = Closure::wrap(Box::new(move |event: MessageEvent| {
                let mut inner = inner_handler_rc.borrow_mut();
                 // Use serde to deserialize if payload is structured JSON from worker
                 match serde_wasm_bindgen::from_value::<HashMap<String, JsValue>>(event.data()) {
                    Ok(message_map) => {
                        if let Some(msg_type) = message_map.get("type").and_then(|v| v.as_string()) {
                            match msg_type.as_str() {
                                "encoded_chunk" => {
                                     if let Some(payload_js) = message_map.get("payload") {
                                         // Assuming payload matches EncodedChunkPayload structure after WASM returns it
                                         match serde_wasm_bindgen::from_value::<EncodedChunkPayload>(payload_js.clone()) {
                                             Ok(chunk_payload) => {
                                                 // Call the modified transform function
                                                 let packet = transform_video_bytes(
                                                     &chunk_payload.data,
                                                     chunk_payload.timestamp,
                                                     chunk_payload.duration,
                                                     chunk_payload.is_keyframe,
                                                     inner.sequence_number,
                                                     &mut inner.transform_buffer, // Use pre-allocated buffer
                                                     &inner.userid,
                                                     inner.aes.clone(),
                                                 );
                                                 // Send the packet via the client
                                                  client_handler_clone.send_packet(packet);
                                                 inner.sequence_number += 1;
                                             },
                                             Err(e) => error!("iOS Encoder: Failed to deserialize encoded_chunk payload: {:?}", e),
                                         }
                                     }
                                 },
                                 "init_complete" => {
                                     log!("iOS Encoder: Worker initialized successfully.");
                                     // Now safe to start sending frames
                                 },
                                 "init_failed" => {
                                     error!("iOS Encoder: Worker WASM initialization failed: {:?}", message_map.get("detail"));
                                     inner.cleanup(); // Stop everything if worker init fails
                                     // TODO: Notify UI
                                 },
                                 "error" => {
                                      error!("iOS Encoder: Error received from worker: {:?}", message_map.get("detail"));
                                      // Decide if error is fatal, potentially call inner.cleanup()
                                 }
                                _ => log!("iOS Encoder: Received unhandled message type from worker: {}", msg_type),
                            }
                        }
                    },
                    Err(e) => error!("iOS Encoder: Failed to deserialize message from worker: {:?} - Data: {:?}", e, event.data()),
                 }


            }) as Box<dyn FnMut(MessageEvent)>);

            worker.set_onmessage(Some(on_message_closure.as_ref().unchecked_ref()));
             inner_rc.borrow_mut().on_message_closure = Some(on_message_closure); // Store closure

            // --- 5. Start Frame Grabbing Loop (requestAnimationFrame) ---
             // Use Rc<RefCell<>> for the state needed inside the RAF closure
            let raf_inner_rc = inner_rc.clone();
            let raf_state_rc = Rc::new(RefCell::new(())); // Dummy state for closure
            let f = Rc::new(RefCell::new(None));
            let g = f.clone();

            *g.borrow_mut() = Some(Closure::wrap(Box::new(move || {
                 let inner = raf_inner_rc.borrow(); // Borrow inner state

                 // Check stop flags first
                 if inner.raf_handle.is_none() { // Check if loop should stop (handle already taken in cleanup)
                     return;
                 }

                 if let (Some(worker), Some(video_element), Some(ctx)) =
                     (&inner.worker, &inner.video_element_ref, &inner.canvas_ctx)
                 {
                     // Draw current video frame to offscreen canvas
                     // Use dimensions defined during setup
                     if let Err(e) = ctx.draw_image_with_html_video_element(video_element, 0.0, 0.0) {
                         error!("iOS Encoder: draw_image failed: {:?}", e);
                         // Consider stopping if drawing fails repeatedly
                     } else {
                         // Create ImageBitmap from the canvas
                         match window().unwrap().create_image_bitmap(ctx.canvas().unwrap().unchecked_ref::<web_sys::ImageBitmapSource>()) {
                             Ok(promise) => {
                                 let worker_clone = worker.clone();
                                 wasm_bindgen_futures::spawn_local(async move {
                                     match JsFuture::from(promise).await {
                                         Ok(bitmap_js) => {
                                             let image_bitmap = bitmap_js.unchecked_into::<ImageBitmap>();
                                             // Transfer bitmap to worker for encoding
                                             if let Err(e) = worker_clone.post_message_with_transfer(
                                                 &serde_wasm_bindgen::to_value(&js_sys::Object::from_entries(&[
                                                         ("type".into(), "encode_frame".into()),
                                                         ("payload".into(), image_bitmap.clone().into()) // Pass bitmap as payload
                                                     ].iter().collect::<js_sys::Map>()).unwrap_or(JsValue::NULL), // Simplified message creation
                                                 &js_sys::Array::of1(&image_bitmap).into() // Transfer list
                                             ) {
                                                 error!("iOS Encoder: postMessage (transfer) to worker failed: {:?}", e);
                                                 image_bitmap.close(); // Close bitmap if transfer fails
                                             }
                                         }
                                         Err(e) => error!("iOS Encoder: createImageBitmap promise failed: {:?}", e),
                                     }
                                 });
                             }
                             Err(e) => error!("iOS Encoder: createImageBitmap failed: {:?}", e),
                         }
                     }
                 }

                 // Schedule the next frame
                 let mut inner_mut = raf_inner_rc.borrow_mut(); // Need mutable borrow to store handle
                 if inner_mut.raf_handle.is_some() { // Check if still running before scheduling next
                      inner_mut.raf_handle = Some(window().unwrap().request_animation_frame(f.borrow().as_ref().unwrap().as_ref().unchecked_ref()).unwrap());
                 }

             }) as Box<dyn FnMut()>));

             // Start the loop
             let handle = window().unwrap().request_animation_frame(g.borrow().as_ref().unwrap().as_ref().unchecked_ref()).unwrap();
             inner_rc.borrow_mut().raf_handle = Some(handle);
             log!("iOS Encoder: Started frame grabbing loop.");

        }); // end of spawn_local for setup/loop
    }

    // --- Helper: Get MediaStream ---
     async fn get_media_stream(device_id: &str) -> Result<MediaStream, JsValue> {
         let window = window().ok_or_else(|| JsValue::from_str("No window"))?;
         let navigator = window.navigator();
         let media_devices = navigator
             .media_devices()?;

         let mut constraints = MediaStreamConstraints::new();
         let mut media_info = web_sys::MediaTrackConstraints::new();
         media_info.device_id(&device_id.into());
         // Request specific resolution for capture if desired
         media_info.width(JsValue::from(js_sys::Number::from(640))); // Example: Request 640 width
         media_info.height(JsValue::from(js_sys::Number::from(480))); // Example: Request 480 height

         constraints.video(&media_info.into());
         constraints.audio(&JsValue::FALSE); // No audio track needed here

         let promise = media_devices.get_user_media_with_constraints(&constraints)?;
         JsFuture::from(promise)
             .await?
             .dyn_into::<MediaStream>()
     }

    // --- Helper: Setup Local Video Element ---
    fn setup_local_video(video_elem_id: &str, stream: &MediaStream) -> Result<HtmlVideoElement, JsValue> {
        let window = window().ok_or_else(|| JsValue::from_str("No window"))?;
        let document = window.document().ok_or_else(|| JsValue::from_str("No document"))?;
        let video_element = document
            .get_element_by_id(video_elem_id)
            .ok_or_else(|| JsValue::from_str(&format!("No element found with ID: {}", video_elem_id)))?
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


// --- Modified transform function to accept bytes + metadata ---

use videocall_types::protos::media_packet::{VideoMetadata,MediaType,MediaPacket};
use videocall_types::protos::packet_wrapper::{PacketWrapper, packet_wrapper::PacketType};
use crate::crypto::aes::Aes128State;
use std::rc::Rc;
use protobuf::Message;

pub fn transform_video_bytes(
    encoded_data: &[u8],
    timestamp: f64,
    duration: Option<f64>,
    is_keyframe: bool,
    sequence: u64,
    buffer: &mut [u8], // Re-purpose buffer for MediaPacket serialization if needed, or remove if unused
    email: &str,
    aes: Rc<Aes128State>,
) -> PacketWrapper {
    let frame_type = if is_keyframe { "key" } else { "delta" }.to_string();

    let mut media_packet = MediaPacket {
        data: encoded_data.to_vec(), // Copy encoded data directly
        frame_type,
        email: email.to_owned(),
        media_type: MediaType::VIDEO.into(),
        timestamp,
        duration: duration.unwrap_or(0.0), // Use 0.0 if duration is None
        video_metadata: Some(VideoMetadata {
            sequence,
            ..Default::default()
        }).into(),
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