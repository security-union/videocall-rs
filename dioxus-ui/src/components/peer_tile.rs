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

use std::cell::RefCell;
use std::rc::Rc;

use crate::components::canvas_generator::{
    generate_for_peer, AudioLevels, PinnedTile, SignalPopupHandlers, TileMode,
};
use crate::components::media_metrics_overlay::{
    next_overlay_fps, overlay_audio_kbps, overlay_audio_kbps_display,
    overlay_audio_kbps_from_status, overlay_painted_fps_sample, parse_resolution,
    MediaMetricsOverlay, MediaMetricsOverlayCtx, ScreenMetricsOverlay,
};
use crate::components::signal_quality::{
    PeerSignalHistory, SampleData, SignalInfo, SignalMeterMode, SignalPopupPosition,
    SignalPopupState,
};
use crate::context::{
    AppearanceSettingsCtx, MeetingTimeCtx, PeerMetadataCtx, PeerSignalHistoryMap, RaisedHandsCtx,
    SignalPopupStateMap, VideoCallClientCtx,
};
use dioxus::prelude::*;
use futures::future::AbortHandle;
use futures::future::Abortable;
use gloo_timers::callback::Timeout;
use videocall_client::adaptive_quality_constants::HEARTBEAT_KEEPALIVE_INTERVAL_MS;
use videocall_client::audio_constants::{MIC_HOLD_DURATION_MS, UI_AUDIO_LEVEL_DELTA};
use videocall_client::decode::peer_decoder::SUBSYSTEM_VIDEO_PAINTED;
use videocall_diagnostics::{
    recv_loop_action, subscribe, DiagEvent, Metric, MetricValue, RecvLoopAction,
};
use wasm_bindgen::JsCast;

