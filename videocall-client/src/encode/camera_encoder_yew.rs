use super::*;
use yew::Callback;

impl CameraEncoder {
    /// Construct a camera encoder, with arguments:
    ///
    /// * `client` - an instance of a [`VideoCallClient`](crate::VideoCallClient).  It does not need to be currently connected.
    ///
    /// * `video_elem_id` - the the ID of an `HtmlVideoElement` to which the camera will be connected.  It does not need to currently exist.
    ///
    /// * `initial_bitrate` - the initial bitrate for the encoder, in kbps.
    ///
    /// * `on_encoder_settings_update` - a callback that will be called when the encoder settings change.
    ///
    /// The encoder is created in a disabled state, [`encoder.set_enabled(true)`](Self::set_enabled) must be called before it can start encoding.
    /// The encoder is created without a camera selected, [`encoder.select(device_id)`](Self::select) must be called before it can start encoding.
    pub fn new(
        client: VideoCallClient,
        video_elem_id: &str,
        initial_bitrate: u32,
        on_encoder_settings_update: Callback<String>,
        on_error: Callback<String>,
    ) -> Self {
        Self {
            client,
            video_elem_id: video_elem_id.to_string(),
            state: EncoderState::new(),
            current_bitrate: Rc::new(AtomicU32::new(initial_bitrate)),
            current_fps: Rc::new(AtomicU32::new(0)),
            on_encoder_settings_update,
            on_error: Some(on_error),
        }
    }

