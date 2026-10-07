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

use std::time::Duration;

/// How often heartbeat pings are sent
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// How long before lack of client response causes a timeout.
/// Set to 30s to tolerate up to 5 missed heartbeat intervals (5s each),
/// reducing false disconnects on flaky networks.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Grace period before broadcasting PARTICIPANT_LEFT after a disconnect.
/// If the same user_id reconnects within this window, the departure is
/// cancelled silently — no PARTICIPANT_LEFT or PARTICIPANT_JOINED is
/// broadcast, avoiding false join/leave notification spam.
pub const RECONNECT_GRACE_PERIOD: Duration = Duration::from_secs(3);

/// Delay between a host kick and the relay closing the kicked transport, so
/// the PARTICIPANT_KICKED notice queued ahead of the close is written (#2934).
pub const KICK_CLOSE_FLUSH_DELAY: Duration = Duration::from_secs(1);

/// Regex pattern for validating user IDs on the deprecated `/lobby/{user_id}/{room}`
/// path. Room IDs use `videocall_types::validation::is_valid_meeting_id` instead.
pub const VALID_USER_ID_PATTERN: &str = "^[a-zA-Z0-9_-]*$";

/// Maximum incoming frame/stream size in bytes for both WebSocket and WebTransport.
///
/// 4 MB accommodates worst-case 1080p VP9 keyframes (1-2 MB raw) plus protobuf
/// wrapping overhead. The previous 1 MB limit caused session termination when a
/// participant shared a high-quality 1080p screen, because VP9 keyframes exceeded
/// the cap and triggered a protocol error that closed the entire connection.
pub const MAX_FRAME_SIZE: usize = 4_000_000;

/// IDLE time between continuation frames. `Pong` refreshes the heartbeat, so `CLIENT_TIMEOUT`
/// never reclaims an open sequence (#2600). Per FRAME, not per byte.
pub const FRAGMENT_ASSEMBLY_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Never refreshed, so it bounds a client pumping empty `Continue` frames to hold
/// [`MAX_FRAME_SIZE`]. Admits 4 MB at ~530 kbps (#2600).
pub const FRAGMENT_ASSEMBLY_MAX_LIFETIME: Duration = Duration::from_secs(60);

/// The screen encode ceiling must not be able to produce a keyframe larger than
/// [`MAX_FRAME_SIZE`]: over the cap the relay closes the connection, so this is
/// a build failure rather than a runtime clamp.
const _: () = {
    let ceiling_px = videocall_aq::constants::SCREEN_MAX_ENCODE_WIDTH as usize
        * videocall_aq::constants::SCREEN_MAX_ENCODE_HEIGHT as usize;
    assert!(
        ceiling_px * videocall_aq::constants::SCREEN_KEYFRAME_BYTES_PER_PIXEL <= MAX_FRAME_SIZE,
        "the screen encode ceiling can produce a keyframe larger than MAX_FRAME_SIZE, \
         which the relay answers with a connection-closing protocol error. Lower \
         SCREEN_MAX_ENCODE_WIDTH/HEIGHT, or raise MAX_FRAME_SIZE in its own PR."
    );
};

// ---------------------------------------------------------------------------
// Server Congestion Feedback
// ---------------------------------------------------------------------------

/// Number of dropped outbound packets within [`CONGESTION_WINDOW`] that triggers
/// a CONGESTION notification back to the sender whose packets are being dropped.
pub const CONGESTION_DROP_THRESHOLD: u32 = 5;

/// Time window over which drops are counted. Drop counters reset after this
/// window elapses without new drops.
pub const CONGESTION_WINDOW: Duration = Duration::from_millis(1000);

/// Minimum interval between CONGESTION notifications sent to the same sender
/// session. Prevents flooding the sender with congestion signals when many
/// packets are dropped in quick succession.
pub const CONGESTION_NOTIFY_MIN_INTERVAL: Duration = Duration::from_millis(1000);

/// Default bounded channel capacity for WebTransport outbound **unistream**
/// relay queue.
///
/// **Fail-fast rationale (issue #979).** This queue exists to absorb
/// short actor/writer scheduling bursts, NOT to buffer for a slow
/// receiver. A deep queue is actively harmful for real-time video: once
/// a receiver's link cannot drain the queue, every frame that sits in it
/// arrives too late to be useful and only delays the frames behind it.
/// At ~30 fps a video stream produces ~30 packets/sec, so the prior
/// 4096-slot bound represented well over a minute of stale backlog per
/// session — a 10-second-late video frame is already useless, never mind
/// a 60-second-late one. Holding that much in memory simply defers the
/// inevitable drop while inflating per-session memory and latency.
///
/// Overridable at deploy time via `WT_OUTBOUND_CHANNEL_CAPACITY`.
pub const WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT: usize = 1024;

/// Resolve the WebTransport outbound channel capacity from the
/// `WT_OUTBOUND_CHANNEL_CAPACITY` environment variable, falling back
/// to [`WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT`] if unset, unparseable,
/// or zero.
///
/// The lookup is memoised: the env var is read exactly once on the
/// first call. A non-zero `usize` parse yields the env value;
/// unparseable values (e.g. `"abc"`) emit a `warn!` and fall back to
/// the default. A literal `0` is also rejected so the channel is
/// never constructed with zero capacity (which would panic inside
/// `tokio::sync::mpsc::channel`).
pub fn wt_outbound_channel_capacity() -> usize {
    use std::sync::OnceLock;
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        resolve_wt_outbound_channel_capacity(
            std::env::var("WT_OUTBOUND_CHANNEL_CAPACITY")
                .ok()
                .as_deref(),
        )
    })
}

/// Pure resolver for [`wt_outbound_channel_capacity`]: maps the raw
/// optional environment string to the concrete channel capacity,
/// applying the same parse, zero-rejection and warn-on-failure rules
/// without touching any process-global state.
///
/// Extracted as a free function so unit tests can exercise the
/// resolution logic without racing against the `OnceLock` cache or
/// mutating the real process environment.
pub(crate) fn resolve_wt_outbound_channel_capacity(raw: Option<&str>) -> usize {
    match raw {
        Some(value) => match value.parse::<usize>() {
            Ok(0) => {
                tracing::warn!(
                    "WT_OUTBOUND_CHANNEL_CAPACITY=0 is invalid; falling back to default {}",
                    WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
                );
                WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
            }
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    "Failed to parse WT_OUTBOUND_CHANNEL_CAPACITY={:?} as usize ({}); falling back to default {}",
                    value, e, WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
                );
                WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
            }
        },
        None => WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT,
    }
}

/// Pure resolver for [`viewport_filter_enabled`]: maps the raw optional
/// environment string to the concrete #1436 kill-switch boolean, applying
/// the same recognised-boolean parsing and warn-on-unknown rules without
/// touching any process-global state.
///
/// Default is `true`: the #988 per-subscriber viewport VIDEO filter is
/// already LIVE in production, so the kill switch defaults to the status
/// quo (filter ON). An empty string trims to `""`, matches no arm, and
/// therefore takes the warn-and-default-`true` path — it is NOT special-cased.
/// No input panics.
///
/// Extracted as a free function so unit tests can exercise the resolution
/// logic without racing against the `OnceLock` cache or mutating the real
/// process environment.
pub(crate) fn resolve_viewport_filter_enabled(raw: Option<&str>) -> bool {
    match raw {
        None => true,
        Some(value) => match value.trim().to_ascii_lowercase().as_str() {
            "false" | "0" | "off" | "no" => false,
            "true" | "1" | "on" | "yes" => true,
            _ => {
                tracing::warn!(
                    "VIEWPORT_FILTER_ENABLED={:?} is not a recognised boolean; falling back to default true",
                    value
                );
                true
            }
        },
    }
}

/// Memoized accessor for the #1436 viewport-filter kill switch.
///
/// Reads `VIEWPORT_FILTER_ENABLED` exactly once on the first call and caches
/// the resolved boolean in a process-global `OnceLock`. Consequently, flipping
/// the environment variable requires a relay **process restart** to take
/// effect — there is NO hot-reload. This is acceptable and intended for an ops
/// kill switch: the #1436 requirement is to be able to revert the #988 filter
/// WITHOUT a client redeploy, and a relay restart satisfies that requirement.
///
/// Cheap on the hot path after the first call (a single atomic load), which is
/// why it can be invoked per VIDEO packet on the relay forward path.
pub fn viewport_filter_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        resolve_viewport_filter_enabled(std::env::var("VIEWPORT_FILTER_ENABLED").ok().as_deref())
    })
}

/// Which QUIC primitive carries downlink audio to a `ds=1` receiver (#2724).
/// A legacy (`Single`-mode) receiver keeps datagrams whatever this says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioDownlinkLane {
    /// The default: a dedicated reliable stream.
    Reliable,
    /// Sub-MTU cleartext audio on an unreliable datagram.
    Datagram,
}

/// Pure resolver for [`wt_audio_downlink_lane`]. Anything unrecognised warns
/// and takes the [`AudioDownlinkLane::Reliable`] default.
pub(crate) fn resolve_audio_downlink_lane(raw: Option<&str>) -> AudioDownlinkLane {
    match raw {
        None => AudioDownlinkLane::Reliable,
        Some(value) => match value.trim().to_ascii_lowercase().as_str() {
            "datagram" | "datagrams" => AudioDownlinkLane::Datagram,
            "reliable" | "stream" => AudioDownlinkLane::Reliable,
            _ => {
                tracing::warn!(
                    "WT_AUDIO_DOWNLINK_LANE={:?} is not recognised; falling back to the default \"reliable\"",
                    value
                );
                AudioDownlinkLane::Reliable
            }
        },
    }
}

/// Memoized accessor: reads `WT_AUDIO_DOWNLINK_LANE` once, so a change needs a
/// relay restart.
pub fn wt_audio_downlink_lane() -> AudioDownlinkLane {
    use std::sync::OnceLock;
    static LANE: OnceLock<AudioDownlinkLane> = OnceLock::new();
    *LANE.get_or_init(|| {
        resolve_audio_downlink_lane(std::env::var("WT_AUDIO_DOWNLINK_LANE").ok().as_deref())
    })
}

/// Pure #988 viewport drop decision. Returns true iff the off-screen VIDEO
/// packet must be dropped. `enabled` is the #1436 kill-switch state.
///
/// Fail-open semantics: `!enabled` -> `false` (the #1436 kill switch is OFF, so
/// forward-all is restored); `None` source -> `false` (unparseable sender,
/// fail open); empty viewport set -> `false` (no viewport signal yet, fail
/// open); otherwise drop iff the source session is NOT present in the set.
pub(crate) fn viewport_should_drop(
    enabled: bool,
    viewport_ids: &std::collections::HashSet<u64>,
    source: Option<u64>,
) -> bool {
    if !enabled {
        return false;
    }
    match source {
        None => false,
        Some(src) => !viewport_ids.is_empty() && !viewport_ids.contains(&src),
    }
}

/// Pure #1437 invariant predicate. Returns `true` iff a NON-VIDEO media kind
/// reached the viewport drop-decision site — the impossible case the #1437
/// tripwire counts. `MediaKind::VIDEO` => `false`; everything else, including an
/// unknown/unparseable kind (`Err(_)`), => `true`. The viewport filter is
/// VIDEO-only and guarded by `is_video` in `chat_server.rs` (#988), so on every
/// real packet this returns `false`; a `true` here means that guard regressed.
/// See #1437, #988.
pub(crate) fn nonvideo_reached_viewport_drop_branch(
    wire_media_kind: Result<
        videocall_types::protos::packet_wrapper::packet_wrapper::MediaKind,
        i32,
    >,
) -> bool {
    use videocall_types::protos::packet_wrapper::packet_wrapper::MediaKind;
    wire_media_kind != Ok(MediaKind::VIDEO)
}

/// Slot capacity of the WS per-receiver outbound relay queue (issue #2261).
pub const WS_OUTBOUND_CHANNEL_CAPACITY: usize = 1024;

/// Depth the byte budgets are anchored to: the multiplier in the two
/// transport-neutral budgets below.
pub const OUTBOUND_LEGACY_SLOT_CAPACITY: usize = 128;

/// Bytes one encoded frame of `tier` occupies at its ideal bitrate.
pub const fn tier_frame_bytes(tier: &videocall_aq::constants::VideoQualityTier) -> usize {
    (tier.ideal_bitrate_kbps as usize) * 1000 / 8 / (tier.target_fps as usize)
}

/// Camera VIDEO budget: legacy slots x one default-tier frame. Transport-neutral.
pub const OUTBOUND_VIDEO_BYTE_BUDGET: usize = OUTBOUND_LEGACY_SLOT_CAPACITY
    * tier_frame_bytes(
        &videocall_aq::constants::VIDEO_QUALITY_TIERS
            [videocall_aq::constants::DEFAULT_VIDEO_TIER_INDEX],
    );

pub const OUTBOUND_SCREEN_BYTE_BUDGET: usize = OUTBOUND_LEGACY_SLOT_CAPACITY
    * tier_frame_bytes(&videocall_aq::constants::SCREEN_QUALITY_TIERS[0]);