#[component]
pub fn PeerTile(
    peer_id: String,
    #[props(default = false)] full_bleed: bool,
    #[props(default)] host_user_id: Option<String>,
    #[props(default)] render_mode: TileMode,
    /// The local user's session_id. Used to identify which tile is the local
    /// user's own. Keyed on session_id, not user_id, so sibling same-user
    /// sessions (HCL issue 828) are not mis-classified as self.
    #[props(default)]
    my_session_id: Option<String>,
    #[props(default)] pinned_peer_id: Option<PinnedTile>,
    #[props(default)] room_id: Option<String>,
    #[props(default = false)] is_current_user_host: bool,
    /// HCL bug #2: scope of the signal-meter popup this tile owns. The
    /// LEFT-panel shared-content tile passes `ScreenOnly` (the popup
    /// surfaces ONLY the screen-share metric). Every other tile leaves
    /// this defaulted to `Full`, which renders whichever series the
    /// sample history actually contains — the legend / tooltip are
    /// already gated on `has_screen_data` (any sample with
    /// `screen_enabled == true`) and per-sample `screen_enabled`, so a
    /// peer who is NOT screen-sharing naturally hides the Screen row
    /// without any mode-level suppression. Keeping the default at
    /// `Full` ensures the peer-tile popup surfaces screen-share metrics
    /// the moment the peer starts sharing, which is the contract the
    /// `peer-screen-diagnostics` E2E spec asserts (a host opens the
    /// guest's peer-tile signal popup and expects the Screen series to
    /// appear once samples arrive). The dedicated LEFT-panel
    /// `ScreenOnly` popup still coexists independently because the
    /// popup-state map keys on `(peer_id, meter_mode)`.
    #[props(default = SignalMeterMode::Full)]
    meter_mode: SignalMeterMode,
    /// Issue #987, task 1a.4: render this tile as an off-budget avatar tile.
    /// When `true`, the adaptive decode-budget controller has excluded the peer
    /// from `active_decode_set`, so the tile shows the avatar/initials
    /// placeholder instead of a live video canvas (no decode pipeline is bound)
    /// while keeping the peer's name, mic state and host controls. Defaults to
    /// `false`, so existing call sites render exactly as before.
    #[props(default = false)]
    force_avatar: bool,
    on_toggle_pin: EventHandler<PinnedTile>,
    /// Issue #1466: fired when the user clicks the per-tile PLAY button on a
    /// decode-budget-PAUSED tile to force-decode this peer. Receives the tile's
    /// `session_id` (the `peer_id`/`key`), which `attendants.rs` toggles into
    /// `UserRequestedDecodeCtx`. Defaulted to a no-op `EventHandler` so the many
    /// call sites that never reach the paused-avatar arm (mock tiles, visible /
    /// camera-off / SS-decoded tiles) need not pass it; only the off-budget
    /// avatar real-peer call sites wire a real handler.
    #[props(default)]
    on_request_decode: EventHandler<String>,
) -> Element {
    let client = use_context::<VideoCallClientCtx>();

    // Issue 2103 follow-up (the #2125 e2e regression): the REACTIVE TRIGGER for
    // this tile's IMPERATIVE peer-identity reads.
    //
    // `peer_uid_for_mute` below and `generate_for_peer` at the bottom resolve
    // this peer's `user_id`, display name and guest flag through non-reactive
    // `VideoCallClient` getters, which subscribe this scope to NOTHING. All
    // three land asynchronously on `PARTICIPANT_JOINED`, which can arrive AFTER
    // the first media packet has already created the peer and mounted this
    // tile. Until issue 2103, the parent's unconditional `MeetingTimeCtx` write
    // dirtied every tile on every parent render and refreshed those reads by
    // ACCIDENT; change-guarding that write removed the accident and left the
    // late-arriving name stranded, so the tile rendered the fallback `user_id`
    // forever. Subscribing to the parent's change-guarded `PeerMetadataCtx`
    // makes the refresh deliberate: this tile is dirtied when the metadata
    // actually moves, and NOT merely because the parent re-rendered.
    //
    // `.read()`, not `()`: subscribing is the whole point, but cloning the
    // snapshot once per tile per render is not. `try_use_context` because the
    // provider is optional — isolated tests mount `PeerTile` without it, the
    // same documented pattern `HostSetCtx` / `RecordingSetCtx` follow inside
    // `generate_for_peer`.
    let peer_metadata_ctx = try_use_context::<PeerMetadataCtx>();
    if let Some(peer_metadata) = peer_metadata_ctx {
        let _ = peer_metadata.0.read();
    }

    // Issue 2135: does THIS peer have a hand up?
    //
    // WHY A MEMO AND NOT A DIRECT READ. `RaisedHandsCtx` is a single room-wide
    // `Signal<Vec<RaisedHand>>`, so ANY hand moving anywhere rewrites it. Reading
    // it directly in this body (or, identically, inside the plain-`fn`
    // `generate_for_peer`, whose context reads bind to THIS scope) subscribes
    // this tile to all of them. That subscription is invisible to the
    // stable-props memoization PR #2125 installed: props are compared only when
    // a PARENT re-renders, whereas a signal write marks every subscribed scope
    // dirty in the runtime directly. So the props gate never even runs, and a
    // single hand wave in a 20-person Q&A re-rendered all N tiles — each one
    // re-running this 1,200-line body plus the 1,155-line `generate_for_peer` —
    // while the machine is already software-decoding up to 30 streams. The
    // decode budget responds to that CPU by shedding tiles, so raising hands
    // could itself trigger video shedding, in exactly the scenario the feature
    // exists for.
    //
    // A `Memo` re-runs its closure on every roster change (an O(hands) scan,
    // which is the cheap part) but only marks ITS subscribers dirty when the
    // OUTPUT changes. The output here is one `bool` about one session, so a
    // toggle dirties exactly the one tile whose own hand moved: N re-renders per
    // toggle becomes 1, flat, regardless of queue depth or position.
    //
    // The queue ORDINAL is deliberately absent. It changes for every peer behind
    // a raiser, so a per-tile memo of it would re-dirty most tiles and hand the
    // fan-out straight back. It lives on the roster row instead — one component
    // rendering all rows, one subscription — and in the banner.
    //
    // Capturing `peer_id` by value is safe because every `PeerTile` call site is
    // keyed on the peer id (`key: "tile-{tile_id}"` / `"ss-active-{peer}"`), so a
    // scope's `peer_id` prop never changes identity for the life of the scope.
    let raised_hands_ctx = try_use_context::<RaisedHandsCtx>();
    let hand_raised = {
        let peer_id = peer_id.clone();
        use_memo(move || {
            raised_hands_ctx
                .as_ref()
                .map(|rh| rh.is_raised(&peer_id))
                .unwrap_or(false)
        })
    };

    let mut audio_enabled = use_signal(|| false);
    let mut video_enabled = use_signal(|| false);
    let mut screen_enabled = use_signal(|| false);
    let mut audio_level = use_signal(|| 0.0_f32);
    // Separate signal for mic icon: holds the last positive value for 1s after
    // audio drops to zero, so the mic stays green briefly after speech ends.
    let mut mic_audio_level = use_signal(|| 0.0_f32);
    // Holds the pending silence timeout so it can be cancelled if new audio arrives.
    let mic_hold_timeout: Rc<RefCell<Option<Timeout>>> = use_hook(|| Rc::new(RefCell::new(None)));
    // Issue 2174: holds the pending glow deadman (see `GLOW_DEADMAN_MS`), which
    // forces the speaking glow dark if every source of speech evidence stops.
    // Owned by the component scope exactly like `mic_hold_timeout`, so unmount
    // drops the `Timeout` and cancels it.
    let glow_deadman: Rc<RefCell<Option<Timeout>>> = use_hook(|| Rc::new(RefCell::new(None)));
    // Issue 2225: wall-clock ms at which the pending deadman above was armed,
    // so the re-arm can be throttled instead of rebuilding a `setTimeout` on
    // every one of the tens of speech events a talking peer emits per second.
    // Same `use_hook`-owned `Rc<RefCell<f64>>` idiom as `last_sample_ts` below;
    // 0.0 means "never armed", which `admit_glow_deadman_rearm` reads as
    // arbitrarily old and therefore always admits.
    let glow_deadman_armed_at: Rc<RefCell<f64>> = use_hook(|| Rc::new(RefCell::new(0.0)));

    // Signal quality tracking: raw metrics from diagnostics events
    let mut fps_received = use_signal(|| 0.0_f64);
    // Issue #1784: PAINTED fps consumed ONLY by the media-metrics overlay's "↓ fps".
    // Sourced from the per-peer `video_painted` diagnostics event the decoder emits
    // at the paint site (frames ACTUALLY drawn to the canvas), NOT from the
    // decode-call `fps_received` bucket above (an arrival count until #2190). Fed in the `video_painted` arm of
    // `handle_diagnostics_event` through the same host-tested `next_overlay_fps`
    // step (issue #1772) that snaps DOWN to 0 the instant painting stops (stopped
    // video → em-dash) and EMA-smooths the residual ±1 fps bucket-boundary jitter.
    // The RAW `fps_received` above is untouched and remains the value the drawer
    // chart / signal popup / health reporter read (a useful network-burstiness
    // signal, deliberately distinct from what the viewer actually sees).
    let mut fps_painted = use_signal(|| 0.0_f64);
    let mut expand_rate = use_signal(|| 0.0_f64);
    let mut video_bitrate = use_signal(|| 0.0_f64);
    // Issue #1769: received-audio nominal kbps for THIS peer, fed from the
    // `peer_status` heartbeat's `audio_enabled` flag (the same authoritative
    // audio-on/off signal that drives the mic icon) using the base received-audio
    // layer nominal. The media-metrics overlay reads this signal directly instead
    // of scanning `per_peer_received_snapshots()` on every render — see the
    // overlay payload below. Also feeds the drawer chart's `SampleData`
    // (previously always 0, since this signal was never populated).
    let mut audio_bitrate = use_signal(|| 0.0_f64);
    let mut audio_buffer_ms = use_signal(|| 0.0_f64);
    let mut screen_fps = use_signal(|| 0.0_f64);
    let mut screen_bitrate = use_signal(|| 0.0_f64);
    let mut latency_ms = use_signal(|| 0.0_f64);
    let mut video_resolution = use_signal(String::new);
    let mut screen_resolution = use_signal(String::new);
    // Publisher's native source resolution for the screen-share track,
    // delivered via the `video_source_resolution` diag event the decoder
    // emits when a `MediaPacket.video_metadata.source_*` field changes.
    // Empty when the publisher is older / doesn't stamp the fields.
    let mut screen_source_resolution = use_signal(String::new);
    // Issue #903: publisher-side encoder state for the screen-share track, // @token-exempt: issue ref, not a color
    // delivered via the `screen_encoder_state` diag event the decoder
    // emits when any of the three values changes. Used by the
    // SignalQualityPopup tooltip to render a `Cause:` sub-line below the
    // Screen row explaining *why* the encoder downscaled in transit.
    // All three default to 0 / empty so older publishers (and the
    // unconstrained-tier path on newer publishers) skip the Cause line.
    let mut screen_encoder_target_bitrate = use_signal(|| 0_u32);
    let mut screen_adaptive_tier = use_signal(String::new);
    let mut screen_cause_hint = use_signal(String::new);
    // Current transport for this peer ("webtransport" / "websocket" /
    // "unknown"), sourced from the `peer_status` diagnostics metric. Stored
    // as a per-tile signal because each `PeerTile` only renders its own
    // peer's badge — no shared map needed. The diagnostics handler guards
    // .set() so the signal updates only when the value actually changes,
    // since `peer_status` fires on every heartbeat — one per peer every
    // HEARTBEAT_KEEPALIVE_INTERVAL_MS (5 s, see
    // `videocall-client/src/connection/connection.rs`), plus an extra
    // emission whenever that peer's media state transitions.
    let mut peer_transport = use_signal(|| None::<String>);
    // Look up or create this peer's signal history in the shared context.
    // The history lives in a context-provided map so it survives PeerTile
    // remounts caused by layout switches (e.g., grid -> split on screen share).
    let mut history_map = use_context::<PeerSignalHistoryMap>();
    let signal_history: Rc<RefCell<PeerSignalHistory>> = {
        let mut map = history_map.write();
        map.entry(peer_id.clone())
            .or_insert_with(|| Rc::new(RefCell::new(PeerSignalHistory::new())))
            .clone()
    };
    // HCL bug #8 + #9: popup open state and drag position are owned by a
    // context-wide map rather than a per-PeerTile signal. When a peer
    // leaves the meeting, that peer's tile remounts under a different
    // sub-tree (the parent re-runs its for-loop / switches between
    // grid and split layouts). With per-tile state every popup on every
    // OTHER peer would also unmount because Dioxus tears down the
    // entire prior tree. The shared map survives the rebuild, so each
    // popup is only torn down when its OWN anchored peer leaves.
    //
    // The map keys on `(peer_id, meter_mode)` so this tile's popup
    // doesn't collide with the same peer's screen-share-only popup
    // (rendered on the shared-content tile in split layout).
    let mut popup_state_map = use_context::<SignalPopupStateMap>();
    let popup_key = (peer_id.clone(), meter_mode);
    let signal_popup_state: Option<SignalPopupState> =
        popup_state_map.read().get(&popup_key).copied();
    let show_signal_popup = signal_popup_state.is_some();
    let signal_popup_free_position = match signal_popup_state {
        Some(SignalPopupState {
            position: SignalPopupPosition::Free { left, top },
        }) => Some((left, top)),
        _ => None,
    };
    let show_tile_menu = use_signal(|| false);

    // Closures for the popup-state map. Each one writes through the
    // shared map so layout switches / peer leaves don't invalidate them.
    let on_toggle_signal_popup: EventHandler<()> = {
        let popup_key = popup_key.clone();
        EventHandler::new(move |_: ()| {
            let mut map = popup_state_map.write();
            if map.contains_key(&popup_key) {
                map.remove(&popup_key);
            } else {
                map.insert(popup_key.clone(), SignalPopupState::default());
            }
        })
    };
    let on_close_signal_popup: EventHandler<()> = {
        let popup_key = popup_key.clone();
        EventHandler::new(move |_: ()| {
            popup_state_map.write().remove(&popup_key);
        })
    };
    let on_signal_popup_drag_commit: EventHandler<(f64, f64)> = {
        let popup_key = popup_key.clone();
        EventHandler::new(move |(left, top): (f64, f64)| {
            let mut map = popup_state_map.write();
            map.insert(
                popup_key.clone(),
                SignalPopupState {
                    position: SignalPopupPosition::Free { left, top },
                },
            );
        })
    };
    let on_signal_popup_reanchor: EventHandler<()> = {
        let popup_key = popup_key.clone();
        EventHandler::new(move |_: ()| {
            let mut map = popup_state_map.write();
            map.insert(
                popup_key.clone(),
                SignalPopupState {
                    position: SignalPopupPosition::Anchored,
                },
            );
        })
    };

    // Counter that increments each time a sample is pushed. Reading this
    // Dioxus Signal triggers re-renders, compensating for the fact that
    // Rc<RefCell<PeerSignalHistory>> is not reactive.
    let mut sample_counter = use_signal(|| 0u32);
    // Track last sample timestamp to throttle to ~1 sample/second
    let last_sample_ts: Rc<RefCell<f64>> = use_hook(|| Rc::new(RefCell::new(0.0)));
    // Issue #906: timestamp (ms since epoch) of the most recent `peer_status` // @token-exempt: issue ref, not a color
    // event seen for this peer. Used to compute the heartbeat-freshness
    // window the screen-state classifier consults when deciding between
    // `Static` and `NoFrames`. Initialised to 0.0; the first event sets
    // it, and the sampling code translates `0.0` into `None` so we don't
    // false-classify an idle session as having a stale heartbeat before any
    // event has arrived.
    let last_peer_status_ts: Rc<RefCell<f64>> = use_hook(|| Rc::new(RefCell::new(0.0)));

    // Initialize from client snapshot and subscribe to diagnostics
    let peer_id_owned = peer_id.clone();
    let effect_client = client.clone();
    let prev_abort_handle = use_hook(|| Rc::new(RefCell::new(None::<AbortHandle>)));
    let mic_hold_for_effect = mic_hold_timeout.clone();
    let glow_deadman_for_effect = glow_deadman.clone();
    let glow_deadman_armed_at_for_effect = glow_deadman_armed_at.clone();
    let last_sample_for_effect = last_sample_ts.clone();
    let last_peer_status_for_effect = last_peer_status_ts.clone();
    let signal_history_for_effect = signal_history.clone();
    // Issue #2190: the diagnostics sampler needs the decode-set predicate
    // (`is_decoding_peer`) to fold the signal-meter enabled flags. Cloned OUT here because
    // the `use_effect` closure below moves its captures while the render body still needs
    // `client`. `VideoCallClient` is `Rc`-backed, so this is a refcount bump.
    let sample_client_for_effect = client.clone();

    use_effect(move || {
        // Abort previous subscription
        if let Some(h) = prev_abort_handle.borrow_mut().take() {
            h.abort();
        }

        // Initialize from client snapshot
        let initial_audio_on = effect_client.is_audio_enabled_for_peer(&peer_id_owned);
        audio_enabled.set(initial_audio_on);
        // The rung is only known from `peer_status`, so seed `None`: one heartbeat
        // of "—" beats a base-rung figure that is wrong for rungs 1 and 2 (#2132).
        audio_bitrate.set(overlay_audio_kbps(initial_audio_on, None));
        video_enabled.set(effect_client.is_video_enabled_for_peer(&peer_id_owned));
        screen_enabled.set(effect_client.is_screen_share_enabled_for_peer(&peer_id_owned));
        // Issue 2224: `audio_level` / `mic_audio_level` are deliberately NOT
        // seeded here. The only snapshot on offer is the client's per-peer
        // audio-level accessor, which resolves through
        // `PeerDecodeManager::peer_audio_level` to
        // `Peer::audio_level` — a field production code assigns nothing but
        // `0.0`. Its only three non-test writes are `Peer::new`, the heartbeat
        // arm of `Peer::decode` (`if !self.is_speaking { .. = 0.0 }`) and
        // `Peer::force_media_off`; the non-zero `self.audio_level = intensity`
        // in `neteq_audio_decoder.rs` belongs to `VadState`, a different struct
        // that is never copied into `Peer`. So
        // the seed could only ever write both signals' `use_signal` default
        // back over themselves, while doing it as an UNGATED write that
        // bypasses the `UI_AUDIO_LEVEL_DELTA` gate in `apply_resolved_level`
        // and the mic's 1 s hold in `update_mic_audio_level` — no upside, and a
        // live-state hazard if this effect ever re-runs inside a scope whose
        // signals are already lit.
        //
        // Both signals therefore start dark and are lit only by
        // `apply_resolved_level`, which is also the only caller of
        // `arm_glow_deadman` — so no glow can exist without a deadman armed to
        // retire it. That is why the alternative (seeding a glow from
        // `is_speaking_for_peer`, which unlike `audio_level` does carry real
        // heartbeat state) was rejected: it would light a tile with no deadman
        // pending, and a peer that crashed right after the mount would stay lit
        // for the rest of the session.

        let peer_id_inner = peer_id_owned.clone();
        let sample_client = sample_client_for_effect.clone();
        let sample_peer_id = peer_id_owned.clone();
        let mic_hold = mic_hold_for_effect.clone();
        let deadman = glow_deadman_for_effect.clone();
        let deadman_armed_at = glow_deadman_armed_at_for_effect.clone();
        let last_sample = last_sample_for_effect.clone();
        let last_peer_status = last_peer_status_for_effect.clone();
        // Clone the Rc for the async block so the outer FnMut closure can be
        // called again without consuming the captured value.
        let signal_hist = signal_history_for_effect.clone();

        // Subscribe to global diagnostics for peer_status updates
        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        *prev_abort_handle.borrow_mut() = Some(abort_handle);

        let fut = async move {
            let mut rx = subscribe();
            loop {
                // Issue 2174: a bare `while let Ok(..)` here died permanently on
                // the first `Overflowed`, which is recoverable — see
                // `videocall_diagnostics::recv_loop_action`. This tile's glow,
                // mic icon and every overlay stat froze at their last value.
                let evt = match rx.recv().await {
                    Ok(evt) => evt,
                    Err(e) => match recv_loop_action(&e) {
                        RecvLoopAction::Continue => continue,
                        RecvLoopAction::Break => break,
                    },
                };
                handle_diagnostics_event(
                    &evt,
                    &peer_id_inner,
                    &mut audio_enabled,
                    &mut video_enabled,
                    &mut screen_enabled,
                    &mut audio_level,
                    &mut mic_audio_level,
                    &mic_hold,
                    &deadman,
                    &deadman_armed_at,
                    &mut fps_received,
                    &mut fps_painted,
                    &mut expand_rate,
                    &mut video_bitrate,
                    &mut audio_bitrate,
                    &mut audio_buffer_ms,
                    &mut screen_fps,
                    &mut screen_bitrate,
                    &mut latency_ms,
                    &mut video_resolution,
                    &mut screen_resolution,
                    &mut screen_source_resolution,
                    &mut screen_encoder_target_bitrate,
                    &mut screen_adaptive_tier,
                    &mut screen_cause_hint,
                    &mut peer_transport,
                    &last_peer_status,
                );
                // Issue 2174: skip the sampler tail for `peer_speaking`. It is
                // by far the highest-rate subsystem on this bus (the decoder
                // VAD emits on every level change > 0.02 while anyone talks),
                // and its handler arm writes only `audio_level` /
                // `mic_audio_level` — neither of which SampleData reads. So the
                // work below (a DOM lookup by id, a canvas measure, ~20 signal
                // peeks) would be pure waste on every speech tick. This matters
                // now in a way it did not before: the overflow fix above revives
                // subscriber loops that the old bug silently killed, so weak
                // devices will actually run this tail again.
                if evt.subsystem == "peer_speaking" {
                    continue;
                }
                // Push a signal quality sample at most once per second,
                // piggybacking on the diagnostics event stream.
                // If resolution is unknown from diagnostics, read it from the
                // canvas element. Skip 300x150 (HTML default before decoder
                // renders the first frame).
                let mut res = video_resolution.peek().clone();
                if res.is_empty() && *video_enabled.peek() {
                    if let Some(canvas) = gloo_utils::document()
                        .get_element_by_id(&peer_id_inner)
                        .and_then(|el| el.dyn_into::<web_sys::HtmlCanvasElement>().ok())
                    {
                        let w = canvas.width();
                        let h = canvas.height();
                        if w > 0 && h > 0 && !(w == 300 && h == 150) {
                            res = format!("{w}x{h}");
                            video_resolution.set(res.clone());
                        }
                    }
                }
                // Issue #906: snapshot the heartbeat age at sample-record  // @token-exempt: issue ref, not a color
                // time so the screen-state classifier can later distinguish
                // a static publisher (fresh heartbeat) from a broken one
                // (stale heartbeat). `0.0` means we have not yet observed
                // a `peer_status` event for this peer — translate to `None`
                // so the classifier doesn't conclude the heartbeat is
                // ancient at start-of-meeting.
                let last_status_ms = *last_peer_status.borrow();
                let peer_status_age_ms = if last_status_ms > 0.0 {
                    Some((js_sys::Date::now() - last_status_ms).max(0.0))
                } else {
                    None
                };
                // Issue #2190: fold the peer's heartbeat flags with whether THIS client is
                // decoding. `is_decoding_peer` reads decode-set membership (`Peer::visible`),
                // not the tile's render mode. The separate pause bit lets the popup omit
                // false zero-valued video/screen points while retaining measured audio.
                let sample_is_decoding = sample_client.is_decoding_peer(&sample_peer_id);
                let sample_decode_paused =
                    !sample_is_decoding && (*video_enabled.peek() || *screen_enabled.peek());
                let (_, sample_video_en, sample_screen_en) =
                    crate::components::signal_quality::signal_enabled_flags(
                        *audio_enabled.peek(),
                        *video_enabled.peek(),
                        *screen_enabled.peek(),
                        sample_is_decoding,
                    );
                let data = SampleData {
                    video_fps: *fps_received.peek(),
                    video_bitrate_kbps: *video_bitrate.peek(),
                    video_resolution: res,
                    audio_bitrate_kbps: *audio_bitrate.peek(),
                    audio_expand_rate: *expand_rate.peek(),
                    audio_buffer_ms: *audio_buffer_ms.peek(),
                    screen_enabled: sample_screen_en,
                    screen_fps: *screen_fps.peek(),
                    screen_bitrate_kbps: *screen_bitrate.peek(),
                    screen_resolution: screen_resolution.peek().clone(),
                    screen_source_resolution: screen_source_resolution.peek().clone(),
                    screen_encoder_target_bitrate_kbps: *screen_encoder_target_bitrate.peek(),
                    screen_adaptive_tier: screen_adaptive_tier.peek().clone(),
                    screen_cause_hint: screen_cause_hint.peek().clone(),
                    peer_status_age_ms,
                    latency_ms: *latency_ms.peek(),
                    audio_enabled: *audio_enabled.peek(),
                    // The folded flags exclude unmeasured streams from scoring.
                    video_enabled: sample_video_en,
                    decode_paused_locally: sample_decode_paused,
                };
                maybe_push_signal_sample(&last_sample, &signal_hist, &data, &mut sample_counter);
            }
        };
        let abortable = Abortable::new(fut, abort_reg);
        spawn(async move {
            let _ = abortable.await;
        });
    });

    let host_uid = host_user_id.as_deref();

    // Re-read signals to trigger reactive re-renders
    let audio_en = audio_enabled();
    let video_en = video_enabled();
    let screen_en = screen_enabled();
    let level = audio_level();
    let mic_level = mic_audio_level();

    // Issue #2190: score the meter on the streams this client is actually DECODING.
    //
    // `is_decoding_peer` reads `Peer::visible` — decode-set membership, the same flag
    // `should_decode_visible_peer` gates decode on — NOT the `force_avatar` render flag: the
    // active screen sharer is force-inserted into the decode set regardless of its ranking
    // bucket, so a ranked-out sharer renders as an avatar tile while genuinely decoding.
    //
    // Load-bearing that this feeds `current_level` and not just the sample: the RENDERED
    // level comes from these arguments, while the sample only carries `*_quality`. Folding
    // the sample alone (the first attempt at this fix) was inert — `video_quality` was
    // already 0.0 from fps 0, so nothing changed and the badge still shipped.
    let is_decoding = client.is_decoding_peer(&peer_id);

    // Read signal history and derive current signal level.
    // Only clone the full sample history when the popup is visible to avoid
    // copying ~3.4 MB/s of data when 20 peers update at ~2 Hz.
    let sig_history = signal_history.borrow();
    let sig_level = rendered_signal_level(&sig_history, audio_en, video_en, screen_en, is_decoding);
    let sig_samples = if show_signal_popup {
        // Reading sample_counter subscribes this component to updates from the
        // diagnostics task, ensuring the chart re-renders when new samples arrive.
        let _ = sample_counter();
        sig_history.samples_vec()
    } else {
        Vec::new()
    };
    drop(sig_history);

    // Only read the transport signal for the POPUP when the popup is visible —
    // avoids subscribing every PeerTile to transport-change re-renders when no
    // popup is even open. The .set() call is already gated on actual
    // change in handle_diagnostics_event, so this is purely a
    // re-render-scope optimization. (The badge below reads the transport
    // unconditionally; see `badge_transport`.)
    let sig_transport = if show_signal_popup {
        peer_transport()
    } else {
        None
    };

    // Issue #1483: per-tile "WT"/"WS" transport badge. Computed on EVERY render
    // (not gated on the popup) so the badge shows whenever the flag is on and
    // the transport is known. Gating order:
    //   1. server-side `transportBadgeEnabled` flag (default OFF). Evaluated
    //      ONCE here per tile render — `transport_badge_enabled()` re-parses the
    //      `__APP_CONFIG` JSON, so hoisting it out of the three render arms in
    //      `generate_for_peer` avoids paying that cost per arm.
    //   2. transport source: map the raw `peer_transport()` signal string,
    //      which is set from the REMOTE `peer_status` diagnostics metric.
    //   3. Only `Some(Wt | Ws)` is threaded down — an `Unknown` (or no transport
    //      yet) collapses to `None`, so the render site never draws a badge for
    //      an unclassified transport. When the flag is off, `badge_transport`
    //      is `None` regardless of transport, so nothing renders.
    //
    // REMOTE-SOURCED here (issue #1883): this signal is fed ONLY by the remote
    // `peer_status` / `peer_transport` diagnostics metric the decode pipeline
    // emits for REMOTE peers (`peer_decode_manager.rs`). Every tile this function
    // renders IS a remote peer — `attendants.rs` filters the local session out of
    // the peer-tile list (`display_peers` excludes `get_own_session_id()`), so
    // `is_self_peer` is only ever true here for a SIBLING SAME-ACCOUNT tab, which
    // is itself a separate remote connection with its OWN announced transport.
    // Sourcing such a sibling tile from the LOCAL transport would be wrong (it is
    // a different connection), so it correctly stays remote-sourced.
    //
    // The local user's OWN self-view is rendered by `Host` (host.rs), NOT a
    // `PeerTile`. Issue #1883 adds the self transport badge THERE, sourced from
    // the public `VideoCallClient::active_transport()` accessor (the client-wide
    // active transport) — mirroring how issue #1768 puts the self media-metrics
    // overlay in `Host` and peer overlays here.
    let badge_transport: Option<crate::components::canvas_generator::TransportBadge> =
        if crate::constants::transport_badge_enabled().unwrap_or(false) {
            use crate::components::canvas_generator::{transport_badge_from_str, TransportBadge};
            let resolved = match peer_transport() {
                Some(raw) => transport_badge_from_str(&raw),
                None => TransportBadge::Unknown,
            };
            // Drop Unknown to None so the render site never draws an
            // unclassified badge (the "Unknown → nothing" half of the gate).
            match resolved {
                TransportBadge::Unknown => None,
                known => Some(known),
            }
        } else {
            // Flag OFF (the default): no badge, no transport read, no extra
            // re-render subscription.
            None
        };

    // Per-peer RECEIVE simulcast diagnostics for THIS peer, for the popup's
    // "Layers" section. Sourced from the SAME `per_peer_received_snapshots`
    // reader the Performance dialog / Diagnostics panel use (via
    // `VideoCallClient`), so the popup and the perf dialog render identical
    // quality dots / metric / reason for the same peer at the same moment.
    //
    // Computed ONLY while the popup is open and re-evaluated on the popup's
    // existing ~1 Hz refresh (`sample_counter` is already read above when
    // `show_signal_popup`), so we add no new high-frequency poll on the hot
    // path. The popup looks itself up by `session_id == peer_id` (the tile key
    // is the peer's session_id rendered via `u64::to_string`), so we parse the
    // id and match here rather than threading every peer's diag down.
    let sig_receive_diag = if show_signal_popup {
        peer_id.parse::<u64>().ok().and_then(|sid| {
            client
                .per_peer_received_snapshots()
                .into_iter()
                .find(|d| d.session_id == sid)
        })
    } else {
        None
    };

    // #1482: this peer's self-reported device/hardware metrics for the popup's
    // compact "Device" line. Resolved the SAME way as `sig_receive_diag`
    // (parse the tile key as session_id, look up live via the client) and only
    // while the popup is open. `None` (unknown / nothing reported) → the popup
    // omits the Device line.
    let sig_device_info = if show_signal_popup {
        peer_id
            .parse::<u64>()
            .ok()
            .and_then(|sid| client.peer_device_info(sid))
    } else {
        None
    };

    let appearance = use_context::<AppearanceSettingsCtx>().0();

    // Only show mute button when: viewer is host, peer is not self, peer is unmuted.
    // `is_self_peer` is true either when the tile's session_id matches the local
    // session_id, OR when the tile's user_id matches the current user's user_id —
    // the latter covers sibling sessions of the same account (e.g. a host with two
    // browser tabs open), so host controls never appear on any of the current
    // user's own tiles.
    let peer_uid_for_mute = client
        .get_peer_user_id(&peer_id)
        .unwrap_or_else(|| peer_id.clone());
    let is_self_peer = my_session_id.as_deref() == Some(peer_id.as_str())
        || peer_uid_for_mute == *client.user_id();
    let on_mute: Option<EventHandler<()>> =
        if is_current_user_host && !is_self_peer && audio_enabled() {
            if let Some(ref meeting_id) = room_id {
                let meeting_id = meeting_id.clone();
                let peer_uid = peer_uid_for_mute.clone();
                Some(EventHandler::new(move |_: ()| {
                    let meeting_id = meeting_id.clone();
                    let peer_uid = peer_uid.clone();
                    let mut audio_enabled = audio_enabled;
                    audio_enabled.set(false);
                    spawn(async move {
                        match crate::constants::meeting_api_client() {
                            Ok(api_client) => {
                                if let Err(e) =
                                    api_client.mute_participant(&meeting_id, &peer_uid).await
                                {
                                    log::warn!("mute_participant failed: {e}");
                                    audio_enabled.set(true);
                                }
                            }
                            Err(e) => {
                                log::warn!("meeting_api_client error: {e}");
                                audio_enabled.set(true);
                            }
                        }
                    });
                }))
            } else {
                None
            }
        } else {
            None
        };
    // Only show disable-video button when: viewer is host, peer is not self, peer's camera is on.
    let on_disable_video: Option<EventHandler<()>> =
        if is_current_user_host && !is_self_peer && video_enabled() {
            if let Some(ref meeting_id) = room_id {
                let meeting_id = meeting_id.clone();
                let peer_uid = peer_uid_for_mute.clone();
                Some(EventHandler::new(move |_: ()| {
                    let meeting_id = meeting_id.clone();
                    let peer_uid = peer_uid.clone();
                    let mut video_enabled = video_enabled;
                    video_enabled.set(false);
                    spawn(async move {
                        match crate::constants::meeting_api_client() {
                            Ok(api_client) => {
                                if let Err(e) = api_client
                                    .disable_video_participant(&meeting_id, &peer_uid)
                                    .await
                                {
                                    log::warn!("disable_video_participant failed: {e}");
                                    video_enabled.set(true);
                                }
                            }
                            Err(e) => {
                                log::warn!("meeting_api_client error: {e}");
                                video_enabled.set(true);
                            }
                        }
                    });
                }))
            } else {
                None
            }
        } else {
            None
        };
    // Show kick button when: viewer is host, peer is not self (no media state check).
    let on_kick: Option<EventHandler<()>> = if is_current_user_host && !is_self_peer {
        if let Some(ref meeting_id) = room_id {
            let meeting_id = meeting_id.clone();
            let peer_uid = peer_uid_for_mute.clone();
            Some(EventHandler::new(move |_: ()| {
                let meeting_id = meeting_id.clone();
                let peer_uid = peer_uid.clone();
                spawn(async move {
                    match crate::constants::meeting_api_client() {
                        Ok(api_client) => {
                            if let Err(e) =
                                api_client.kick_participant(&meeting_id, &peer_uid).await
                            {
                                log::warn!("kick_participant failed: {e}");
                            }
                        }
                        Err(e) => log::warn!("meeting_api_client error: {e}"),
                    }
                });
            }))
        } else {
            None
        }
    } else {
        None
    };

    let peer_is_guest = client.get_peer_is_guest(&peer_id).unwrap_or(false);

    // "Transfer host": hand off and step down. Any admitted non-guest peer.
    let on_transfer_host: Option<EventHandler<()>> = if is_current_user_host
        && !is_self_peer
        && !peer_is_guest
    {
        if let Some(ref meeting_id) = room_id {
            let meeting_id = meeting_id.clone();
            let peer_uid = peer_uid_for_mute.clone();
            Some(EventHandler::new(move |_: ()| {
                let meeting_id = meeting_id.clone();
                let peer_uid = peer_uid.clone();
                spawn(async move {
                    match crate::constants::meeting_api_client() {
                        Ok(api_client) => {
                            if let Err(e) = api_client.transfer_host(&meeting_id, &peer_uid).await {
                                log::warn!("transfer_host failed: {e}");
                            }
                        }
                        Err(e) => log::warn!("meeting_api_client error: {e}"),
                    }
                });
            }))
        } else {
            None
        }
    } else {
        None
    };
    // Issue 1768: per-tile media-metrics overlay payload. Computed ONLY when the
    // diagnostics "Show media metrics on tiles" checkbox is on (default off).
    // COST: with the checkbox OFF a tile pays nothing — only the enabled flag
    // (which rarely changes) is read, so none of the per-metric signals are
    // subscribed. With it ON, the payload is rebuilt on EVERY render of this tile
    // (the component reads `audio_level()` / `mic_audio_level()` unconditionally,
    // so a SPEAKING tile re-renders at several Hz), but each rebuild is now just
    // O(1) reads of per-tile signals this component already maintains at the ~1 Hz
    // diagnostics cadence — decoded resolution, the smoothed received fps (#1772),
    // and the received-audio kbps signal (#1769). There is NO per-render
    // `per_peer_received_snapshots()` scan any more, so the payload build is O(1)
    // per tile instead of the previous O(N²)-across-N-speaking-tiles snapshot walk.
    let overlay_enabled = try_use_context::<MediaMetricsOverlayCtx>()
        .map(|c| (c.0)())
        .unwrap_or(false);
    let metrics_overlay: Option<MediaMetricsOverlay> = if overlay_enabled {
        // A grid PeerTile is always a REMOTE peer: the local user's own self-view
        // is rendered by `Host` (which filters the local session out of the grid),
        // and the self SENDING overlay is rendered there. So this branch only ever
        // builds the RECEIVED overlay; guard on self defensively in case a future
        // layout ever renders the local session as a tile.
        let is_self = my_session_id.as_deref() == Some(peer_id.as_str());
        if is_self {
            None
        } else {
            // Every field is a live per-tile signal read (O(1)), gated by
            // `overlay_enabled` so nothing is subscribed while the checkbox is off:
            //   * resolution — decoded WxH from `video_resolution`;
            //   * fps — the #1784 PAINTED rate (`fps_painted`): frames actually
            //     drawn to the canvas, NOT the decode-call `fps_received` bucket,
            //     so the readout matches what the viewer sees (capped at the source
            //     rate once #1783 coalesces late-frame bursts to one draw);
            //   * audio kbps — the #1769 `audio_bitrate` signal, driven off the
            //     `peer_status` heartbeat's audio-on flag (the same signal as the
            //     mic icon). This replaces the former per-render
            //     `per_peer_received_snapshots()` scan; the snapshot path still
            //     backs the diagnostics drawer / signal popup, it is just no longer
            //     walked here.
            let resolution = parse_resolution(&video_resolution());
            let fps_now = fps_painted();
            let audio_kbps = audio_bitrate();
            Some(MediaMetricsOverlay {
                is_self: false,
                resolution,
                fps: (fps_now > 0.0).then_some(fps_now),
                // Em-dash fallback for a genuinely-absent value (audio off, or no
                // rung arriving → signal is 0).
                audio_kbps: overlay_audio_kbps_display(audio_kbps),
            })
        }
    } else {
        None
    };

    // Issue 1821: shared-content tile stats. Only the ScreenOnly sharer tile
    // renders the received screen share, so build these only for it (reading the
    // screen signals subscribes THIS tile to resolution/fps changes — desired for
    // the sharer tile, avoided for every other tile). `screen_resolution` is
    // populated ALWAYS (the actual-size 1:1 control re-derives its target live off
    // presenter-resolution changes, independent of the diagnostics checkbox); the
    // overlay payload is gated by the SAME checkbox as the camera overlay.
    let is_screen_only_tile = matches!(render_mode, TileMode::ScreenOnly);
    let screen_resolution_now: Option<(u32, u32)> = if is_screen_only_tile {
        parse_resolution(&screen_resolution())
    } else {
        None
    };
    let screen_metrics_overlay: Option<ScreenMetricsOverlay> =
        if is_screen_only_tile && overlay_enabled {
            let fps_now = screen_fps();
            Some(ScreenMetricsOverlay {
                resolution: screen_resolution_now,
                fps: (fps_now > 0.0).then_some(fps_now),
            })
        } else {
            None
        };

    generate_for_peer(
        &client,
        &peer_id,
        full_bleed,
        AudioLevels {
            raw: level,
            mic: mic_level,
        },
        host_uid,
        render_mode,
        my_session_id.as_deref(),
        SignalInfo {
            level: sig_level,
            decode_paused_locally: sig_level.is_unmeasured(),
            history: sig_samples,
            meeting_start_ms: {
                let mt = use_context::<MeetingTimeCtx>();
                mt().meeting_start_time.unwrap_or_else(js_sys::Date::now)
            },
            transport: sig_transport,
            meter_mode,
            receive_diag: sig_receive_diag,
            device_info: sig_device_info,
            badge_transport,
            metrics_overlay,
            screen_resolution: screen_resolution_now,
            screen_metrics_overlay,
        },
        SignalPopupHandlers {
            show: show_signal_popup,
            free_position: signal_popup_free_position,
            on_toggle: on_toggle_signal_popup,
            on_close: on_close_signal_popup,
            on_drag_commit: on_signal_popup_drag_commit,
            on_reanchor: on_signal_popup_reanchor,
        },
        show_tile_menu,
        on_mute,
        on_disable_video,
        on_kick,
        on_transfer_host,
        pinned_peer_id.as_ref(),
        on_toggle_pin,
        &appearance,
        on_request_decode,
        force_avatar,
        // Reading the memo HERE (not the roster) is what keeps the subscription
        // narrow: this scope depends on one `bool`, not on the whole roster.
        hand_raised(),
    )
}

