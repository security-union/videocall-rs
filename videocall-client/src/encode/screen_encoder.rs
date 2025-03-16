use gloo_utils::window;
use js_sys::Array;
use js_sys::JsString;
use js_sys::Reflect;
use log::error;
use log::debug;
use std::sync::atomic::Ordering;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::LatencyMode;
use web_sys::MediaStream;
use web_sys::MediaStreamTrack;
use web_sys::MediaStreamTrackProcessor;
use web_sys::MediaStreamTrackProcessorInit;
use web_sys::ReadableStreamDefaultReader;
use web_sys::VideoEncoder;
use web_sys::VideoEncoderConfig;
use web_sys::VideoEncoderEncodeOptions;
use web_sys::VideoEncoderInit;
use web_sys::VideoFrame;
use web_sys::VideoTrack;

use super::super::client::VideoCallClient;
use super::encoder_state::EncoderState;
use super::transform::transform_screen_chunk;

use crate::constants::SCREEN_HEIGHT;
use crate::constants::SCREEN_WIDTH;
use crate::constants::VIDEO_CODEC;

/// [ScreenEncoder] encodes the user's screen and sends it through a [`VideoCallClient`](crate::VideoCallClient) connection.
///
/// See also:
/// * [CameraEncoder](crate::CameraEncoder)
/// * [MicrophoneEncoder](crate::MicrophoneEncoder)
///
pub struct ScreenEncoder {
    client: VideoCallClient,
    state: EncoderState,
}

impl ScreenEncoder {
    /// Construct a screen encoder:
    ///
    /// * `client` - an instance of a [`VideoCallClient`](crate::VideoCallClient).  It does not need to be currently connected.
    ///
    /// The encoder is created in a disabled state, [`encoder.set_enabled(true)`](Self::set_enabled) must be called before it can start encoding.
    pub fn new(client: VideoCallClient) -> Self {
        Self {
            client,
            state: EncoderState::new(),
        }
    }

    // The next two methods delegate to self.state

    /// Enables/disables the encoder.   Returns true if the new value is different from the old value.
    ///
    /// The encoder starts disabled, [`encoder.set_enabled(true)`](Self::set_enabled) must be
    /// called prior to starting encoding.
    ///
    /// Disabling encoding after it has started will cause it to stop.
    pub fn set_enabled(&mut self, value: bool) -> bool {
        self.state.set_enabled(value)
    }

    /// Stops encoding after it has been started.
    pub fn stop(&mut self) {
        self.state.stop()
    }

    /// Start encoding and sending the data to the client connection (if it's currently connected).
    /// The user is prompted by the browser to select which window or screen to encode.
    ///
    /// This will not do anything if [`encoder.set_enabled(true)`](Self::set_enabled) has not been
    /// called.
    pub fn start(&mut self) {
        let EncoderState {
            enabled, destroy, ..
        } = self.state.clone();
        let client = self.client.clone();
        let userid = client.userid().clone();
        let aes = client.aes();
        let screen_output_handler = {
            let mut buffer: [u8; 150000] = [0; 150000];
            let mut sequence_number = 0;
            let client_clone = client.clone();
            
            Box::new(move |chunk: JsValue| {
                let chunk = web_sys::EncodedVideoChunk::from(chunk);
                
                // Log the start of packet processing
                debug!("Screen encoder processing chunk: timestamp={}, type={:?}, size={}B", 
                       chunk.timestamp(), 
                       chunk.type_(), 
                       chunk.byte_length());
                
                // Get DiagnosticsManager if available
                let mut diag_manager_option = None;
                if let Ok(diag) = client_clone.try_get_diagnostics_manager() {
                    debug!("Successfully acquired diagnostics manager for screen encoder");
                    diag_manager_option = Some(diag);
                } else {
                    debug!("Failed to acquire diagnostics manager for screen encoder");
                }
                
                let packet: PacketWrapper = transform_screen_chunk(
                    chunk,
                    sequence_number,
                    &mut buffer,
                    &userid,
                    aes.clone(),
                    diag_manager_option.as_mut(),
                );
                
                debug!("Screen encoder sending packet: sequence={}, email={}", sequence_number, userid);
                
                // Try to send the packet and log any issues
                client_clone.send_packet(packet);
                
                sequence_number += 1;
            })
        };
        wasm_bindgen_futures::spawn_local(async move {
            let navigator = window().navigator();
            let media_devices = navigator.media_devices().unwrap();
            let screen_to_share: MediaStream =
                JsFuture::from(media_devices.get_display_media().unwrap())
                    .await
                    .unwrap()
                    .unchecked_into::<MediaStream>();

            // TODO: How can we determine the actual width and height of the screen to set the encoder config?
            let screen_track = Box::new(
                screen_to_share
                    .get_video_tracks()
                    .find(&mut |_: JsValue, _: u32, _: Array| true)
                    .unchecked_into::<VideoTrack>(),
            );

            let screen_error_handler = Closure::wrap(Box::new(move |e: JsValue| {
                error!("error_handler error {:?}", e);
            }) as Box<dyn FnMut(JsValue)>);

            let screen_output_handler =
                Closure::wrap(screen_output_handler as Box<dyn FnMut(JsValue)>);

            let screen_encoder_init = VideoEncoderInit::new(
                screen_error_handler.as_ref().unchecked_ref(),
                screen_output_handler.as_ref().unchecked_ref(),
            );

            let screen_encoder = Box::new(VideoEncoder::new(&screen_encoder_init).unwrap());
            let mut screen_encoder_config =
                VideoEncoderConfig::new(VIDEO_CODEC, SCREEN_HEIGHT, SCREEN_WIDTH);
            screen_encoder_config.bitrate(64_000f64);
            screen_encoder_config.latency_mode(LatencyMode::Realtime);
            screen_encoder.configure(&screen_encoder_config);

            let screen_processor =
                MediaStreamTrackProcessor::new(&MediaStreamTrackProcessorInit::new(
                    &screen_track.unchecked_into::<MediaStreamTrack>(),
                ))
                .unwrap();

            let screen_reader = screen_processor
                .readable()
                .get_reader()
                .unchecked_into::<ReadableStreamDefaultReader>();

            let mut screen_frame_counter = 0;

            let poll_screen = async {
                loop {
                    if destroy.load(Ordering::Acquire) {
                        return;
                    }
                    if !enabled.load(Ordering::Acquire) {
                        return;
                    }
                    match JsFuture::from(screen_reader.read()).await {
                        Ok(js_frame) => {
                            let video_frame = Reflect::get(&js_frame, &JsString::from("value"))
                                .unwrap()
                                .unchecked_into::<VideoFrame>();
                            let mut opts = VideoEncoderEncodeOptions::new();
                            screen_frame_counter = (screen_frame_counter + 1) % 50;
                            opts.key_frame(screen_frame_counter == 0);
                            screen_encoder.encode_with_options(&video_frame, &opts);
                            video_frame.close();
                        }
                        Err(e) => {
                            error!("error {:?}", e);
                        }
                    }
                }
            };
            poll_screen.await;
        });
    }
}
