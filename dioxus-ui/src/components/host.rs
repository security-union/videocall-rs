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

use crate::components::attendants::PreAcquiredScreenStream;
use crate::components::device_settings_modal::DeviceSettingsModal;
use crate::components::performance_settings::{
    load_performance_preference, load_receive_preference, preference_to_encoder_bounds,
    save_performance_preference, save_receive_preference, DiagnosticsReader, KindReceivePref,
    PerfControlsHandle, PerformancePreference, ReceivedReader, ScreenSnapshotReader,
    SimulcastSummary, SnapshotReader,
};
use crate::constants::*;
use crate::context::{
    load_preferred_device_ids, restore_device_id, save_preferred_camera_id, save_preferred_mic_id,
    save_preferred_speaker_id, TransportPreferenceCtx, VideoCallClientCtx,
};
use crate::types::DeviceInfo;
use dioxus::prelude::*;
use gloo_timers::callback::Timeout;
use videocall_client::Callback as VcCallback;
use videocall_client::{create_microphone_encoder, MicrophoneEncoderTrait};
use videocall_client::{
    initial_screen_tier, CameraEncoder, MediaDeviceList, PrefMediaKind, ScreenEncoder,
    ScreenShareEvent,
};

use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::MediaStream;

const VIDEO_ELEMENT_ID: &str = "webcam";

struct EncoderSettings {
    camera: Option<String>,
    microphone: Option<String>,
    screen: Option<String>,
}

impl std::fmt::Display for EncoderSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut settings = Vec::new();
        if let Some(camera) = &self.camera {
            settings.push(format!("Camera: {camera}"));
        }
        if let Some(microphone) = &self.microphone {
            settings.push(format!("Microphone: {microphone}"));
        }
        if let Some(screen) = &self.screen {
            settings.push(format!("Screen: {screen}"));
        }
        write!(f, "{}", settings.join(", "))
    }
}