/// Glow level used when the only evidence a peer is speaking is the heartbeat
/// boolean — i.e. the sender's VAD says "talking" but no real intensity is
/// available (issue 2174).
///
/// Deliberately MODEST rather than full-scale. The heartbeat rides the
/// reliable control stream while decoded audio rides datagrams, so on a lossy
/// link the boolean can keep arriving long after the audio itself has stopped
/// being decodable. Lighting such a peer at 1.0 would render an inaudible
/// participant as the brightest, most confident speaker on screen for the
/// duration of the outage. A mid-scale value reads as "probably talking"
/// without out-shouting peers whose glow is backed by a measured level.
const HEARTBEAT_SOURCED_GLOW_LEVEL: f32 = 0.5;

/// The level at or below which the tile counts as **dark** — i.e. as showing
/// nothing the decoder-VAD fast path is still steering (issue 2224).
///
/// This is [`UI_AUDIO_LEVEL_DELTA`], the same gate [`apply_resolved_level`] and
/// [`update_mic_audio_level`] write through, for two independent reasons that
/// land on the same number:
///
/// * **Nothing in that band is a live, graded glow.** It sits *below the
///   resolution of the pipeline that produced it*: a fast-path event whose
///   level lands within `UI_AUDIO_LEVEL_DELTA` of what the signal already holds
///   is dropped by the write gate, so a `current` down there is a residue its
///   producer can no longer nudge — only replace wholesale. Rule 3's "leave the
///   finely-graded value to its owner" has no owner left to defer to.
/// * **Nothing in that band is visually graded either.** Where the level
///   enters `calculate_glow_params` (`canvas_generator.rs`) at all, it enters
///   as `BASE + level * K`, before per-setting scaling and clamping — so a
///   level of `0.01` contributes **at most 1 %** of the level-driven range of
///   any term it appears in, and less wherever the clamp bites (at the default
///   brightness the border alpha is pinned at its `1.0` ceiling for every
///   level). `inner_spread` is a hardcoded `0.0` and carries no level term at
///   all. What renders is the floor of the ramp, nothing like the mid-scale
///   [`HEARTBEAT_SOURCED_GLOW_LEVEL`] a heartbeat-only speaker is meant to
///   get.
///
/// The old boundary was exact zero, which let a `current` of e.g. `0.008` pin a
/// peer whose fast path had died at the bottom of the ramp for as long as they
/// kept talking, with [`refreshes_glow_deadman`] re-arming on every heartbeat so
/// the timeout could not retire it either.
///
/// That residue is producible — the sequence has to clear TWO gates, the
/// producer's and this crate's:
///
/// 1. `0.0 -> 0.05`, emitted on the VAD's `false -> true` speaking toggle
///    (`VadState::observe` broadcasts on a boolean flip whatever the delta is),
///    and written here because `0.05 > UI_AUDIO_LEVEL_DELTA`.
/// 2. `0.05 -> 0.008`, a fade-out: the producer emits it because
///    `|0.008 - 0.05| = 0.042` clears its own `AUDIO_LEVEL_DELTA_THRESHOLD`
///    (0.02), and this crate writes it because `0.042` clears
///    `UI_AUDIO_LEVEL_DELTA` (0.01). The signal parks on `0.008`.
///
/// The physical precondition is narrow but real: `rms_to_intensity` is a `sqrt`
/// curve, so an intensity of `0.008` needs `rms ~= 0.0200051` at the deployed
/// `vadThreshold = 0.02` — a fade-out that stalls within ~5 micro of the
/// threshold. (`GLOW_DARK_CEILING` itself sits at `rms = 0.020008`.) Then the
/// fast path dies with the signal parked there.
const GLOW_DARK_CEILING: f32 = UI_AUDIO_LEVEL_DELTA;

/// Is the tile showing a glow the decoder-VAD fast path is still steering?
///
/// The resolver's rule 3 and [`refreshes_glow_deadman`] must answer this the
/// same way or they disagree about what the tile is doing: a `current` the
/// resolver treats as dark (and therefore raises) while the deadman treats as
/// lit re-arms a timer guarding a value nobody owns. Both call this, so the
/// boundary cannot drift between them.
fn holds_live_glow(current: f32) -> bool {
    current > GLOW_DARK_CEILING
}

/// Would writing `lvl` over `prev` actually reach the `audio_level` signal?
///
/// The change gate [`apply_resolved_level`] writes through, extracted so the
/// coupling between it and [`GLOW_DARK_CEILING`] can be asserted on the real
/// predicate rather than on a copy of the comparison.
///
/// Two clauses, and the first is not redundant: a drop to exactly `0.0` must
/// always land, even from a `prev` inside the delta, or a tile could be left
/// lit by a level too small to clear the gate on its way down.
fn glow_write_reaches_signal(lvl: f32, prev: f32) -> bool {
    (lvl == 0.0 && prev != 0.0) || (lvl - prev).abs() > UI_AUDIO_LEVEL_DELTA
}