/// Dormant publish-side ladder DEPTH (#2620/#2621). NOT the fan-out
/// multiplier: that is [`AUDIO_PUBLISHED_LAYER_COUNT`] (#2279).
pub const AUDIO_SIMULCAST_RUNGS: usize = 3;

pub const AUDIO_PACKETS_PER_SEC_PER_RUNG: usize = 50;

pub const RELAY_SIZING_TARGET_PARTICIPANTS: usize = 27;

pub const RELAY_QUEUE_ABSORPTION_TARGET_MS: usize = 250;

pub const fn audio_fanout_packets_per_sec(
    participants: usize,
    rungs: usize,
    per_rung_pps: usize,
) -> usize {
    participants.saturating_sub(1) * rungs * per_rung_pps
}

/// Milliseconds of a `packets_per_sec` arrival rate that `slots` absorbs.
pub const fn queue_absorption_millis(slots: usize, packets_per_sec: usize) -> usize {
    if packets_per_sec == 0 {
        return usize::MAX;
    }
    slots * 1000 / packets_per_sec
}

/// Audio LAYERS a publisher puts on the wire (#2279), not [`AUDIO_SIMULCAST_RUNGS`].
pub const AUDIO_PUBLISHED_LAYER_COUNT: usize = 1;

pub const fn audio_tier_packet_bytes(bitrate_kbps: usize, packets_per_sec: usize) -> usize {
    if packets_per_sec == 0 {
        return 0;
    }
    bitrate_kbps * 1000 / 8 / packets_per_sec
}

pub const fn buffer_absorption_millis(buffer_bytes: usize, bytes_per_sec: usize) -> usize {
    if bytes_per_sec == 0 {
        return usize::MAX;
    }
    buffer_bytes * 1000 / bytes_per_sec
}

/// WebTransport session-ID header prepended before quinn's queue accounting.
pub const WT_DATAGRAM_SESSION_HEADER_BYTES: usize = 1;

/// Bytes one top-tier audio datagram occupies in quinn's queue: the serialized
/// `PacketWrapper` plus the session header, NOT the bare Opus payload.
pub const WT_DATAGRAM_AUDIO_WIRE_BYTES: usize = 203;

pub const WT_QUIC_DATAGRAM_SEND_BUFFER_BYTES: usize = 65_975;

/// quinn's CONNECTION-level unacked-byte cap, not a per-stream one. Bounds the
/// backlog a receiver must absorb; its own test pins the resulting ceiling from
/// both sides. Overridable per cluster via `QUIC_SEND_WINDOW_BYTES`.
pub const WT_QUIC_SEND_WINDOW_BYTES: u64 = 1_048_576;

pub const WT_QUIC_KEEP_ALIVE_DEFAULT_SECS: u64 = 5;

pub const WT_QUIC_MAX_IDLE_TIMEOUT_DEFAULT_SECS: u64 = 30;

pub const WT_QUIC_UDP_BUFFER_DEFAULT_BYTES: usize = 4 * 1024 * 1024;

/// Bounded channel capacity for the WebTransport **datagram** outbound queue.
///
/// As of the Phase 2 WT-freeze fix (discussion #756), the per-session
/// outbound channel is split into two: a unistream channel and a
/// datagram channel. Splitting the channels is the architectural change;
/// the unistream side keeps the env-tunable
/// [`WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT`], while the datagram side carries
/// only sub-MTU traffic:
///
/// * Datagrams are independent: there is no QUIC flow-control coupling
///   between them, so a slow receiver cannot stall the queue.
/// * `session.send_datagram` returns immediately on the wire (UDP-style
///   semantics inside the QUIC connection), so the queue exists only to
///   absorb actor-side bursts during scheduling jitter — not to buffer
///   for receiver congestion.
///
/// This value is NOT env-tunable today. If a future workload genuinely
/// needs a larger datagram queue (e.g. very chatty diagnostics), promote
/// it to an env-resolved getter mirroring [`wt_outbound_channel_capacity`].
pub const WT_DATAGRAM_CHANNEL_CAPACITY: usize = 512;

/// Grace period a write onto a server→client WebTransport uni stream may stay
/// parked WHILE THE OUTBOUND CHANNEL IS BACKED UP before the writer sheds the
/// wedged stream (issue #1638). The gate is the channel backing up, never
/// wall-clock alone, so executor starvation on a healthy stream cannot shed it.
///
/// Pinned to [`CONGESTION_WINDOW`] and below [`KEYFRAME_CONGESTION_RELAX_WINDOW`];
/// the assertion below holds that pin. Lives in `videocall-types`: the client
/// halves it, and nothing else sees both.
pub const WT_UNISTREAM_WRITE_DEADLINE: Duration =
    Duration::from_millis(videocall_types::wt_downlink::WT_UNISTREAM_WRITE_DEADLINE_MS);

const _: () = assert!(
    WT_UNISTREAM_WRITE_DEADLINE.as_millis() == CONGESTION_WINDOW.as_millis()
        && WT_UNISTREAM_WRITE_DEADLINE.as_millis() < KEYFRAME_CONGESTION_RELAX_WINDOW.as_millis(),
    "the shed deadline is pinned to one CONGESTION_WINDOW and must stay below \
     KEYFRAME_CONGESTION_RELAX_WINDOW, as the rationale above claims"
);

/// Fill ratio of a bounded dimension — slot depth, or a media byte bucket — at
/// or above which the outbound lane counts as under REAL backpressure, arming
/// the [`WT_UNISTREAM_WRITE_DEADLINE`] shed grace (issue #1638). Below it the
/// accumulator resets, so a parked write on a lane that is not backing up is
/// left to park. Sits above a transient burst's noise floor and below
/// [`crate::actors::priority_drop::PRIORITY_DROP_VIDEO_FILL_RATIO`], so the two
/// backpressure responses compose rather than fight.
pub const WT_UNISTREAM_BACKPRESSURE_SHED_RATIO: f64 = 0.5;

/// Interval at which the writer re-evaluates whether a parked write is
/// stalled-while-backed-up (issue #1638). One arm of a `select!` against the
/// write future, so it only runs while a write is actually parked; the timer is
/// built once per writer task and `reset()` per frame.
pub const WT_UNISTREAM_BACKPRESSURE_POLL: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// Per-publisher downlink streams (issue #2723)
// ---------------------------------------------------------------------------

/// Concurrent SERVER-initiated unidirectional streams a Chrome WebTransport
/// session can hold open: quiche's 100-stream default, less the 3 HTTP/3
/// reserves, which Chromium leaves untouched unless the JS supplies
/// `anticipatedConcurrentIncomingUnidirectionalStreams` (videocall's does not).
///
/// NOT the relay's own `max_concurrent_uni_streams(100)` in `webtransport::mod`,
/// which bounds CLIENT-initiated uplink streams out of a different pool.
pub const WT_BROWSER_MAX_SERVER_UNI_STREAMS: usize = 100;

/// Concurrent downlink streams the relay will hold open for ONE receiver
/// (#2723): 1 control + 1 audio (#2724) + 1 overflow + 45 publisher-keyed.
///
/// #2724 paid for its audio lane out of the PUBLISHER budget rather than by
/// raising this number, so the peak stream-ID demand the assertion below bounds
/// is unchanged.
pub const WT_MAX_DOWNLINK_STREAMS: usize = 48;

const _: () = assert!(
    2 * WT_MAX_DOWNLINK_STREAMS <= WT_BROWSER_MAX_SERVER_UNI_STREAMS,
    "a whole-map shed needs one fresh stream ID per lane while every reset one is \
     still counted, so 2 * WT_MAX_DOWNLINK_STREAMS must fit under the verified \
     browser limit documented on WT_BROWSER_MAX_SERVER_UNI_STREAMS."
);

/// Per-PUBLISHER hand-off queue between the downlink dispatcher and one key's
/// writer task (#2723). Absorbs dispatch pipelining only: the RECEIVER's total
/// backlog is bounded by [`wt_outbound_channel_capacity`] and the byte budgets
/// at admission, and a wedged key sheds long before this queue can tail-drop for
/// long. The control lane aggregates the whole room and is sized separately.
pub const WT_DOWNLINK_KEY_CHANNEL_CAPACITY: usize = 32;

/// How long a run of #2723 lane tail drops must last before it arms the #1219
/// relief epoch (#2745). The drop is booked immediately; only the stamp waits,
/// because arming costs the receiver shed non-base camera layers and a client
/// dwell on top. An ENTRY gate, so it can delay arming but never wedge a
/// receiver out of relief.
pub const WT_DOWNLINK_LANE_DROP_RELIEF_SUSTAIN: Duration = WT_UNISTREAM_WRITE_DEADLINE;

/// Hand-off queue for the receiver-scoped CONTROL lane (#2723). Unlike a
/// publisher lane this one aggregates the whole room — relayed heartbeats, the
/// join-time keyframe-request burst, probe echoes, oversized audio and all
/// Critical control — so the per-publisher size does not apply.
pub const WT_DOWNLINK_CONTROL_CHANNEL_CAPACITY: usize = 256;

/// Slots of the CONTROL lane's queue that only the never-preempted classes —
/// `Critical` and `Control` — may use (#2723).
///
/// Without a reserve a lane wedged with media tail-drops the lifecycle packets
/// #2718 routes onto the reliable stream. The protected set is exactly
/// `evaluate_dual`'s `Critical | Control` arm.
///
/// Applied to the control lane ONLY: `DownlinkStreamKey::for_media` never routes
/// a `Critical` or non-media `Control` frame to a publisher or overflow lane.
pub const WT_DOWNLINK_CONTROL_RESERVE: usize = 16;

/// Hand-off queue for the SHARED overflow lane (#2723). Carries every publisher
/// older than `PacketWrapper.media_kind` — whose above-MTU media the relay
/// cannot attribute — plus any keys past the publisher cap, so it is deeper than
/// a publisher lane but deliberately not deep enough to trade freshness for
/// backlog. No reserve: `Critical` and non-media `Control` never key here.
pub const WT_DOWNLINK_OVERFLOW_CHANNEL_CAPACITY: usize = 128;

/// Hand-off queue for the receiver-scoped AUDIO lane (#2724).
///
/// This lane does NOT run the #1638 shed
/// ([`DownlinkStreamClass::sheds_on_backpressure`]), so it is not that shed's
/// arming surface. Under congestion its protection is send priority, not a send
/// buffer (contract A18). No reserve: only audio keys here.
pub const WT_DOWNLINK_AUDIO_CHANNEL_CAPACITY: usize = 512;