#[component]
pub fn Host(
    share_screen: bool,
    mic_enabled: bool,
    video_enabled: bool,
    on_encoder_settings_update: EventHandler<String>,
    device_settings_open: bool,
    on_device_settings_toggle: EventHandler<()>,
    #[props(default)] on_microphone_error: EventHandler<String>,
    #[props(default)] on_camera_error: EventHandler<String>,
    #[props(default)] device_settings_initial_section: Option<String>,
    #[props(default)] device_settings_generation: u32,
    on_screen_share_state: EventHandler<ScreenShareEvent>,
    reload_devices_counter: u32,
    /// Sink the parent (attendants) reads to feed the Diagnostics drawer's
    /// "Simulcast layers" section the live SEND simulcast snapshots. Host owns the
    /// encoders, so it builds the `DiagnosticsReader` and publishes the handle here
    /// once on mount; `None` until then. Optional so callers that don't need the
    /// cross-component detail still compile. (#1095 §6 MOVE)
    #[props(default)]
    publish_diagnostics_reader: Option<Signal<Option<DiagnosticsReader>>>,
    /// Sink the parent (attendants) reads to feed the Diagnostics drawer the
    /// Performance controls (sliders/Auto/meters). The panel now lives in the
    /// drawer — a sibling of `Host` that can't reach the encoders or the
    /// preference signals — so `Host` bundles them into a [`PerfControlsHandle`]
    /// and publishes it here once on mount; `None` until then. (#1131 unify)
    #[props(default)]
    publish_perf_controls: Option<Signal<Option<PerfControlsHandle>>>,
) -> Element {
    let client = use_context::<VideoCallClientCtx>();
    let transport_pref_ctx = use_context::<TransportPreferenceCtx>();
    let pre_acquired_stream = use_context::<PreAcquiredScreenStream>();

    // Indirection cells for callbacks: updated each render, closed over by encoder callbacks
    let camera_settings_handler: Rc<RefCell<Option<EventHandler<String>>>> =
        use_hook(|| Rc::new(RefCell::new(None)));
    let mic_settings_handler: Rc<RefCell<Option<EventHandler<String>>>> =
        use_hook(|| Rc::new(RefCell::new(None)));
    let screen_settings_handler: Rc<RefCell<Option<EventHandler<String>>>> =
        use_hook(|| Rc::new(RefCell::new(None)));
    let camera_error_handler: Rc<RefCell<Option<EventHandler<String>>>> =
        use_hook(|| Rc::new(RefCell::new(None)));
    let mic_error_handler: Rc<RefCell<Option<EventHandler<String>>>> =
        use_hook(|| Rc::new(RefCell::new(None)));
    let screen_state_handler: Rc<RefCell<Option<EventHandler<ScreenShareEvent>>>> =
        use_hook(|| Rc::new(RefCell::new(None)));

    // Use Rc<RefCell<>> to hold mutable encoder state that persists across renders
    let state = use_hook(|| {
        let video_bitrate = video_bitrate_kbps().unwrap_or(1000);
        let audio_bitrate = audio_bitrate_kbps().unwrap_or(65);
        let screen_bitrate = screen_bitrate_kbps().unwrap_or(1000);

        // Simulcast layer ceiling (issue #989 / #1082): the lesser of the runtime
        // flag (`experimentalSimulcastMaxLayers`, defaults to 3 = ON) and what
        // this device can actually encode without stalling. A weak device's
        // capability ceiling auto-gates this DOWN to 1–2 layers even with the
        // flag at its default 3, so default-ON is safe.
        //
        // This `use_hook` closure runs once per Host component mount, so the
        // INFO below fires once per session — never per frame.
        //
        // VIDEO/SCREEN share the CPU-derived capability ceiling because each
        // extra WebCodecs VIDEO encoder is ~N× main-thread encode cost (the
        // 2-core Intel Mac stall mode, discussion #562/#890).
        let flag_max_layers = experimental_simulcast_max_layers();
        let effective_max_layers = flag_max_layers
            .min(crate::components::capability_check::capability_max_simulcast_layers());
        log::info!("CameraEncoder: effective simulcast layers = {effective_max_layers}");

        // AUDIO is decoupled from the VIDEO capability ceiling (issue #1082):
        // Opus encoders run on the AudioWorklet thread (off the main thread) and
        // cost ~1-3% of call bandwidth, so a device that is correctly capped to
        // 1 VIDEO layer can still run the full audio ladder. Audio's ceiling is
        // the audio ladder size itself (`max_layers_for_kind(Audio)`), still
        // gated by the SAME runtime flag — so setting the flag to 1 disables
        // audio simulcast too (it now defaults to 3 = ON).
        let audio_capability_ceiling = videocall_client::max_layers_for_kind(PrefMediaKind::Audio);
        let audio_effective_max_layers = flag_max_layers.min(audio_capability_ceiling);
        log::info!(
            "MicrophoneEncoder: effective audio simulcast layers = {audio_effective_max_layers} \
             (flag={flag_max_layers}, audio_ceiling={audio_capability_ceiling})"
        );
        if effective_max_layers > 1 {
            log::info!(
                "SIMULCAST: publishing {effective_max_layers} video layers (default ON). \
                 Receiver-driven per-peer layer selection is live — each receiver \
                 automatically pulls the best layer its own downlink can sustain (and the \
                 relay drops the layers it did not select per source+kind). This increases \
                 this sender's encode CPU and uplink/relay egress; weak devices auto-gate \
                 to fewer layers via the capability ceiling. To disable, set \
                 experimentalSimulcastMaxLayers=1."
            );
        }

        let cam_settings_cell = camera_settings_handler.clone();
        let camera_settings_cb = VcCallback::from(move |settings: String| {
            if let Some(handler) = cam_settings_cell.borrow().as_ref() {
                handler.call(settings);
            }
        });
        let cam_error_cell = camera_error_handler.clone();
        let camera_error_cb = VcCallback::from(move |err: String| {
            if let Some(handler) = cam_error_cell.borrow().as_ref() {
                handler.call(err);
            }
        });
        let mut camera = CameraEncoder::new(
            client.clone(),
            VIDEO_ELEMENT_ID,
            video_bitrate,
            camera_settings_cb,
            camera_error_cb,
            effective_max_layers,
        );

        let mic_settings_cell = mic_settings_handler.clone();
        let mic_settings_cb = VcCallback::from(move |settings: String| {
            if let Some(handler) = mic_settings_cell.borrow().as_ref() {
                handler.call(settings);
            }
        });
        let mic_error_cell = mic_error_handler.clone();
        let mic_error_cb = VcCallback::from(move |err: String| {
            if let Some(handler) = mic_error_cell.borrow().as_ref() {
                handler.call(err);
            }
        });
        // Microphone encoder is created after camera so it can share the
        // camera's audio tier atomics (avoiding a duplicate quality manager).
        let microphone = create_microphone_encoder(
            client.clone(),
            audio_bitrate,
            mic_settings_cb,
            mic_error_cb,
            vad_threshold().ok(),
            Some(camera.shared_audio_tier_bitrate()),
            Some(camera.shared_audio_tier_fec()),
            // Audio simulcast layer ceiling (issue #989, Phase 3c → #1082):
            // decoupled from the VIDEO CPU ceiling (audio encodes off-main-thread
            // and is cheap), but still gated by the SAME runtime flag so it stays
            // OFF by default (single audio layer, byte-identical to the
            // pre-simulcast mic path).
            audio_effective_max_layers,
        );

        let screen_settings_cell = screen_settings_handler.clone();
        let screen_settings_cb = VcCallback::from(move |settings: String| {
            if let Some(handler) = screen_settings_cell.borrow().as_ref() {
                handler.call(settings);
            }
        });
        let screen_state_cell = screen_state_handler.clone();
        let screen_state_cb = VcCallback::from(move |event: ScreenShareEvent| {
            match &event {
                ScreenShareEvent::Started(stream) => {
                    attach_screen_preview(stream);
                }
                _ => {
                    detach_screen_preview();
                }
            }
            if let Some(handler) = screen_state_cell.borrow().as_ref() {
                handler.call(event);
            }
        });
        let mut screen = ScreenEncoder::new(
            client.clone(),
            screen_bitrate,
            screen_settings_cb,
            screen_state_cb,
            camera.screen_sharing_flag(),
            // Screen simulcast layer ceiling (issue #989, Phase 3b) — same
            // flag + capability gating as the camera, so it's OFF by default
            // (single layer, byte-identical to the pre-simulcast screen path).
            effective_max_layers,
        );

        // Wire up congestion step-down, PLI keyframe, and re-election flags
        camera.set_congestion_step_down_flag(client.congestion_step_down_flag());
        camera.set_force_keyframe_flag(client.force_camera_keyframe_flag());
        camera.set_reelection_completed_signal(client.reelection_completed_signal());
        screen.set_force_keyframe_flag(client.force_screen_keyframe_flag());
        screen.set_reelection_completed_signal(client.reelection_completed_signal());
        // Issue #1199: give the screen encoder its own CONGESTION step-down flag
        // so it responds to server congestion in parity with the camera. The
        // client sets both flags on a self-targeted CONGESTION; each AQ loop
        // consumes its own (separate atoms avoid a swap race).
        screen.set_congestion_step_down_flag(client.screen_congestion_step_down_flag());

        // Wire the relay layer-union hint atoms (issue #1108, Stage 3). Each
        // encoder OWNS its `shared_union_requested_layer` atom (initialized to the
        // u32::MAX fail-open sentinel) and its AQ control loop reads it; here we
        // hand the SAME atom to the client so the inbound LAYER_HINT dispatch arm
        // writes the relay's per-source max-requested-layer into it. This is the
        // inverse of the keyframe-flag wiring (atom owned by the encoder, not the
        // client). The client also resets these atoms to u32::MAX on reconnect.
        client.set_camera_union_requested_layer(camera.shared_union_requested_layer());
        client.set_screen_union_requested_layer(screen.shared_union_requested_layer());

        // Forced-keyframe cooldown reset on reconnect (issue #1311). Hand the
        // camera- and screen-owned reset atoms to the client so its `Connected`
        // lifecycle callback clears each encode loop's `last_keyframe_emit_ms` on
        // every reconnect — the first post-reconnect PLI then emits immediately
        // instead of being coalesced away by a stale pre-reconnect cooldown
        // timestamp. The re-election case is handled inside each encoder itself
        // (quality task). Both encoders are armed from the SAME `Connected`
        // transition so they reset together (camera was #1348; screen is the #1311
        // follow-up, now unblocked by the screen `last_keyframe_emit_ms` from
        // #1322/#1344).
        client.set_camera_keyframe_cooldown_reset(camera.keyframe_cooldown_reset());
        client.set_screen_keyframe_cooldown_reset(screen.keyframe_cooldown_reset());

        // Wire adaptive quality tier indices to health reporter for metrics
        client.set_adaptive_tier_sources(
            camera.shared_video_tier_index(),
            camera.shared_audio_tier_index(),
        );

        // Wire encoder decision inputs + screen tier to health reporter for metrics
        client.set_encoder_metric_sources(
            camera.shared_encoder_queue_depth_report(),
            camera.shared_encoder_target_bitrate_kbps(),
            screen.shared_screen_tier_index(),
            camera.screen_sharing_flag(),
            camera.shared_encoder_output_fps(),
            camera.shared_tier_transitions(),
            screen.shared_tier_transitions(),
            camera.shared_climb_limiter_snapshot(),
            camera.shared_dwell_samples(),
            // #1143: send-side simulcast layer counts (camera encoder atoms).
            camera.shared_effective_layer_count(),
            camera.shared_active_layer_count(),
        );

        // Wire up encoder controls. Issue #1108: the encoder AQ is now a
        // self-timer driven by the sender's OWN encoder backpressure — it no
        // longer subscribes to receiver-reported diagnostics, so there are no
        // diagnostics channels to wire here. (The microphone encoder still reads
        // audio tier settings from the camera encoder's shared atomics.)
        camera.set_encoder_control();
        screen.set_encoder_control();

        // Apply the user's persisted performance (quality-bounds) preference
        // (issue #961) before the encoder starts, so the very first encode honors
        // the bounds. All-Auto (the default) is a no-op. The bounds are also
        // stored inside the encoder and re-applied on every (re)start, so this
        // single call survives camera restarts.
        let perf_pref = load_performance_preference();
        let bounds = preference_to_encoder_bounds(&perf_pref);
        camera.set_quality_tier_bounds(
            bounds.video_best,
            bounds.video_worst,
            bounds.audio_best,
            bounds.audio_worst,
        );
        // Screen share bounds live on the separate ScreenEncoder (issue #961).
        screen.set_quality_tier_bounds(bounds.screen_best, bounds.screen_worst);
        // User SEND layer-ceiling ("layers published" control): cap how many
        // simulcast layers each publisher emits. None = Auto / full ladder. Like
        // the bounds above, the value persists in the encoder's shared atomic and
        // is re-read by the AQ control loop on every (re)start, so this single
        // call survives camera/screen restarts + reconnect.
        camera.set_user_layer_ceiling(perf_pref.video_layers);
        screen.set_user_layer_ceiling(perf_pref.screen_layers);
        // Audio's published layer count is adjustable too (runtime publish-gate in
        // the mic encoder — no restart). Same persist/reapply contract: the value
        // lives in the mic encoder's shared atomic (cloned into every layer's
        // publish handler), so it survives reconnect and this re-applies it from
        // the persisted preference on re-init.
        microphone.set_user_layer_ceiling(perf_pref.audio_layers);

        // Create MediaDeviceList
        let media_devices = MediaDeviceList::new();

        Rc::new(RefCell::new(HostState {
            camera,
            microphone,
            screen,
            media_devices,
            encoder_settings: EncoderSettings {
                camera: None,
                microphone: None,
                screen: None,
            },
            prev_share_screen: false,
            prev_mic_enabled: false,
            prev_video_enabled: false,
            prev_device_settings_open: false,
            initialized: false,
            last_reload_counter: 0,
        }))
    });

    // Update the indirection cells so encoder callbacks route to the current EventHandlers.
    // This runs on every render to keep them in sync with the latest prop values.
    *camera_settings_handler.borrow_mut() = Some(on_encoder_settings_update);
    *mic_settings_handler.borrow_mut() = Some(on_encoder_settings_update);
    *screen_settings_handler.borrow_mut() = Some(on_encoder_settings_update);
    *camera_error_handler.borrow_mut() = Some(on_camera_error);
    *mic_error_handler.borrow_mut() = Some(on_microphone_error);
    *screen_state_handler.borrow_mut() = Some(on_screen_share_state);

    // Initialize devices once
    {
        let state = state.clone();
        let value = client.clone();
        use_effect(move || {
            let state_for_loaded = state.clone();
            let client_for_loaded = value.clone();
            let mut s = state.borrow_mut();
            if !s.initialized {
                s.media_devices.on_loaded = VcCallback::from(move |_| {
                    let mut s = state_for_loaded.borrow_mut();

                    // Honor the pre-join device selection (issue #959): resolve
                    // the persisted device ids against the live device lists,
                    // falling back to the first device when a stored id no
                    // longer exists. This is what makes the camera/mic/speaker
                    // chosen on the pre-join screen the ones actually used when
                    // capture starts.
                    let (stored_cam, stored_mic, stored_spk) = load_preferred_device_ids();
                    let cam_ids: Vec<String> = s
                        .media_devices
                        .video_inputs
                        .devices()
                        .iter()
                        .map(|d| d.device_id())
                        .collect();
                    let mic_ids: Vec<String> = s
                        .media_devices
                        .audio_inputs
                        .devices()
                        .iter()
                        .map(|d| d.device_id())
                        .collect();
                    let spk_ids: Vec<String> = s
                        .media_devices
                        .audio_outputs
                        .devices()
                        .iter()
                        .map(|d| d.device_id())
                        .collect();
                    if let Some(id) = restore_device_id(stored_cam.as_deref(), &cam_ids) {
                        s.media_devices.video_inputs.select(&id);
                        save_preferred_camera_id(&id);
                    }
                    if let Some(id) = restore_device_id(stored_mic.as_deref(), &mic_ids) {
                        s.media_devices.audio_inputs.select(&id);
                        save_preferred_mic_id(&id);
                    }
                    if let Some(id) = restore_device_id(stored_spk.as_deref(), &spk_ids) {
                        s.media_devices.audio_outputs.select(&id);
                        save_preferred_speaker_id(&id);
                        // Apply the persisted speaker to the shared audio sink so
                        // the pre-join speaker choice is honored in-meeting.
                        // No-op where setSinkId is unsupported (handled inside
                        // SharedAudioContext::update_speaker_device). (issue #959)
                        if let Err(e) = client_for_loaded.update_speaker_device(Some(id.clone())) {
                            log::warn!("Failed to apply pre-join speaker selection: {e:?}");
                        }
                    }

                    let video_id = s.media_devices.video_inputs.selected();
                    let audio_id = s.media_devices.audio_inputs.selected();
                    let num_cameras = s.media_devices.video_inputs.devices().len();
                    let num_mics = s.media_devices.audio_inputs.devices().len();

                    log::info!(
                        "Host on_loaded: cameras={num_cameras} mics={num_mics} \
                         video_id='{video_id}' audio_id='{audio_id}' \
                         prev_video={} prev_mic={}",
                        s.prev_video_enabled,
                        s.prev_mic_enabled,
                    );

                    // Auto-select camera device
                    let cam_needs_start = if !video_id.is_empty() {
                        s.media_devices.video_inputs.select(&video_id);
                        // Restore the lobby camera (issue #1295). This is a clean
                        // restore: select() records the device while the encoder
                        // is (re)enabled below, and because no prior loop is alive
                        // in the lobby the deferred start() simply spawns exactly
                        // one loop bound to this device. (select() here runs while
                        // the encoder is still DISABLED, so it does NOT raise
                        // `switching` — the start() single-loop/epoch guard, not a
                        // switching flag, is what would prevent duplicates if a
                        // loop ever were alive.) `was_enabled` keeps a camera that
                        // was OFF in the lobby from being force-started.
                        let was_enabled = s.prev_video_enabled;
                        s.camera.select(video_id);
                        if was_enabled {
                            s.camera.set_enabled(true);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    // Auto-select microphone device
                    let mic_needs_start = if !audio_id.is_empty() {
                        s.media_devices.audio_inputs.select(&audio_id);
                        let was_enabled = s.prev_mic_enabled;
                        if was_enabled {
                            s.microphone.stop();
                        }
                        s.microphone.select(audio_id);
                        if was_enabled {
                            s.microphone.set_enabled(true);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    log::info!(
                        "Host on_loaded: cam_needs_start={cam_needs_start} mic_needs_start={mic_needs_start}"
                    );

                    drop(s);

                    // Start encoders that were already enabled (camera/mic were
                    // toggled on before devices finished loading).
                    if cam_needs_start {
                        let sc = state_for_loaded.clone();
                        Timeout::new(500, move || {
                            log::info!("Host on_loaded: starting camera after timeout");
                            sc.borrow_mut().camera.start();
                        })
                        .forget();
                    }
                    if mic_needs_start {
                        let sc = state_for_loaded.clone();
                        Timeout::new(500, move || {
                            log::info!("Host on_loaded: starting microphone after timeout");
                            sc.borrow_mut().microphone.start();
                        })
                        .forget();
                    }
                });
                let state_for_devices_changed = state.clone();
                let client_for_devices_changed = value.clone();
                s.media_devices.on_devices_changed = VcCallback::from(move |_| {
                    let mut s = state_for_devices_changed.borrow_mut();

                    let audio_device_id = s.media_devices.audio_inputs.selected();
                    let video_device_id = s.media_devices.video_inputs.selected();
                    let speaker_device_id = s.media_devices.audio_outputs.selected();

                    let mut mic_needs_start = false;
                    if !audio_device_id.is_empty() {
                        s.media_devices.audio_inputs.select(&audio_device_id);
                        mic_needs_start = s.microphone.select(audio_device_id);
                    }

                    let mut cam_needs_start = false;
                    if !video_device_id.is_empty() {
                        s.media_devices.video_inputs.select(&video_device_id);
                        cam_needs_start = s.camera.select(video_device_id);
                    }

                    if !speaker_device_id.is_empty() {
                        s.media_devices.audio_outputs.select(&speaker_device_id);
                        if let Err(e) = client_for_devices_changed
                            .update_speaker_device(Some(speaker_device_id))
                        {
                            log::error!("Failed to update speaker device: {e:?}");
                        }
                    }

                    drop(s);

                    if mic_needs_start {
                        let sc = state_for_devices_changed.clone();
                        Timeout::new(1000, move || {
                            sc.borrow_mut().microphone.start();
                        })
                        .forget();
                    }
                    if cam_needs_start {
                        let sc = state_for_devices_changed.clone();
                        Timeout::new(1000, move || {
                            sc.borrow_mut().camera.start();
                        })
                        .forget();
                    }
                });
                s.media_devices.load();
                s.initialized = true;
            }
        });
    }

    // Handle prop changes for screen/mic/video enables.
    // NOTE: This runs in the component body (not use_effect) because Dioxus 0.7
    // use_effect does NOT re-run when ReadOnlySignal props change.  The component
    // function itself re-runs whenever the parent passes new prop values.
    {
        let mut s = state.borrow_mut();

        let did_full_reload = s.last_reload_counter != reload_devices_counter;
        if did_full_reload {
            s.media_devices.load();
            s.last_reload_counter = reload_devices_counter;
        }

        if !did_full_reload && !s.prev_device_settings_open && device_settings_open {
            s.media_devices.refresh_devices_safely();
        }
        s.prev_device_settings_open = device_settings_open;

        // Edge-triggered: log only when video/mic/screen state actually CHANGES,
        // not on every Host re-render. This component re-renders many times per
        // second, so an unconditional log here was the dominant console-log line
        // after the #1100/#1129 per-tick demotions (~65% of all lines in a
        // captured preview session). The `prev_*` fields are still updated below
        // (lines further down), so this comparison sees the prior render's state.
        if video_enabled != s.prev_video_enabled
            || mic_enabled != s.prev_mic_enabled
            || share_screen != s.prev_share_screen
        {
            log::info!(
                "Host render: video={video_enabled} prev={} mic={mic_enabled} prev={} screen={share_screen} prev={}",
                s.prev_video_enabled, s.prev_mic_enabled, s.prev_share_screen,
            );
        }

        // Screen share
        if s.prev_share_screen != share_screen {
            s.prev_share_screen = share_screen;
            if share_screen {
                s.screen.set_enabled(true);

                // Adaptive initial tier selection: inspect network signals at
                // the moment screen sharing starts to choose a conservative
                // starting tier that gives a readable first frame on constrained
                // uplinks without waiting for the PID loop to ramp down.
                let rtt_ms = client.average_rtt_ms();
                let camera_tier_index = client.camera_tier_index();
                let initial_tier = initial_screen_tier(rtt_ms, camera_tier_index);

                log::info!(
                    "Start screen share encoder: rtt={:?}ms, camera_tier={:?}, initial_tier={}",
                    rtt_ms,
                    camera_tier_index,
                    initial_tier
                );

                // Check if the onclick handler already acquired a MediaStream
                // (required for Safari which mandates getDisplayMedia be called
                // synchronously within a user gesture handler).
                let maybe_stream = pre_acquired_stream.borrow_mut().take();
                if let Some(stream) = maybe_stream {
                    log::info!("Start screen share encoder with pre-acquired stream");
                    s.screen.start_with_stream(stream, initial_tier);
                } else {
                    // Fallback: let the encoder call getDisplayMedia itself.
                    // This path works on Chrome/Firefox where the gesture
                    // chain survives the timeout + spawn_local boundaries.
                    log::info!("Start screen share encoder (encoder-acquired stream)");
                    let state_clone = state.clone();
                    Timeout::new(1000, move || {
                        state_clone.borrow_mut().screen.start(initial_tier);
                    })
                    .forget();
                }
            } else {
                s.screen.set_enabled(false);
                s.screen.stop();
                detach_screen_preview();
                s.encoder_settings.screen = None;
            }
        }

        // Microphone
        if s.prev_mic_enabled != mic_enabled {
            s.prev_mic_enabled = mic_enabled;
            if mic_enabled {
                let device_id = s.media_devices.audio_inputs.selected();
                if !device_id.is_empty() {
                    s.microphone.select(device_id);
                }
                s.microphone.set_enabled(true);
                s.microphone.start();
            } else {
                s.microphone.set_enabled(false);
                s.microphone.stop();
                s.encoder_settings.microphone = None;
            }
            client.set_audio_enabled(mic_enabled);
        }

        // Camera
        if s.prev_video_enabled != video_enabled {
            s.prev_video_enabled = video_enabled;
            if video_enabled {
                let device_id = s.media_devices.video_inputs.selected();
                log::info!("Host render: camera ON, auto-select device_id='{device_id}'");
                if !device_id.is_empty() {
                    s.camera.select(device_id);
                }
                s.camera.set_enabled(true);
                s.camera.start();
            } else {
                log::info!("Host render: camera OFF");
                s.camera.set_enabled(false);
                s.camera.stop();
                s.encoder_settings.camera = None;
            }
            client.set_video_enabled(video_enabled);
        }

        drop(s);
    }

    // Periodically re-affirm audio/video enabled flags on the active connection.
    //
    // In Yew, the Host component re-renders ~every second (driven by encoder
    // settings update messages), and rendered() calls set_video_enabled /
    // set_audio_enabled each time.  In Dioxus, the component body only runs
    // when props change, so after the initial enable the flags are never
    // re-affirmed.  If the underlying connection is lost and re-established,
    // the new Connection object starts with video_enabled=false.  Without
    // periodic re-affirmation the heartbeat keeps reporting video_enabled=false
    // to the server, causing peers to see the video toggling off.
    {
        let state = state.clone();
        let client = client.clone();
        use_effect(move || {
            let state = state.clone();
            let client = client.clone();
            spawn(async move {
                loop {
                    gloo_timers::future::TimeoutFuture::new(1_000).await;
                    let s = state.borrow();
                    client.set_audio_enabled(s.prev_mic_enabled);
                    client.set_video_enabled(s.prev_video_enabled);
                }
            });
        });
    }

    // Device change handlers (Rc-wrapped so they can be shared between two components)
    let on_mic_change: Rc<dyn Fn(DeviceInfo)> = {
        let state = state.clone();
        Rc::new(move |audio: DeviceInfo| {
            let mut s = state.borrow_mut();
            s.media_devices.audio_inputs.select(&audio.device_id);
            // Persist so the next pre-join screen restores this mic. (issue #959)
            save_preferred_mic_id(&audio.device_id);
            if s.microphone.select(audio.device_id.clone()) {
                let state_clone = state.clone();
                Timeout::new(1000, move || {
                    state_clone.borrow_mut().microphone.start();
                })
                .forget();
            }
        })
    };

    let on_cam_change: Rc<dyn Fn(DeviceInfo)> = {
        let state = state.clone();
        Rc::new(move |video: DeviceInfo| {
            let mut s = state.borrow_mut();
            s.media_devices.video_inputs.select(&video.device_id);
            // Persist so the next pre-join screen restores this camera. (issue #959)
            save_preferred_camera_id(&video.device_id);
            if s.camera.select(video.device_id.clone()) {
                let state_clone = state.clone();
                Timeout::new(1000, move || {
                    state_clone.borrow_mut().camera.start();
                })
                .forget();
            }
        })
    };

    // Performance (quality-bounds) preference, restored from localStorage and
    // surfaced to the settings modal (issue #961). The encoder already had the
    // restored bounds applied at construction time above; this signal is the
    // UI's source of truth for the selector positions.
    let performance_preference = use_signal(load_performance_preference);

    // Apply a changed performance preference: persist it and push the inverted
    // best/worst bounds to the live encoder. The encoder stores them and
    // re-applies on every (re)start, so this works whether or not the camera is
    // currently running.
    let on_performance_change: Rc<dyn Fn(PerformancePreference)> = {
        let state = state.clone();
        Rc::new(move |pref: PerformancePreference| {
            save_performance_preference(&pref);
            let mut performance_preference = performance_preference;
            performance_preference.set(pref);
            let bounds = preference_to_encoder_bounds(&pref);
            let mut s = state.borrow_mut();
            s.camera.set_quality_tier_bounds(
                bounds.video_best,
                bounds.video_worst,
                bounds.audio_best,
                bounds.audio_worst,
            );
            // Screen share is a separate encoder object (issue #961).
            s.screen
                .set_quality_tier_bounds(bounds.screen_best, bounds.screen_worst);
            // User SEND layer-ceiling: apply LIVE as the user drags the "layers
            // published" thumb. The encoder stores the count in a shared atomic
            // the AQ control loop reads each tick (≤1s), composing it as a further
            // `min` with the union hint + ramp — so lowering it sheds the top
            // layer(s) at once and raising it re-earns them. None = Auto.
            s.camera.set_user_layer_ceiling(pref.video_layers);
            s.screen.set_user_layer_ceiling(pref.screen_layers);
            // Audio layer ceiling applies LIVE too: the mic encoder's per-layer
            // publish handlers read the shared atomic at publish time, so lowering
            // it stops sending the top audio layer(s) on the next frame and raising
            // it resumes them — no mic restart, no audio interruption.
            s.microphone.set_user_layer_ceiling(pref.audio_layers);
        })
    };

    // Live snapshot reader for the VU meters. Returns `None` while the camera is
    // off so the meters show placeholders rather than a stale pinned tier.
    // Built once per mount (via `use_hook`) so its `Rc` identity is stable and
    // the meter component doesn't see a "changed" prop every render.
    let read_quality_snapshot: SnapshotReader = {
        let state = state.clone();
        use_hook(move || {
            SnapshotReader(Rc::new(move || {
                let s = state.borrow();
                if s.prev_video_enabled {
                    Some(s.camera.live_quality_snapshot())
                } else {
                    None
                }
            }))
        })
    };

    // Live screen-share snapshot reader for the third VU meter. The encoder
    // already returns `None` while not sharing, so no extra gate is needed.
    let read_screen_snapshot: ScreenSnapshotReader = {
        let state = state.clone();
        use_hook(move || {
            ScreenSnapshotReader(Rc::new(move || {
                state.borrow().screen.live_screen_snapshot()
            }))
        })
    };

    let on_speaker_change: Rc<dyn Fn(DeviceInfo)> = {
        let state = state.clone();
        let client = client.clone();
        Rc::new(move |speaker: DeviceInfo| {
            let mut s = state.borrow_mut();
            s.media_devices.audio_outputs.select(&speaker.device_id);
            // Persist so the next pre-join screen restores this speaker. (issue #959)
            save_preferred_speaker_id(&speaker.device_id);
            if let Err(e) = client.update_speaker_device(Some(speaker.device_id.clone())) {
                log::error!("Failed to update speaker device: {e:?}");
            }
        })
    };

    // RECEIVE-side layer-bounds preference (simulcast P4/P5), restored from
    // localStorage and surfaced to the settings modal. The signal is the UI's
    // source of truth for the slider positions.
    let receive_preference = use_signal(load_receive_preference);

    // Apply the persisted receive bounds to the client once on mount, so the
    // restored caps take effect immediately (each kind's effective bounds, with
    // Auto → (None, None)). Re-applying is idempotent.
    {
        let client = client.clone();
        use_hook(move || {
            let pref = load_receive_preference();
            for kind in [
                PrefMediaKind::Video,
                PrefMediaKind::Audio,
                PrefMediaKind::Screen,
            ] {
                let (min, max) = pref.effective_bounds(kind);
                client.set_receive_layer_bounds(kind, min, max);
            }
        });
    }

    // Apply a changed receive bound: persist it and push the effective bounds for
    // the changed kind to the client (immediate downlink effect).
    let on_receive_change: Rc<dyn Fn((PrefMediaKind, KindReceivePref))> = {
        let client = client.clone();
        Rc::new(move |(kind, sub): (PrefMediaKind, KindReceivePref)| {
            let mut receive_preference = receive_preference;
            let next = receive_preference().with_kind(kind, sub);
            save_receive_preference(&next);
            receive_preference.set(next);
            let (min, max) = next.effective_bounds(kind);
            client.set_receive_layer_bounds(kind, min, max);
        })
    };

    // Per-kind received-layer snapshot reader for the P5 needles. Built once per
    // mount (stable `Rc`) so the meter component doesn't see a "changed" prop.
    let received_reader: ReceivedReader = {
        let client = client.clone();
        use_hook(move || {
            ReceivedReader(Rc::new(move |kind: PrefMediaKind| {
                client.received_layer_snapshot(kind)
            }))
        })
    };

    // Live simulcast/AQ diagnostics reader for the Performance panel's "Live
    // diagnostics" disclosure (#1095). Built once per mount (stable `Rc`s) so the
    // panel prop is `PartialEq`-stable. The effective-setting summary is computed
    // here (same `min(flag, capability)` rule the encoders are constructed with);
    // the SEND/RECEIVE closures read the live encoder atomics / client per-peer
    // state on each poll.
    let diagnostics_reader: DiagnosticsReader = {
        let state = state.clone();
        let client = client.clone();
        use_hook(move || {
            let flag = experimental_simulcast_max_layers();
            let video_capability =
                crate::components::capability_check::capability_max_simulcast_layers();
            let audio_capability = videocall_client::max_layers_for_kind(PrefMediaKind::Audio);
            let summary = SimulcastSummary {
                flag,
                video_capability,
                audio_capability,
                effective_video: flag.min(video_capability),
                effective_audio: flag.min(audio_capability),
            };
            let state_v = state.clone();
            let state_s = state.clone();
            DiagnosticsReader {
                summary,
                // Gate the camera snapshot on the camera being enabled, mirroring
                // the quality needle (and the screen path below). The encoder's
                // `live_simulcast_snapshot()` keys off the STATIC effective layer
                // count and its active-layer/bitrate atomics are not reset on
                // `set_enabled(false)`/`stop`, so without this gate the footer
                // would render stale "N of M layers active" with the camera off.
                send_video: Rc::new(move || {
                    let s = state_v.borrow();
                    if s.prev_video_enabled {
                        Some(s.camera.live_simulcast_snapshot())
                    } else {
                        None
                    }
                }),
                send_screen: Rc::new(move || state_s.borrow().screen.live_simulcast_snapshot()),
                per_peer_receive: Rc::new(move || client.per_peer_received_snapshots()),
            }
        })
    };

    // Publish the reader handle to the parent (attendants) so the Diagnostics
    // panel — a sibling of Host that can't reach the encoders — can read the live
    // SEND simulcast snapshots for its "Simulcast layers" section. The handle is a
    // cheap `Rc`-closure bundle built once per mount; we publish it in a
    // `use_effect` (post-render, so we never write a parent signal mid-render). The
    // effect body has no reactive reads, so it runs once after first render.
    // (#1095 §6 MOVE)
    {
        let reader = diagnostics_reader.clone();
        use_effect(move || {
            if let Some(mut sink) = publish_diagnostics_reader {
                sink.set(Some(reader.clone()));
            }
        });
    }

    // Effective VIDEO/SCREEN simulcast ladder depth for the SEND layer-count
    // sliders. Computed ONCE per Host mount via `use_hook` (not every render):
    // `capability_max_simulcast_layers()` allocates a UA string + emits an info
    // log, and the value is deterministic per session (runtime flag + CPU core
    // count are stable), so a per-render recompute is pure waste. This is the same
    // formula the encoder setup uses (host.rs ~L134); both kinds share the
    // CPU-derived ceiling (each extra video encoder is ~N× main-thread cost).
    let send_layer_max: usize = use_hook(|| {
        experimental_simulcast_max_layers()
            .min(crate::components::capability_check::capability_max_simulcast_layers())
            as usize
    });

    // Effective AUDIO ladder depth for the SEND audio layer-count slider. UNLIKE
    // video/screen this is NOT CPU-clamped: audio Opus encode runs off the main
    // thread and is cheap, so the ceiling is just `min(flag, audio ladder size)`
    // (`max_layers_for_kind(Audio)` == 3). So audio typically shows the full
    // 3-layer ladder even on weak runners that clamp video to 1. Same formula as
    // the mic encoder setup (host.rs ~L146). Computed once per mount.
    let audio_layer_max: usize = use_hook(|| {
        experimental_simulcast_max_layers()
            .min(videocall_client::max_layers_for_kind(PrefMediaKind::Audio)) as usize
    });

    // Bundle the Performance controls into ONE handle and publish it to the parent
    // (attendants) so the Diagnostics drawer — a sibling of `Host` that can't
    // reach the encoders or the preference signals — can mount the
    // `PerformanceSettingsPanel`. Built once per mount via `use_hook` so its
    // closures/readers keep stable `Rc` identity and the handle stays
    // `PartialEq`-stable; the two preference fields are the live `Signal`s (not
    // values), so the drawer's sliders track edits reactively. (#1131 unify)
    let perf_controls: PerfControlsHandle = {
        let on_change = on_performance_change.clone();
        let on_recv = on_receive_change.clone();
        let read_snap = read_quality_snapshot.clone();
        let read_screen_snap = read_screen_snapshot.clone();
        let recv_reader = received_reader.clone();
        let diag_reader = diagnostics_reader.clone();
        use_hook(move || PerfControlsHandle {
            performance_preference,
            receive_preference,
            on_change,
            on_receive_change: on_recv,
            read_snapshot: read_snap,
            read_screen_snapshot: read_screen_snap,
            received_reader: recv_reader,
            diagnostics_reader: diag_reader,
            // Video/screen share the CPU-derived effective ceiling; audio uses its
            // own (non-CPU-clamped) ladder depth. Same values the modal used to
            // forward before the panel moved into the drawer.
            video_layer_max: send_layer_max,
            screen_layer_max: send_layer_max,
            audio_layer_max,
        })
    };
    {
        let handle = perf_controls.clone();
        use_effect(move || {
            if let Some(mut sink) = publish_perf_controls {
                sink.set(Some(handle.clone()));
            }
        });
    }

    // Get device data
    let s = state.borrow();
    let microphones = s.media_devices.audio_inputs.devices();
    let cameras = s.media_devices.video_inputs.devices();
    let speakers = s.media_devices.audio_outputs.devices();
    let selected_microphone_id = s.media_devices.audio_inputs.selected();
    let selected_camera_id = s.media_devices.video_inputs.selected();
    let selected_speaker_id = s.media_devices.audio_outputs.selected();
    drop(s);

    rsx! {
        // Always render the <video> element so Dioxus never destroys it.
        // The camera encoder attaches srcObject via JS; if Dioxus recreates
        // the element on re-render the stream reference is lost (dark square).
        // Dioxus patches individual CSS properties (doesn't replace the whole
        // style attribute), so both branches must set ALL properties explicitly.
        div {
            class: "host-video-wrapper",
            style: if video_enabled {
                "position:relative; width:100%; height:auto; opacity:1; overflow:hidden; pointer-events:auto;"
            } else {
                "position:absolute; width:1px; height:1px; opacity:0; overflow:hidden; pointer-events:none;"
            },
            video { class: "self-camera", autoplay: true, id: VIDEO_ELEMENT_ID, playsinline: "true", muted: true, controls: false }
        }
        // Always-mounted screen share preview — toggled via style so the element
        // exists in the DOM before attach_screen_preview() runs.
        // Positioned AFTER the camera so the preview appears below it.
        video {
            id: "screen-share-preview",
            class: "screen-share-preview",
            style: if share_screen { "display:block;" } else { "display:none;" },
            autoplay: true,
            muted: true,
            playsinline: "true",
            controls: false,
        }
        if !video_enabled {
            div {
                style: "padding:1rem; display:flex; align-items:center; justify-content:center; border-radius: 0; position:relative; width:100%; aspect-ratio:16/9;",
                div { class: "placeholder-content",
                    svg { xmlns: "http://www.w3.org/2000/svg", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                        path { d: "M16 16v1a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2h2m5.66 0H14a2 2 0 0 1 2 2v3.34l1 1L23 7v10" }
                        line { x1: "1", y1: "1", x2: "23", y2: "23" }
                    }
                    span { class: "placeholder-text", "Camera Off" }
                }
            }
        }

        // Device Settings Menu Button
        button {
            class: "device-settings-menu-button btn-apple btn-secondary",
            onclick: move |_| on_device_settings_toggle.call(()),
            title: "Device Settings",
            svg { xmlns: "http://www.w3.org/2000/svg", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                circle { cx: "12", cy: "12", r: "3" }
                 path { d: "M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06-.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1 1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06-.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z" }
             }
        }

        // Device Settings Modal — Audio / Video / Network / Appearance only. The
        // Performance tab moved into the Diagnostics drawer (#1131), so the modal
        // no longer forwards any of the SEND/RECEIVE preference, snapshot-reader,
        // diagnostics, layer-max, or mic-state props; those now flow through the
        // published `PerfControlsHandle` (above) directly into the drawer.
        {
            let on_mic = on_mic_change.clone();
            let on_cam = on_cam_change.clone();
            let on_spk = on_speaker_change.clone();
            rsx! {
                DeviceSettingsModal {
                    key: "{device_settings_generation}",
                    microphones: microphones,
                    cameras: cameras,
                    speakers: speakers,
                    selected_microphone_id: selected_microphone_id,
                    selected_camera_id: selected_camera_id,
                    selected_speaker_id: selected_speaker_id,
                    on_microphone_select: move |d: DeviceInfo| on_mic(d),
                    on_camera_select: move |d: DeviceInfo| on_cam(d),
                    on_speaker_select: move |d: DeviceInfo| on_spk(d),
                    visible: device_settings_open,
                    on_close: move |_| on_device_settings_toggle.call(()),
                    transport_preference: (transport_pref_ctx.0)(),
                    initial_section: device_settings_initial_section.clone(),
                }
            }
        }
    }
}

struct HostState {
    camera: CameraEncoder,
    microphone: Box<dyn MicrophoneEncoderTrait>,
    screen: ScreenEncoder,
    media_devices: MediaDeviceList,
    encoder_settings: EncoderSettings,
    prev_share_screen: bool,
    prev_mic_enabled: bool,
    prev_video_enabled: bool,
    prev_device_settings_open: bool,
    initialized: bool,
    last_reload_counter: u32,
}

fn attach_screen_preview(stream: &MediaStream) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("screen-share-preview"))
    {
        let video: web_sys::HtmlVideoElement = el.unchecked_into();
        // Explicitly set the muted property (not just the HTML attribute) so that
        // Chrome's autoplay policy recognises the element as muted and allows play().
        video.set_muted(true);
        video.set_src_object(Some(stream));
        // Properly await the play() Promise via spawn_local.
        // Dropping the Promise with `let _` causes Chrome to silently abort
        // playback for display-capture streams; Edge is more lenient.
        wasm_bindgen_futures::spawn_local(async move {
            match video.play() {
                Ok(promise) => {
                    if let Err(e) = JsFuture::from(promise).await {
                        log::warn!("Screen preview play() rejected: {:?}", e);
                    }
                }
                Err(e) => log::warn!("Screen preview play() error: {:?}", e),
            }
        });
    }
}

fn detach_screen_preview() {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("screen-share-preview"))
    {
        let video: web_sys::HtmlVideoElement = el.unchecked_into();
        video.set_src_object(None);
    }
}