/// Resolve the glow level for a peer from the `audio_level` float, the
/// speaking boolean, and the level the tile is *currently* showing.
///
/// # Semantics (issue 2174)
///
/// The old rule was "float first, boolean as a fallback", which made the
/// boolean *unreachable* on the heartbeat path: `broadcast_peer_status`
/// (`videocall-client/src/decode/peer_decode_manager.rs`) always emits an
/// `audio_level` metric, and the `Peer::audio_level` field backing it is only
/// ever assigned `0.0` — initialised to `0.0` and reset to `0.0` on a
/// not-speaking heartbeat, with no production write of a non-zero value. So a
/// heartbeat for a *talking* peer resolved to `Some(0.0)`, and its genuinely
/// fresh `is_speaking` flag (the sender's own ~50 ms encoder VAD, relayed in
/// the heartbeat packet) was discarded.
///
/// `None` means **leave the signal alone** — it is not "no glow".
///
/// The rules, in order:
///
/// 1. `speaking == Some(false)` → `Some(0.0)`, whatever the float says. An
///    explicit "not speaking" from the sender is authoritative silence, so a
///    stale or hardcoded float can never hold a glow lit. It is safe to treat
///    it as authoritative because `Connection::set_speaking` fires an
///    edge-triggered heartbeat on every flip, sent on the reliable ordered
///    control stream rather than the expendable keepalive datagram.
/// 2. A float `> 0.0` → that float. This is the decoder-VAD fast path's real
///    measured intensity and always wins over the boolean. `rms_to_intensity`
///    returns exactly `0.0` below the VAD threshold and strictly positive
///    above it, and the fast path only reports `speaking = true` when
///    `rms > threshold`, so a genuine fast-path speaking event always lands
///    here — unchanged from before.
/// 3. `speaking == Some(true)` with no usable float (absent, or the
///    producer's dead `0.0`) splits on what the tile is already showing, as
///    decided by [`holds_live_glow`]:
///    - **already glowing** (`current > `[`GLOW_DARK_CEILING`]) → `None`. The
///      5 s heartbeat must not overwrite a live, finely-graded glow: yanking a
///      quiet talker's ~0.25 up to a constant produced a visible pulse-to-full
///      once per heartbeat. Leaving it untouched lets the fast path keep owning
///      the value.
///    - **dark** (`current <= `[`GLOW_DARK_CEILING`]) →
///      [`HEARTBEAT_SOURCED_GLOW_LEVEL`]. This is the light-from-dark case the
///      fix exists for: the fast path is silent (dead, stalled, or its audio
///      never arrived) and the heartbeat is the only evidence this peer is
///      talking. Issue 2224: the boundary is the write-gate delta, not exact
///      zero, because a strictly-positive level below that gate is a stale
///      residue rather than a glow anyone still owns — see
///      [`GLOW_DARK_CEILING`].
/// 4. No boolean → the float unchanged, or `None` when neither is present.
fn resolve_audio_level(
    audio_lvl: Option<f32>,
    speaking: Option<bool>,
    current: f32,
) -> Option<f32> {
    if speaking == Some(false) {
        return Some(0.0);
    }
    // A strictly positive float is a real measurement — always preferred.
    if audio_lvl.is_some_and(|lvl| lvl > 0.0) {
        return audio_lvl;
    }
    match speaking {
        Some(true) => {
            if holds_live_glow(current) {
                None
            } else {
                Some(HEARTBEAT_SOURCED_GLOW_LEVEL)
            }
        }
        // No boolean at all: keep the float's pass-through semantics.
        _ => audio_lvl,
    }
}

/// Resolve the glow level for a `peer_status` heartbeat, which — unlike
/// `peer_speaking` — also carries the peer's `audio_enabled` flag.
///
/// A heartbeat claiming `audio_enabled = 0` while `is_speaking = 1` is
/// self-contradictory: it would render the tile with a muted mic icon AND a
/// lit speaking glow. An honest client cannot produce it, because
/// `set_enabled(false)` clears the speaking flag before the mute edge is sent.
/// A peer that sends it anyway is either buggy or hand-crafting packets to
/// impersonate an active speaker, so the mute claim wins and the glow goes
/// dark — the conservative reading either way.
fn effective_level(
    audio_enabled: Option<bool>,
    audio_lvl: Option<f32>,
    speaking: Option<bool>,
    current: f32,
) -> Option<f32> {
    if audio_enabled == Some(false) {
        return Some(0.0);
    }
    resolve_audio_level(audio_lvl, speaking, current)
}

/// Parse a `peer_speaking` event and resolve the glow level it should produce
/// for `peer_id`, vetoed by the tile's live `audio_enabled` state.
///
/// Returns `None` when the event belongs to another peer (nothing to apply);
/// otherwise the parsed speaking claim and the resolved level, both handed
/// straight to [`apply_resolved_level`].
///
/// Issue 2174 follow-up: this arm used to call [`resolve_audio_level`]
/// directly, skipping the [`effective_level`] mute veto that the `peer_status`
/// arm applies. `peer_speaking` carries the decoder's raw VAD result and has no
/// `audio_enabled` field of its own, so a straggler event could re-light a full
/// glow on a peer the host had just muted: `handle_pcm_data` in
/// `neteq_audio_decoder.rs` runs on DECODED PCM, so audio already inside the
/// decoder when the mute landed still surfaced `speaking: 1` with a real
/// positive level afterwards — and rule 2 above ("a positive float always
/// wins") let it straight through.
///
/// That specific straggler is now also closed at the producer, in this same
/// change: `VadState::observe` early-returns while `suppressed`, which
/// `set_muted(true)` sets. So this veto is no longer the only guard — but it
/// remains load-bearing on a path the producer gate cannot cover. That gate
/// keys off the LOCAL decoder being muted, whereas this tile's `audio_enabled`
/// legitimately leads it: the host's own force-mute writes the signal
/// optimistically on click (see `on_mute` above), before the peer has been
/// muted anywhere, so genuine `speaking: 1` events keep arriving from a
/// perfectly unsuppressed VAD for the whole round trip. Only this veto keeps
/// the glow consistent with the mic glyph that same click just changed.
///
/// What the veto guarantees, precisely: a speaker whose `audio_enabled` is
/// KNOWN `true` can never be silenced by it. A `speaking: 1` event only exists
/// if an audio frame reached the decoder, and the AUDIO arm of
/// `peer_decode_manager.rs` guards that call: with no heartbeat seen yet it
/// sets `audio_enabled = true` and calls `broadcast_peer_status()` *before*
/// decoding, and once a heartbeat has reported mute it returns `SKIPPED`
/// without decoding at all. That status event and the speaking event travel the
/// same ordered `global_sender()` bus this tile drains in FIFO order, so the
/// `peer_status` arm has already set the signal `true` by the time the first
/// `peer_speaking` for a talking peer arrives.
///
/// It does NOT guarantee that every real speaker stays lit, because this tile's
/// `audio_enabled` is a `Signal<bool>` with no unknown state and its mount seed
/// fails closed: `is_audio_enabled_for_peer` returns `false` both for a peer
/// absent from the decode manager AND when `inner.try_borrow()` loses a race
/// during the effect flush. Either collapses unknown to muted and keeps the
/// glow dark until the next `peer_status` corrects it — bounded by one
/// keepalive, self-healing, and self-consistent, since the mic glyph and the
/// audio-kbps readout are seeded from that same `false` and already render the
/// peer as muted for that window. The tri-state alternative is the roster's
/// (see `resolve_roster_speaking` in `peer_list.rs`, which fails OPEN); the
/// tile deliberately fails closed rather than show a glow beside a muted mic.
///
/// The optimistic-mute window above cuts both ways, which is worth naming: if
/// the mute never takes — a hostile client that ignores it and keeps sending
/// audio — the host sees a dark glow on a peer they can still hear, until the
/// corrective heartbeat restores `audio_enabled` and the fast path re-lights
/// it. The direction is the fail-safe one (it understates activity rather than
/// faking it), it is bounded by one keepalive, and the alternative — trusting
/// the raw VAD over the host's own mute — is what this fix exists to stop.
fn speaking_event_resolution(
    metrics: &[Metric],
    peer_id: &str,
    audio_enabled: bool,
    current: f32,
) -> Option<(Option<bool>, Option<f32>)> {
    let mut to_peer: Option<&str> = None;
    let mut audio_lvl: Option<f32> = None;
    let mut speaking: Option<bool> = None;
    for m in metrics {
        match (m.name, &m.value) {
            ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.as_ref()),
            ("audio_level", MetricValue::F64(v)) => audio_lvl = Some(*v as f32),
            ("speaking", MetricValue::U64(v)) => speaking = Some(*v != 0),
            _ => {}
        }
    }
    if to_peer != Some(peer_id) {
        return None;
    }
    Some((
        speaking,
        effective_level(Some(audio_enabled), audio_lvl, speaking, current),
    ))
}

/// Should this event refresh the glow deadman (see [`GLOW_DEADMAN_MS`])?
///
/// The deadman asks "have I seen evidence of ongoing speech recently?", which
/// is broader than "did the level change?". Two things count as evidence: a
/// positive level actually resolved, or the resolver deliberately declining to
/// touch a live glow because the sender still reports speech (rule 3 above).
/// Without that second clause a peer whose level sits stable would have its
/// glow zeroed mid-sentence.
///
/// The second clause tests the SAME boundary rule 3 branches on, via
/// [`holds_live_glow`] — issue 2224. When the two disagreed, a `current` the
/// resolver refused to raise (it read as "already glowing") was also read here
/// as a live glow worth guarding, so a 5 s heartbeat re-armed the 12.5 s
/// timeout indefinitely and nothing could retire the stale value.
fn refreshes_glow_deadman(resolved: Option<f32>, speaking: Option<bool>, current: f32) -> bool {
    match resolved {
        Some(lvl) => lvl > 0.0,
        None => speaking == Some(true) && holds_live_glow(current),
    }
}

/// How long a lit glow may survive with no further evidence of speech before
/// the deadman forces it dark (issue 2174).
///
/// Derived as 2.5x the peer heartbeat so it is immune to ordinary jitter: a
/// healthy speaking peer refreshes it every `HEARTBEAT_KEEPALIVE_INTERVAL_MS`
/// via `is_speaking`, so two consecutive keepalives (plus half a period of
/// slack) must ALL fail to arrive before it fires. Anything tighter would
/// blink the glow on a single dropped datagram.
///
/// This is the last-resort guard for the case no resolver rule can reach: a
/// peer that crashes, is force-quit, or simply stops emitting — every event
/// stops, so nothing is left to drive the glow dark and it would otherwise
/// stay lit at its last value for the rest of the session. It is deliberately
/// a plain timeout rather than a consecutive-success counter, which under
/// ongoing contention can wedge a healthy peer indefinitely.
const GLOW_DEADMAN_MS: u32 = glow_deadman_ms(HEARTBEAT_KEEPALIVE_INTERVAL_MS);

/// The 2.5x rule behind [`GLOW_DEADMAN_MS`], as a function of the heartbeat
/// period so the relationship can be exercised directly.
const fn glow_deadman_ms(keepalive_ms: u32) -> u32 {
    keepalive_ms * 5 / 2
}

/// How long after a successful arm the deadman re-arm is suppressed (issue
/// 2225).
///
/// [`arm_glow_deadman`] allocates a boxed `Closure`, a wasm-bindgen heap slot
/// and a `setTimeout`, and cancels the previous one by dropping it — on EVERY
/// event that counts as evidence of speech. That is emphatically not a
/// per-heartbeat rate. The NeteQ worker delivers PCM at 100 Hz (see the
/// `RMS_SCRATCH` note in `neteq_audio_decoder.rs`), and `VadState::observe`
/// re-broadcasts `peer_speaking` whenever the intensity moves further than
/// `AUDIO_LEVEL_DELTA_THRESHOLD` (0.02). That bar is low, because
/// `rms_to_intensity` is a sqrt curve: `linear = intensity^2`, so near
/// mid-scale a 0.02 step in intensity is only about 2 % of the RMS range
/// (`d(linear)/d(intensity) = 2 * intensity`) — a swing ordinary speech crosses
/// many times a second. Tens of events per second per speaking peer is the
/// realistic figure. Throttling the *re-creation* of an already-pending timer
/// collapses that to at most one create/destroy pair per throttle period.
///
/// It changes only how OFTEN the timer is rebuilt, never WHICH events count:
/// [`refreshes_glow_deadman`] remains the sole gate on that, and a suppressed
/// re-arm by construction leaves a deadman pending (see
/// [`admit_glow_deadman_rearm`]), so the invariant that no lit glow exists
/// without an armed deadman is untouched.
const GLOW_DEADMAN_REARM_THROTTLE_MS: u32 =
    glow_deadman_rearm_throttle_ms(HEARTBEAT_KEEPALIVE_INTERVAL_MS);

/// The throttle period as a fraction of the heartbeat, for the same reason
/// [`glow_deadman_ms`] is one: the budget it spends is denominated in
/// keepalives.
///
/// A suppressed re-arm leaves the OLDER timer running, so the deadman fires
/// early by however long ago that timer was armed. Writing `D` for
/// [`GLOW_DEADMAN_MS`] and `T` for this throttle: the pending timer was armed
/// at `t_arm` and fires at `t_arm + D`; any qualifying event at `t` with
/// `t - t_arm >= T` re-arms, so the LAST qualifying event before silence,
/// `t_last`, satisfies `t_last - t_arm < T`. The fire therefore lands in
/// `(t_last + D - T, t_last + D]` — the effective silence-to-dark window.
///
/// At the shipped 5 000 ms keepalive: `D = 12 500`, `T = 1 000`, so the window
/// is `(11 500, 12 500]` ms. The floor it must clear is two keepalives =
/// 10 000 ms — the case [`GLOW_DEADMAN_MS`] is documented against, a peer whose
/// fast path dies but whose heartbeats keep arriving must not be darkened while
/// it is still speaking. 11 500 > 10 000, margin 1 500 ms.
///
/// The 1/5 factor makes that structural rather than incidental:
/// `D - T = 2.5k - 0.2k = 2.3k > 2k` for every keepalive `k`, so the floor
/// clears by 0.3 of a keepalive at any heartbeat period — see
/// [`glow_deadman_floor_ms`] and the test that exercises it. A coarser throttle
/// buys little (the churn is already cut by one to two orders of magnitude at
/// 1 Hz) and eats the margin: 1/2 of a keepalive would leave 250 ms.
const fn glow_deadman_rearm_throttle_ms(keepalive_ms: u32) -> u32 {
    keepalive_ms / 5
}

/// Lower bound of the throttled deadman's effective window, measured from the
/// last piece of speech evidence — `D - T` from the derivation above.
///
/// A `const fn` of the keepalive so the "still clears two missed keepalives"
/// property can be asserted on the relationship between the real constants
/// rather than on a restated literal.
const fn glow_deadman_floor_ms(keepalive_ms: u32) -> u32 {
    glow_deadman_ms(keepalive_ms) - glow_deadman_rearm_throttle_ms(keepalive_ms)
}

/// The floor claim, as a build-time invariant rather than a comment.
///
/// The throttle spends part of the deadman's slack, and the one thing it must
/// never spend is the two-keepalive guarantee [`GLOW_DEADMAN_MS`] exists to
/// give — a peer whose fast path dies but whose heartbeats keep arriving
/// refreshes the deadman once per keepalive and must not be darkened while it
/// is still speaking. Any future edit to either `const fn` that eats that
/// margin fails the build here instead of silently shortening the window in
/// production. (The unit test covers the same relationship across a range of
/// heartbeat periods, and pins that the throttle actually shortens anything.)
const _: () = assert!(
    glow_deadman_floor_ms(HEARTBEAT_KEEPALIVE_INTERVAL_MS) > 2 * HEARTBEAT_KEEPALIVE_INTERVAL_MS,
    "the throttled glow deadman must still outlast two missed keepalives"
);

