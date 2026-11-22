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
mod capture_session;
mod encoder_dimensions;
mod encoder_state;
mod keyframe_schedule;
mod microphone_encoder;
mod screen_encoder;
mod transform;

use crate::constants::get_video_codec_string;
use crate::VideoCallClient;
use encoder_dimensions::FrameSize;
use keyframe_schedule::KeyframeSchedule;
use videocall_types::Callback;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    LatencyMode, ReadableStreamDefaultReader, ReadableStreamReadResult, VideoEncoder,
    VideoEncoderConfig, VideoFrame,
};

pub use camera_encoder::CameraEncoder;
pub use microphone_encoder::MicrophoneEncoder;
pub use screen_encoder::{ScreenEncoder, ScreenShareEvent};

/// What one read of a capture's frame stream produced.
enum FrameRead {
    Frame(VideoFrame),
    /// The stream closed: the source ended or the reader was cancelled.
    Ended,
    Failed(JsValue),
}

async fn read_frame(reader: &ReadableStreamDefaultReader) -> FrameRead {
    let result = match JsFuture::from(reader.read()).await {
        Ok(result) => result,
        Err(error) => return FrameRead::Failed(error),
    };
    // read() results are plain dictionaries, so no instanceof check applies.
    let result = result.unchecked_into::<ReadableStreamReadResult>();
    if result.get_done().unwrap_or(true) {
        return FrameRead::Ended;
    }
    match result.get_value().dyn_into::<VideoFrame>() {
        Ok(frame) => FrameRead::Frame(frame),
        Err(value) => FrameRead::Failed(value),
    }
}

/// Configures `encoder` with a fresh config, so a bitrate change never
/// carries stale dimensions.
fn configure_encoder(
    encoder: &VideoEncoder,
    size: FrameSize,
    bitrate_bps: u32,
) -> Result<(), JsValue> {
    // web-sys takes height before width.
    let config = VideoEncoderConfig::new(get_video_codec_string(), size.height, size.width);
    config.set_bitrate_f64(bitrate_bps as f64);
    config.set_latency_mode(LatencyMode::Realtime);
    encoder.configure(&config)
}

/// Reconfigures `encoder` when a `frame_size` frame, fitted to `cap`, no
/// longer matches `current`. Returns whether it did.
fn reconfigure_if_resized(
    encoder: &VideoEncoder,
    current: &mut FrameSize,
    frame_size: FrameSize,
    cap: FrameSize,
    bitrate_bps: u32,
) -> bool {
    let target = frame_size.fit_within(cap);
    if target.is_empty() || target == *current {
        return false;
    }
    log::info!(
        "Encoder size {}x{} -> {}x{}, reconfiguring",
        current.width,
        current.height,
        target.width,
        target.height,
    );
    *current = target;
    if let Err(e) = configure_encoder(encoder, target, bitrate_bps) {
        log::error!("Error reconfiguring encoder: {e:?}");
    }
    true
}

/// Whether `frame` is a keyframe: periodic, a viewer's PLI, or the first
/// frame after a resize, which receivers need to decode the new size.
fn keyframe_due(
    schedule: &mut KeyframeSchedule,
    interval_frames: u32,
    pli: bool,
    resized: bool,
    frame: u32,
    label: &str,
) -> bool {
    if resized {
        log::info!("{label}: forcing keyframe at frame {frame} (reconfigured)");
    }
    if pli {
        log::info!("{label}: forcing keyframe at frame {frame} (PLI)");
    }
    schedule.next_is_keyframe(interval_frames, pli || resized)
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
pub fn create_microphone_encoder(
    client: VideoCallClient,
    bitrate_kbps: u32,
    on_encoder_settings_update: Callback<String>,
    on_error: Callback<String>,
    vad_threshold: Option<f32>,
) -> Box<dyn MicrophoneEncoderTrait> {
    Box::new(MicrophoneEncoder::new(
        client,
        bitrate_kbps,
        on_encoder_settings_update,
        on_error,
        vad_threshold,
    ))
}