    pub fn set_encoder_control(
        &mut self,
        mut diagnostics_receiver: UnboundedReceiver<DiagnosticsPacket>,
    ) {
        let current_bitrate = self.current_bitrate.clone();
        let current_fps = self.current_fps.clone();
        let on_encoder_settings_update = self.on_encoder_settings_update.clone();
        let enabled = self.state.enabled.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let mut encoder_control = EncoderBitrateController::new(
                current_bitrate.load(Ordering::Relaxed),
                current_fps.clone(),
            );
            while let Some(event) = diagnostics_receiver.next().await {
                let output_wasted = encoder_control.process_diagnostics_packet(event);
                if let Some(bitrate) = output_wasted {
                    if enabled.load(Ordering::Acquire) {
                        // Only update if change is greater than threshold
                        let current = current_bitrate.load(Ordering::Relaxed) as f64;
                        let new = bitrate;
                        let percent_change = (new - current).abs() / current;

                        if percent_change > BITRATE_CHANGE_THRESHOLD {
                            on_encoder_settings_update.emit(format!("Bitrate: {bitrate:.2} kbps"));
                            current_bitrate.store(bitrate as u32, Ordering::Relaxed);
                        }
                    } else {
                        on_encoder_settings_update.emit("Disabled".to_string());
                    }
                }
            }
        });
    }

    /// Start encoding and sending the data to the client connection (if it's currently connected).
    ///
    /// This will not do anything if [`encoder.set_enabled(true)`](Self::set_enabled) has not been
    /// called, or if [`encoder.select(device_id)`](Self::select) has not been called.
    pub fn start(&mut self) {
        // 1. Query the first device with a camera and a mic attached.
        // 2. setup WebCodecs, in particular
        // 3. send encoded video frames and raw audio to the server.
        let client = self.client.clone();
        let userid = client.userid().clone();
        let aes = client.aes();
        let video_elem_id = self.video_elem_id.clone();
        let EncoderState {
            enabled, switching, ..
        } = self.state.clone();
        let current_bitrate = self.current_bitrate.clone();
        let current_fps = self.current_fps.clone();
        let video_output_handler = {
            let mut buffer: Vec<u8> = Vec::with_capacity(100_000);
            let mut sequence_number = 0;
            let mut last_chunk_time = window().performance().unwrap().now();
            let mut chunks_in_last_second = 0;

            Box::new(move |chunk: JsValue| {
                let now = window().performance().unwrap().now();
                let chunk = web_sys::EncodedVideoChunk::from(chunk);

                // Update FPS calculation
                chunks_in_last_second += 1;
                if now - last_chunk_time >= 1000.0 {
                    let fps = chunks_in_last_second;
                    current_fps.store(fps, Ordering::Relaxed);
                    log::debug!("Encoder output FPS: {fps}");
                    chunks_in_last_second = 0;
                    last_chunk_time = now;
                }

                // Ensure the backing buffer is large enough for this chunk
                let byte_length = chunk.byte_length() as usize;
                if buffer.len() < byte_length {
                    buffer.resize(byte_length, 0);
                }

                let packet: PacketWrapper = transform_video_chunk(
                    chunk,
                    sequence_number,
                    buffer.as_mut_slice(),
                    &userid,
                    aes.clone(),
                );
                client.send_packet(packet);
                sequence_number += 1;
            })
        };
        let device_id = if let Some(vid) = &self.state.selected {
            vid.to_string()
        } else {
            return;
        };
        let on_error = self.on_error.clone();

        log::info!(
            "CameraEncoder::start(): using video device_id = {}",
            device_id
        );

        wasm_bindgen_futures::spawn_local(async move {
            let navigator = window().navigator();

            // Wait for <video id="{video_elem_id}"> to be mounted in the DOM
            // Yew renders components asynchronously
            let mut attempt = 0;
            let video_element = loop {
                if let Some(doc) = window().document() {
                    if let Some(elem) = doc.get_element_by_id(&video_elem_id) {
                        if let Ok(video_elem) = elem.dyn_into::<HtmlVideoElement>() {
                            log::info!(
                                "CameraEncoder: found <video id='{}'> after {} attempts",
                                video_elem_id,
                                attempt
                            );
                            break video_elem;
                        }
                    }
                }
                // Sleep a bit and retry
                sleep(Duration::from_millis(50)).await;
                attempt += 1;
                if attempt > 20 {
                    let msg = format!(
                        "Camera error: video element '{}' not found in DOM after 1 second",
                        video_elem_id
                    );
                    error!("{msg}");
                    if let Some(cb) = &on_error {
                        cb.emit(msg);
                    }
                    return;
                }
            };

            let media_devices = match navigator.media_devices() {
                Ok(d) => d,
                Err(e) => {
                    let msg = format!("Failed to access media devices: {e:?}");
                    error!("{msg}");
                    if let Some(cb) = &on_error {
                        cb.emit(msg);
                    }
                    return;
                }
            };
            let constraints = MediaStreamConstraints::new();
            let media_info = web_sys::MediaTrackConstraints::new();

            // Force exact deviceId match (avoids partial/ideal matching surprises).
            let exact = js_sys::Object::new();
            js_sys::Reflect::set(
                &exact,
                &JsValue::from_str("exact"),
                &JsValue::from_str(&device_id),
            )
            .unwrap();

            log::debug!("CameraEncoder: deviceId.exact = {}", device_id);
            media_info.set_device_id(&exact.into());

            constraints.set_video(&media_info.into());
            constraints.set_audio(&Boolean::from(false));

            let devices_query = match media_devices.get_user_media_with_constraints(&constraints) {
                Ok(p) => p,
                Err(e) => {
                    let msg = format!("Camera access failed: {e:?}");
                    error!("{msg}");
                    if let Some(cb) = &on_error {
                        cb.emit(msg);
                    }
                    return;
                }
            };

            let device = match JsFuture::from(devices_query).await {
                Ok(s) => s.unchecked_into::<MediaStream>(),
                Err(e) => {
                    let msg = format!("Failed to get camera stream: {e:?}");
                    error!("{msg}");
                    if let Some(cb) = &on_error {
                        cb.emit(msg);
                    }
                    return;
                }
            };

            log::info!(
                "CameraEncoder: getUserMedia OK, stream id={:?}, tracks={}",
                device.id(),
                device.get_tracks().length()
            );
            // Configure the local preview element
            // Muted must be set before calling play() to avoid autoplay restrictions
            video_element.set_muted(true);
            video_element.set_attribute("playsinline", "true").unwrap();
            video_element.set_src_object(None);
            video_element.set_src_object(Some(&device));

            if let Err(e) = video_element.play() {
                error!("VIDEO PLAY ERROR: {:?}", e);
            } else {
                log::info!(
                    "VIDEO PLAY started successfully on element {}",
                    video_elem_id
                );
            }

            let video_track = Box::new(
                device
                    .get_video_tracks()
                    .find(&mut |_: JsValue, _: u32, _: Array| true)
                    .unchecked_into::<VideoTrack>(),
            );

            // Setup video encoder
            let video_error_handler = Closure::wrap(Box::new(move |e: JsValue| {
                error!("error_handler error {e:?}");
            }) as Box<dyn FnMut(JsValue)>);

            let video_output_handler =
                Closure::wrap(video_output_handler as Box<dyn FnMut(JsValue)>);

            let video_encoder_init = VideoEncoderInit::new(
                video_error_handler.as_ref().unchecked_ref(),
                video_output_handler.as_ref().unchecked_ref(),
            );

            let video_encoder = match VideoEncoder::new(&video_encoder_init) {
                Ok(enc) => Box::new(enc),
                Err(e) => {
                    let msg = format!("Failed to create video encoder: {e:?}");
                    error!("{msg}");
                    if let Some(cb) = &on_error {
                        cb.emit(msg);
                    }
                    return;
                }
            };

            // Get track settings to get actual width and height
            let media_track = video_track
                .as_ref()
                .clone()
                .unchecked_into::<MediaStreamTrack>();
            let track_settings = media_track.get_settings();

            let width = track_settings.get_width().expect("width is None");
            let height = track_settings.get_height().expect("height is None");

            let video_encoder_config =
                VideoEncoderConfig::new(get_video_codec_string(), height as u32, width as u32);
            video_encoder_config
                .set_bitrate(current_bitrate.load(Ordering::Relaxed) as f64 * 1000.0);
            video_encoder_config.set_latency_mode(LatencyMode::Realtime);

            if let Err(e) = video_encoder.configure(&video_encoder_config) {
                error!("Error configuring video encoder: {e:?}");
            }

            let video_processor =
                MediaStreamTrackProcessor::new(&MediaStreamTrackProcessorInit::new(
                    &video_track.clone().unchecked_into::<MediaStreamTrack>(),
                ))
                .unwrap();
            let video_reader = video_processor
                .readable()
                .get_reader()
                .unchecked_into::<ReadableStreamDefaultReader>();

            // Start encoding video and audio.
            let mut video_frame_counter = 0;

            // Cache the initial bitrate
            let mut local_bitrate: u32 = current_bitrate.load(Ordering::Relaxed) * 1000;

            // Track current encoder dimensions for dynamic reconfiguration
            let mut current_encoder_width = width as u32;
            let mut current_encoder_height = height as u32;

            loop {
                if !enabled.load(Ordering::Acquire) || switching.load(Ordering::Acquire) {
                    switching.store(false, Ordering::Release);
                    let video_track = video_track.clone().unchecked_into::<MediaStreamTrack>();
                    video_track.stop();
                    log::info!("CameraEncoder: stopped");
                    if let Err(e) = video_encoder.close() {
                        error!("Error closing video encoder: {e:?}");
                    }
                    return;
                }

                // Update the bitrate if it has changed more than the threshold percentage
                let new_current_bitrate = current_bitrate.load(Ordering::Relaxed) * 1000;
                if new_current_bitrate != local_bitrate {
                    log::info!("Updating video bitrate to {new_current_bitrate}");
                    local_bitrate = new_current_bitrate;
                    video_encoder_config.set_bitrate(local_bitrate as f64);
                    if let Err(e) = video_encoder.configure(&video_encoder_config) {
                        error!("Error configuring video encoder: {e:?}");
                    }
                }

                match JsFuture::from(video_reader.read()).await {
                    Ok(js_frame) => {
                        let video_frame = Reflect::get(&js_frame, &JsString::from("value"))
                            .unwrap()
                            .unchecked_into::<VideoFrame>();

                        // Check for dimension changes (rotation, camera switch)
                        let frame_width = video_frame.display_width();
                        let frame_height = video_frame.display_height();

                        if frame_width > 0
                            && frame_height > 0
                            && (frame_width != current_encoder_width
                                || frame_height != current_encoder_height)
                        {
                            log::info!("Camera dimensions changed from {current_encoder_width}x{current_encoder_height} to {frame_width}x{frame_height}, reconfiguring encoder");

                            current_encoder_width = frame_width;
                            current_encoder_height = frame_height;

                            let new_config = VideoEncoderConfig::new(
                                get_video_codec_string(),
                                current_encoder_height,
                                current_encoder_width,
                            );
                            new_config.set_bitrate(local_bitrate as f64);
                            new_config.set_latency_mode(LatencyMode::Realtime);
                            if let Err(e) = video_encoder.configure(&new_config) {
                                error!(
                                    "Error reconfiguring camera encoder with new dimensions: {e:?}"
                                );
                            }
                        }

                        let video_encoder_encode_options = VideoEncoderEncodeOptions::new();
                        video_encoder_encode_options.set_key_frame(video_frame_counter % 150 == 0);
                        if let Err(e) = video_encoder
                            .encode_with_options(&video_frame, &video_encoder_encode_options)
                        {
                            error!("Error encoding video frame: {e:?}");
                        }
                        video_frame.close();
                        video_frame_counter += 1;
                    }
                    Err(e) => {
                        error!("error {e:?}");
                    }
                }
            }
        });
    }
}