/// Should [`arm_glow_deadman`] actually rebuild the timer — and if so, record
/// that it did.
///
/// `armed_at_ms` is the wall-clock time the pending deadman was created, and is
/// advanced to `now_ms` on every admitted arm, so the decision and its
/// bookkeeping are a single step a test can drive directly.
///
/// Two conditions must BOTH hold to suppress a re-arm:
///
/// 1. **A timer is actually pending.** Not an optimisation — the correctness
///    half. [`apply_resolved_level`] explicitly `take()`s the deadman when a
///    level resolves to zero, so a glow that goes dark and re-lights inside one
///    throttle period would otherwise come back with NO deadman behind it,
///    which is the stuck-glow defect of issue 2174.
/// 2. **The pending timer is younger than the throttle.** This is what bounds
///    how early the fire can land (see [`glow_deadman_rearm_throttle_ms`]).
///
/// The age test is a half-open *interval* rather than `elapsed < throttle`
/// because `js_sys::Date::now()` is a wall clock. An NTP step backwards makes
/// `elapsed` negative, and a bare `<` would then suppress every re-arm for the
/// whole size of the step; the pending `setTimeout` does not follow the wall
/// clock, so it would still fire on schedule and leave a talking peer lit with
/// no deadman until the clock caught up. Treating any out-of-range delta as
/// "re-arm" fails in the direction that costs only a timer allocation.
fn admit_glow_deadman_rearm(armed_at_ms: &mut f64, now_ms: f64, timer_pending: bool) -> bool {
    if timer_pending {
        let elapsed = now_ms - *armed_at_ms;
        if (0.0..f64::from(GLOW_DEADMAN_REARM_THROTTLE_MS)).contains(&elapsed) {
            return false;
        }
    }
    *armed_at_ms = now_ms;
    true
}

/// What a deadman fire must drive dark, given the tile's current levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GlowDeadmanAction {
    /// Zero the speaking glow (`audio_level`).
    clear_glow: bool,
    /// Hand the mic indicator a zero level so its hold runs out.
    clear_mic: bool,
}

/// Decide what a deadman fire must clear.
///
/// The speaking glow (tile border) and the mic icon are driven from the same
/// speech evidence, so they must go dark together — a peer that crashes
/// mid-sentence otherwise keeps a green mic forever even once the border
/// clears. Each is gated on being non-zero so a fire never dirties a signal
/// that is already dark.
///
/// Pure so the crashed-peer scenario is testable: the deadman body itself is a
/// JS `Timeout` closure over Dioxus signals, neither of which exists on the
/// native `--lib` test target.
fn glow_deadman_action(current_glow: f32, current_mic: f32) -> GlowDeadmanAction {
    GlowDeadmanAction {
        clear_glow: current_glow != 0.0,
        clear_mic: current_mic != 0.0,
    }
}

/// Arm (or re-arm) the glow deadman, replacing any pending one.
///
/// Dropping the previous `Timeout` cancels it, so only the most recent
/// evidence of speech is ever counted — and because the holder is a
/// `use_hook`-owned `Rc<RefCell<..>>` (the same idiom as `mic_hold_timeout`),
/// tile unmount drops the `Timeout` and cancels the pending fire. That is why
/// this must NOT use `Timeout::forget()`.
///
/// The closure captures a clone of the `mic_hold_timeout` holder so it can
/// route the mic clear through [`update_mic_audio_level`], the code that owns
/// that timer. That introduces no cycle and no leak past unmount: every strong
/// reference to the holder is transitively owned by this component's scope
/// (the hook, the diagnostics task, and this `Timeout` — which itself lives in
/// a hook-owned `Rc`), so unmount drops them all and cancels both timers.
///
/// Issue 2225: rebuilding that `Timeout` is not free, and the callers fire tens
/// of times per second while a peer talks, so the rebuild is rate-limited by
/// [`admit_glow_deadman_rearm`]. When it declines, the previously-armed timer
/// is still pending and still covering the glow — see
/// [`glow_deadman_rearm_throttle_ms`] for what that costs at the fire end.
fn arm_glow_deadman(
    audio_level: &mut Signal<f32>,
    mic_audio_level: &mut Signal<f32>,
    mic_hold_timeout: &Rc<RefCell<Option<Timeout>>>,
    glow_deadman: &Rc<RefCell<Option<Timeout>>>,
    glow_deadman_armed_at: &Rc<RefCell<f64>>,
) {
    // Read `pending` into a local first: the temporary borrow must end before
    // the `borrow_mut()` that installs the new timer below.
    let timer_pending = glow_deadman.borrow().is_some();
    let now = js_sys::Date::now();
    // A stale `Some` — a deadman that already FIRED, since nothing clears the
    // holder from inside the closure — cannot be mistaken for a live one here.
    // A fire happens `GLOW_DEADMAN_MS` after the arm, and this branch is only
    // reachable while the arm is younger than `GLOW_DEADMAN_REARM_THROTTLE_MS`,
    // which is a fifth of a keepalive against the deadman's two and a half.
    if !admit_glow_deadman_rearm(&mut glow_deadman_armed_at.borrow_mut(), now, timer_pending) {
        return;
    }
    let mut glow_sig = *audio_level;
    let mut mic_sig = *mic_audio_level;
    let mic_hold = mic_hold_timeout.clone();
    let timeout = Timeout::new(GLOW_DEADMAN_MS, move || {
        // Peek BEFORE writing so an already-dark tile is not marked dirty for
        // a no-op, and use the fallible accessors: in Dioxus 0.7 `peek()` /
        // `set()` are `try_*().unwrap()` and panic outright on a dropped
        // scope. Both signals belong to the same `PeerTile` scope, so a
        // successful read here also establishes that the rest of this closure
        // (including the `peek()` inside `update_mic_audio_level`) is safe.
        let (Ok(glow), Ok(mic)) = (
            glow_sig.try_peek().map(|v| *v),
            mic_sig.try_peek().map(|v| *v),
        ) else {
            return;
        };
        let action = glow_deadman_action(glow, mic);
        if action.clear_glow {
            if let Ok(mut w) = glow_sig.try_write() {
                *w = 0.0;
            }
        }
        if action.clear_mic {
            // Route through the owner of the mic-hold timer rather than
            // writing the signal directly, so the pending hold is cancelled or
            // replaced by the code that arms it. The mic then fades out over
            // its normal 1 s hold instead of snapping.
            update_mic_audio_level(0.0, &mut mic_sig, &mic_hold);
        }
    });
    *glow_deadman.borrow_mut() = Some(timeout);
}

/// Apply a resolved glow level to the tile: the change-gated `audio_level`
/// write, the mic-icon hold, and the deadman arm/disarm.
///
/// Shared by the `peer_status` and `peer_speaking` arms so both paths get
/// identical gating and identical deadman coverage.
fn apply_resolved_level(
    resolved: Option<f32>,
    speaking: Option<bool>,
    audio_level: &mut Signal<f32>,
    mic_audio_level: &mut Signal<f32>,
    mic_hold_timeout: &Rc<RefCell<Option<Timeout>>>,
    glow_deadman: &Rc<RefCell<Option<Timeout>>>,
    glow_deadman_armed_at: &Rc<RefCell<f64>>,
) {
    let prev = *audio_level.peek();
    if refreshes_glow_deadman(resolved, speaking, prev) {
        arm_glow_deadman(
            audio_level,
            mic_audio_level,
            mic_hold_timeout,
            glow_deadman,
            glow_deadman_armed_at,
        );
    }
    let Some(lvl) = resolved else {
        return;
    };
    if glow_write_reaches_signal(lvl, prev) {
        audio_level.set(lvl);
    }
    if lvl == 0.0 {
        // The glow is going dark on its own; a pending deadman has nothing
        // left to guard.
        glow_deadman.borrow_mut().take();
    }
    update_mic_audio_level(lvl, mic_audio_level, mic_hold_timeout);
}

#[allow(clippy::too_many_arguments)]
fn handle_diagnostics_event(
    evt: &DiagEvent,
    peer_id: &str,
    audio_enabled: &mut Signal<bool>,
    video_enabled: &mut Signal<bool>,
    screen_enabled: &mut Signal<bool>,
    audio_level: &mut Signal<f32>,
    mic_audio_level: &mut Signal<f32>,
    mic_hold_timeout: &Rc<RefCell<Option<Timeout>>>,
    glow_deadman: &Rc<RefCell<Option<Timeout>>>,
    glow_deadman_armed_at: &Rc<RefCell<f64>>,
    fps_received: &mut Signal<f64>,
    fps_painted: &mut Signal<f64>,
    expand_rate: &mut Signal<f64>,
    video_bitrate: &mut Signal<f64>,
    audio_bitrate: &mut Signal<f64>,
    audio_buffer_ms: &mut Signal<f64>,
    screen_fps: &mut Signal<f64>,
    screen_bitrate: &mut Signal<f64>,
    latency_ms: &mut Signal<f64>,
    video_resolution: &mut Signal<String>,
    screen_resolution: &mut Signal<String>,
    screen_source_resolution: &mut Signal<String>,
    screen_encoder_target_bitrate: &mut Signal<u32>,
    screen_adaptive_tier: &mut Signal<String>,
    screen_cause_hint: &mut Signal<String>,
    peer_transport: &mut Signal<Option<String>>,
    last_peer_status_ts: &Rc<RefCell<f64>>,
) {
    match evt.subsystem {
        "peer_status" => {
            let mut to_peer: Option<String> = None;
            let mut audio: Option<bool> = None;
            let mut video: Option<bool> = None;
            let mut screen: Option<bool> = None;
            let mut audio_lvl: Option<f32> = None;
            let mut speaking: Option<bool> = None;
            let mut transport: Option<String> = None;
            for m in &evt.metrics {
                match (m.name, &m.value) {
                    ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.to_string()),
                    ("audio_enabled", MetricValue::U64(v)) => audio = Some(*v != 0),
                    ("video_enabled", MetricValue::U64(v)) => video = Some(*v != 0),
                    ("screen_enabled", MetricValue::U64(v)) => screen = Some(*v != 0),
                    ("audio_level", MetricValue::F64(v)) => audio_lvl = Some(*v as f32),
                    ("is_speaking", MetricValue::U64(v)) => speaking = Some(*v != 0),
                    ("peer_transport", MetricValue::Text(t)) => transport = Some(t.to_string()),
                    _ => {}
                }
            }
            if to_peer.as_deref() != Some(peer_id) {
                return;
            }
            // Issue #906: stamp the heartbeat timestamp the first time we   // @token-exempt: issue ref, not a color
            // confirm this `peer_status` event is for our peer. The screen-
            // state classifier consults this to decide between `Static` and
            // `NoFrames` — fresh heartbeat means the publisher is alive,
            // stale heartbeat means the connection is the problem.
            *last_peer_status_ts.borrow_mut() = js_sys::Date::now();
            if let Some(a) = audio {
                if a != *audio_enabled.peek() {
                    audio_enabled.set(a);
                }
                // Issue #1769: drive the received-audio kbps off the SAME
                // authoritative audio-on flag that sets the mic icon, so the
                // overlay's audio field agrees with the mic state. #2132: the rung
                // rides this same event, so the readout tracks the layer actually
                // being decoded instead of always reporting the base. `peer_status`
                // fires once per 5 s heartbeat per peer (plus on media-state
                // transitions), so gate the `.set()` on an actual change.
                let ab = overlay_audio_kbps_from_status(a, &evt.metrics, peer_id);
                if (*audio_bitrate.peek() - ab).abs() > f64::EPSILON {
                    audio_bitrate.set(ab);
                }
            }
            if let Some(v) = video {
                if v != *video_enabled.peek() {
                    video_enabled.set(v);
                }
            }
            if let Some(s) = screen {
                if s != *screen_enabled.peek() {
                    screen_enabled.set(s);
                }
            }
            // Issue 2174: the boolean is authoritative here — this heartbeat's
            // `audio_level` float is producer-hardcoded to 0.0 — and a
            // heartbeat that claims muted-but-speaking is rejected outright.
            // See `effective_level` / `resolve_audio_level`.
            let resolved_level = effective_level(audio, audio_lvl, speaking, *audio_level.peek());
            apply_resolved_level(
                resolved_level,
                speaking,
                audio_level,
                mic_audio_level,
                mic_hold_timeout,
                glow_deadman,
                glow_deadman_armed_at,
            );
            // Update the transport signal only when the value actually
            // changes — `peer_status` fires once per 5 s heartbeat (and on
            // every media-state transition), so a naive `.set()` here would
            // wake every PeerTile subscriber on each one even though
            // transport rarely changes.
            if let Some(t) = transport {
                let prev = peer_transport.peek();
                let changed = match prev.as_deref() {
                    Some(p) => p != t.as_str(),
                    None => true,
                };
                drop(prev);
                if changed {
                    peer_transport.set(Some(t));
                }
            }
        }
        "peer_speaking" => {
            // Fast-path speaking updates from decoded audio frames. Issue 2174
            // follow-up: routed through `speaking_event_resolution` so this arm
            // applies the SAME `audio_enabled` mute veto as the `peer_status`
            // arm above — a straggler from the decoder pipeline must not
            // re-light a peer the host just muted.
            let Some((speaking, resolved_level)) = speaking_event_resolution(
                &evt.metrics,
                peer_id,
                *audio_enabled.peek(),
                *audio_level.peek(),
            ) else {
                return;
            };
            apply_resolved_level(
                resolved_level,
                speaking,
                audio_level,
                mic_audio_level,
                mic_hold_timeout,
                glow_deadman,
                glow_deadman_armed_at,
            );
        }
        "video" => {
            // Extract fps_received, bitrate_kbps, and media_type for quality scoring.
            let mut to_peer: Option<String> = None;
            let mut fps: Option<f64> = None;
            let mut bitrate: Option<f64> = None;
            let mut media_type_str: Option<String> = None;
            for m in &evt.metrics {
                match (m.name, &m.value) {
                    ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.to_string()),
                    ("fps_received", MetricValue::F64(v)) => fps = Some(*v),
                    ("bitrate_kbps", MetricValue::F64(v)) => bitrate = Some(*v),
                    ("media_type", MetricValue::Text(t)) => media_type_str = Some(t.to_string()),
                    _ => {}
                }
            }
            if to_peer.as_deref() != Some(peer_id) {
                return;
            }
            let is_screen = media_type_str.as_deref() == Some("SCREEN");
            if is_screen {
                if let Some(f) = fps {
                    screen_fps.set(f);
                }
                if let Some(b) = bitrate {
                    screen_bitrate.set(b);
                }
            } else {
                if let Some(f) = fps {
                    // ARRIVAL rate: still feeds the drawer chart, signal popup, and
                    // health reporter. Issue #1784 moved the OVERLAY's "↓ fps" off
                    // this bucket and onto the painted rate (the `video_painted` arm
                    // below), so this no longer touches the overlay signal.
                    fps_received.set(f);
                }
                if let Some(b) = bitrate {
                    video_bitrate.set(b);
                }
            }
        }
        sub if sub == SUBSYSTEM_VIDEO_PAINTED => {
            // Issue #1784: PAINTED fps — the media-metrics overlay's "↓ fps" source.
            // The decoder emits this per-peer at the paint site (frames actually
            // drawn), so it reflects what the viewer sees, not packet arrival. The
            // pure `overlay_painted_fps_sample` parser returns the camera sample for
            // THIS peer (filtering wrong-peer and SCREEN events); feeding it through
            // `next_overlay_fps` preserves the mandatory snap-down-to-0 (a stopped
            // video paints nothing → sample 0 → em-dash) plus residual jitter EMA.
            if let Some(sample) = overlay_painted_fps_sample(&evt.metrics, peer_id) {
                let prev = *fps_painted.peek();
                fps_painted.set(next_overlay_fps(prev, sample));
            }
        }
        "neteq" => {
            // Extract expand_rate and audio_buffer_ms from neteq metrics.
            let mut target_peer: Option<String> = None;
            let mut er: Option<f64> = None;
            let mut buf_ms: Option<f64> = None;
            for m in &evt.metrics {
                match (m.name, &m.value) {
                    ("target_peer", MetricValue::Text(p)) => target_peer = Some(p.to_string()),
                    ("expand_rate", MetricValue::F64(v)) => er = Some(*v),
                    ("audio_buffer_ms", MetricValue::U64(v)) => buf_ms = Some(*v as f64),
                    _ => {}
                }
            }
            if target_peer.as_deref() != Some(peer_id) {
                return;
            }
            if let Some(rate) = er {
                // Convert from Q14 to per-mille: value / 16.384
                expand_rate.set(rate / 16.384);
            }
            if let Some(b) = buf_ms {
                audio_buffer_ms.set(b);
            }
        }
        "video_resolution" => {
            // Track video resolution changes broadcast by the decoder. The
            // `media_type` metric distinguishes the camera-video decoder
            // ("VIDEO") from the screen-share decoder ("SCREEN").
            let mut to_peer: Option<String> = None;
            let mut res_w: Option<u64> = None;
            let mut res_h: Option<u64> = None;
            let mut media_type_str: Option<String> = None;
            for m in &evt.metrics {
                match (m.name, &m.value) {
                    ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.to_string()),
                    ("resolution_width", MetricValue::U64(w)) => res_w = Some(*w),
                    ("resolution_height", MetricValue::U64(h)) => res_h = Some(*h),
                    ("media_type", MetricValue::Text(t)) => media_type_str = Some(t.to_string()),
                    _ => {}
                }
            }
            if to_peer.as_deref() != Some(peer_id) {
                return;
            }
            if let (Some(w), Some(h)) = (res_w, res_h) {
                let res = format!("{w}x{h}");
                let is_screen = media_type_str.as_deref() == Some("SCREEN");
                let target = if is_screen {
                    &mut *screen_resolution
                } else {
                    &mut *video_resolution
                };
                if *target.peek() != res {
                    target.set(res);
                }
            }
        }
        "video_source_resolution" => {
            // Publisher's native capture dimensions, broadcast by the
            // decoder when it sees a `MediaPacket.video_metadata.source_*`
            // field change. We only track this for screen-share today; the
            // camera-video branch carries `media_type=VIDEO` and we let the
            // UI ignore it for now (no UI consumer requested it yet).
            let mut to_peer: Option<String> = None;
            let mut src_w: Option<u64> = None;
            let mut src_h: Option<u64> = None;
            let mut media_type_str: Option<String> = None;
            for m in &evt.metrics {
                match (m.name, &m.value) {
                    ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.to_string()),
                    ("source_width", MetricValue::U64(w)) => src_w = Some(*w),
                    ("source_height", MetricValue::U64(h)) => src_h = Some(*h),
                    ("media_type", MetricValue::Text(t)) => media_type_str = Some(t.to_string()),
                    _ => {}
                }
            }
            if to_peer.as_deref() != Some(peer_id) {
                return;
            }
            if media_type_str.as_deref() != Some("SCREEN") {
                return;
            }
            if let (Some(w), Some(h)) = (src_w, src_h) {
                let res = format!("{w}x{h}");
                if *screen_source_resolution.peek() != res {
                    screen_source_resolution.set(res);
                }
            }
        }
        "screen_encoder_state" => {
            // Issue #903: publisher's encoder state for the screen-share // @token-exempt: issue ref, not a color
            // track, dispatched by the decoder when any of the three
            // fields changes. We filter strictly on `media_type=SCREEN`
            // mirroring the `video_source_resolution` arm — the camera
            // decoder doesn't emit this subsystem today, but the guard
            // documents the contract and prevents a future spillover
            // from corrupting the screen signal.
            //
            // .set() is gated on change to avoid waking PeerTile
            // subscribers when the values are unchanged. The decoder
            // already dedupes at the source so this is belt-and-braces
            // but cheap.
            let mut to_peer: Option<String> = None;
            let mut bitrate: Option<u32> = None;
            let mut tier: Option<String> = None;
            let mut hint: Option<String> = None;
            let mut media_type_str: Option<String> = None;
            for m in &evt.metrics {
                match (m.name, &m.value) {
                    ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.to_string()),
                    ("encoder_target_bitrate_kbps", MetricValue::F64(v)) => {
                        bitrate = Some(v.round().max(0.0) as u32);
                    }
                    ("adaptive_tier", MetricValue::Text(t)) => tier = Some(t.to_string()),
                    ("cause_hint", MetricValue::Text(t)) => hint = Some(t.to_string()),
                    ("media_type", MetricValue::Text(t)) => media_type_str = Some(t.to_string()),
                    _ => {}
                }
            }
            if to_peer.as_deref() != Some(peer_id) {
                return;
            }
            if media_type_str.as_deref() != Some("SCREEN") {
                return;
            }
            if let Some(b) = bitrate {
                if *screen_encoder_target_bitrate.peek() != b {
                    screen_encoder_target_bitrate.set(b);
                }
            }
            if let Some(t) = tier {
                if *screen_adaptive_tier.peek() != t {
                    screen_adaptive_tier.set(t);
                }
            }
            if let Some(h) = hint {
                if *screen_cause_hint.peek() != h {
                    screen_cause_hint.set(h);
                }
            }
        }
        "connection_manager" => {
            // RTT is a global metric (not per-peer), but we store it per-sample
            // so the chart can show latency alongside quality lines.
            let mut rtt: Option<f64> = None;
            for m in &evt.metrics {
                if let ("active_server_rtt", MetricValue::F64(v)) = (m.name, &m.value) {
                    rtt = Some(*v);
                }
            }
            if let Some(r) = rtt {
                latency_ms.set(r);
            }
        }
        _ => {}
    }
}