/// Idle grace after which a publisher-keyed downlink stream is finished and
/// evicted (#2723). The relay has no per-peer "publisher left" fan-out event,
/// so absence of frames IS the signal; it must therefore stay above both GOP
/// intervals so an ordinary keyframe gap cannot reap a live stream.
pub const WT_DOWNLINK_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Cadence at which the downlink dispatcher sweeps for idle keys (#2723).
pub const WT_DOWNLINK_STREAM_IDLE_SWEEP: Duration = Duration::from_secs(2);

/// How long a reaped lane is given to exit on its own before the sweep aborts it
/// (#2723). Dropping its sender cancels nothing while the task is parked inside
/// `write_all`, so the abort is what makes the slot count the truth.
pub const WT_DOWNLINK_LANE_RETIRE_GRACE: Duration = WT_DOWNLINK_STREAM_IDLE_SWEEP;

/// Minimum spacing between replacements of a lane whose task has exited, so a
/// session whose `open_uni` is itself failing cannot spawn a task per frame.
pub const WT_DOWNLINK_LANE_RESPAWN_COOLDOWN: Duration = Duration::from_millis(500);

/// How long receiver teardown waits for every downlink lane to drain and
/// `finish` its stream before aborting the stragglers (#2723). A lane parked in
/// `open_uni` with no stream credit left has no bound of its own.
pub const WT_DOWNLINK_TEARDOWN_DRAIN: Duration = Duration::from_millis(2500);

/// A `write_timeout` shed inside the open round does not open a new one (#2726,
/// contract E1). Equal to [`WT_UNISTREAM_WRITE_DEADLINE`]: one lane cannot shed
/// faster, and a whole-map shed smears across at most that.
pub const WT_SHED_ESCALATION_ROUND: Duration = WT_UNISTREAM_WRITE_DEADLINE;

/// Rounds in [`WT_SHED_ESCALATION_STAGE1_WINDOW`] that arm stage 1.
pub const WT_SHED_ESCALATION_STAGE1_ROUNDS: usize = 3;

/// Sliding window the stage-1 round count is taken over (#2726).
pub const WT_SHED_ESCALATION_STAGE1_WINDOW: Duration = Duration::from_secs(10);

/// Rounds stage 1 must have been armed for before stage 2 may fire (#2726,
/// contract E17). Closing sooner would cut off the recovery stage 1 enables.
pub const WT_SHED_ESCALATION_STAGE1_RUNWAY_ROUNDS: usize = 4;

/// How long stage 1 holds after the last round (#2726). An ALIAS, not a copy:
/// one outliving #2718's base level would re-enable non-base layers with no
/// camera video at all.
pub const WT_SHED_ESCALATION_HOLD: Duration = RECEIVER_DOWNLINK_RELIEF_WINDOW;

/// Rounds in [`WT_SHED_ESCALATION_STAGE2_WINDOW`] that close the session.
pub const WT_SHED_ESCALATION_STAGE2_ROUNDS: usize = 10;

/// Memory bound on the retained round history (#2726). NOT the stage-2 bar,
/// which is a run contiguous within [`WT_SHED_ESCALATION_MAX_ROUND_GAP`].
pub const WT_SHED_ESCALATION_STAGE2_WINDOW: Duration = Duration::from_secs(30);

/// Largest gap between consecutive rounds still counted as one sustained wedge
/// (#2726, contract E18). Wide enough to survive one missed round, narrow
/// enough that a series of transient bursts never reaches the stage-2 bar.
pub const WT_SHED_ESCALATION_MAX_ROUND_GAP: Duration = Duration::from_secs(2);

/// Consecutive rounds with no lane write completing that reach stage 2 early
/// (#2726, contract E16). Nothing is being accepted against the peer's credit,
/// AUDIO included, so stage 1 has nothing left to free.
pub const WT_SHED_ESCALATION_DELIVERY_STALLED_ROUNDS: usize = 4;

// ---------------------------------------------------------------------------
// Inbound fan-out mailbox headroom (issues #1144 / #1145)
// ---------------------------------------------------------------------------

/// Multiplier applied to the per-receiver outbound-channel capacity when
/// sizing the actor MAILBOX that fronts it (issues #1144, #1145).
///
/// ## Background — the two-queue path
///
/// A fan-out packet to one receiver passes through two bounded producer-side
/// queues drained on the SAME single actor event loop:
///
/// ```text
/// NATS fan-out --try_send--> [actor MAILBOX] --Handler<Message>--> try_send--> [outbound channel] --> socket
///                            ^ dumb: indiscriminate                            ^ policy-aware: priority_drop
///                              drop on Full, no CONGESTION                       (video-first) + CONGESTION
/// ```
///
/// Issue #1057 sized the mailbox EQUAL to the outbound channel so the mailbox
/// stopped being the overflow point in front of the dumb-vs-smart asymmetry —
/// at mailbox == channel, a *steady-state* overflow lands on the policy-aware
/// channel instead of the indiscriminate mailbox.
///
/// ## Why #1057's equal sizing is still not enough for a publisher-join burst
///
/// Issue #1144 reproduced (on a build that ALREADY had the #1057 fix, WS
/// mailbox = 128) a transient where enabling ONE camera in a 3-person WS call
/// produced **303 `mailbox_full` drops in a single second** (then cleared
/// within ~10 s once the room settled). Adding a publisher triggers a
/// keyframe / join fan-out SPIKE: every receiver requests a keyframe from the
/// new sender and the burst arrives in a tight sub-second window — faster than
/// the actor is next scheduled to drain its mailbox. The mailbox fills during
/// that scheduling gap and drops indiscriminately, *before* the policy-aware
/// channel (whose `priority_drop` only runs at the channel-enqueue hop) ever
/// sees the traffic.
///
/// Critically, the mailbox→channel hand-off in `Handler<Message>` is
/// CPU-bound (parse + classify + `try_send` into the channel); it does NOT
/// block on the socket write (that happens separately in the outbound-drain
/// `StreamHandler`). So once the actor IS scheduled it drains the mailbox
/// quickly into the channel. The mailbox therefore only needs enough slack to
/// hold the burst across one scheduling gap and let it SPILL onto the
/// policy-aware channel — which then sheds video-first, protects audio, and
/// fires CONGESTION. This is the "relocate overflow onto the shedding surface"
/// direction #1145 calls for, NOT "buffer for a slow receiver" (the deep-queue
/// anti-pattern the [`WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT`] doc warns against —
/// that hazard is on the *channel*, which is unchanged here and still enforces
/// fail-fast video staleness bounds).
///
/// ## Sizing
///
/// `2×` doubles the burst-absorption slack while staying modest:
/// * WS: mailbox = 2 × [`WS_OUTBOUND_CHANNEL_CAPACITY`] (2048 post-#2261).
/// * WT: mailbox `unistream + datagram` (default 1536) → **3072**
///   (the deep-stale-video bound is the unistream lane's byte budget, so a
///   4096 mailbox does NOT create a 4096-deep stale-video buffer).
///
/// The factor is intentionally NOT large: this absorbs a single join-fan-out
/// wave for our target room sizes (10–15 meetings × ≤20 users), not unbounded
/// buffering. It does NOT, on its own, guarantee zero drops for the full
/// 303/s burst — a sustained over-arrival that exceeds the actor's drain
/// cadence will still spill, but it spills onto the SHEDDING channel
/// (video-first + CONGESTION) instead of the dumb mailbox. Fully eliminating
/// the transient requires the orthogonal follow-up of letting the socket
/// writer progress independently of `Handler<Message>` (out of scope for
/// #1144/#1145).
///
/// `2` is a FIRST, conservative value: validate against a multi-bot
/// publisher-join repro (sample the actor's intra-second drain cadence) before
/// raising it. Raising the mailbox far above the channel would re-introduce
/// the mailbox as a deep dumb buffer in front of the smart channel — the exact
/// thing #1057 removed — so keep this small.
pub const INBOUND_MAILBOX_HEADROOM_FACTOR: usize = 2;

/// The actix actor-mailbox capacity a `WsChatSession` installs in `started()`
/// via `ctx.set_mailbox_capacity(...)` (issues #1057 + #1144).
///
/// SINGLE SOURCE OF TRUTH (issue #1062): `WsChatSession::started` calls THIS
/// function, and the guard test asserts properties of THIS function — not a
/// parallel hand-copied constant. So altering the value passed to
/// `set_mailbox_capacity` means editing this one binding, which the test then
/// tracks. (The guard test cannot read the capacity back off a live
/// `WebsocketContext` without standing up NATS, so it pins the value the call
/// site feeds; this removes the prior drift hazard where a duplicated
/// `WS_MAILBOX_CAPACITY` test constant could diverge from `started()`.)
pub const fn ws_mailbox_capacity() -> usize {
    WS_OUTBOUND_CHANNEL_CAPACITY * INBOUND_MAILBOX_HEADROOM_FACTOR
}

/// The actix actor-mailbox capacity a `WtChatSession` installs in `started()`
/// via `ctx.set_mailbox_capacity(...)` (issues #1057 + PR #1060 review +
/// #1144).
///
/// SINGLE SOURCE OF TRUTH (issue #1062): `WtChatSession::started` calls THIS
/// function, so the value fed to `set_mailbox_capacity` is defined in exactly
/// one place. The WT mailbox fronts TWO policy-aware channels (unistream +
/// datagram) and a `Message` only splits between them AFTER leaving the
/// mailbox, so it is sized to the SUM of both channel capacities (not `max()`)
/// times the burst-headroom factor — see [`INBOUND_MAILBOX_HEADROOM_FACTOR`].
///
/// This reads the memoised, env-tunable [`wt_outbound_channel_capacity`]
/// (via the shared pure resolver [`resolve_wt_mailbox_capacity`]), so it
/// reflects any `WT_OUTBOUND_CHANNEL_CAPACITY` override the operator set. The
/// guard test exercises env-override behaviour through that same pure resolver
/// to avoid racing the `OnceLock`, and pins the default-env value against THIS
/// function.
pub fn wt_mailbox_capacity() -> usize {
    // Built from the SAME memoised getter the outbound channels are sized with
    // (`wt_outbound_channel_capacity()`), so the mailbox stays in lock-step with
    // the channel under any env override. The guard test asserts this equals
    // `resolve_wt_mailbox_capacity(None)` at the default env so the memoised
    // call-site path and the pure test path cannot drift (issue #1062).
    (wt_outbound_channel_capacity() + WT_DATAGRAM_CHANNEL_CAPACITY)
        * INBOUND_MAILBOX_HEADROOM_FACTOR
}

/// Pure resolver mirror of [`wt_mailbox_capacity`]: maps a raw optional
/// `WT_OUTBOUND_CHANNEL_CAPACITY` env string to the same mailbox capacity
/// `started()` would install, WITHOUT touching the memoised `OnceLock`.
///
/// The guard test verifies the env-override path (`Some("1024")`, etc.)
/// deterministically here, and separately asserts
/// `resolve_wt_mailbox_capacity(None) == wt_mailbox_capacity()` so this resolver
/// and the memoised call site [`wt_mailbox_capacity`] cannot drift — both apply
/// the identical `(unistream + datagram) × headroom` formula. Test-only: the
/// production call site uses the memoised [`wt_mailbox_capacity`].
#[cfg(test)]
pub(crate) fn resolve_wt_mailbox_capacity(raw: Option<&str>) -> usize {
    (resolve_wt_outbound_channel_capacity(raw) + WT_DATAGRAM_CHANNEL_CAPACITY)
        * INBOUND_MAILBOX_HEADROOM_FACTOR
}

// ---------------------------------------------------------------------------
// KEYFRAME_REQUEST Rate Limiting
// ---------------------------------------------------------------------------

/// Maximum number of KEYFRAME_REQUEST packets allowed per receiver session
/// within [`KEYFRAME_REQUEST_WINDOW_MS`], **across all target senders**.
///
/// This is a coarse, defense-in-depth ceiling that prevents a malicious or
/// malfunctioning client from issuing an unbounded fan-out of requests in a
/// short window. Per-target throttling is enforced separately by
/// [`KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER`]. This global cap is sized to
/// allow a fresh joiner to request keyframes from many existing senders
/// simultaneously without being clipped (legitimate behaviour during the
/// first second after joining a populated room) while still bounding abuse.
pub const KEYFRAME_REQUEST_MAX_PER_SEC: u32 = 32;

/// Maximum number of KEYFRAME_REQUEST packets allowed per
/// `(receiver, target_sender)` pair within [`KEYFRAME_REQUEST_WINDOW_MS`].
///
/// Sized to 1/sec because a healthy decoder should at most need a single
/// keyframe per second per remote stream. The global per-receiver cap above
/// still applies as a safety net. Per-pair limiting is what fixes the
/// frozen-video-on-join bug observed in cc7tp on 2026-05-06: with the prior
/// global-only limiter at 2/sec, a fresh joiner into a 17-peer meeting could
/// only get keyframes for the first 2 of the 16 existing senders.
pub const KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER: u32 = 1;

/// Relaxed per-`(receiver, target_sender)` KEYFRAME_REQUEST budget that
/// applies while the requesting receiver is in **active congestion**
/// (issue #979).
///
/// When the relay has recently had to drop inbound media destined for a
/// receiver (i.e. its [`CongestionTracker`] crossed the drop threshold
/// within [`KEYFRAME_CONGESTION_RELAX_WINDOW`]), that receiver's video is
/// the most likely to be frozen and in genuine need of a fresh keyframe to
/// recover. The normal steady-state per-pair budget of
/// [`KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER`] (1/sec) is too tight for
/// recovery: a single dropped keyframe response leaves the receiver frozen
/// for a full second before it may retry.
///
/// This raises the per-pair budget to 4/sec **only** for congested
/// receivers. It deliberately does NOT uncap the limiter: the global
/// per-receiver ceiling ([`KEYFRAME_REQUEST_MAX_PER_SEC`]) still applies
/// unchanged, so the pre-existing PLI/keyframe-storm risk (OSS #814:
/// WebTransport per-packet uni streams can amplify keyframe requests into a
/// storm) remains bounded. 4/sec is enough to recover within a few hundred
/// ms even if some requests are lost, while staying well under the global
/// cap and far short of a storm.
pub const KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_CONGESTED: u32 = 4;

/// Steady-state per-`(receiver, target_sender)` KEYFRAME_REQUEST budget for a
/// **SCREEN** stream (issue #1899).
///
/// The camera budget ([`KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER`], 1/sec) is
/// tuned for a stream that paints continuously: a dropped keyframe request only
/// costs a moment of sharpness because the next inter-frame repaints the tile,
/// and the tight 1/sec cap is what keeps a receiver from melting the publisher
/// with a PLI storm (OSS #814 / #1479). A **static** SCREEN share inverts that
/// cost/benefit: new content arrives ONLY on keyframes, so a receiver that
/// misses one (rejoin, layer switch, a single lost keyframe response) has NO
/// inter-frame fallback and holds its last-good frame FROZEN until the next
/// keyframe lands. Field evidence (meeting_sync 2026-07-21, 9 receivers, static
/// 720p@8fps share): every receiver froze 13–88s while audio stayed clean, and
/// the relay logged 106 camera-budget rate-limit drops against the broadcaster's
/// screen in the freeze window. At 1/sec a receiver could re-request at most
/// once per second, and the #1297 delivery-aware relaxation is DEFEATED here
/// because each re-encode of the retained static frame (a delta, useless without
/// its keyframe) is a SCREEN delivery that clears the still-waiting flag and
/// throws the receiver back onto the strict 1/sec budget.
///
/// `4`/sec: a static screen share is permanently in "recovery posture" (no
/// inter-frame fallback), so its STEADY-STATE budget is set to the camera's
/// RECOVERY rate ([`KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_CONGESTED`], the
/// value the repo already validated as a safe per-pair upper rate under the
/// global cap). That gives ~250ms re-request granularity so a frozen tile
/// recovers within a few hundred ms even if a keyframe response is lost, while
/// staying at 1/8 of the unchanged global per-receiver ceiling
/// ([`KEYFRAME_REQUEST_MAX_PER_SEC`], 32/sec). This raises the SCREEN budget
/// ONLY; VIDEO keeps its 1/sec budget byte-for-byte (the #1479 protection is
/// unchanged), because the two kinds already occupy separate limiter buckets
/// (#1297). It does NOT re-open the PLI-storm risk: the client publisher's SCREEN
/// encoder coalesces incoming PLIs at a 2s encoder-side cooldown
/// (`ENCODER_PLI_COOLDOWN_MS` = 2000ms, the #1287 emit coalescer in
/// `videocall-client/src/encode/screen_encoder.rs`), so however many PLIs the
/// relay forwards, the publisher emits at most one keyframe per cooldown — the
/// publisher coalescer, not this relay cap, is the storm backstop. The in-flight
/// #1903 client change adds a wall-clock retained-frame keyframe floor during
/// static periods, which only strengthens that backstop; this relay cap does not
/// depend on it.
pub const KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_SCREEN: u32 = 4;

/// Relaxed per-`(receiver, target_sender)` KEYFRAME_REQUEST budget for a
/// **SCREEN** stream while the requesting receiver is in **active congestion**
/// (issue #1899, the SCREEN analogue of
/// [`KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_CONGESTED`]).
///
/// Mirrors the camera design's "congested is strictly more permissive than
/// steady-state" relationship: a SCREEN share on a genuinely lossy link (its
/// [`CongestionTracker`] crossed the drop threshold within
/// [`KEYFRAME_CONGESTION_RELAX_WINDOW`]) may lose keyframe responses AND has no
/// inter-frame fallback, the worst case for a frozen tile. `8`/sec (2× the
/// SCREEN steady-state) buys headroom for lost responses while still sitting at
/// 1/4 of the unchanged global per-receiver ceiling
/// ([`KEYFRAME_REQUEST_MAX_PER_SEC`], 32/sec) — the ceiling is NEVER relaxed, so
/// the storm bound holds, and the publisher-side PLI coalescer (#1287, the 2s
/// `ENCODER_PLI_COOLDOWN_MS`) remains the actual backstop against publisher
/// meltdown under a reconnection wave. VIDEO congested behaviour is unchanged.
pub const KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_SCREEN_CONGESTED: u32 = 8;

/// How recently the requesting receiver must have been flagged congested
/// (its [`CongestionTracker`] crossed the drop threshold) for the relaxed
/// keyframe budget [`KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_CONGESTED`] to
/// apply (issue #979).
///
/// 2 seconds: long enough to cover the recovery window after a congestion
/// burst (during which the receiver is re-requesting keyframes to unfreeze)
/// without leaving the relaxed budget armed indefinitely once the link has
/// recovered. After this window the limiter reverts to the strict 1/sec
/// steady-state budget.
pub const KEYFRAME_CONGESTION_RELAX_WINDOW: Duration = Duration::from_secs(2);

/// Time window (in milliseconds) for KEYFRAME_REQUEST rate limiting.
pub const KEYFRAME_REQUEST_WINDOW_MS: u64 = 1000;

/// Stale-entry cleanup interval for the per-pair KEYFRAME_REQUEST limiter.
///
/// Cleanup runs every N requests (where N = this value) to amortize the
/// O(n) `retain()` cost. Mirrors the strategy used by `CongestionTracker`.
pub const KEYFRAME_LIMITER_CLEANUP_INTERVAL: u32 = 64;

/// Upper bound on the simulcast `layer` dimension of the KEYFRAME_REQUEST
/// limiter key (#1068, defense-in-depth).
///
/// The per-pair limiter keys on `(target_sender, layer)` so a receiver that
/// deliberately switches the simulcast layer it wants from a sender gets a
/// fresh per-layer budget instead of being throttled as a duplicate (#989,
/// Phase 1b). The `layer` comes from the cleartext, attacker-controllable
/// `PacketWrapper.simulcast_layer_id`, which is an unbounded `u32`. Without a
/// bound, a malicious receiver could cycle DISTINCT layer ids against a SINGLE
/// sender to open an unbounded number of fresh `(target, layer)` buckets — each
/// with its own [`KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER`] budget — and so
/// concentrate up to the GLOBAL per-receiver cap ([`KEYFRAME_REQUEST_MAX_PER_SEC`],
/// ~32/sec) of keyframe pressure on that one victim sender, amplifying the
/// PLI/keyframe-storm risk (OSS #814).
///
/// Clamping the layer component to `0..=this` (via `min`) bounds the number of
/// distinct per-layer buckets per target to `this + 1`, so per-victim keyframe
/// pressure is capped at `(this + 1) × KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER`
/// per window regardless of how many distinct layer ids an attacker cycles.
/// Ids above the bound collapse onto the top bucket (they share its budget)
/// rather than each opening a new one.
///
/// `2` matches the production ladder: every kind ships at most 3 simulcast
/// layers (ids 0,1,2 — see [`LAYER_PREFERENCE_MAX_LAYER_ID`]'s note), so all
/// REAL layer switches still get an independent bucket and the fix is invisible
/// to legitimate clients; only ids beyond the real ladder are clamped. This is
/// the keyframe-pressure bound, NOT the layer-preference value bound
/// ([`LAYER_PREFERENCE_MAX_LAYER_ID`]) — they protect different subsystems and
/// are intentionally separate constants.
pub const KEYFRAME_REQUEST_MAX_LAYER_ID: u32 = 2;

/// Compile-time link tying [`KEYFRAME_REQUEST_MAX_LAYER_ID`] to the ACTUAL
/// simulcast ladder depth (#1185).
///
/// `KEYFRAME_REQUEST_MAX_LAYER_ID` is the TOP real layer id, i.e. `ladder
/// depth - 1`. It is hand-set to `2` above because the production ladder ships
/// 3 layers (ids 0,1,2). Before this assert that pairing was purely a comment:
/// if the ladder grew (e.g. video → 5 layers, top id 4) and nobody bumped this
/// constant, the `layer.min(KEYFRAME_REQUEST_MAX_LAYER_ID)` clamp at
/// `packet_handler.rs` would SILENTLY collapse a real upper layer's keyframe
/// budget onto the id-2 bucket — a genuine functional regression with no build
/// failure to catch it (#1068's clamp depends on this bound equalling the real
/// ladder top).
///
/// The ladder depth's single source of truth is the `videocall-aq` crate
/// (`SIMULCAST_MAX_LAYERS` / `SCREEN_SIMULCAST_MAX_LAYERS`), which the browser
/// client (`layer_chooser.rs`, `camera_encoder.rs`) also derives its caps from.
/// `videocall-aq` builds on native targets (it is explicitly "shared between
/// the browser client and native consumers"), so the relay can reference it
/// directly here — this is the FIRST relay-side compile-time tie to the ladder
/// (the relay is otherwise deliberately layer-count-agnostic on the forwarding
/// path; this assert is a build-time guard, not runtime ladder knowledge).
///
/// VIDEO and SCREEN have independent caps; the keyframe limiter keys on the
/// cleartext `simulcast_layer_id` WITHOUT knowing the packet's media kind, so a
/// single clamp bound must cover the DEEPEST ladder across kinds. We therefore
/// tie to the MAX of the two caps: video ships 3 rungs and screen 1, so the
/// deepest is 3 and the top id is 2.
///
/// If either cap changes in `videocall-aq`, this assert FAILS the build with a
/// clear message until `KEYFRAME_REQUEST_MAX_LAYER_ID` (and the doc above) are
/// updated to match.
const _: () = {
    // Deepest ladder across the two video/screen caps the relay must cover.
    let max_ladder_depth = if videocall_aq::constants::SIMULCAST_MAX_LAYERS
        >= videocall_aq::constants::SCREEN_SIMULCAST_MAX_LAYERS
    {
        videocall_aq::constants::SIMULCAST_MAX_LAYERS
    } else {
        videocall_aq::constants::SCREEN_SIMULCAST_MAX_LAYERS
    };
    // Top real layer id == ladder depth - 1. Keep the keyframe clamp's bucket
    // ceiling (`KEYFRAME_REQUEST_MAX_LAYER_ID + 1` buckets) exactly equal to the
    // real ladder so no real layer id collapses onto a shared bucket.
    assert!(
        KEYFRAME_REQUEST_MAX_LAYER_ID as usize + 1 == max_ladder_depth,
        "KEYFRAME_REQUEST_MAX_LAYER_ID is out of sync with the simulcast ladder \
         (videocall_aq::constants::SIMULCAST_MAX_LAYERS / SCREEN_SIMULCAST_MAX_LAYERS). \
         It must equal max(ladder depths) - 1. Update KEYFRAME_REQUEST_MAX_LAYER_ID \
         and its doc comment to match the new ladder, then re-check the #1068 clamp \
         at packet_handler.rs."
    );
};

/// Minimum interval (in milliseconds) between two KEYFRAME_REQUESTs the
/// **delivery-aware** relaxation path will admit from a receiver that is STILL
/// WAITING for a keyframe (issue #1297).
///
/// The strict steady-state per-pair budget
/// ([`KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER`], 1/sec) throttles a genuinely
/// frozen receiver identically to a flooder, and its only relaxation path
/// ([`KEYFRAME_REQUEST_MAX_PER_SEC_PER_SENDER_CONGESTED`]) is not available
/// unless the receiver is already marked congested. On the common deployment
/// (small, all-WS,
/// capable HW, good network) a frozen receiver's legitimate recovery requests
/// were therefore dropped, leaving its video stuck frozen.
///
/// The fix tracks, per `(target, media_kind)` bucket, whether the relay has
/// observed a qualifying keyframe-bearing frame DELIVERED since the receiver's
/// last request. While the receiver is still waiting (no delivery seen), the
/// limiter admits a retry even when the strict per-pair budget is exhausted —
/// but no faster than this interval, so a receiver hammering faster than it is
/// still throttled. Once a frame is delivered the waiting flag clears and the
/// strict budget re-engages, so a receiver that keeps requesting AFTER recovery
/// is throttled again (a spammer-after-delivery cannot reopen the storm).
///
/// `200`ms: admits ~5 retries/sec while waiting — comfortably under the
/// unchanged global per-receiver ceiling ([`KEYFRAME_REQUEST_MAX_PER_SEC`],
/// 32/sec, which still bounds the still-waiting allow), and matched to
/// real-world links: on a 200ms+ RTT path (Change Impact Policy) a single
/// retry's round trip is ~200ms, so retrying faster than this cannot have
/// observed the previous request's result yet and would only add redundant
/// PLI pressure. 5 retries/sec recovers a frozen tile within a few hundred ms
/// even if some keyframe responses are themselves lost.
pub const KEYFRAME_REQUEST_STILL_WAITING_MIN_RETRY_MS: u64 = 200;

// ---------------------------------------------------------------------------
// REACTION Rate Limiting (issue #1884)
// ---------------------------------------------------------------------------

/// Maximum number of REACTION packets the relay forwards from a single sending
/// session within [`REACTION_WINDOW_MS`] (issue #1884).
///
/// A REACTION is a client-authored packet the relay RE-BROADCASTS to the whole
/// room on the media fan-out (unlike VIEWPORT/LAYER_PREFERENCE, which the relay
/// consumes and never re-broadcasts). That broadcast reach makes it a spam /
/// amplification surface, so — like the KEYFRAME_REQUEST path — every reaction
/// is metered per sender at a single tumbling-window bucket (there is no
/// per-target dimension: a reaction is aimed at the room, not one peer).
///
/// `4` per second is the RELAY ceiling. The browser client self-throttles
/// STRICTLY below it (≤3 per rolling 1000ms AND ≥350ms between sends — see
/// videocall-client's reaction self-throttle), so a well-behaved client never
/// reaches this cap; it exists to clamp a misbehaving or forged client. The
/// closed-enum validation in `classify_packet` runs BEFORE this limiter, so a
/// flood of invalid reactions is dropped without consuming a sender's valid
/// budget window.
pub const REACTION_MAX_PER_WINDOW: u32 = 4;

/// Time window (in milliseconds) for REACTION rate limiting (issue #1884).
///
/// Paired with [`REACTION_MAX_PER_WINDOW`] as the per-sender tumbling window.
/// 1000ms mirrors [`KEYFRAME_REQUEST_WINDOW_MS`]; the two are separate
/// constants because they meter unrelated surfaces (keyframe requests vs.
/// reaction broadcasts) and may diverge without affecting each other.
pub const REACTION_WINDOW_MS: u64 = 1000;

/// Maximum number of BYTES of a REACTION's cosmetic `display_name` the relay
/// will RE-BROADCAST (issue #1884).
///
/// `ReactionPacket.display_name` is an attacker-controlled `bytes` field the
/// relay fans out room-wide. The browser client caps it at 64 CHARACTERS on
/// send, but a modified/forged client can put an arbitrarily large value on the
/// wire — and because a REACTION is re-broadcast to every participant, an
/// oversized name is an egress-amplification surface (up to the frame limit ×
/// [`REACTION_MAX_PER_WINDOW`]/sec × O(participants)). The relay therefore
/// bounds it at ingress, independent of any client cooperation.
///
/// 256 bytes is deliberately generous — ~4× the client's 64-char contract, so a
/// legitimate multi-byte (e.g. CJK/emoji) display name is never truncated —
/// while still bounding amplification. The REACTION path TRUNCATES to this
/// bound (at a UTF-8 char boundary) rather than dropping: the reaction itself is
/// valid (its enum passed ingress validation) and the name is only a cosmetic
/// fallback, so discarding a valid reaction over an oversized cosmetic field
/// would be user-hostile. The client re-sanitizes and re-caps on consume.
pub const REACTION_DISPLAY_NAME_MAX_BYTES: usize = 256;

/// Maximum number of BYTES of a CUSTOM reaction's `custom_emoji` field the relay
/// will accept at ingress (issue #1884).
///
/// A CUSTOM reaction carries exactly ONE standard Unicode emoji in
/// `ReactionPacket.custom_emoji`. This cap mirrors the client's
/// `REACTION_CUSTOM_EMOJI_MAX_BYTES` (videocall-client's
/// `client/reactions.rs`); the two MUST stay in lockstep because both enforce
/// the SAME allowlist over the SAME `emojis` table.
///
/// 32 bytes admits every DEFAULT-skin-tone emoji the client picker offers — the
/// longest of those is 28 bytes (a tag-sequence flag, e.g. 🏴󠁧󠁢󠁳󠁣󠁴󠁿), leaving 4
/// bytes of headroom — while bounding a forged sender's decoded payload
/// independent of table contents. It does NOT admit the whole `emojis` table:
/// the 95 full-table skin-tone kiss-couple sequences are 35 bytes and are
/// DELIBERATELY rejected (fail-closed). That is not a coverage gap — the picker
/// never offers those variants, so a well-behaved client cannot send one, and a
/// crafted 35-byte payload is exactly what the cap is meant to reject. UNLIKE
/// `display_name`, an over-cap `custom_emoji` is DROPPED, not truncated: a
/// truncated emoji is not a valid emoji, so the whole reaction is invalid.
/// See [`crate::actors::packet_handler::custom_emoji_is_valid`].
pub const REACTION_CUSTOM_EMOJI_MAX_BYTES: usize = 32;

// ---------------------------------------------------------------------------
// RAISE_HAND Rate Limiting (issue #2135)
// ---------------------------------------------------------------------------

/// The relay's per-sender RAISE_HAND budget (issue #2135).
///
/// DEFINED IN `videocall-types`, not here, and re-exported so every
/// `crate::constants::RAISE_HAND_*` call site is unchanged. The move is
/// deliberate: the ceiling is a WIRE CONTRACT, not private server policy — the
/// client is required to keep its own send interval strictly under it, and the
/// test that pins that property lives in `videocall-client`, which cannot see
/// this crate. While the value lived here, that test could only hand-copy the
/// literals, so lowering the budget would not have failed anything.
///
/// See [`videocall_types::limits`] for the full rationale behind the values.
pub use videocall_types::limits::{RAISE_HAND_MAX_PER_WINDOW, RAISE_HAND_WINDOW_MS};

/// Maximum number of BYTES of a RAISE_HAND's cosmetic `display_name` the relay
/// will RE-BROADCAST (issue #2135).
///
/// Same value and same rationale as [`REACTION_DISPLAY_NAME_MAX_BYTES`]: the
/// field is attacker-controlled, fanned out room-wide, and generous enough
/// (~4× the client's 64-char contract) that a legitimate multi-byte name is
/// never truncated. Kept as a SEPARATE constant rather than aliasing the
/// reaction one so the two wire contracts can diverge without a silent
/// cross-feature regression; if you change one, changing the other must be a
/// deliberate decision, not a side effect.
///
/// TRUNCATES (at a UTF-8 char boundary) rather than dropping, for the same
/// reason as REACTION: the hand state itself is valid and the name is only a
/// cosmetic fallback, so discarding a real state transition over an oversized
/// cosmetic field would leave the room's view of that participant wrong.
pub const RAISE_HAND_DISPLAY_NAME_MAX_BYTES: usize = 256;

/// Maximum number of BYTES of the inner `RaiseHandPacket` payload
/// (`PacketWrapper.data`) the relay will accept at ingress (issue #2135).
///
/// A legitimate RaiseHandPacket is tiny. Worst case, field by field: `raised`
/// = 1 tag + 1 varint = 2 bytes; `raised_at_ms` = 1 tag + a 10-byte max u64
/// varint = 11 bytes; `display_name` = 1 tag + a 2-byte length varint +
/// [`RAISE_HAND_DISPLAY_NAME_MAX_BYTES`] (256) = 259 bytes. Total 272. 512
/// leaves ~2× headroom for a future field while keeping the bound meaningful.
///
/// WHY THIS EXISTS AT ALL — it is not redundant with the `display_name` cap.
/// rust-protobuf PRESERVES UNKNOWN FIELDS across parse/serialize (they live in
/// `special_fields` and are re-emitted on `write_to_bytes`), which is exactly
/// what we want for forward compatibility: a newer client's new field must
/// survive an older relay rather than being silently stripped. But it means the
/// `display_name` bound alone does NOT bound the packet — a forged RAISE_HAND
/// carrying megabytes of unknown fields would be re-broadcast verbatim to every
/// participant. This cap is the only thing that bounds that amplification, and
/// it is checked on the RAW wire bytes BEFORE any parse, so an oversized payload
/// costs the relay one length comparison rather than a full protobuf decode.
///
/// Over-cap DROPS (does not truncate): the bytes are opaque at this point, so
/// there is no safe place to cut. A well-behaved client cannot reach this cap
/// (its own display name is capped at 64 chars), so nothing legitimate is lost.
pub const RAISE_HAND_PACKET_MAX_BYTES: usize = 512;

// ---------------------------------------------------------------------------
// MEETING_TIMER ingress validation + rate limiting (issue #2136)
// ---------------------------------------------------------------------------

/// The relay's per-sender MEETING_TIMER budget and the client-side
/// `duration_ms` bound (issue #2136).
///
/// DEFINED IN `videocall-types`, not here, and re-exported so every
/// `crate::constants::MEETING_TIMER_*` call site is unchanged. Same reasoning as
/// the RAISE_HAND budget above: these are WIRE CONTRACT, not private server
/// policy. The host client must keep its heartbeat + transition-repeat rate
/// strictly under the ceiling, and must clamp `duration_ms` independently of
/// whatever relay it happens to be connected to — and the tests that pin both
/// live in `videocall-client`, which cannot see this crate. While the values
/// lived here those tests could only hand-copy the literals, so lowering the
/// budget would not have failed anything.
///
/// `MEETING_TIMER_PACKET_MAX_BYTES` deliberately does NOT move: it bounds an
/// ingress payload the relay alone enforces, and no client behaviour depends on
/// agreeing with it.
///
/// See [`videocall_types::limits`] for the full rationale behind the values.
pub use videocall_types::limits::{
    MEETING_TIMER_MAX_DURATION_MS, MEETING_TIMER_MAX_PER_WINDOW, MEETING_TIMER_WINDOW_MS,
};

/// Maximum number of BYTES of the inner `MeetingTimerPacket` payload
/// (`PacketWrapper.data`) the relay accepts at ingress (issue #2136).
///
/// A legitimate MeetingTimerPacket is tiny. Worst case, field by field:
/// `running` = 1 tag + 1 varint = 2 bytes; each of `ends_at_ms`, `duration_ms`
/// and `updated_at_ms` = 1 tag + a 10-byte max u64 varint = 11 bytes. Total 35.
/// 256 leaves ~7x headroom for future fields while keeping the bound meaningful.
///
/// WHY THIS EXISTS AT ALL — it is not redundant with the `duration_ms` cap, and
/// unlike REACTION/RAISE_HAND this packet has no string field to bound either.
/// rust-protobuf PRESERVES UNKNOWN FIELDS across parse/serialize (they live in
/// `special_fields` and are re-emitted on `write_to_bytes`), which is what we
/// want for forward compatibility: a newer client's new field must survive an
/// older relay rather than being silently stripped. But it means the scalar
/// bounds do NOT bound the payload — a forged MEETING_TIMER whose inner packet
/// carries megabytes of unknown fields would be re-broadcast verbatim to every
/// participant. This cap is what bounds that.
///
/// SCOPE, precisely (the earlier wording here overstated both halves and was
/// corrected): the check is `packet_wrapper.data.len()`, i.e. the INNER payload,
/// evaluated after `classify_packet` has already parsed the OUTER `PacketWrapper`
/// — it is "before the inner parse", not "before any parse", and it does not
/// save the outer decode. It also bounds the inner payload ONLY: unknown fields
/// on the OUTER wrapper are equally round-tripped by
/// `stamp_wrapper_for_broadcast` and are bounded only by `MAX_FRAME_SIZE`. That
/// outer surface is not specific to this packet type (it predates it and applies
/// to every class), and here it is reachable only by the authorized host, who
/// can already send multi-MB media frames — so it is noted rather than addressed
/// by this constant. Do not read this cap as a total-frame bound.
///
/// Over-cap DROPS (does not truncate): the bytes are opaque at this point, so
/// there is no safe place to cut.
pub const MEETING_TIMER_PACKET_MAX_BYTES: usize = 256;

/// Maximum number of `session_ids` the relay will accept from a single
/// VIEWPORT control packet (HCL issue #988).
///
/// `ViewportPacket.session_ids` is an unbounded `repeated uint64`. Because the
/// relay's NATS fan-out delivers every packet to every receiver, an attacker
/// spamming huge VIEWPORT lists would impose O(list length) collect work per
/// packet. This cap bounds that work. It is sized comfortably above the number
/// of camera tiles realistically visible at once in our target 20-user rooms
/// (a 20-tile grid leaves ample headroom), so legitimate clients are never
/// truncated. Packets exceeding the cap have their list truncated to the first
/// [`VIEWPORT_MAX_SESSION_IDS`] entries (fail-open on the excess rather than
/// rejecting the whole update).
pub const VIEWPORT_MAX_SESSION_IDS: usize = 64;

/// Minimum interval between accepted VIEWPORT updates for a single session
/// (HCL issue #988).
///
/// VIEWPORT packets are client-driven (viewport scroll / tile-visibility
/// changes) and should be infrequent. This throttle bounds how often a session
/// can mutate its desired-streams set, blunting a client that spams VIEWPORT
/// updates to force repeated set rebuilds. Updates that arrive sooner than this
/// after the last accepted one are dropped (the packet is still consumed and
/// never re-broadcast). 200ms = up to 5 viewport updates/sec, well above any
/// human-driven scroll cadence.
pub const VIEWPORT_MIN_UPDATE_INTERVAL: Duration = Duration::from_millis(200);

/// Maximum number of per-source layer-preference entries the relay will accept
/// from a single LAYER_PREFERENCE control packet (#989, Phase 1b).
///
/// `LayerPreferencePacket.entries` is an unbounded `repeated`. As with
/// [`VIEWPORT_MAX_SESSION_IDS`] the relay's NATS fan-out delivers every packet
/// to every receiver, so an attacker spamming a huge entries list would impose
/// O(list length) work per packet. This cap bounds that work. It is sized to
/// match the viewport cap (one layer preference per visible tile), so a
/// legitimate client rendering up to a 64-tile grid is never truncated.
/// Packets exceeding the cap have their list truncated to the first
/// [`LAYER_PREFERENCE_MAX_ENTRIES`] entries (fail-open on the excess rather
/// than rejecting the whole update).
pub const LAYER_PREFERENCE_MAX_ENTRIES: usize = VIEWPORT_MAX_SESSION_IDS;

/// Minimum interval between accepted LAYER_PREFERENCE updates for a single
/// session (#989, Phase 1b).
///
/// Mirrors [`VIEWPORT_MIN_UPDATE_INTERVAL`]: layer-preference packets are
/// client-driven (a receiver switching the layer it wants for a tile) and
/// should be infrequent. This throttle bounds how often a session can mutate
/// its layer-preference map, blunting a client that spams updates to force
/// repeated map rebuilds. Updates that arrive sooner than this after the last
/// accepted one are dropped (the packet is still consumed and never
/// re-broadcast).
pub const LAYER_PREFERENCE_MIN_UPDATE_INTERVAL: Duration = VIEWPORT_MIN_UPDATE_INTERVAL;

/// Upper bound on the `desired_layer` id the relay will record from a single
/// LAYER_PREFERENCE entry (#1082, defense-in-depth).
///
/// The relay is deliberately **layer-count-agnostic**: it never learns how many
/// simulcast layers a source actually produces (see the "AVAILABILITY NOT
/// VALIDATED" note on the forwarding path in `chat_server.rs`). It only compares
/// the receiver's recorded `desired_layer` against the cleartext
/// `simulcast_layer_id` on each media packet. That means a forged or garbage
/// LAYER_PREFERENCE could otherwise stuff an arbitrary `u32` into the per-source
/// layer map. Such an entry never matches any real packet, so the source's
/// non-base layers all get dropped and the forger self-degrades to base — but it
/// still consumes a map slot and represents nonsense state the relay should not
/// retain.
///
/// This bound caps the *value range* of a recorded layer id. It is NOT the real
/// layer count (which the relay does not and must not know): today every kind
/// ships at most 3 layers (ids 0..=2; #1082 keeps video=3/audio=3/content=3),
/// and even the assessed video=5 ceiling is ids 0..=4. `7` leaves comfortable
/// headroom for near-future ladders while still rejecting obviously-forged ids.
/// Entries whose `desired_layer` exceeds this bound are **skipped** (not
/// recorded) — fail-open per source: the receiver simply self-degrades to base
/// for that source, exactly as if no preference had been sent. The packet is
/// never dropped wholesale and the connection is never errored.
pub const LAYER_PREFERENCE_MAX_LAYER_ID: u32 = 7;

// ---------------------------------------------------------------------------
// Publish-side layer suppression (#1108, Stage 3)
// ---------------------------------------------------------------------------

/// Debounce window (in milliseconds) before the relay emits a LOWER layer-union
/// hint to a publisher (#1108, Stage 3 — publish-side layer suppression).
///
/// The relay computes, per source, the UNION (max) over every receiver of the
/// simulcast layer that receiver requested, and emits a LAYER_HINT telling the
/// publisher it may stop encoding layers above that union (see
/// [`crate::actors::chat_server`] `RecomputeLayerHints`). The emit policy is
/// deliberately ASYMMETRIC:
///
/// * **Suppress-lazy (DOWN):** a hint that LOWERS the union below what the
///   publisher is currently encoding is only emitted after the union has stayed
///   below that level for this entire window. This absorbs transient flaps — a
///   receiver briefly dropping a tile, a viewport scroll, a reconnect wave —
///   so we do not tell a publisher to tear down an upper encode that a receiver
///   re-requests a few hundred ms later (re-spinning a simulcast layer is
///   expensive and visibly stutters every consumer of it). The debounce is
///   realized with a deferred `notify_later` re-check, so the lower hint fires
///   even when no further preference change occurs.
/// * **Restore-eager (UP):** a hint that RAISES the union (a receiver now wants
///   a higher layer, or a constraining receiver left so the fail-open union
///   grows) is emitted IMMEDIATELY — never debounced. Delaying restoration
///   would leave a receiver black-tiled / stuck on a low layer for the window;
///   over-encoding briefly is the safe failure (fail-open).
///
/// 2000 ms is a FIRST GUESS and is PENDING PERF REVIEW. It is long enough to
/// ride out a reconnection wave on a high-latency (200 ms+) link and short
/// viewport flaps, while short enough that a genuine, sustained drop in demand
/// reclaims publisher CPU / uplink within a couple of seconds. Tune against real
/// traffic once Stage 3 is wired end-to-end (it mirrors the order of magnitude
/// of the keyframe congestion-relax window but is intentionally separate).
pub const LAYER_HINT_SUPPRESS_DEBOUNCE_MS: u64 = 2000;

/// Maximum number of receiver sessions the relay will scan when computing the
/// per-source layer union for a LAYER_HINT (#1108, Stage 3 — DoS bound).
///
/// The union is an INVERTED query: for one source it must inspect every other
/// receiver's recorded layer preference for that source (the prefs map is
/// receiver-keyed, so there is no per-source index). That scan is O(room size)
/// and runs inside the single-threaded `ChatServer` actor, so an adversary who
/// could inflate a room's membership could otherwise make each recompute
/// arbitrarily expensive and stall the actor for every room it serves.
///
/// This caps the scan at a fixed number of receivers. Mirrors the
/// [`LAYER_PREFERENCE_MAX_ENTRIES`] philosophy (bound the per-event work an
/// attacker can induce) and is sized well above our target 20-user rooms with
/// comfortable headroom, so a legitimate meeting's union is always computed over
/// every real receiver. When a room exceeds the cap the union is computed over
/// the first [`LAYER_HINT_MAX_RECEIVERS_SCANNED`] receivers encountered and is
/// FAIL-OPEN on the remainder: an un-scanned receiver is treated exactly like a
/// receiver with no recorded preference (it contributes the full-ladder
/// sentinel), so truncation can only ever cause the relay to suppress LESS, never
/// to suppress a layer some unseen receiver still wants. FIRST GUESS / PENDING
/// PERF REVIEW.
pub const LAYER_HINT_MAX_RECEIVERS_SCANNED: usize = 256;

/// Trailing-debounce window (in milliseconds) for COALESCING room-wide
/// LAYER_HINT recomputes triggered by DEPARTURES (leave / evict) (#1203).
///
/// ## The O(n) storm this absorbs
///
/// A room-wide recompute (`RecomputeLayerHints { source: None }`) fans out over
/// every publisher in the room, and each per-source union scan is itself
/// O(receivers) (see [`LAYER_HINT_MAX_RECEIVERS_SCANNED`]). So a single
/// room-wide recompute is O(publishers × receivers). The relay fires one such
/// recompute per DEPARTURE (the `leave_rooms` and `forget_session`/evict paths).
/// A reconnection wave or a meeting ending disconnects many sessions in a tight
/// burst, firing the handler once PER departing connection — an O(n) storm that
/// runs inside the single-threaded `ChatServer` actor and stalls every room it
/// serves (the exact O(n)-per-connection fan-out hazard the Change Impact Policy
/// warns about).
///
/// ## Debounce policy per recompute trigger
///
/// The emit policy has three tiers (per-LAYER_PREFERENCE is immediate; joins
/// and departures are both debounced behind this trailing window):
///
/// * **Departures (leave/evict) → DEBOUNCE.** A leaving receiver can only RAISE
///   a remaining publisher's fail-open union (its constraint disappears), and a
///   leaving publisher's own per-source state is reaped synchronously regardless.
///   A raise is "restore-eager" demand, but NOBODY is actively waiting on a
///   departure-driven recompute: the union only governs whether the relay tells a
///   publisher it MAY drop an upper layer (suppress). Coalescing a burst of
///   departures into ONE trailing recompute computes the correct FINAL union once
///   the burst settles, instead of N times over transient intermediate
///   membership. Delaying it by this window cannot black-tile anyone (a publisher
///   over-encoding for a few hundred ms is the fail-open-safe direction).
///
/// * **Joins → DEBOUNCED (same window as departures, issue #1288).** A join can
///   only RAISE the union (fail-open demand), which is the restore-eager
///   direction. The 300ms delay is acceptable because: (a) a new receiver's NATS
///   subscription setup takes ~100-200ms before media flows, (b) the publisher's
///   AQ controller needs an encoder tick + keyframe to re-enable upper layers
///   (~300-1000ms), so the recompute delay is subsumed by the encoder reaction.
///   Meanwhile, a join burst of K participants (call start, reconnection wave)
///   collapses K separate O(publishers × receivers) recomputes into 1, avoiding
///   actor starvation on the ChatServer's serialized mailbox.
///
/// * **Per-LAYER_PREFERENCE recompute → IMMEDIATE (never debounced).** That path
///   is the latency-sensitive UPGRADE case and is already rate-limited upstream by
///   [`LAYER_PREFERENCE_MIN_UPDATE_INTERVAL`]; debouncing it would slow real
///   viewport-driven layer switches.
///
/// ## Why 300 ms
///
/// 300 ms is an order of magnitude below the ~5 s receiver chooser / AQ
/// adaptation loop, so it CANNOT delay simulcast convergence — it only dedups a
/// sub-second departure burst into a single recompute. It is long enough to
/// swallow a reconnection wave's disconnect cluster (which arrives within tens to
/// low-hundreds of ms) yet short enough that a genuine sustained drop in demand
/// still reclaims publisher CPU/uplink well within a second. It sits comfortably
/// under [`LAYER_HINT_SUPPRESS_DEBOUNCE_MS`] (2000 ms): the coalesce window only
/// decides WHEN the union is recomputed; the suppress-lazy debounce then still
/// governs whether a LOWER hint is actually emitted, so the two debounces
/// compose without double-counting.
pub const LAYER_HINT_RECOMPUTE_COALESCE_MS: u64 = 300;

/// Trailing-debounce window for coalescing cross-server presence re-announces.
///
/// On a join, a peer publishes one `PARTICIPANT_LIST_REQUEST` and every existing
/// active peer answers with its own `PARTICIPANT_JOINED`. A reconnection wave of
/// M joiners would otherwise make each of N peers publish M replies. Instead a
/// peer records itself once and arms one trailing timer of this length; when it
/// fires, the flush re-announces that peer exactly once, so the wave's publishes
/// drop from M·N to N. (The M·N inbound request messages still arrive, but the
/// arm path is a cheap O(1) per message.)
///
/// The flush broadcasts to `room.{room}.system` for a wave (≥2 distinct
/// requesters) but unicasts to the lone requester for an ordinary single join, so
/// the common path keeps its O(N) relay→client fan-out instead of O(N²).
///
/// This is safe for late joiners: the join flow subscribes to the room wildcard
/// before publishing its request, so any joiner counted in the window is
/// subscribed before the trailing re-announce fires. A broadcast's already-present
/// recipients get the same packet they got at activation and dedup it.
///
/// Separate from [`LAYER_HINT_RECOMPUTE_COALESCE_MS`] (unrelated concern) but the
/// same 300 ms reasoning: long enough to swallow a wave's request cluster, short
/// enough that a late joiner still learns every peer well within a second.
pub const PARTICIPANT_REBROADCAST_COALESCE_MS: u64 = 300;

/// Minimum interval between two consecutive re-announces of the SAME responder,
/// enforced ACROSS coalescing windows (#1600 item 1).
///
/// [`PARTICIPANT_REBROADCAST_COALESCE_MS`] caps a responder at one re-announce
/// *per window*, but places no floor *between* windows: a peer that re-publishes
/// `PARTICIPANT_LIST_REQUEST` every ~300 ms could drive a chosen responder at
/// ~3.3 Hz indefinitely. This constant is that floor, so #1039's "cannot be made
/// to re-publish unboundedly" holds in the strict sense.
///
/// A re-announce that arrives while the responder is cooling down is **deferred,
/// never dropped** — the flush keeps the pending entry and re-arms for the
/// remaining cooldown, so no joiner loses its presence answer.
///
/// ## Why 1000 ms
///
/// * **Floor:** it must exceed [`PARTICIPANT_REBROADCAST_COALESCE_MS`] (300 ms),
///   or it adds nothing the trailing debounce does not already do. 1000 ms is
///   3.3× that window.
/// * **Ceiling:** the deferral stays invisible to users only while it fits
///   inside the joiner's own RTT election period — a joiner renders no peers
///   until it elects a connection. That period is 2000 ms
///   (`SERVER_ELECTION_PERIOD_MS:-2000` in `docker/start-dioxus.sh`, matching the
///   `server_election_period_ms` default the UI falls back to). Budgeting 300 ms
///   for the coalescing window plus ~400 ms for two 200 ms+ WAN/NATS legs
///   (request out, re-announce back) leaves ~1300 ms of headroom; 1000 ms keeps
///   ~300 ms of slack on top of that on a high-latency link.
/// * **Adversarial worst case:** a request that lands the instant after its
///   responder published waits the FULL cooldown, so the end-to-end worst case is
///   300 ms (window) + 1000 ms (cooldown) + ~400 ms (two WAN/NATS legs) ≈
///   **1700 ms** — still inside the 2000 ms election period, with ~300 ms of
///   margin rather than the ~1300 ms the non-adversarial case enjoys.
/// * **Effect:** a single requester looping requests at 300 ms drives a
///   responder at ≤1 re-announce/s instead of ~3.3/s.
///
/// ## Operators: this is COUPLED to `SERVER_ELECTION_PERIOD_MS`
///
/// Nothing enforces the relationship at build or start time — there is no assert,
/// no test reading both (the election period is a UI/runtime env var, not a Rust
/// constant), and no startup warning. The invariant is therefore yours to keep:
///
/// > `PARTICIPANT_REBROADCAST_MIN_INTERVAL_MS` ≤ `SERVER_ELECTION_PERIOD_MS` −
/// > ~700 ms (the 300 ms coalescing window plus ~400 ms of WAN/NATS legs).
///
/// At the 2000 ms default that permits up to ~1300 ms and 1000 ms is comfortable.
/// **If `SERVER_ELECTION_PERIOD_MS` is lowered below ~1700 ms, this constant MUST
/// be lowered with it**, otherwise a deferred re-announce can land after the
/// joiner has already elected its connection and the joiner renders a room that
/// is missing a peer until something else re-announces.
pub const PARTICIPANT_REBROADCAST_MIN_INTERVAL_MS: u64 = 1000;

/// How many distinct requester SESSIONS a single deduped requester identity may
/// be unicast to before the flush falls back to one broadcast (#1600 item 2).
///
/// One client's candidate sessions during RTT election share an `instance_id` but
/// hold distinct `session_id`s, and the re-announce must reach **every** one of
/// them: only the elected connection survives, the relay cannot know which that
/// will be, and the client drops inbound packets from non-elected connections
/// once election completes. So the flush unicasts per candidate session rather
/// than picking one.
///
/// ## Why 4 is still the right cap
///
/// Capping only pays where a broadcast is genuinely cheaper. A broadcast is one
/// NATS publish but `S` relay→client deliveries for a room of `S` sessions, while
/// the unicast fan is `C` publishes to just the sessions that asked — so unicast
/// wins while `C < S`. At `C = 4` that holds for every room larger than 4
/// sessions, i.e. exactly the rooms where the O(N²) fan-out #1600 removes would
/// hurt. Past the cap a single broadcast is both cheaper and safer than a growing
/// fan, and it is exactly what the pre-#1600 code did — which is also what makes
/// the cap fail SAFE: a broadcast reaches a superset of the recorded targets, so
/// no requester is dropped by hitting it.
pub const PARTICIPANT_REBROADCAST_MAX_UNICAST_TARGETS: usize = 4;

/// Period of the [`ChatServer`](crate::actors::chat_server) sweep that publishes
/// the DEMAND-side simulcast gauge `relay_layer_preference_sessions{room, kind,
/// layer_id}` (#1170 item 2).
///
/// ## Why a periodic sweep (not event-driven)
///
/// The gauge is a STATE snapshot ("how many sessions currently request each
/// layer"), not an event rate, so it is naturally a poll: one read-only pass
/// over the live rooms that re-SETs each `{room, kind, layer_id}` cell. Driving
/// it off every LAYER_PREFERENCE update / join / leave would couple a metric to
/// the hot control path and add work to the O(n) reconnection storms the
/// [`LAYER_HINT_RECOMPUTE_COALESCE_MS`] debounce already exists to absorb. A
/// gauge that lags real demand by roughly one cycle plus the bounded
/// continuation time is correct for dashboards.
///
/// ## Why 10 s
///
/// 10 s remains appropriate at dashboard timescales without making the sweep
/// chatty. Demand shifts become visible after the next completed cycle. A cycle
/// is split into bounded mailbox messages (see
/// [`LAYER_PREFERENCE_SESSIONS_SWEEP_ROOMS_PER_MESSAGE`]), so the cadence controls
/// snapshot freshness without making any single actor turn O(rooms).
pub const LAYER_PREFERENCE_SESSIONS_SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// Maximum live rooms refreshed by one layer-preference gauge sweep mailbox
/// message (#1284).
///
/// A room scan is itself bounded by [`LAYER_HINT_MAX_RECEIVERS_SCANNED`]. Capping
/// this second dimension prevents thousands of otherwise-small rooms from
/// monopolizing the single-threaded chat-server actor in one turn. Continuation
/// messages are appended to the mailbox, allowing connection and media-control
/// work already queued there to run between chunks. The timer attempts a new
/// round-robin cycle every [`LAYER_PREFERENCE_SESSIONS_SWEEP_INTERVAL`]; if an
/// unusually large prior cycle is still draining, that start is skipped rather
/// than duplicating work.
pub const LAYER_PREFERENCE_SESSIONS_SWEEP_ROOMS_PER_MESSAGE: usize = 8;

// ---------------------------------------------------------------------------
// Receiver Downlink Congestion (#1219 Half 2)
// ---------------------------------------------------------------------------

/// How recently THIS receiver's downlink must have overflowed for the relay to
/// keep it in downlink-relief mode (#1219 Half 2).
///
/// ## What drives the signal (B1)
///
/// The relay enters relief mode for a receiver when that receiver's REAL
/// downlink backpressure surface — the bounded per-session `outbound_tx`
/// channel overflowing on a slow socket — fires an outbound drop
/// (`SessionLogic::on_outbound_drop`). The transport actor stamps a monotonic
/// epoch into a shared atomic on EVERY drop unconditionally (#1481), and the
/// per-receiver fan-out closure reads it against THIS window. While the most
/// recent drop is within the window, the closure (a) discards non-base-layer
/// camera VIDEO BEFORE `try_send`, giving the downlink headroom to drain, and
/// (b) emits one DOWNLINK_CONGESTION control packet so the client's
/// LayerChooser steps its own receive layers down. SCREEN is protected here per
/// issue 1977 (the shared content outranks cameras; its relief is the
/// priority_drop 90% fill backstop, one rung above camera VIDEO's 80%). AUDIO
/// and base layer are NEVER shed.
///
/// This is DELIBERATELY NOT keyed off the relay's actor-mailbox `Full` (which an
/// earlier draft used): the mailbox sits in front of `outbound_tx` and overflows
/// on a room-wide fan-out / scheduling burst that says nothing about any single
/// receiver's downlink. The per-receiver outbound channel overflow is the
/// genuine per-receiver signal.
///
/// ## Why a windowed decay, not a consecutive-success exit (B2)
///
/// Relief is a windowed LEVEL: it lapses on its own once this window elapses
/// with no fresh downlink overflow, exactly like
/// [`KEYFRAME_CONGESTION_RELAX_WINDOW`]. There is NO strictly-consecutive
/// success counter — an earlier draft required N clean-in-a-row deliveries to
/// exit, which on a link with even occasional drops could reset forever and pin
/// a perfectly healthy receiver at base-layer-only video indefinitely. A
/// time-decaying window recovers automatically: once the receiver stops
/// overflowing for `RECEIVER_DOWNLINK_RELIEF_WINDOW`, full layers resume.
///
/// ## Why 8 s (widened from 2 s in #1481)
///
/// Field data (CC7, 2026-06-18) showed the shed flapping on WebTransport in a
/// cycle: shed activates → drops stop (works!) → epoch ages over 2 s → shed
/// deactivates → L1+L2 resume → buffer slowly refills over 10-14 s → overflow
/// → shed reactivates. The 2 s window was too short to bridge the quiet gap
/// that naturally follows a successful shed on a constrained downlink.
///
/// 8 s bridges the typical 5-10 s quiet gap after a successful shed. The
/// buffer drains in 2-5 s (only L0 = low bitrate), so 8 s gives 3-6 s of
/// additional hold after the buffer is drained — enough for the receiver to
/// stabilize. Recovery is still bounded: if drops truly stop (link improves),
/// shedding releases after 8 s.
///
/// NOTE: [`KEYFRAME_CONGESTION_RELAX_WINDOW`] (2 s) is a SEPARATE concern
/// (per-pair keyframe budget expansion) and is intentionally NOT changed here.
/// They were previously equal but serve different purposes: the keyframe relax
/// window arms a short budget burst for immediate recovery retries, while the
/// downlink relief window holds the L1/L2 shed for sustained stabilization.
pub const RECEIVER_DOWNLINK_RELIEF_WINDOW: Duration = Duration::from_secs(8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_wt_outbound_channel_capacity_unset_uses_default() {
        assert_eq!(
            resolve_wt_outbound_channel_capacity(None),
            WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
        );
    }

    #[test]
    fn resolve_wt_outbound_channel_capacity_valid_value_used_verbatim() {
        // Neither equals the default, or a silently-ignored env would pass.
        assert_eq!(resolve_wt_outbound_channel_capacity(Some("2048")), 2048);
        assert_eq!(resolve_wt_outbound_channel_capacity(Some("8192")), 8192);
        assert_ne!(2048, WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT);
    }

    #[test]
    fn wt_outbound_channel_capacity_default_is_1024() {
        // Sentinel (#979, #2717): change the doc, helm overlays and operator
        // docs first, then this assertion.
        assert_eq!(WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT, 1024);
    }

    #[test]
    fn resolve_wt_outbound_channel_capacity_garbage_falls_back_to_default() {
        assert_eq!(
            resolve_wt_outbound_channel_capacity(Some("abc")),
            WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
        );
    }

    #[test]
    fn resolve_wt_outbound_channel_capacity_zero_falls_back_to_default() {
        // A literal "0" must be rejected; mpsc::channel(0) panics.
        assert_eq!(
            resolve_wt_outbound_channel_capacity(Some("0")),
            WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
        );
    }

    #[test]
    fn resolve_wt_outbound_channel_capacity_negative_falls_back_to_default() {
        assert_eq!(
            resolve_wt_outbound_channel_capacity(Some("-1")),
            WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
        );
    }

    #[test]
    fn resolve_wt_outbound_channel_capacity_empty_falls_back_to_default() {
        assert_eq!(
            resolve_wt_outbound_channel_capacity(Some("")),
            WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT
        );
    }

    #[test]
    fn resolve_viewport_filter_enabled_unset_defaults_to_true() {
        // #988 filter is already LIVE in prod; the #1436 kill switch must
        // default to the status quo (filter ON) when unset.
        assert!(resolve_viewport_filter_enabled(None));
    }

    #[test]
    fn resolve_audio_downlink_lane_defaults_and_fails_safe() {
        assert_eq!(
            resolve_audio_downlink_lane(None),
            AudioDownlinkLane::Reliable
        );
        for unrecognised in [
            Some(""),
            Some("0"),
            Some("off"),
            Some("datagrm"),
            Some("true"),
        ] {
            assert_eq!(
                resolve_audio_downlink_lane(unrecognised),
                AudioDownlinkLane::Reliable,
                "{unrecognised:?} must not turn the lossy route back on",
            );
        }
    }

    #[test]
    fn resolve_audio_downlink_lane_accepts_the_documented_values() {
        for raw in ["datagram", "DATAGRAM", " datagrams ", "Datagram"] {
            assert_eq!(
                resolve_audio_downlink_lane(Some(raw)),
                AudioDownlinkLane::Datagram,
                "{raw:?} is the documented revert",
            );
        }
        for raw in ["reliable", "RELIABLE", " stream "] {
            assert_eq!(
                resolve_audio_downlink_lane(Some(raw)),
                AudioDownlinkLane::Reliable,
            );
        }
    }

    #[test]
    fn resolve_viewport_filter_enabled_recognised_false_values_disable() {
        assert!(!resolve_viewport_filter_enabled(Some("false")));
        assert!(!resolve_viewport_filter_enabled(Some("0")));
        assert!(!resolve_viewport_filter_enabled(Some("off")));
        assert!(!resolve_viewport_filter_enabled(Some("no")));
    }

    #[test]
    fn resolve_viewport_filter_enabled_recognised_true_values_enable() {
        assert!(resolve_viewport_filter_enabled(Some("true")));
        assert!(resolve_viewport_filter_enabled(Some("1")));
        assert!(resolve_viewport_filter_enabled(Some("on")));
        assert!(resolve_viewport_filter_enabled(Some("yes")));
    }

    #[test]
    fn resolve_viewport_filter_enabled_is_case_insensitive() {
        assert!(!resolve_viewport_filter_enabled(Some("FALSE")));
        assert!(resolve_viewport_filter_enabled(Some("True")));
    }

    #[test]
    fn resolve_viewport_filter_enabled_garbage_falls_back_to_default_true() {
        // Unrecognised tokens warn and fall back to the default (ON).
        assert!(resolve_viewport_filter_enabled(Some("GARBAGE")));
    }

    #[test]
    fn resolve_viewport_filter_enabled_empty_falls_back_to_default_true() {
        // "" trims to "", matches no arm, warns, and defaults to true.
        // It is NOT special-cased.
        assert!(resolve_viewport_filter_enabled(Some("")));
    }

    #[test]
    fn viewport_should_drop_kill_switch_off_forwards_all() {
        // enabled=false: forward-all restored regardless of set membership.
        let ids: std::collections::HashSet<u64> = [1, 2, 3].into_iter().collect();
        assert!(!viewport_should_drop(false, &ids, Some(99)));
    }

    #[test]
    fn viewport_should_drop_enabled_drops_source_not_in_set() {
        // enabled=true, non-empty set EXCLUDING the source -> drop.
        let ids: std::collections::HashSet<u64> = [1, 2, 3].into_iter().collect();
        assert!(viewport_should_drop(true, &ids, Some(99)));
    }

    #[test]
    fn viewport_should_drop_enabled_keeps_source_in_set() {
        // enabled=true, source present in the set -> forward.
        let ids: std::collections::HashSet<u64> = [1, 2, 99].into_iter().collect();
        assert!(!viewport_should_drop(true, &ids, Some(99)));
    }

    #[test]
    fn viewport_should_drop_enabled_empty_set_fails_open() {
        // enabled=true, empty set (no viewport signal yet) -> fail open.
        let ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
        assert!(!viewport_should_drop(true, &ids, Some(99)));
    }

    #[test]
    fn viewport_should_drop_enabled_none_source_fails_open() {
        // enabled=true, unparseable source (None) -> fail open.
        let ids: std::collections::HashSet<u64> = [1, 2, 3].into_iter().collect();
        assert!(!viewport_should_drop(true, &ids, None));
    }

    #[test]
    fn nonvideo_reached_viewport_drop_branch_video_does_not_trip() {
        use videocall_types::protos::packet_wrapper::packet_wrapper::MediaKind;
        // VIDEO is the only kind that legitimately reaches the viewport drop
        // branch — the tripwire must NOT fire.
        assert!(!nonvideo_reached_viewport_drop_branch(Ok(MediaKind::VIDEO)));
    }

    #[test]
    fn nonvideo_reached_viewport_drop_branch_nonvideo_and_err_trip() {
        use videocall_types::protos::packet_wrapper::packet_wrapper::MediaKind;
        // AUDIO, SCREEN, and an unknown/unparseable kind (Err) are all the
        // impossible case — each must trip. MUTATION GUARD: if the helper is
        // neutered to always return `false` (the "guard removed" mutation),
        // every assert below fails.
        assert!(nonvideo_reached_viewport_drop_branch(Ok(MediaKind::AUDIO)));
        assert!(nonvideo_reached_viewport_drop_branch(Ok(MediaKind::SCREEN)));
        assert!(nonvideo_reached_viewport_drop_branch(Err(999)));
    }

    /// Every per-receiver queue on BOTH transports, at the one-layer rate.
    #[test]
    fn per_receiver_queues_absorb_the_target_room_audio_fanout() {
        const _: () = assert!(
            AUDIO_PUBLISHED_LAYER_COUNT < AUDIO_SIMULCAST_RUNGS,
            "the fan-out multiplier below is the PUBLISHED layer count, not the \
             dormant ladder depth. If the ladder is re-enabled (#2620/#2621) \
             every queue below must be re-sized against the new rate rather \
             than silently absorbing a fraction of it",
        );

        let pps = audio_fanout_packets_per_sec(
            RELAY_SIZING_TARGET_PARTICIPANTS,
            AUDIO_PUBLISHED_LAYER_COUNT,
            AUDIO_PACKETS_PER_SEC_PER_RUNG,
        );
        assert_eq!(pps, 1_300, "26 peers x 1 published audio layer x 50 pkt/s");

        // PURE resolvers, not the memoised getters: a CI host exporting
        // `WT_OUTBOUND_CHANNEL_CAPACITY` would otherwise swing this test.
        let wt_unistream = resolve_wt_outbound_channel_capacity(None);
        let wt_mailbox =
            (wt_unistream + WT_DATAGRAM_CHANNEL_CAPACITY) * INBOUND_MAILBOX_HEADROOM_FACTOR;

        let queues: [(&str, usize); 5] = [
            ("WS outbound channel", WS_OUTBOUND_CHANNEL_CAPACITY),
            ("WS actor mailbox", ws_mailbox_capacity()),
            ("WT unistream channel", wt_unistream),
            ("WT datagram channel", WT_DATAGRAM_CHANNEL_CAPACITY),
            ("WT actor mailbox", wt_mailbox),
        ];

        for (name, slots) in queues {
            let ms = queue_absorption_millis(slots, pps);
            assert!(
                ms >= RELAY_QUEUE_ABSORPTION_TARGET_MS,
                "{name} ({slots} slots) absorbs only {ms}ms of a {pps} pkt/s \
                 audio fan-out; target is {RELAY_QUEUE_ABSORPTION_TARGET_MS}ms",
            );
        }

        // The unistream lane must match the WS channel: both are the ordered
        // media lane. The 250ms floor alone would not pin that.
        let ws_ms = queue_absorption_millis(WS_OUTBOUND_CHANNEL_CAPACITY, pps);
        assert_eq!(
            queue_absorption_millis(wt_unistream, pps),
            ws_ms,
            "the WT unistream lane must absorb the same {ws_ms}ms of fan-out \
             the WS channel does",
        );

        // The datagram lane is held to the 250ms target only: it absorbs
        // scheduling jitter, not receiver congestion.
        let datagram_ms = queue_absorption_millis(WT_DATAGRAM_CHANNEL_CAPACITY, pps);
        assert!(
            datagram_ms < ws_ms,
            "the datagram lane holds {datagram_ms}ms against the media lanes' \
             {ws_ms}ms; if it has been grown to parity, re-read why it is a \
             jitter absorber and not a congestion buffer",
        );
    }

    /// One top-tier audio packet in the shape
    /// `microphone_encoder::transform_audio_chunk` produces and
    /// `session_logic::handle_outbound` forwards verbatim.
    fn representative_audio_wrapper_bytes(e2ee: bool) -> usize {
        use protobuf::Message as _;
        use videocall_types::protos::media_packet::media_packet::MediaType;
        use videocall_types::protos::media_packet::{AudioMetadata, MediaPacket};
        use videocall_types::protos::packet_wrapper::packet_wrapper::{MediaKind, PacketType};
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        let opus_bytes = audio_tier_packet_bytes(
            videocall_aq::constants::AUDIO_QUALITY_TIERS[0].bitrate_kbps as usize,
            AUDIO_PACKETS_PER_SEC_PER_RUNG,
        );
        assert_eq!(opus_bytes, 120, "top audio tier is 48 kbps at 50 pkt/s");

        let media_packet = MediaPacket {
            media_type: MediaType::AUDIO.into(),
            frame_type: "key".to_string(),
            data: vec![0xA5; opus_bytes],
            timestamp: 1_762_000_000_000.0,
            audio_metadata: Some(AudioMetadata {
                sequence: 4_000_000,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };
        let inner = media_packet
            .write_to_bytes()
            .expect("serialize MediaPacket");
        // PKCS7 appends a FULL block when already aligned, so not `div_ceil`.
        let data_len = if e2ee {
            (inner.len() / 16 + 1) * 16
        } else {
            inner.len()
        };

        PacketWrapper {
            data: vec![0u8; data_len],
            user_id: b"participant@example.com".to_vec(),
            packet_type: PacketType::MEDIA.into(),
            media_kind: MediaKind::AUDIO.into(),
            session_id: 0x0123_4567_89AB_CDEF,
            ..Default::default()
        }
        .write_to_bytes()
        .expect("serialize PacketWrapper")
        .len()
    }

    /// LOCKSTEP (#2716): [`WT_DATAGRAM_AUDIO_WIRE_BYTES`] must equal the packet
    /// the relay really queues, in the E2EE-ON mode it is modelled on.
    #[test]
    fn audio_datagram_wire_size_matches_the_model() {
        let sealed = representative_audio_wrapper_bytes(true);
        assert_eq!(
            sealed + WT_DATAGRAM_SESSION_HEADER_BYTES,
            WT_DATAGRAM_AUDIO_WIRE_BYTES,
            "an E2EE-sealed top-tier audio PacketWrapper serializes to {sealed}B; \
             with the {WT_DATAGRAM_SESSION_HEADER_BYTES}B WebTransport session \
             header that is not the modelled {WT_DATAGRAM_AUDIO_WIRE_BYTES}B",
        );

        let cleartext = representative_audio_wrapper_bytes(false);
        assert_eq!(
            cleartext + WT_DATAGRAM_SESSION_HEADER_BYTES,
            188,
            "the cleartext wire size is the OTHER mode the buffer must cover",
        );
        assert!(
            sealed > cleartext,
            "PKCS7 must grow the wrapper ({cleartext}B -> {sealed}B); if it does \
             not, the E2EE model is no longer the larger of the two modes",
        );
    }

    /// LOCKSTEP (#2716): the queue must hold exactly
    /// [`RELAY_QUEUE_ABSORPTION_TARGET_MS`] of the fan-out, inverting the
    /// derivation (bytes -> ms) instead of recomputing it. Bracketed on the
    /// E2EE-OFF side too, where the smaller unit buys more milliseconds.
    #[test]
    fn quinn_datagram_buffer_holds_the_absorption_target() {
        let pps = audio_fanout_packets_per_sec(
            RELAY_SIZING_TARGET_PARTICIPANTS,
            AUDIO_PUBLISHED_LAYER_COUNT,
            AUDIO_PACKETS_PER_SEC_PER_RUNG,
        );
        assert_eq!(pps, 1_300, "26 peers x 1 published audio layer x 50 pkt/s");

        let ms = buffer_absorption_millis(
            WT_QUIC_DATAGRAM_SEND_BUFFER_BYTES,
            pps * WT_DATAGRAM_AUDIO_WIRE_BYTES,
        );
        assert_eq!(
            ms, RELAY_QUEUE_ABSORPTION_TARGET_MS,
            "quinn's datagram queue absorbs {ms}ms of the {pps} pkt/s x \
             {WT_DATAGRAM_AUDIO_WIRE_BYTES}B audio fan-out; target is \
             {RELAY_QUEUE_ABSORPTION_TARGET_MS}ms",
        );

        // E2EE off is the live default and its smaller unit buys MORE time;
        // bracket the overshoot against NetEq's 40-120ms jitter target.
        let cleartext_ms = buffer_absorption_millis(WT_QUIC_DATAGRAM_SEND_BUFFER_BYTES, pps * 188);
        assert!(
            (RELAY_QUEUE_ABSORPTION_TARGET_MS..=300).contains(&cleartext_ms),
            "with E2EE off the queue absorbs {cleartext_ms}ms, outside the \
             {RELAY_QUEUE_ABSORPTION_TARGET_MS}..=300ms band the buffer is sized for",
        );
    }

    /// #2716: `send_window` must park `write_all` in seconds without capping a
    /// downlink at a long-haul RTT.
    #[test]
    fn quinn_send_window_bounds_queue_without_capping_bdp() {
        const LONG_HAUL_RTT_MS: u64 = 200;
        let sustainable_bits_per_sec = WT_QUIC_SEND_WINDOW_BYTES * 8 * 1000 / LONG_HAUL_RTT_MS;
        assert!(
            sustainable_bits_per_sec >= 20_000_000,
            "a {WT_QUIC_SEND_WINDOW_BYTES}B send window sustains only \
             {sustainable_bits_per_sec} bps at {LONG_HAUL_RTT_MS}ms RTT, below \
             the 20 Mbps a receiver taking several publishers needs",
        );

        let video_bytes_per_sec: u64 = videocall_aq::constants::SIMULCAST_VIDEO_LAYERS
            .iter()
            .map(|layer| layer.ideal_bitrate_kbps as u64 * 1000 / 8)
            .sum();
        let seconds_queued = WT_QUIC_SEND_WINDOW_BYTES as f64 / video_bytes_per_sec as f64;
        assert!(
            seconds_queued <= 5.0,
            "a {WT_QUIC_SEND_WINDOW_BYTES}B send window holds {seconds_queued:.1}s \
             of one publisher's {video_bytes_per_sec}B/s three-video-layer \
             profile before write_all parks; the #1638 shed must arm in seconds",
        );
    }

    #[test]
    fn outbound_byte_budgets_track_the_encode_tiers_they_are_derived_from() {
        assert_eq!(
            tier_frame_bytes(
                &videocall_aq::constants::VIDEO_QUALITY_TIERS
                    [videocall_aq::constants::DEFAULT_VIDEO_TIER_INDEX],
            ),
            3_000,
            "default camera tier is 600 kbps at 25 fps",
        );
        assert_eq!(
            tier_frame_bytes(&videocall_aq::constants::SCREEN_QUALITY_TIERS[0]),
            55_287,
            "the single SCREEN rung at its encode ceiling, 10 fps",
        );
        assert_eq!(OUTBOUND_VIDEO_BYTE_BUDGET, 384_000);
        assert_eq!(OUTBOUND_SCREEN_BYTE_BUDGET, 7_076_736);
        // Locks for the RelayQueueNearFullWS*Bytes alert exprs (YAML, no import).
        assert_eq!(OUTBOUND_VIDEO_BYTE_BUDGET * 80 / 100, 307_200);
        assert_eq!(OUTBOUND_SCREEN_BYTE_BUDGET * 90 / 100, 6_369_062);
    }

    #[test]
    fn audio_fanout_and_absorption_helpers_are_well_behaved() {
        assert_eq!(audio_fanout_packets_per_sec(1, 3, 50), 0, "solo call");
        assert_eq!(audio_fanout_packets_per_sec(0, 3, 50), 0, "no underflow");
        assert_eq!(audio_fanout_packets_per_sec(2, 1, 50), 50);
        assert_eq!(queue_absorption_millis(100, 0), usize::MAX);
        assert_eq!(queue_absorption_millis(1_000, 1_000), 1_000);
    }
}
