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

mod camera_encoder;
mod encoder_dimensions;
mod encoder_state;
mod keyframe_schedule;
mod microphone_encoder;
mod screen_encoder;
mod transform;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32};

use crate::VideoCallClient;
use videocall_types::Callback;
use wasm_bindgen::JsValue;
use web_sys::ReadableStreamDefaultReader;

pub use camera_encoder::CameraEncoder;
pub use microphone_encoder::MicrophoneEncoder;
pub use screen_encoder::{ScreenEncoder, ScreenShareEvent};

/// Whether `slot` still holds `reader` (the same JS object).
///
/// An encoder loop parks its reader in a shared slot so stop() can cancel
/// it. stop() and a device switch take the reader out before cancelling, so
/// a loop whose read ends while its reader is still in the slot was not
/// stopped by us: capture ended on its own.
fn holds_reader(
    slot: &RefCell<Option<ReadableStreamDefaultReader>>,
    reader: &ReadableStreamDefaultReader,
) -> bool {
    slot.borrow()
        .as_ref()
        .is_some_and(|held| JsValue::from(held) == JsValue::from(reader))
}

/// Trait to abstract over different microphone encoder implementations
pub trait MicrophoneEncoderTrait {
    fn start(&mut self);
    fn stop(&mut self);
    fn select(&mut self, device_id: String) -> bool;
    fn set_enabled(&mut self, enabled: bool) -> bool;
    fn set_error_callback(&mut self, on_error: Callback<String>);
}

// Implement trait for Safari microphone encoder
impl MicrophoneEncoderTrait for MicrophoneEncoder {
    fn start(&mut self) {
        self.start();
    }

    fn stop(&mut self) {
        self.stop();
    }

    fn select(&mut self, device_id: String) -> bool {
        self.select(device_id)
    }

    fn set_enabled(&mut self, enabled: bool) -> bool {
        self.set_enabled(enabled)
    }

    fn set_error_callback(&mut self, on_error: Callback<String>) {
        self.set_error_callback(on_error)
    }
}

/// Factory function to create the appropriate microphone encoder based on platform detection.
///
/// `shared_audio_tier_bitrate` and `shared_audio_tier_fec` are optional shared
/// atomics from the `CameraEncoder`. When provided, the microphone encoder
/// reads the audio quality tier from the camera encoder's quality manager
/// instead of creating its own `EncoderBitrateController`.
pub fn create_microphone_encoder(
    client: VideoCallClient,
    bitrate_kbps: u32,
    on_encoder_settings_update: Callback<String>,
    on_error: Callback<String>,
    vad_threshold: Option<f32>,
    shared_audio_tier_bitrate: Option<Rc<AtomicU32>>,
    shared_audio_tier_fec: Option<Rc<AtomicBool>>,
) -> Box<dyn MicrophoneEncoderTrait> {
    Box::new(MicrophoneEncoder::new(
        client,
        bitrate_kbps,
        on_encoder_settings_update,
        on_error,
        vad_threshold,
        shared_audio_tier_bitrate,
        shared_audio_tier_fec,
    ))
}