/// Push a signal quality sample at most once per second.
/// Increments `sample_counter` so the UI re-renders when the popup is open.
fn maybe_push_signal_sample(
    last_ts: &Rc<RefCell<f64>>,
    signal_history: &Rc<RefCell<PeerSignalHistory>>,
    data: &SampleData,
    sample_counter: &mut Signal<u32>,
) {
    let now = js_sys::Date::now();
    let prev = *last_ts.borrow();
    if now - prev < 1000.0 {
        return;
    }
    *last_ts.borrow_mut() = now;
    signal_history.borrow_mut().push_sample(data);
    let prev_count = *sample_counter.peek();
    sample_counter.set(prev_count.wrapping_add(1));
}

/// Update `mic_audio_level` with a 1-second hold: when audio drops to zero the
/// mic signal keeps its last positive value for 1 s so the icon stays green.
/// If new audio arrives before the timeout fires the pending timeout is cancelled.
fn update_mic_audio_level(
    level: f32,
    mic_audio_level: &mut Signal<f32>,
    mic_hold_timeout: &Rc<RefCell<Option<Timeout>>>,
) {
    if level > 0.0 {
        // Cancel any pending silence timeout — speaker is still active.
        mic_hold_timeout.borrow_mut().take();
        let prev = *mic_audio_level.peek();
        if (level - prev).abs() > UI_AUDIO_LEVEL_DELTA {
            mic_audio_level.set(level);
        }
    } else {
        // Audio dropped to zero. If already silent (or timeout already pending), skip.
        if *mic_audio_level.peek() == 0.0 {
            return;
        }
        if mic_hold_timeout.borrow().is_some() {
            // A timeout is already queued — let it fire.
            return;
        }
        let mut sig = *mic_audio_level;
        let timeout = Timeout::new(MIC_HOLD_DURATION_MS, move || {
            // Issue 2174: `set()` is `try_write().unwrap()` in Dioxus 0.7 and
            // panics on a dropped scope. The holder is owned by the component
            // so unmount cancels this, but the glow deadman now arms this timer
            // too — use the fallible write rather than rely on that invariant
            // holding at every future call site.
            if let Ok(mut w) = sig.try_write() {
                *w = 0.0;
            }
        });
        *mic_hold_timeout.borrow_mut() = Some(timeout);
    }
}

/// The signal level a peer tile renders (issue #2190).
///
/// Folds the peer's heartbeat-sourced enabled flags with whether THIS client is decoding,
/// then resolves the level from the sample history. This is the ONE decision the tile
/// renders, extracted so a host test can observe the value the renderer actually consumes.
///
/// That extraction is the point, not cosmetic. The first attempt at this fix folded only the
/// `SampleData` literal — which feeds `video_quality`, already 0.0 from fps 0 — while the
/// rendered level comes from `current_level`'s ARGUMENTS. The fold was inert, the red
/// "connection lost" badge still shipped, and the tests stayed green because they pinned the
/// pure fold helper in isolation while the call site went unguarded. Routing the whole
/// decision through here means unwiring it fails a test.
///
/// MUTATION: passing the raw `video_enabled`/`screen_enabled` instead of the folded flags
/// reproduces the badge and fails `rendered_level_excludes_streams_we_are_not_decoding`.
fn rendered_signal_level(
    history: &crate::components::signal_quality::PeerSignalHistory,
    audio_enabled: bool,
    video_enabled: bool,
    screen_enabled: bool,
    is_decoding: bool,
) -> crate::components::signal_quality::SignalLevel {
    if !is_decoding && (video_enabled || screen_enabled) {
        return crate::components::signal_quality::SignalLevel::Unmeasured;
    }
    let (a, v, sc) = crate::components::signal_quality::signal_enabled_flags(
        audio_enabled,
        video_enabled,
        screen_enabled,
        is_decoding,
    );
    history.current_level(a, v, sc)
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A dark tile. Used where the `current` level is irrelevant to the rule
    /// under test, so the interesting argument stands out.
    const DARK: f32 = 0.0;

    /// Issue #2190: the level the tile RENDERS must exclude streams this client is not
    /// decoding — so a healthy parked peer is never badged "connection lost".
    ///
    /// This asserts on `rendered_signal_level`, the fn the component actually calls, rather
    /// than on the fold helper in isolation. That distinction is the whole finding from the
    /// second review round: the first fix folded only the `SampleData` literal (which feeds
    /// `video_quality`, already 0.0 from fps 0) while the RENDERED level comes from
    /// `current_level`'s arguments — so the fix was inert, the badge shipped, and the tests
    /// stayed green because nothing observed the wiring.
    ///
    /// MUTATION: replacing the folded flags in `rendered_signal_level` with the raw
    /// `video_enabled`/`screen_enabled` makes the parked case read `Lost` and fails here.
    #[test]
    fn rendered_level_excludes_streams_we_are_not_decoding() {
        use crate::components::signal_quality::{PeerSignalHistory, SampleData, SignalLevel};

        // The sample a parked peer produces: video/screen 0.0 (nothing decoded), audio
        // healthy (audio decodes regardless of tile visibility).
        let mut history = PeerSignalHistory::new();
        history.push_sample_at(
            &SampleData {
                video_fps: 0.0,
                screen_fps: 0.0,
                video_enabled: false,
                screen_enabled: false,
                audio_enabled: true,
                audio_expand_rate: 0.0,
                audio_buffer_ms: 60.0,
                ..Default::default()
            },
            1_000.0,
        );

        // MUTED + parked, camera-on per heartbeat: the exact state that rendered the red slash.
        let parked = rendered_signal_level(&history, false, true, false, false);
        assert_eq!(
            parked,
            SignalLevel::Unmeasured,
            "a muted peer we are not decoding must render the neutral `Unmeasured` state, not \
             a network-quality claim"
        );

        // Same peer, but we ARE decoding: the zero video is real and must be scored.
        let decoding = rendered_signal_level(&history, false, true, false, true);
        assert_eq!(
            decoding,
            SignalLevel::Lost,
            "when we ARE decoding, a muted peer with zero video is genuinely lost and must \
             keep the indicator — the fix must exclude only what we do not measure"
        );

        // The two states must be distinguishable — collapsing them is the bug either way.
        assert_ne!(
            parked, decoding,
            "parked and decoding-but-frozen must render differently"
        );

        // Camera-on + parked with healthy audio is still unmeasured for video. Healthy audio
        // must not turn an unknown video signal into a strong video-quality claim.
        let camera_on_parked = rendered_signal_level(&history, true, true, false, false);
        assert_eq!(
            camera_on_parked,
            SignalLevel::Unmeasured,
            "audio health must not produce a false video signal score while decode is paused"
        );
    }

    /// An explicit "not speaking" is authoritative silence: the glow goes out
    /// no matter what the float says and no matter what is on screen.
    ///
    /// Mutation sensitivity: this is the headline issue-2174 assertion. The
    /// old float-first rule returned `Some(0.5)` for the first case, so
    /// reverting `resolve_audio_level` fails this test.
    #[test]
    fn not_speaking_zeroes_the_glow_regardless_of_the_float() {
        assert_eq!(
            resolve_audio_level(Some(0.5), Some(false), DARK),
            Some(0.0),
            "a stale/non-zero float must not keep the glow lit when the sender says it is silent"
        );
        assert_eq!(resolve_audio_level(Some(0.0), Some(false), DARK), Some(0.0));
        assert_eq!(
            resolve_audio_level(None, Some(false), DARK),
            Some(0.0),
            "boolean-only silence must still resolve to 0.0"
        );
        assert_eq!(
            resolve_audio_level(Some(0.9), Some(false), 0.8),
            Some(0.0),
            "silence must zero a brightly-lit tile, not leave it alone"
        );
    }

    /// The 5 s heartbeat must never drive a talking peer's glow to zero.
    ///
    /// `broadcast_peer_status` always emits an `audio_level` metric and its
    /// backing `Peer::audio_level` field is only ever assigned `0.0`, so every
    /// heartbeat for a talking peer arrives as `(Some(0.0), Some(true))`. Under
    /// the old float-first rule that resolved to `Some(0.0)` and drove the glow
    /// to zero once per heartbeat.
    ///
    /// This is the anti-blink guarantee and it holds from BOTH tile states —
    /// whatever the resolver returns, it is never a zero level.
    ///
    /// Mutation sensitivity: reverting `resolve_audio_level` returns
    /// `Some(0.0)` for both and fails this test.
    #[test]
    fn a_speaking_heartbeat_never_resolves_to_a_zero_glow() {
        for current in [DARK, 0.25, 1.0] {
            assert_ne!(
                resolve_audio_level(Some(0.0), Some(true), current),
                Some(0.0),
                "a `speaking = true` heartbeat must never zero the glow (current = {current})"
            );
        }
    }

    /// Light-from-dark: when the tile is dark and the only evidence of speech
    /// is the heartbeat boolean, the glow lights at the modest
    /// [`HEARTBEAT_SOURCED_GLOW_LEVEL`] rather than full scale — an inaudible
    /// peer on a lossy link must not out-shine peers with a measured level.
    #[test]
    fn speaking_lights_a_dark_tile_at_the_modest_heartbeat_level() {
        assert_eq!(
            resolve_audio_level(Some(0.0), Some(true), DARK),
            Some(HEARTBEAT_SOURCED_GLOW_LEVEL)
        );
        assert_eq!(
            resolve_audio_level(None, Some(true), DARK),
            Some(HEARTBEAT_SOURCED_GLOW_LEVEL),
            "a producer that omits the float takes the same path"
        );
        // Assert the observable property through the resolver rather than on
        // the constant itself: the tile must end up lit, but sub-maximal.
        let lit = resolve_audio_level(Some(0.0), Some(true), DARK)
            .expect("a dark tile with a speaking heartbeat must resolve to a level");
        assert!(
            lit > 0.0 && lit < 1.0,
            "heartbeat-sourced glow {lit} must be lit but sub-maximal"
        );
    }

    /// The pulse-to-full fix: when the tile is ALREADY glowing and the
    /// heartbeat carries no usable float, the resolver returns `None` —
    /// "leave it alone" — so a quiet talker's finely-graded level is not
    /// yanked up to a constant once per heartbeat.
    ///
    /// Mutation sensitivity: returning a level here instead of `None`
    /// reintroduces the visible 5 s pulse.
    #[test]
    fn speaking_leaves_an_already_lit_glow_untouched() {
        assert_eq!(
            resolve_audio_level(Some(0.0), Some(true), 0.25),
            None,
            "a live glow must be left to the fast path, not overwritten"
        );
        assert_eq!(resolve_audio_level(None, Some(true), 0.25), None);
        assert_eq!(
            resolve_audio_level(Some(0.0), Some(true), GLOW_DARK_CEILING + 0.001),
            None,
            "anything above the dark ceiling is the fast path's to own"
        );
    }

    /// Issue 2224 — the headline regression. A `current` that is strictly
    /// positive but at or below the write-gate delta is a stale residue, not a
    /// glow the fast path still owns, and a speaking heartbeat must raise it.
    ///
    /// The band is reachable through BOTH gates — the producer's and this
    /// crate's: `0.0 -> 0.05` rides the VAD's `false -> true` toggle, then
    /// `0.05 -> 0.008` is a fade-out whose delta of `0.042` clears the
    /// producer's `AUDIO_LEVEL_DELTA_THRESHOLD` (0.02) and the write gate's
    /// `UI_AUDIO_LEVEL_DELTA` (0.01) alike, parking the signal on `0.008`
    /// (intensity `0.008` = `rms ~= 0.0200051` at `vadThreshold = 0.02`). Under
    /// the old exact-zero boundary every subsequent heartbeat took the "already
    /// glowing" branch and returned `None`, so a peer whose decoder fast path
    /// had died stayed pinned at the bottom of the glow ramp for as long as
    /// they kept talking.
    ///
    /// MUTATION: restoring `if current > 0.0` in `resolve_audio_level` returns
    /// `None` for every case here and fails this test.
    #[test]
    fn a_sub_gate_residue_is_raised_by_a_speaking_heartbeat() {
        for current in [f32::MIN_POSITIVE, 0.001, 0.008, GLOW_DARK_CEILING] {
            assert_eq!(
                resolve_audio_level(Some(0.0), Some(true), current),
                Some(HEARTBEAT_SOURCED_GLOW_LEVEL),
                "a residue of {current} must be raised to the heartbeat level, not preserved"
            );
            assert_eq!(
                resolve_audio_level(None, Some(true), current),
                Some(HEARTBEAT_SOURCED_GLOW_LEVEL),
                "a producer that omits the float takes the same path (current = {current})"
            );
        }
    }

    /// Issue 2224 — the other half of the wedge. While the resolver declines to
    /// touch a level, `refreshes_glow_deadman` re-arms the 12.5 s timeout on
    /// every 5 s heartbeat, so a `None` verdict over a residue meant the
    /// timeout could never fire and nothing was left that could retire the
    /// stale value. The deadman must not treat a sub-gate residue as a live
    /// glow worth guarding.
    ///
    /// MUTATION: restoring `current > 0.0` in `refreshes_glow_deadman` makes
    /// every case here refresh and fails this test.
    #[test]
    fn the_deadman_is_not_re_armed_by_a_sub_gate_residue() {
        for current in [f32::MIN_POSITIVE, 0.001, 0.008, GLOW_DARK_CEILING] {
            assert!(
                !refreshes_glow_deadman(None, Some(true), current),
                "a residue of {current} is not a live glow — the deadman must be allowed to lapse"
            );
        }
    }

    /// Issue 2224 — the two sites must agree on where dark ends.
    ///
    /// `resolve_audio_level` returning `None` for a speaking claim and
    /// `refreshes_glow_deadman` refreshing on that same `None` are the two
    /// halves of one decision ("the fast path still owns this glow"). When they
    /// disagreed, the resolver's refusal to raise a level was read by the
    /// deadman as a glow worth guarding — the wedge. This cross-checks the two
    /// production functions against each other across the boundary rather than
    /// asserting a constant against itself.
    ///
    /// MUTATION: changing the comparison at either site alone splits the two
    /// columns somewhere in this sweep and fails here.
    #[test]
    fn the_resolver_and_the_deadman_share_one_dark_boundary() {
        for current in [
            0.0,
            f32::MIN_POSITIVE,
            0.008,
            GLOW_DARK_CEILING,
            GLOW_DARK_CEILING + 0.001,
            0.25,
            1.0,
        ] {
            let left_alone = resolve_audio_level(Some(0.0), Some(true), current).is_none();
            let guarded = refreshes_glow_deadman(None, Some(true), current);
            assert_eq!(
                left_alone, guarded,
                "at current = {current} the resolver says leave-alone={left_alone} while the \
                 deadman says guard={guarded}; a glow nobody owns must not re-arm the timeout"
            );
        }
    }

    /// Issue 2224 — [`GLOW_DARK_CEILING`] and the write gate must stay coupled.
    ///
    /// The ceiling is currently an alias of `UI_AUDIO_LEVEL_DELTA`, which is
    /// the whole point but also the risk: nothing structural stops a later
    /// change from hardcoding a different ceiling, or from retuning the gate's
    /// comparison, and the two would then silently disagree. Asserting
    /// `GLOW_DARK_CEILING == UI_AUDIO_LEVEL_DELTA` would be worthless — a
    /// literal against itself, the exact anti-pattern this file already shipped
    /// once. So this ties them through their MEANING, by running the two
    /// production predicates against each other:
    ///
    /// > **a level is a live glow if and only if the write gate could have put
    /// > it on screen starting from dark.**
    ///
    /// Both directions carry a reason, which is why the biconditional rather
    /// than an implication:
    ///
    /// * Ceiling **below** the gate → [`resolve_audio_level`] would defend
    ///   levels as "the fast path's to own" that
    ///   [`glow_write_reaches_signal`] could never have written from dark. It
    ///   would be deferring to a value nothing in the pipeline could produce —
    ///   the issue-2224 wedge, in its general form.
    /// * Ceiling **above** the gate → a heartbeat would stomp levels the fast
    ///   path genuinely wrote and is actively steering, reintroducing the
    ///   once-per-5 s pulse-to-full that rule 3 exists to prevent.
    ///
    /// # What this does NOT cover
    ///
    /// Retuning `UI_AUDIO_LEVEL_DELTA` itself moves both sides and stays green —
    /// correctly, since the alias is the mechanism that keeps them coupled.
    /// What fails is **decoupling**: changing one without the other.
    ///
    /// MUTATION (both run): raising `GLOW_DARK_CEILING` to `0.05` while leaving
    /// the gate alone splits the sweep at `0.05` — the ceiling is swept
    /// symbolically, so its own value is the first level that disagrees;
    /// lowering the gate in `glow_write_reaches_signal` to `0.001` while
    /// leaving the ceiling alone splits it at `0.005`. In both runs this was
    /// the ONLY failing test in the crate, which is the gap it exists to
    /// close.
    #[test]
    fn a_live_glow_is_exactly_a_level_the_write_gate_can_propagate() {
        // `prev = 0.0` is the question being asked: could this level have been
        // put on screen from dark? At a dark `prev` the gate's zero-drop clause
        // is inert, so what is compared is the delta clause against the ceiling.
        const DARK_PREV: f32 = 0.0;

        for level in [
            0.0,
            f32::MIN_POSITIVE,
            0.001,
            0.005,
            0.008,
            GLOW_DARK_CEILING,
            GLOW_DARK_CEILING + 0.001,
            0.02,
            0.25,
            1.0,
        ] {
            let can_reach_the_screen = glow_write_reaches_signal(level, DARK_PREV);
            let counts_as_live = holds_live_glow(level);
            assert_eq!(
                can_reach_the_screen, counts_as_live,
                "at level {level} the write gate says reachable={can_reach_the_screen} while the \
                 resolver says live={counts_as_live}; the dark boundary and the gate that fills \
                 it must move together"
            );
        }
    }

    /// Issue 2224, end to end: a peer whose fast path dies with the signal
    /// parked on a residue, still sending `is_speaking = true` every 5 s.
    ///
    /// Composed from the two calls `apply_resolved_level` makes, in that order,
    /// so this walks the real runtime sequence rather than a paraphrase of it.
    /// The old code returned `None` here forever — never lighting the tile and
    /// re-arming the deadman on every heartbeat, so neither the glow nor the
    /// timeout could resolve the state. Now the first heartbeat raises the tile
    /// to the heartbeat level, and the deadman that gets re-armed is guarding a
    /// real one.
    ///
    /// MUTATION: restoring `current > 0.0` in `resolve_audio_level` fails the
    /// first assertion. It does NOT catch the same revert in
    /// `refreshes_glow_deadman` — both deadman assertions here sit on the
    /// `Some(lvl)` arm or on `current = 0.5`, so neither consults the boundary.
    /// That revert is caught by `the_deadman_is_not_re_armed_by_a_sub_gate_residue`
    /// and `the_resolver_and_the_deadman_share_one_dark_boundary` — not by
    /// `a_live_glow_is_exactly_a_level_the_write_gate_can_propagate`, which
    /// never calls `refreshes_glow_deadman`.
    #[test]
    fn a_dead_fast_path_over_a_residue_is_lit_by_the_next_heartbeat() {
        // Where `0.0 -> 0.05 -> 0.008` leaves the signal: the second step's
        // delta of 0.042 clears the producer's 0.02 emit gate and this crate's
        // 0.01 write gate alike. The fast path then dies, so nothing else can
        // move it.
        //
        // See `GLOW_DARK_CEILING` for the full trace, including why the two
        // gates have to be checked separately.
        let parked = 0.008_f32;

        // Heartbeat #1: `peer_status` for an unmuted, speaking peer. Its float
        // is the producer's hardcoded 0.0 (`Peer::audio_level`).
        let resolved = effective_level(Some(true), Some(0.0), Some(true), parked);
        assert_eq!(
            resolved,
            Some(HEARTBEAT_SOURCED_GLOW_LEVEL),
            "the first heartbeat must light the tile instead of preserving the residue"
        );
        assert!(
            refreshes_glow_deadman(resolved, Some(true), parked),
            "a resolved positive level is evidence of speech and must arm the deadman"
        );

        // The raise also has to clear the write gate in `apply_resolved_level`,
        // or the tile would never repaint.
        let lit = resolved.expect("checked above");
        assert!(
            glow_write_reaches_signal(lit, parked),
            "the raise ({parked} -> {lit}) must clear the write gate or the tile never repaints"
        );

        // Heartbeat #2, from the new state: the tile is genuinely lit now, so
        // the resolver hands it back to the fast path (no 5 s pulse) and the
        // deadman keeps guarding it.
        assert_eq!(
            effective_level(Some(true), Some(0.0), Some(true), lit),
            None,
            "a genuinely lit glow must still be left alone — the anti-pulse rule survives"
        );
        assert!(
            refreshes_glow_deadman(None, Some(true), lit),
            "the re-armed deadman must now be guarding the real level"
        );
    }

    /// Issue 2224, defect 2: the tile must not seed its glow signals from the
    /// client's per-peer audio-level snapshot.
    ///
    /// That accessor resolves through `PeerDecodeManager::peer_audio_level` to
    /// `Peer::audio_level`, which production code only ever assigns `0.0`, so
    /// the seed could only write both signals' `use_signal` default back over
    /// themselves — as an UNGATED write that bypasses the
    /// `UI_AUDIO_LEVEL_DELTA` gate and the mic's 1 s hold, and that would
    /// destroy live glow state on any run where the signals are not already
    /// dark.
    ///
    /// This is a source-text guard, the same idiom (and the same honest limits)
    /// as `diagnostics_subscribers_handle_recoverable_overflow` below. There is
    /// no runtime seam: the seed lived inside an `async`-spawning `use_effect`
    /// in a `#[component]` fn, needing both a Dioxus runtime and a real
    /// `VideoCallClient`, neither of which exists on this native `--lib` target
    /// — and a pure deletion has no return value to assert on regardless.
    ///
    /// # What this does NOT cover
    ///
    /// It matches on literal spelling, so it is a revert-guard and nothing
    /// more. A reintroduced seed evades it by reaching the same field another
    /// way. Treat a green result as "nobody pasted that call back", not as
    /// coverage of the effect's behaviour.
    ///
    /// MUTATION: restoring the deleted `let initial_level = effect_client
    /// .<snapshot>(&peer_id_owned);` line fails this test.
    #[test]
    fn the_tile_does_not_seed_its_glow_from_the_always_zero_field() {
        // Assembled from fragments so the needle does not appear verbatim in
        // this file, which is the file being scanned.
        let needle = concat!("audio_level", "_for_peer");
        let src = include_str!("peer_tile.rs");
        assert!(
            !src.contains(needle),
            "peer_tile.rs seeds a glow signal from the client's always-zero `Peer::audio_level` \
             snapshot (issue 2224); both glow signals must start at their `use_signal` default \
             and be lit only through `apply_resolved_level`, the sole caller of `arm_glow_deadman`"
        );
    }

    /// When the decoder-VAD fast path supplies a real intensity, that
    /// intensity is preferred over every boolean-derived value — the fix must
    /// not flatten speaking peers to a constant.
    ///
    /// `rms_to_intensity` (videocall-client) returns exactly `0.0` below the
    /// VAD threshold and strictly positive above it, and the fast path only
    /// reports `speaking = true` when `rms > threshold`, so a real fast-path
    /// event always lands in this branch.
    #[test]
    fn a_positive_float_always_wins() {
        assert_eq!(
            resolve_audio_level(Some(0.62), Some(true), DARK),
            Some(0.62)
        );
        assert_eq!(
            resolve_audio_level(Some(0.62), Some(true), 0.25),
            Some(0.62),
            "a real level overwrites a live glow — only the dead float defers"
        );
        assert_eq!(
            resolve_audio_level(Some(f32::MIN_POSITIVE), Some(true), DARK),
            Some(f32::MIN_POSITIVE),
            "any strictly positive level counts as a usable reading"
        );
    }

    /// With no boolean at all the float passes through untouched, and an event
    /// carrying neither metric resolves to `None` (leaving the signal alone).
    #[test]
    fn float_only_events_pass_through_unchanged() {
        assert_eq!(resolve_audio_level(Some(0.4), None, DARK), Some(0.4));
        assert_eq!(resolve_audio_level(Some(0.0), None, DARK), Some(0.0));
        assert_eq!(resolve_audio_level(None, None, DARK), None);
        assert_eq!(
            resolve_audio_level(Some(0.0), None, 0.7),
            Some(0.0),
            "without a boolean the float still zeroes a lit tile, as before"
        );
    }

    /// A heartbeat claiming `audio_enabled = 0` while `is_speaking = 1` is
    /// self-contradictory — it would draw a muted mic icon next to a lit glow.
    /// The mute claim wins.
    ///
    /// Mutation sensitivity: dropping the `audio_enabled` guard from
    /// `effective_level` lets the speaking claim through and fails this test.
    #[test]
    fn a_muted_peer_cannot_claim_to_be_speaking() {
        assert_eq!(
            effective_level(Some(false), Some(0.0), Some(true), DARK),
            Some(0.0),
            "audio_enabled = 0 must veto a speaking claim"
        );
        assert_eq!(
            effective_level(Some(false), Some(0.9), Some(true), 0.8),
            Some(0.0),
            "the veto also overrides a positive level and darkens a lit tile"
        );
    }

    /// The mute veto must not disturb the honest cases: an audio-enabled peer
    /// (or a producer that omits the flag) resolves exactly as the bare
    /// resolver does.
    #[test]
    fn effective_level_defers_to_the_resolver_when_not_muted() {
        for audio_enabled in [Some(true), None] {
            for (lvl, speaking, current) in [
                (Some(0.62), Some(true), 0.0),
                (Some(0.0), Some(true), 0.0),
                (Some(0.0), Some(true), 0.25),
                (Some(0.5), Some(false), 0.5),
                (None, None, 0.0),
            ] {
                assert_eq!(
                    effective_level(audio_enabled, lvl, speaking, current),
                    resolve_audio_level(lvl, speaking, current),
                    "audio_enabled = {audio_enabled:?} must not change the outcome"
                );
            }
        }
    }

    /// A `peer_speaking`-shaped metric set, exactly as `handle_pcm_data` in
    /// `neteq_audio_decoder.rs` emits it.
    fn speaking_metrics(to_peer: &'static str, speaking: u64, level: f64) -> [Metric; 3] {
        [
            Metric {
                name: "to_peer",
                value: MetricValue::text_static(to_peer),
            },
            Metric {
                name: "speaking",
                value: MetricValue::U64(speaking),
            },
            Metric {
                name: "audio_level",
                value: MetricValue::F64(level),
            },
        ]
    }

    /// Issue 2174 follow-up: a straggler `peer_speaking` must not re-light a
    /// peer the host just muted. The decoder VAD runs on DECODED PCM, so audio
    /// already inside the decoder when the mute landed still surfaces
    /// `speaking: 1` with a real positive level afterwards.
    ///
    /// Mutation sensitivity: this is the whole fix. Reverting
    /// `speaking_event_resolution` to the bare `resolve_audio_level` this arm
    /// used to call yields `Some(0.7)` for the first case — rule 2, "a positive
    /// float always wins" — and fails this test.
    #[test]
    fn a_straggler_speaking_event_cannot_relight_a_muted_peer() {
        assert_eq!(
            speaking_event_resolution(&speaking_metrics("peer-1", 1, 0.7), "peer-1", false, DARK),
            Some((Some(true), Some(0.0))),
            "a muted peer's straggler speaking event must not light the glow"
        );
        assert_eq!(
            speaking_event_resolution(&speaking_metrics("peer-1", 1, 0.7), "peer-1", false, 0.8),
            Some((Some(true), Some(0.0))),
            "and it must darken a glow the pre-mute fast path had already lit"
        );
    }

    /// The veto is confined to muted peers: an audio-enabled peer's
    /// `peer_speaking` event resolves exactly as the bare resolver does, so the
    /// decoder-VAD fast path keeps owning the finely-graded glow.
    ///
    /// Mutation sensitivity: hardcoding the veto on (always returning
    /// `Some(0.0)`) fails the speaking rows here.
    #[test]
    fn an_audio_enabled_peer_resolves_through_the_normal_rules() {
        for (speaking, level) in [(1u64, 0.7f64), (1, 0.0), (0, 0.0), (0, 0.5)] {
            assert_eq!(
                speaking_event_resolution(
                    &speaking_metrics("peer-1", speaking, level),
                    "peer-1",
                    true,
                    DARK
                ),
                Some((
                    Some(speaking != 0),
                    resolve_audio_level(Some(level as f32), Some(speaking != 0), DARK)
                )),
                "an audio-enabled peer must be unaffected by the mute veto"
            );
        }
    }

    /// The diagnostics bus is global — every tile sees every peer's events — so
    /// the `to_peer` filter has to reject another peer's speaking event before
    /// it reaches this tile's signals.
    #[test]
    fn a_speaking_event_for_another_peer_is_ignored() {
        assert!(
            speaking_event_resolution(&speaking_metrics("peer-2", 1, 0.7), "peer-1", true, DARK)
                .is_none(),
            "a speaking event addressed to another peer must resolve to nothing"
        );
    }

    /// The deadman is a liveness guard, so it must be refreshed by evidence of
    /// ongoing speech — including the case where the resolver deliberately
    /// returns `None` to leave a live glow alone. Without that second clause a
    /// peer whose level sits stable would have its glow zeroed mid-sentence.
    #[test]
    fn the_deadman_is_refreshed_by_any_evidence_of_speech() {
        assert!(
            refreshes_glow_deadman(Some(0.62), Some(true), 0.0),
            "a positive resolved level is evidence of speech"
        );
        assert!(
            refreshes_glow_deadman(None, Some(true), 0.25),
            "a `leave it alone` verdict on a live glow is still evidence of speech"
        );
    }

    /// ...and NOT refreshed by silence, by a dark tile, or by unrelated events,
    /// so a peer that stops emitting entirely will actually time out.
    ///
    /// Mutation sensitivity: refreshing unconditionally (e.g. `true`) makes the
    /// deadman unable to ever fire, defeating the crashed-peer guard.
    #[test]
    fn the_deadman_is_not_refreshed_without_speech() {
        assert!(
            !refreshes_glow_deadman(Some(0.0), Some(false), 0.5),
            "an authoritative zero must let the deadman lapse"
        );
        assert!(
            !refreshes_glow_deadman(None, Some(true), 0.0),
            "nothing to guard while the tile is already dark"
        );
        assert!(
            !refreshes_glow_deadman(None, None, 0.5),
            "an event carrying no speaking claim is not evidence of speech"
        );
    }

    /// The crashed-peer scenario: a peer that dies mid-sentence leaves BOTH
    /// indicators lit and emits nothing further, so when the deadman fires it
    /// must drive both dark. The mic icon is the sibling path of the tile
    /// border — clearing only the border leaves a peer who is gone showing a
    /// green mic for the rest of the session.
    ///
    /// Mutation sensitivity: dropping the mic clear (`clear_mic: false`) fails
    /// the second assertion.
    #[test]
    fn a_crashed_peer_has_both_glow_and_mic_cleared() {
        let action = glow_deadman_action(0.6, 0.6);
        assert!(action.clear_glow, "the speaking glow must be driven dark");
        assert!(
            action.clear_mic,
            "the mic icon must be driven dark too — same speech evidence, same fate"
        );
    }

    /// A fire must not dirty a signal that is already dark, and must still
    /// clear whichever indicator IS lit. The mixed case is real: the glow can
    /// already be zero (an authoritative heartbeat zeroed it) while the mic is
    /// still green inside its 1 s hold.
    #[test]
    fn the_deadman_only_clears_what_is_actually_lit() {
        assert_eq!(
            glow_deadman_action(0.0, 0.0),
            GlowDeadmanAction {
                clear_glow: false,
                clear_mic: false
            },
            "an already-dark tile must not be marked dirty"
        );
        let mixed = glow_deadman_action(0.0, 0.4);
        assert!(!mixed.clear_glow);
        assert!(
            mixed.clear_mic,
            "a lit mic must be cleared even when the glow is already dark"
        );
        let mixed = glow_deadman_action(0.4, 0.0);
        assert!(mixed.clear_glow);
        assert!(!mixed.clear_mic);
    }

    /// The deadman window must clear at least two consecutive keepalives so
    /// ordinary jitter or a single dropped datagram cannot blink the glow.
    ///
    /// Mutation sensitivity: the window is exercised as a function of the
    /// keepalive period, so shrinking the multiplier to 2x (or growing it to
    /// 3x) fails here — and the last assertion pins that the shipped constant
    /// really is that formula applied to the real
    /// `HEARTBEAT_KEEPALIVE_INTERVAL_MS`, not a hand-typed 12_500.
    #[test]
    fn the_deadman_window_survives_two_missed_keepalives() {
        for keepalive in [1_000u32, 5_000, 8_000] {
            let window = glow_deadman_ms(keepalive);
            assert!(
                window > 2 * keepalive,
                "window {window} must clear two {keepalive} ms keepalives"
            );
            assert!(
                window < 3 * keepalive,
                "window {window} must not let a stuck glow linger three keepalives"
            );
        }
        assert_eq!(
            GLOW_DEADMAN_MS,
            glow_deadman_ms(HEARTBEAT_KEEPALIVE_INTERVAL_MS),
            "the shipped deadman must be derived from the real keepalive"
        );
    }

    /// Issue 2225 — the whole point of the throttle: a talking peer emits tens
    /// of qualifying events per second, and all of them inside one throttle
    /// period must cost exactly ONE timer.
    ///
    /// Drives the production state transition (`admit_glow_deadman_rearm` owns
    /// both the decision and the `armed_at` bookkeeping), with `timer_pending`
    /// held true because an admitted arm always leaves a timer behind.
    ///
    /// Mutation sensitivity: removing the throttle (returning `true`
    /// unconditionally, i.e. reverting to the un-fixed `arm_glow_deadman`)
    /// makes this 100, not 1.
    #[test]
    fn a_burst_of_speech_events_arms_the_deadman_once() {
        let throttle = f64::from(GLOW_DEADMAN_REARM_THROTTLE_MS);
        let mut armed_at = 0.0_f64;
        let start = 1_000_000.0_f64;

        // The first event has nothing pending, so it must arm.
        assert!(
            admit_glow_deadman_rearm(&mut armed_at, start, false),
            "the first evidence of speech must arm the deadman"
        );
        assert_eq!(armed_at, start, "an admitted arm records its own timestamp");

        // 100 further events spread across the rest of the throttle period —
        // the realistic emission rate for one speaking peer.
        let arms = (1..=100)
            .filter(|i| {
                let now = start + throttle * f64::from(*i) / 101.0;
                admit_glow_deadman_rearm(&mut armed_at, now, true)
            })
            .count();
        assert_eq!(
            arms, 0,
            "every event inside the throttle window must reuse the pending timer"
        );
        assert_eq!(
            armed_at, start,
            "a suppressed re-arm must not advance the arm timestamp, or the \
             window would slide forward one event at a time"
        );
    }

    /// ...and the throttle must not become a mute button: once it lapses, the
    /// next piece of evidence re-arms and the full window starts over.
    ///
    /// Mutation sensitivity: making the suppression unconditional (dropping the
    /// elapsed check) fails the first assertion — and with it the deadman would
    /// fire mid-sentence on any peer that has been talking longer than one
    /// window.
    #[test]
    fn evidence_after_the_throttle_window_re_arms() {
        let throttle = f64::from(GLOW_DEADMAN_REARM_THROTTLE_MS);
        let start = 1_000_000.0_f64;
        let mut armed_at = start;

        let boundary = start + throttle;
        assert!(
            admit_glow_deadman_rearm(&mut armed_at, boundary, true),
            "the throttle is half-open: an event exactly one period later re-arms"
        );
        assert_eq!(armed_at, boundary, "the new window starts at the new arm");

        // ...and the event just before the boundary does not.
        let mut armed_at = start;
        assert!(
            !admit_glow_deadman_rearm(&mut armed_at, boundary - 1.0, true),
            "one millisecond short of the period must still reuse the timer"
        );
    }

    /// The correctness half of the throttle, and the reason it checks
    /// `timer_pending` at all: `apply_resolved_level` `take()`s the deadman
    /// whenever a level resolves to zero, so a glow that goes dark and re-lights
    /// inside one throttle period has NOTHING pending. Suppressing that re-arm
    /// would leave a lit glow with no deadman behind it — the stuck-glow defect
    /// of issue 2174, reintroduced by a performance fix.
    ///
    /// Mutation sensitivity: throttling on elapsed time alone (dropping the
    /// `if timer_pending` guard) fails this outright.
    #[test]
    fn a_relit_glow_always_re_arms_even_inside_the_throttle_window() {
        let start = 1_000_000.0_f64;
        let mut armed_at = start;
        assert!(
            admit_glow_deadman_rearm(&mut armed_at, start + 1.0, false),
            "with no deadman pending the re-arm must happen regardless of age"
        );
        assert_eq!(armed_at, start + 1.0);
    }

    /// `js_sys::Date::now()` is a WALL clock, so `elapsed` can go negative on an
    /// NTP step. The pending `setTimeout` keeps its own schedule and fires
    /// anyway, so suppressing re-arms across the step would strand a talking
    /// peer with a lit glow and no deadman for the whole size of the step.
    ///
    /// Mutation sensitivity: replacing the range test with `elapsed < throttle`
    /// fails here.
    #[test]
    fn a_backwards_clock_step_never_suppresses_the_re_arm() {
        let mut armed_at = 1_000_000.0_f64;
        let after_step = armed_at - 30_000.0;
        assert!(
            admit_glow_deadman_rearm(&mut armed_at, after_step, true),
            "a negative elapsed must be treated as `too old`, never as `too new`"
        );
        assert_eq!(
            armed_at, after_step,
            "the throttle must re-seed on the stepped clock rather than stay in the future"
        );
    }

    /// Issue 2225 — throttling the re-arm shortens the deadman's effective
    /// window, and it must not shorten it past the guarantee `GLOW_DEADMAN_MS`
    /// exists to give.
    ///
    /// The lower bound is `D - T`: the pending timer can be up to one throttle
    /// period old when the last piece of evidence arrives (anything older would
    /// have re-armed), so the fire lands in `(t_last + D - T, t_last + D]`.
    /// That bound must stay above two keepalives, because a peer whose fast
    /// path dies but whose heartbeats still arrive refreshes the deadman once
    /// per keepalive and must never be darkened while it is still speaking.
    ///
    /// Exercised as a relationship between the real `const fn`s across a range
    /// of heartbeat periods, so it pins the 1/5 factor rather than restating
    /// 11 500.
    ///
    /// Mutation sensitivity (both run): widening the throttle to
    /// `keepalive_ms / 2` fails the floor assertion at every keepalive;
    /// narrowing the deadman to 2x fails it too. The final assertions pin that
    /// the SHIPPED constants are these formulas applied to the real keepalive,
    /// not hand-typed numbers.
    #[test]
    fn the_throttled_deadman_window_still_survives_two_missed_keepalives() {
        for keepalive in [1_000u32, 5_000, 8_000] {
            let floor = glow_deadman_floor_ms(keepalive);
            assert!(
                floor > 2 * keepalive,
                "throttled window floor {floor} must still clear two {keepalive} ms keepalives"
            );
            assert!(
                floor < glow_deadman_ms(keepalive),
                "the throttle must actually shorten the window, or it is not throttling"
            );
        }
        assert_eq!(
            GLOW_DEADMAN_REARM_THROTTLE_MS,
            glow_deadman_rearm_throttle_ms(HEARTBEAT_KEEPALIVE_INTERVAL_MS),
            "the shipped throttle must be derived from the real keepalive"
        );
        assert_eq!(
            glow_deadman_floor_ms(HEARTBEAT_KEEPALIVE_INTERVAL_MS),
            GLOW_DEADMAN_MS - GLOW_DEADMAN_REARM_THROTTLE_MS,
            "the floor must be the shipped deadman less the shipped throttle"
        );
    }

    /// Structural guard for the other half of issue 2174: every diagnostics
    /// subscriber in this crate must route `recv()` errors through
    /// `videocall_diagnostics::recv_loop_action`, which treats `Overflowed` as
    /// recoverable. The bare `while let Ok(..) = rx.recv().await` form exits
    /// the loop on *any* `Err`, so one 1024-event burst killed the subscriber
    /// for the rest of the session and froze every signal it fed.
    ///
    /// There is no runtime seam to assert this on: each loop is an inline
    /// `async move` block inside a `use_effect` in a `#[component]` fn, with no
    /// named function to call and no observable state on a native test target.
    /// The policy itself is unit-tested in `videocall-diagnostics`; what can
    /// still silently regress is *adoption*, which is what this guard pins.
    ///
    /// # What this does NOT cover
    ///
    /// It matches on **literal spelling**, so it is an honest revert-guard and
    /// nothing more. A reintroduced bare loop evades it by binding a different
    /// name (`while let Ok(event) = ..`), by using a receiver not called `rx`,
    /// or by living in a file outside the hard-coded list below — including any
    /// new component. It also says nothing about the three converted loops in
    /// `videocall-client`, which are in a different crate. Treat a green result
    /// as "nobody reverted these five files", not as coverage.
    ///
    /// Mutation sensitivity: restoring the bare `while let Ok(..)` form in any
    /// of these files fails this test.
    #[test]
    fn diagnostics_subscribers_handle_recoverable_overflow() {
        // Assembled from fragments so this needle does not appear verbatim in
        // `peer_tile.rs` — which is one of the files being scanned, and would
        // otherwise match its own assertion.
        let bare_form = concat!("while let ", "Ok(evt) = rx.recv().await");

        let subscribers = [
            ("peer_tile.rs", include_str!("peer_tile.rs")),
            ("peer_list.rs", include_str!("peer_list.rs")),
            (
                "connection_quality_indicator.rs",
                include_str!("connection_quality_indicator.rs"),
            ),
            ("diagnostics.rs", include_str!("diagnostics.rs")),
            ("attendants.rs", include_str!("attendants.rs")),
        ];

        for (name, src) in subscribers {
            assert!(
                !src.contains(bare_form),
                "{name}: a diagnostics subscriber still uses the bare `while let Ok(..)` form, \
                 which dies permanently on a recoverable `Overflowed` (issue 2174)"
            );
            assert!(
                src.contains(concat!("recv_loop_action", "(&e)")),
                "{name}: expected this file's diagnostics subscriber(s) to route `recv()` \
                 errors through `videocall_diagnostics::recv_loop_action`"
            );
        }
    }
}
