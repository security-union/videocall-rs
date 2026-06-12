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

use crate::{
    constants::{
        LAYER_HINT_MAX_RECEIVERS_SCANNED, LAYER_HINT_RECOMPUTE_COALESCE_MS,
        LAYER_HINT_SUPPRESS_DEBOUNCE_MS, LAYER_PREFERENCE_MAX_ENTRIES,
        LAYER_PREFERENCE_MAX_LAYER_ID, LAYER_PREFERENCE_MIN_UPDATE_INTERVAL,
        LAYER_PREFERENCE_SESSIONS_SWEEP_INTERVAL, RECONNECT_GRACE_PERIOD, VIEWPORT_MAX_SESSION_IDS,
        VIEWPORT_MIN_UPDATE_INTERVAL,
    },
    messages::{
        server::{
            ActivateConnection, ClientMessage, Connect, Disconnect, JoinRoom, Leave,
            RebroadcastPresence,
        },
        session::Message,
    },
    models::build_subject_and_queue,
    session_manager::{SessionEndResult, SessionManager},
};

use actix::{
    Actor, Addr, AsyncContext, Context, Handler, Message as ActixMessage, MessageResult, Recipient,
    SpawnHandle,
};
// `SendError` is re-exported only from `actix::prelude`, not the crate root, so
// it needs its own import. We match on it (`Full` vs `Closed`) at the fan-out
// hop to distinguish transient backpressure (a shed) from a gone receiver.
use actix::prelude::SendError;
use futures::StreamExt;
use protobuf::Message as ProtobufMessage;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, trace, warn};

use crate::actors::priority_drop::OutboundPriority;
use crate::metrics::{
    RELAY_CONGESTION_FILTERED_TOTAL, RELAY_INBOUND_MAILBOX_DROPS_TOTAL, RELAY_LAYER_FILTERED_TOTAL,
    RELAY_LAYER_FORWARDED_BY_LAYER_TOTAL, RELAY_LAYER_FORWARDED_TOTAL,
    RELAY_LAYER_HINT_EMITTED_TOTAL, RELAY_LAYER_ID_BUCKETS, RELAY_LAYER_PREFERENCE_SESSIONS,
    RELAY_LAYER_PREFERENCE_UPDATES_TOTAL, RELAY_NATS_PUBLISH_LATENCY_MS, RELAY_PACKET_DROPS_TOTAL,
    RELAY_VIEWPORT_FILTERED_TOTAL, RELAY_VIEWPORT_FORWARDED_TOTAL, RELAY_VIEWPORT_SET_SIZE,
    RELAY_VIEWPORT_UPDATES_TOTAL,
};
use videocall_types::protos::layer_hint_packet::layer_hint_packet::Entry as LayerHintEntry;
use videocall_types::protos::layer_hint_packet::LayerHintPacket;
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::validation::validate_display_name;
use videocall_types::SYSTEM_USER_ID;

use super::session_logic::{ConnectionState, SessionId};

/// Internal message sent via `notify_later` after the reconnection grace period
/// expires. If the user has not reconnected by the time this message is handled,
/// the actual `leave_rooms()` + PARTICIPANT_LEFT flow executes.
#[derive(ActixMessage)]
#[rtype(result = "()")]
struct ExecutePendingDeparture {
    session: SessionId,
    room: String,
    user_id: String,
    /// Second component of the `pending_departures` HashMap key — the
    /// client-instance identifier captured at `Disconnect` time. The handler
    /// MUST use this when looking up its own entry, because `(room, user_id)`
    /// is no longer unique across same-user multi-session participants.
    instance_key: String,
    display_name: String,
    is_host: bool,
    end_on_host_leave: bool,
}

/// NATS subject for cross-server stale session eviction.
const EVICT_INSTANCE_SUBJECT: &str = "internal.evict_instance";

/// NATS subject for fanout of per-meeting policy flag changes from
/// `meeting-api`. Mirrors [`EVICT_INSTANCE_SUBJECT`]: JSON payload over a
/// non-protobuf internal channel, consumed by every chat_server instance to
/// keep the in-memory `room_policy` cache fresh after PATCH /meetings.
///
/// This is intentionally a **separate** subject from the client-facing
/// `MEETING_SETTINGS_UPDATED` protobuf event published in
/// `meeting-api/src/nats_events.rs`: that one tells clients to re-fetch
/// settings via REST, this one tells servers to refresh their cached
/// policy without a DB round-trip on host disconnect.
const MEETING_SETTINGS_UPDATE_SUBJECT: &str = "internal.meeting_settings_updated";

/// NATS subject for chat_server -> meeting-api notifications that a host
/// just left a meeting whose `end_on_host_leave=true` policy fired. The
/// meeting-api consumer writes `state='ended'` to the DB so the meetings
/// list reflects the same authoritative outcome the clients see in their
/// MEETING_ENDED broadcast.
///
/// This event ONLY fires on the legitimate broadcast path inside
/// [`ChatServer::leave_rooms`] — never from the [`Disconnect`] handler
/// directly, and never from the deferred-departure timer callback before
/// it has actually decided to broadcast. The reconnect grace period is
/// honored automatically: if the host reconnects within
/// [`RECONNECT_GRACE_PERIOD`], `ExecutePendingDeparture` is cancelled
/// before `leave_rooms` runs, so this event is never published.
const MEETING_ENDED_BY_HOST_SUBJECT: &str = "internal.meeting_ended_by_host";

/// NATS subject for chat_server -> meeting-api notifications that a room just
/// became empty (the last present participant disconnected/left) for a meeting
/// that did NOT end. The meeting-api consumer writes `state='idle'` to the DB
/// (via `db_meetings::set_idle`) so the meetings list reflects "no one is
/// currently here" without ending the meeting.
///
/// Fired from two sites — the normal-departure path in
/// [`ChatServer::leave_rooms`], and the `was_active=false` branch of
/// [`ExecutePendingDeparture`]'s handler (a never-activated session, e.g. an
/// RTT-election loser, whose grace period expired while it was the last member).
/// In both cases the event is emitted only when the in-memory `room_members`
/// count for the room reaches zero — exactly once per room-becomes-empty, not
/// once per disconnect (the actor is single-threaded, so only the departure that
/// drains the Vec to empty observes `is_empty()`). It is deliberately NOT
/// emitted on the host-leave-ends-meeting path, where MEETING_ENDED +
/// [`MEETING_ENDED_BY_HOST_SUBJECT`] fire instead and `ended` (terminal) must
/// win. A non-ending host leave (`end_on_host_leave=false`) is treated as a
/// normal departure and DOES contribute to this transition.
///
/// The consumer's `set_idle` guards on `state='active'`, so an idle event that
/// races a host-leave END is harmless in either ordering.
const MEETING_BECAME_EMPTY_SUBJECT: &str = "internal.meeting_became_empty";

/// Payload published to NATS for cross-server stale session eviction.
/// When a client reconnects (possibly to a different server), the new server
/// broadcasts this so the old server can clean up silently.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct EvictInstancePayload {
    instance_id: String,
    room: String,
    user_id: String,
    new_session_id: SessionId,
}

/// Payload for [`MEETING_SETTINGS_UPDATE_SUBJECT`].
///
/// Carries the four per-meeting policy flags that determine server-side
/// behavior (host-leave handling, waiting room admission gating, guest
/// access). Each field is present so a single payload can refresh the
/// full policy snapshot — the chat_server consumer overwrites all of
/// them rather than merging field-by-field, so the meeting-api publisher
/// must always send the post-update authoritative values.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct MeetingSettingsUpdatePayload {
    room_id: String,
    end_on_host_leave: bool,
    admitted_can_admit: bool,
    waiting_room_enabled: bool,
    allow_guests: bool,
}

/// Payload for [`MEETING_ENDED_BY_HOST_SUBJECT`].
///
/// Sent from chat_server to meeting-api when a host leaves a meeting whose
/// `end_on_host_leave=true` policy fired and `MEETING_ENDED` was broadcast
/// to peers. The meeting-api consumer writes `state='ended'` for the
/// matching `room_id` so the meetings list stays consistent with the
/// clients' view of the meeting.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct MeetingEndedByHostPayload {
    room_id: String,
}

/// Payload for [`MEETING_BECAME_EMPTY_SUBJECT`].
///
/// Sent from chat_server to meeting-api when the last present participant left a
/// room whose meeting did NOT end. The meeting-api consumer looks up the meeting
/// by `room_id` and transitions its DB row to `state='idle'` (no-op if the
/// meeting already ended). Mirrors [`MeetingEndedByHostPayload`] — a single
/// `room_id` field, JSON over an internal subject.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct MeetingBecameEmptyPayload {
    room_id: String,
}

/// Internal actix message delivered when a NATS eviction message is received.
#[derive(ActixMessage)]
#[rtype(result = "()")]
struct EvictInstance(EvictInstancePayload);

/// Internal actix message delivered when a `MEETING_SETTINGS_UPDATE_SUBJECT`
/// payload is received. Updates the `room_policy` cache so the next host
/// disconnect reads the freshest policy values without hitting the DB.
#[derive(ActixMessage)]
#[rtype(result = "()")]
struct UpdateRoomPolicy(MeetingSettingsUpdatePayload);

/// Internal actix message to update a room member's display name.
/// Sent from the per-session NATS subscription loop when a
/// PARTICIPANT_DISPLAY_NAME_CHANGED event is received.
///
/// `session_id == 0` is the legacy "no session scoping" sentinel that mirrors
/// the proto-3 default for `MeetingPacket.session_id` — older clients that
/// haven't been updated for HCL issue #828 still send rename requests without
/// a session_id, and the handler falls back to updating every member row
/// matching `user_id`. When `session_id != 0`, the handler updates only the
/// single row whose `(session, user_id)` pair matches and silently no-ops
/// (with a `warn!`) when no such row exists — preventing a forged or stale
/// session_id from controlling whose name gets rewritten.
#[derive(ActixMessage)]
#[rtype(result = "()")]
struct UpdateMemberDisplayName {
    room_id: String,
    user_id: String,
    display_name: String,
    /// `0` means "legacy, no session scoping — rename all rows for this user".
    /// Non-zero means "scope the rename to this single session; require it
    /// already belong to `user_id` in `room_members`".
    session_id: u64,
}

/// State stored while a departure is pending (waiting for possible reconnection).
struct PendingDepartureState {
    /// Handle returned by `ctx.notify_later()`, used to cancel the delayed
    /// `ExecutePendingDeparture` message if the user reconnects in time.
    spawn_handle: SpawnHandle,
    /// The old session ID that disconnected — used for cleanup.
    old_session: SessionId,
    /// Whether the disconnecting session had been activated (Testing -> Active).
    /// Only Active sessions should trigger PARTICIPANT_LEFT, because only Active
    /// sessions had their PARTICIPANT_JOINED broadcast. Testing sessions (e.g.,
    /// the losing connection during RTT election) never announced themselves.
    was_active: bool,
}

/// Information about a room member tracked by the ChatServer.
#[derive(Clone, Debug)]
struct RoomMemberInfo {
    session: SessionId,
    user_id: String,
    display_name: String,
    is_host: bool,
    /// **Note:** this field is captured from the JWT at JoinRoom time and is
    /// retained for backward compatibility with [`Disconnect`] / [`Leave`]
    /// handlers that still propagate the per-session value. It can become
    /// stale if the host PATCHes meeting settings mid-call. The freshest
    /// value lives in [`ChatServer::room_policy`] and is read at
    /// host-disconnect time inside [`ChatServer::leave_rooms`].
    end_on_host_leave: bool,
}

/// Cached per-room policy flags. Populated at first JoinRoom for the room and
/// refreshed on `MEETING_SETTINGS_UPDATE_SUBJECT` NATS events from
/// `meeting-api`.
///
/// This cache exists so the host-disconnect path can read the current
/// `end_on_host_leave` (and the other three flags, for future use) without a
/// DB round-trip — `actix-api` has no `sqlx::PgPool` at runtime, all
/// authoritative meeting state lives in `meeting-api`. See
/// [`MEETING_SETTINGS_UPDATE_SUBJECT`].
#[derive(Clone, Debug)]
struct RoomPolicy {
    end_on_host_leave: bool,
    #[allow(dead_code)]
    admitted_can_admit: bool,
    #[allow(dead_code)]
    waiting_room_enabled: bool,
    #[allow(dead_code)]
    allow_guests: bool,
}

/// Context passed to [`ChatServer::leave_rooms`] describing the session that
/// is departing and the policies that govern what side-effects should fire.
/// Bundling these into a struct avoids a long positional argument list and
/// makes each call site self-documenting.
pub struct LeaveContext<'a> {
    pub session_id: &'a SessionId,
    pub room: Option<&'a str>,
    pub user_id: Option<&'a str>,
    pub display_name: Option<&'a str>,
    pub observer: bool,
    pub is_host: bool,
    pub end_on_host_leave: bool,
}

/// Per-session viewport state (HCL issue #988): the set of source
/// `session_id`s the owning receiver is currently rendering, plus the time of
/// the last accepted VIEWPORT update (used to rate-limit updates).
///
/// **Fail-open invariant:** an empty `ids` set means "no viewport signal yet"
/// and the relay forwards everything, behaving exactly as it did before #988.
#[derive(Default)]
struct ViewportState {
    /// Source session_ids the receiver is rendering. Empty = fail-open.
    ids: std::collections::HashSet<u64>,
    /// Instant of the last accepted VIEWPORT update, for rate-limiting.
    /// `None` until the first accepted update.
    last_update: Option<std::time::Instant>,
}

/// Per-session viewport state, shared between the session's NATS subscription
/// task and [`ChatServer`].
///
/// It is read on the hot forwarding path to drop VIDEO for off-screen peers
/// and written by the same session's NATS loop when a fresh VIEWPORT packet
/// arrives. An `Arc<RwLock<..>>` is used (rather than a plain actor-owned
/// `HashMap`) so the spawned per-session subscription task can read it
/// per-packet without a round-trip back into the actor.
type DesiredStreams = Arc<RwLock<ViewportState>>;

/// Per-session simulcast layer-preference state (#989, Phase 1b): the map from
/// source `session_id` → the simulcast layer the owning receiver wants the
/// relay to forward for that source, plus the time of the last accepted
/// LAYER_PREFERENCE update (used to rate-limit updates).
///
/// **No-op / fail-open invariant:** an empty `layers` map means "no layer
/// signal yet" and the relay forwards every layer, behaving exactly as it did
/// before #989. A source with no entry in `layers` is likewise forwarded
/// unchanged. This is what makes the feature DARK on an empty map: with no
/// recorded preference the forwarding path is byte-identical to today.
#[derive(Default)]
struct LayerPrefsState {
    /// Map of `(source session_id, media_kind)` → desired simulcast layer.
    /// Empty / absent = fail-open (forward all layers). The `media_kind` is the
    /// normalized wire discriminant (see [`normalize_pref_media_kind`]):
    /// VIDEO(1) / AUDIO(2) / SCREEN(3). Keying by media kind (issue #989,
    /// Phase 3) lets a receiver request, e.g., a low SCREEN layer while keeping
    /// full camera VIDEO from the SAME source independently.
    layers: HashMap<(u64, i32), u32>,
    /// Instant of the last accepted LAYER_PREFERENCE update, for rate-limiting.
    /// `None` until the first accepted update.
    last_update: Option<std::time::Instant>,
}

/// Normalize a `LayerPreferencePacket.Entry.media_kind` (or a wire
/// `PacketWrapper.media_kind`) into the canonical layer-preference key
/// discriminant (issue #989, Phase 3).
///
/// Both enums share the same numbering (`UNSPECIFIED=0, VIDEO=1, AUDIO=2,
/// SCREEN=3`). BACK-COMPAT: `0` (UNSPECIFIED) maps to VIDEO(1) — pre-Phase-3
/// clients omit the field, and before Phase 3 the relay only filtered VIDEO, so
/// an absent media_kind is exactly "this preference is about camera video".
/// Anything outside `{1,2,3}` also collapses to VIDEO(1) (defensive; the relay
/// only ever filters those three kinds).
fn normalize_pref_media_kind(raw: i32) -> i32 {
    match raw {
        2 => 2, // AUDIO
        3 => 3, // SCREEN
        _ => 1, // VIDEO (covers UNSPECIFIED=0, VIDEO=1, and any unknown)
    }
}

/// Bucket a wire `simulcast_layer_id` into the BOUNDED label set used by the
/// per-layer forwarded counter (`relay_layer_forwarded_by_layer_total`, #1105).
///
/// The wire `simulcast_layer_id` is a forgeable `u32` that lives OUTSIDE the
/// AEAD seal (#993), so it MUST NOT be used as a metric label verbatim — a
/// malicious or buggy client could otherwise emit arbitrary ids and explode the
/// series count. Today every kind ships at most three layers (ids 0..=2; see
/// `LAYER_PREFERENCE_MAX_LAYER_ID`), so we map 0/1/2 to their own bucket and
/// collapse EVERYTHING else (3..=u32::MAX) into a single `"other"` bucket. This
/// caps the `layer_id` label to EXACTLY 4 distinct values regardless of what
/// arrives on the wire — the cardinality bound is enforced HERE, not merely
/// asserted in a comment. `"other"` doubles as the early-warning signal that a
/// real >3-layer ladder has shipped without this bucketer being widened.
fn layer_id_bucket(layer_id: u32) -> &'static str {
    match layer_id {
        0 => "0",
        1 => "1",
        2 => "2",
        _ => "other",
    }
}

/// The media kinds that carry a simulcast ladder, as `(normalized preference-map
/// discriminant, gauge `kind` label)` pairs, for the DEMAND-side gauge
/// `relay_layer_preference_sessions` (#1170).
///
/// VIDEO(1) and SCREEN(3) only — AUDIO(2) has no simulcast layers, so a receiver
/// never expresses a per-layer preference for it and it is excluded. The string
/// here is the EXACT `kind` label and MUST stay in lockstep with
/// [`crate::metrics::RELAY_LAYER_PREFERENCE_KINDS`] (the room-drain GC iterates
/// that array; this one drives the sweep — a mismatch would leak series for any
/// kind present in one but not the other). The unit test
/// `layer_preference_gauge_kinds_match_metrics_taxonomy` pins them together.
const LAYER_PREFERENCE_GAUGE_KINDS: [(i32, &str); 2] = [(1, "video"), (3, "screen")];

/// Classify ONE receiver's recorded layer preferences into a per-kind MAX-layer
/// bucket, for the demand-side gauge (#1170).
///
/// For each kind in [`LAYER_PREFERENCE_GAUGE_KINDS`] (VIDEO, SCREEN) this finds
/// the MAX `desired_layer` the receiver has requested across ALL sources for
/// that kind, then buckets it via [`layer_id_bucket`] (so a forged id collapses
/// to `"other"`). The max is the right reduction because the relay must
/// forward/produce up to the highest layer the receiver asked for, so that
/// single layer characterizes the session's demand for the kind.
///
/// The return is parallel to [`LAYER_PREFERENCE_GAUGE_KINDS`] by index:
/// `result[i]` is `Some(bucket)` when the receiver has at least one preference
/// entry for that kind, or `None` when it has expressed NO preference for the
/// kind. `None` means FAIL-OPEN (wants the full ladder) and the caller MUST NOT
/// count it — absence of demand is not a request for the top layer.
///
/// Free function (takes the layers map by reference, no `&self`) so it is
/// unit-testable without constructing a NATS-backed actor.
fn classify_session_max_layer_buckets(
    layers: &HashMap<(u64, i32), u32>,
) -> [Option<&'static str>; LAYER_PREFERENCE_GAUGE_KINDS.len()] {
    let mut out: [Option<&'static str>; LAYER_PREFERENCE_GAUGE_KINDS.len()] =
        [None; LAYER_PREFERENCE_GAUGE_KINDS.len()];
    for (idx, (kind, _label)) in LAYER_PREFERENCE_GAUGE_KINDS.iter().enumerate() {
        // Max desired_layer over every (source, this-kind) entry the receiver
        // recorded. `None` (no entry for the kind) stays fail-open / uncounted.
        let max_for_kind = layers
            .iter()
            .filter(|((_, k), _)| k == kind)
            .map(|(_, &layer)| layer)
            .max();
        out[idx] = max_for_kind.map(layer_id_bucket);
    }
    out
}

/// Per-session layer-preference state, shared between the session's NATS
/// subscription task and [`ChatServer`].
///
/// Read on the hot forwarding path (after the viewport filter) to drop
/// simulcast VIDEO layers the receiver did not select, and written by the same
/// session's NATS loop when a fresh LAYER_PREFERENCE packet arrives. Uses an
/// `Arc<RwLock<..>>` for the same reason as [`DesiredStreams`]: the spawned
/// per-session subscription task can read it per-packet without re-entering the
/// actor.
///
/// In addition to the lock, it carries a lock-free `non_empty` hint (an
/// `AtomicBool`) updated by the writer. The forwarding hot path checks this
/// FIRST so that — during the common interim where publishers have started
/// stamping simulcast layer ids but no receiver has sent a LAYER_PREFERENCE yet
/// — every non-zero-layer VIDEO packet short-circuits WITHOUT taking the read
/// lock. This keeps the no-preference path lock-free (the no-op-first posture).
/// The hint is intentionally a HINT, not authoritative: it is only ever set
/// from `false`→`true` (a recorded map is never auto-emptied), so a spurious
/// `true` merely costs one read-lock that finds no matching entry and fails
/// open — never an incorrect drop.
#[derive(Clone)]
struct LayerPrefs {
    state: Arc<RwLock<LayerPrefsState>>,
    /// Lock-free fast-path hint: `true` once at least one layer preference has
    /// been recorded for this session. See the type doc for why this is safe
    /// to consult outside the lock.
    non_empty: Arc<std::sync::atomic::AtomicBool>,
}

impl Default for LayerPrefs {
    fn default() -> Self {
        Self {
            state: Arc::new(RwLock::new(LayerPrefsState::default())),
            non_empty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

impl LayerPrefs {
    /// Cheap, lock-free check used to short-circuit the forwarding hot path.
    /// `false` guarantees the map is empty (no preference recorded yet) → the
    /// caller forwards without taking the read lock.
    fn has_any(&self) -> bool {
        self.non_empty.load(std::sync::atomic::Ordering::Relaxed)
    }
}

// ===========================================================================
// Publish-side layer suppression (#1108, Stage 3 — LAYER_HINT)
// ===========================================================================

/// Fail-open sentinel for the per-source layer union: "some receiver wants the
/// FULL ladder for this (source, kind), so suppress nothing".
///
/// A receiver that has NO recorded preference for a `(source, kind)` is treated
/// as wanting everything (this is the LAYER_PREFERENCE fail-open contract: no
/// entry = forward all layers). It therefore contributes this value to the
/// union, and because the union is a MAX, the presence of even one such receiver
/// pins the union here — i.e. the relay suppresses a layer only when EVERY
/// receiver explicitly asked for less. `u32::MAX` is well above any real layer
/// id, so when the publisher clamps the emitted hint against its own ladder this
/// resolves to "encode the full ladder". A poisoned prefs lock also collapses to
/// this value (fail-open), mirroring the forwarding filter's `unwrap_or`.
const LAYER_HINT_FULL_LADDER_SENTINEL: u32 = u32::MAX;

/// The media kinds the relay computes a layer union for, as the normalized
/// preference-map discriminants (VIDEO=1, AUDIO=2, SCREEN=3 — see
/// [`normalize_pref_media_kind`]). A publisher produces at most one ladder per
/// kind, so a hint carries at most one [`LayerHintEntry`] per element here.
const LAYER_HINT_MEDIA_KINDS: [i32; 3] = [1, 2, 3];

/// Map a normalized preference-map media-kind discriminant (1/2/3) onto the
/// `LayerHintPacket.MediaKind` wire enum used when emitting a hint. Anything
/// outside `{1,2,3}` maps to UNSPECIFIED(0) (never produced on the happy path —
/// [`LAYER_HINT_MEDIA_KINDS`] only contains 1/2/3).
fn layer_hint_media_kind(
    kind: i32,
) -> videocall_types::protos::layer_hint_packet::layer_hint_packet::MediaKind {
    use videocall_types::protos::layer_hint_packet::layer_hint_packet::MediaKind;
    match kind {
        1 => MediaKind::VIDEO,
        2 => MediaKind::AUDIO,
        3 => MediaKind::SCREEN,
        _ => MediaKind::MEDIA_KIND_UNSPECIFIED,
    }
}

/// Actor message asking the [`ChatServer`] to recompute per-source layer unions
/// and emit LAYER_HINT packets where the publisher's encode set should change
/// (#1108, Stage 3).
///
/// This MUST run in the actor: the per-source union is an INVERTED query over
/// the receiver-keyed `session_layer_prefs` map, and only the actor can see
/// across all receivers (each per-session NATS task holds just its own one prefs
/// `Arc`). The interceptors / teardown paths therefore `do_send` this message
/// rather than computing the union themselves.
///
/// * `source = Some(s)`: recompute ONLY source `s` (used when a single
///   receiver's preference for `s` changed).
/// * `source = None`: recompute EVERY current publisher in `room` (used on
///   receiver join / leave, which can shift many sources' fail-open unions at
///   once).
///
/// SECURITY: this message is constructed ONLY by trusted, subject-authoritative
/// relay paths (the LAYER_PREFERENCE interceptor after it has recorded an update
/// on the receiver's OWN subject, and the join/leave lifecycle). There is NO
/// path that constructs it from an inbound, client-sent LAYER_HINT — the relay
/// never ingests LAYER_HINT at all.
#[derive(ActixMessage)]
#[rtype(result = "()")]
struct RecomputeLayerHints {
    room: String,
    source: Option<SessionId>,
}

/// Trailing-debounce flush for coalesced DEPARTURE-driven recomputes (#1203).
///
/// Self-sent via `notify_later` exactly [`LAYER_HINT_RECOMPUTE_COALESCE_MS`]
/// after the FIRST departure of a burst arms the timer (see
/// [`ChatServer::schedule_coalesced_recompute`]). The handler drains
/// [`ChatServer::pending_recompute_rooms`] and runs ONE room-wide recompute per
/// distinct room that saw a departure during the window, collapsing an O(n)
/// per-connection storm into O(distinct rooms) work over settled membership.
#[derive(ActixMessage)]
#[rtype(result = "()")]
struct FlushPendingRecomputes;

/// Periodic self-tick that refreshes the DEMAND-side simulcast gauge
/// `relay_layer_preference_sessions{room, kind, layer_id}` (#1170 item 2).
///
/// Sent by the [`LAYER_PREFERENCE_SESSIONS_SWEEP_INTERVAL`] `run_interval`
/// armed in [`Actor::started`]. The handler is a READ-ONLY pass over the live
/// rooms (`RwLock::read()` on each session's prefs, bounded by
/// [`LAYER_HINT_MAX_RECEIVERS_SCANNED`] per room) that re-SETs every active
/// room's gauge cells. It never takes a write lock and never mutates actor
/// state, so it cannot block the forwarding hot path or starve the mailbox.
#[derive(ActixMessage)]
#[rtype(result = "()")]
struct SweepLayerPreferenceGauge;

/// Per-`(room, source, media_kind)` emit/debounce state for LAYER_HINT (#1108).
///
/// Tracks what the relay last TOLD a publisher and, when the union has dropped
/// below that, when the downgrade was first observed (so the suppress-lazy
/// window can be honored even without further preference changes — a deferred
/// `notify_later` re-check drives the eventual emit).
#[derive(Clone, Copy, Debug)]
struct LayerHintEmitState {
    /// The `max_requested_layer` value most recently EMITTED to the publisher
    /// for this `(room, source, kind)`. Initialized to the full-ladder sentinel
    /// the first time a key is seen, so the publisher's assumed starting state is
    /// "encoding everything" (matching the fail-open default before any hint).
    last_emitted: u32,
    /// When a union STRICTLY below `last_emitted` was first observed. `None` when
    /// the current union is `>= last_emitted` (no pending downgrade). Used to
    /// enforce [`LAYER_HINT_SUPPRESS_DEBOUNCE_MS`] before emitting the lower hint.
    pending_lower_since: Option<std::time::Instant>,
}

/// The outcome of [`decide_layer_hint`]: what the actor should do for one
/// `(room, source, kind)` given the freshly computed union.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayerHintDecision {
    /// Emit a hint now. `direction` labels the metric (`"suppress"` for a lower
    /// union, `"restore"` for a higher one). `value` is the union to send.
    Emit {
        value: u32,
        direction: LayerHintDirection,
    },
    /// Do nothing now, but a lower union is pending; re-check after the debounce
    /// window elapses (the `Instant` is the deadline to schedule `notify_later`
    /// for). Returned the FIRST time a downgrade is observed.
    ScheduleRecheck { deadline: std::time::Instant },
    /// Do nothing, and the union is NOT below what was last emitted — so any
    /// previously-pending downgrade must be CLEARED. Returned when the union is
    /// unchanged (`== last_emitted`): a downgrade that was counting down has been
    /// cancelled by demand returning, and a future downgrade must start a FRESH
    /// debounce window rather than inheriting the stale `pending_lower_since`.
    SkipClearPending,
    /// Do nothing, and a downgrade is still pending within its window — KEEP the
    /// pending timestamp so the scheduled re-check continues counting down toward
    /// the original deadline.
    SkipKeepPending,
}

/// Direction of an emitted LAYER_HINT, used only for the metric label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayerHintDirection {
    /// Lower union than last emitted — publisher may drop an upper layer.
    Suppress,
    /// Higher union than last emitted — publisher should restore a layer.
    Restore,
}

impl LayerHintDirection {
    fn as_label(self) -> &'static str {
        match self {
            LayerHintDirection::Suppress => "suppress",
            LayerHintDirection::Restore => "restore",
        }
    }
}

/// Pure debounce decision for one `(room, source, kind)` (#1108, Stage 3).
///
/// Given the previous emit state (`None` = never emitted; assumed full-ladder),
/// the freshly computed `union`, the current time `now`, and the debounce
/// `window`, decide whether to emit, schedule a deferred re-check, or skip.
///
/// Policy:
/// * `union > last_emitted` → **restore-eager**: emit immediately (clears any
///   pending downgrade via the fresh emit state).
/// * `union == last_emitted` → **change-detect**: skip, and CLEAR any pending
///   downgrade (`SkipClearPending`) — demand returned to the emitted level, so a
///   later drop must start a fresh debounce window, not inherit a stale one.
/// * `union < last_emitted` → **suppress-lazy**:
///     * first observation (no pending timestamp) → schedule a re-check at
///       `now + window`; do not emit yet.
///     * pending and `now - pending_since >= window` → emit (the lower union has
///       been stable for the whole window).
///     * pending and still within the window → `SkipKeepPending` (keep waiting;
///       the scheduled re-check re-evaluates the *then-current* union).
///
/// The split Skip variants exist so the caller can correctly reconcile
/// `pending_lower_since`: WITHOUT clearing it on the `==` path, a downgrade that
/// was cancelled by restored demand would leave a stale timestamp, letting a much
/// later downgrade bypass its debounce window (emit a suppress immediately).
///
/// Extracted as a free function (no `&self`, no NATS) so it is unit-testable in
/// isolation with synthetic `Instant`s, mirroring the pure-resolver pattern used
/// for the channel-capacity constant.
fn decide_layer_hint(
    prev: Option<LayerHintEmitState>,
    union: u32,
    now: std::time::Instant,
    window: std::time::Duration,
) -> LayerHintDecision {
    let last_emitted = prev
        .map(|s| s.last_emitted)
        .unwrap_or(LAYER_HINT_FULL_LADDER_SENTINEL);

    use std::cmp::Ordering::{Equal, Greater, Less};
    match union.cmp(&last_emitted) {
        Greater => LayerHintDecision::Emit {
            value: union,
            direction: LayerHintDirection::Restore,
        },
        // Union back at the emitted level: cancel any pending downgrade.
        Equal => LayerHintDecision::SkipClearPending,
        Less => match prev.and_then(|s| s.pending_lower_since) {
            // Already counting down: emit once the window has fully elapsed.
            Some(since) if now.duration_since(since) >= window => LayerHintDecision::Emit {
                value: union,
                direction: LayerHintDirection::Suppress,
            },
            // Still within the window — keep counting toward the original deadline.
            Some(_) => LayerHintDecision::SkipKeepPending,
            // First time we see a downgrade — start the timer.
            None => LayerHintDecision::ScheduleRecheck {
                deadline: now + window,
            },
        },
    }
}

/// Compute the set of SOURCE session ids whose desired layer changed between an
/// old and a new per-receiver preference map (#1108, Stage 3).
///
/// A source is "changed" if, for ANY media kind, `old.get((source, kind))`
/// differs from `new.get((source, kind))` — this includes entries that were
/// ADDED (no old value), REMOVED (no new value; the source reverts to fail-open
/// full ladder, which can raise its union), or had their value altered. Used by
/// the LAYER_PREFERENCE interceptor to trigger a per-source layer-hint recompute
/// only for the sources that actually moved, instead of the whole room.
///
/// Returned as a deduplicated `Vec` (source ids are unique). Pure / no `&self`
/// so it is unit-testable in isolation.
fn changed_pref_sources(
    old: &HashMap<(u64, i32), u32>,
    new: &HashMap<(u64, i32), u32>,
) -> Vec<SessionId> {
    let mut changed: std::collections::HashSet<SessionId> = std::collections::HashSet::new();
    // Any (source, kind) present in old whose new value differs.
    for (&(src, kind), &old_val) in old.iter() {
        if new.get(&(src, kind)).copied() != Some(old_val) {
            changed.insert(src);
        }
    }
    // Any (source, kind) present in new whose old value differs (catches ADDED
    // entries the first loop could not see).
    for (&(src, kind), &new_val) in new.iter() {
        if old.get(&(src, kind)).copied() != Some(new_val) {
            changed.insert(src);
        }
    }
    changed.into_iter().collect()
}

/// Pure per-source union computation for one `(source, media_kind)` (#1108).
///
/// Enumerates every receiver in `members` (capped at
/// [`LAYER_HINT_MAX_RECEIVERS_SCANNED`] for DoS bounding), reads each receiver's
/// recorded preference for `(source, kind)` from `prefs`, and returns the MAX
/// (the union). FAIL-OPEN throughout:
/// * A receiver with NO recorded entry for `(source, kind)` contributes the
///   full-ladder sentinel ([`LAYER_HINT_FULL_LADDER_SENTINEL`]).
/// * A receiver whose prefs `RwLock` is POISONED also contributes the sentinel
///   (mirrors the forwarding filter's `unwrap_or` fail-open at the layer drop
///   site).
/// * Receivers beyond the scan cap are not inspected and are implicitly treated
///   as wanting the full ladder (truncation can only ever suppress LESS).
///
/// The source itself is skipped (a publisher is not its own receiver). The
/// result is the RAW max-requested-layer id: the relay is layer-count-agnostic
/// (see the "AVAILABILITY NOT VALIDATED" note on the forwarding path) and never
/// learns the source's real ladder depth, so the publisher clamps this against
/// its own ladder.
///
/// Free function (takes the maps by reference, no `&self`) so it is unit-testable
/// without constructing a NATS-backed actor.
fn compute_max_requested_layer(
    members: &[SessionId],
    prefs: &HashMap<SessionId, LayerPrefs>,
    source: SessionId,
    kind: i32,
) -> u32 {
    let mut union: u32 = 0;
    let mut scanned: usize = 0;
    for &receiver in members {
        // A publisher is not a receiver of itself.
        if receiver == source {
            continue;
        }
        if scanned >= LAYER_HINT_MAX_RECEIVERS_SCANNED {
            // DoS bound: anything past the cap is treated as fail-open (wants
            // the full ladder), so pin the union to the sentinel and stop.
            return LAYER_HINT_FULL_LADDER_SENTINEL;
        }
        scanned += 1;

        let contribution = match prefs.get(&receiver) {
            // No prefs map for this receiver at all = fail-open (full ladder).
            None => LAYER_HINT_FULL_LADDER_SENTINEL,
            Some(p) => match p.state.read() {
                Ok(guard) => guard
                    .layers
                    .get(&(source, kind))
                    .copied()
                    // No recorded preference for this (source, kind) = fail-open.
                    .unwrap_or(LAYER_HINT_FULL_LADDER_SENTINEL),
                // Poisoned lock = fail-open (mirror the forward filter).
                Err(_) => LAYER_HINT_FULL_LADDER_SENTINEL,
            },
        };

        union = union.max(contribution);
        // Once the union hits the sentinel it can never decrease — short-circuit.
        if union == LAYER_HINT_FULL_LADDER_SENTINEL {
            return LAYER_HINT_FULL_LADDER_SENTINEL;
        }
    }
    union
}

pub struct ChatServer {
    nats_connection: async_nats::client::Client,
    sessions: HashMap<SessionId, Recipient<Message>>,
    active_subs: HashMap<SessionId, JoinHandle<()>>,
    session_manager: SessionManager,
    connection_states: HashMap<SessionId, ConnectionState>,
    /// Track which sessions are in which room, with their user_id, display_name,
    /// and host status. Used to send PARTICIPANT_JOINED for existing peers to
    /// new joiners and to determine host-leave behavior.
    room_members: HashMap<String, Vec<RoomMemberInfo>>,
    /// Per-room policy flag cache. Refreshed by
    /// [`MEETING_SETTINGS_UPDATE_SUBJECT`] events so toggles like
    /// `end_on_host_leave` take effect mid-meeting without requiring a host
    /// reconnect. Read by [`ChatServer::leave_rooms`] when deciding whether
    /// to broadcast `MEETING_ENDED`. See [`RoomPolicy`].
    room_policy: HashMap<String, RoomPolicy>,
    /// Pending departures keyed by `(room_id, instance_key)`.
    ///
    /// `instance_key` is the client's `instance_id` when one was provided at
    /// `JoinRoom` time (the common path — sourced from per-tab sessionStorage),
    /// or a per-session sentinel (`"__session__:<session_id>"`) when no
    /// `instance_id` is available. This is the **per-tab / per-client-instance**
    /// identifier and intentionally NOT keyed on `user_id`: PR #851 lifted the
    /// "one session per user per room" invariant, so two concurrent sessions of
    /// the same `user_id` (e.g. two browser tabs of the same authenticated
    /// account) must each have independent pending-departure state.
    ///
    /// When a session disconnects we defer the PARTICIPANT_LEFT broadcast by
    /// [`RECONNECT_GRACE_PERIOD`]. If the SAME tab reconnects (same
    /// `instance_id`) before the timer fires, the departure is cancelled
    /// silently — no PARTICIPANT_LEFT or PARTICIPANT_JOINED is sent. A fresh
    /// second session of the same user (different `instance_id`) does NOT match
    /// this key and is correctly treated as a real new join.
    pending_departures: HashMap<(String, String), PendingDepartureState>,
    /// Sessions that should NOT have PARTICIPANT_JOINED broadcast at activation.
    /// This is used for reconnection sessions: the user never "left" from peers'
    /// perspective, so announcing a "join" would be misleading.
    suppress_join_broadcast: std::collections::HashSet<SessionId>,
    /// Maps `instance_id` → `SessionId` for the current active session of each
    /// client instance. Used to find and evict stale sessions on reconnection.
    instance_index: HashMap<String, SessionId>,
    /// Reverse map: `SessionId` → `instance_id`. Enables O(1) cleanup of
    /// `instance_index` when a session disconnects, instead of an O(n) retain scan.
    session_instance: HashMap<SessionId, String>,
    /// Per-session server-authoritative guest flag, sourced from the JWT
    /// `is_guest` claim captured at `JoinRoom`.
    session_is_guest: HashMap<SessionId, bool>,
    /// Per-session viewport / "desired streams" set (HCL issue #988).
    ///
    /// Maps a receiver `SessionId` to the set of source `session_id`s it is
    /// currently rendering. Populated from VIEWPORT control packets and read
    /// on the forwarding path to drop VIDEO for off-screen peers. The
    /// `Arc<RwLock<..>>` value is shared with the session's NATS task so the
    /// task can read/update it without re-entering the actor.
    ///
    /// Absent or empty = fail-open (forward all video). This is a
    /// **subtract-only** filter layered AFTER JWT/observer authorization; it
    /// never grants access. See [`DesiredStreams`].
    ///
    /// The actor never *reads* this map; all reads/writes of the viewport go
    /// through the `Arc<RwLock>` clone held by the spawned NATS task. The map
    /// exists purely to own the shared handle and bound its lifetime (entries
    /// are removed on `leave_rooms`/`forget_session`), not as a read source.
    session_desired_streams: HashMap<SessionId, DesiredStreams>,
    /// Per-session simulcast layer preferences (#989, Phase 1b).
    ///
    /// Maps a receiver `SessionId` to its [`LayerPrefs`] (source `session_id` →
    /// desired simulcast layer). Populated from LAYER_PREFERENCE control packets
    /// and read on the forwarding path — AFTER the viewport filter — to drop
    /// simulcast VIDEO layers the receiver did not select. The
    /// `Arc<RwLock<..>>` value is shared with the session's NATS task so the
    /// task can read/update it without re-entering the actor.
    ///
    /// Absent or empty = fail-open (forward all layers). Like
    /// `session_desired_streams` this is a **subtract-only** filter layered
    /// AFTER JWT/observer authorization; it never grants access. The actor never
    /// *reads* this map; the map exists purely to own the shared handle and
    /// bound its lifetime (entries are removed on `leave_rooms`/`forget_session`).
    session_layer_prefs: HashMap<SessionId, LayerPrefs>,
    /// Reverse index: `SessionId` → `room_id`. Enables O(1) room lookup in
    /// paths like `RebroadcastPresence` instead of scanning all rooms.
    /// Populated for non-observer sessions at JoinRoom; removed at
    /// `leave_rooms` / `forget_session`.
    session_room: HashMap<SessionId, String>,
    /// Per-`(room, source session, media_kind)` LAYER_HINT emit/debounce state
    /// (#1108, Stage 3 — publish-side layer suppression).
    ///
    /// Records what `max_requested_layer` the relay last EMITTED to each
    /// publisher, plus the pending-downgrade timestamp that powers the
    /// suppress-lazy debounce ([`LAYER_HINT_SUPPRESS_DEBOUNCE_MS`]). Consulted
    /// and updated only inside [`Handler<RecomputeLayerHints>`] (actor-owned, no
    /// lock needed). Entries are reaped when the publisher leaves (its
    /// `(room, source, _)` keys are dropped in `leave_rooms` / `forget_session`).
    layer_hint_state: HashMap<(String, SessionId, i32), LayerHintEmitState>,
    /// Rooms with a DEPARTURE-driven (leave/evict) room-wide LAYER_HINT recompute
    /// pending behind the coalescing debounce window (#1203).
    ///
    /// Departures fire one room-wide recompute per disconnecting connection
    /// (`leave_rooms` / `forget_session`). A reconnection wave or a meeting
    /// ending disconnects many sessions in a burst → an O(n) recompute storm in
    /// the single-threaded actor (each recompute is itself O(publishers ×
    /// receivers)). Instead of `do_send`-ing a recompute per departure, we record
    /// the affected room HERE and arm a single trailing
    /// [`LAYER_HINT_RECOMPUTE_COALESCE_MS`] timer ([`recompute_coalesce_handle`]);
    /// when it fires, [`Handler<FlushPendingRecomputes>`] drains this set and runs
    /// exactly ONE room-wide recompute per affected room over the FINAL settled
    /// membership. JOIN and per-LAYER_PREFERENCE recomputes intentionally bypass
    /// this set (they are restore-eager / latency-sensitive — see the constant).
    pending_recompute_rooms: std::collections::HashSet<String>,
    /// Single in-flight coalescing timer handle for [`pending_recompute_rooms`]
    /// (#1203). `Some` while a trailing flush is armed; `None` otherwise. Only one
    /// timer is ever outstanding — re-arming while pending is a no-op (the
    /// existing trailing deadline still fires), which is what makes the debounce
    /// TRAILING (dedups the whole burst) rather than per-event. Cancelled in
    /// [`Actor::stopping`] so a stopped actor leaks no `SpawnHandle`.
    recompute_coalesce_handle: Option<SpawnHandle>,
}

impl ChatServer {
    pub async fn new(nats_connection: async_nats::client::Client) -> Self {
        ChatServer {
            nats_connection,
            active_subs: HashMap::new(),
            sessions: HashMap::new(),
            session_manager: SessionManager::new(),
            connection_states: HashMap::new(),
            room_members: HashMap::new(),
            room_policy: HashMap::new(),
            pending_departures: HashMap::new(),
            suppress_join_broadcast: std::collections::HashSet::new(),
            instance_index: HashMap::new(),
            session_instance: HashMap::new(),
            session_is_guest: HashMap::new(),
            session_desired_streams: HashMap::new(),
            session_layer_prefs: HashMap::new(),
            session_room: HashMap::new(),
            layer_hint_state: HashMap::new(),
            pending_recompute_rooms: std::collections::HashSet::new(),
            recompute_coalesce_handle: None,
        }
    }

    pub fn leave_rooms(&mut self, leave_ctx: LeaveContext<'_>, actor_ctx: &mut Context<Self>) {
        let LeaveContext {
            session_id,
            room,
            user_id,
            display_name,
            observer,
            is_host,
            end_on_host_leave,
        } = leave_ctx;
        // Remove the subscription task if it exists
        if let Some(task) = self.active_subs.remove(session_id) {
            task.abort();
        }

        // Drop the per-session viewport set (HCL issue #988). Mirrors the
        // cleanup in `forget_session`; both teardown paths must release this
        // so the map cannot leak entries for departed sessions.
        let _ = self.session_desired_streams.remove(session_id);

        // Drop the per-session layer-preference map (#989, Phase 1b). Same
        // teardown invariant as the viewport set above.
        let _ = self.session_layer_prefs.remove(session_id);

        // Clean up instance_index via reverse map: O(1) instead of O(n) retain.
        // If the entry was already replaced by a newer session (eviction), the
        // reverse map was already updated, so this is a no-op.
        if let Some(iid) = self.session_instance.remove(session_id) {
            // Only remove from instance_index if it still points to this session.
            if self.instance_index.get(&iid) == Some(session_id) {
                self.instance_index.remove(&iid);
            }
        }

        // Resolve the freshest `end_on_host_leave` value BEFORE we mutate
        // `room_members`. Reading from `room_policy` here closes the
        // cache-staleness window left open by the JoinRoom-time JWT capture:
        // mid-meeting PATCH /meetings updates land via
        // `MEETING_SETTINGS_UPDATE_SUBJECT` and refresh `room_policy`, so this
        // lookup sees the post-toggle value even though the host's session
        // still carries the old JWT-time flag in `Disconnect`.
        //
        // Falls back to the per-session parameter (which is itself set from
        // the JWT) only when the cache has no entry — the cache is populated
        // at first JoinRoom for the room, so the only realistic gap is the
        // `Leave` handler in tests / synthetic flows where no policy has
        // ever been pushed. The result is "no worse than today" in that
        // window.
        let effective_end_on_host_leave = match room {
            Some(r) => self
                .room_policy
                .get(r)
                .map(|p| p.end_on_host_leave)
                .unwrap_or(end_on_host_leave),
            None => end_on_host_leave,
        };

        // Remove from room_members tracking, then defer the empty-room
        // policy-cache cleanup to the shared helper. We deliberately do NOT
        // treat "no entry in room_members" as room-empty-now: the cache may
        // have been seeded by a `MEETING_SETTINGS_UPDATE_SUBJECT` event
        // before the first JoinRoom, and a stale `Leave` / `Disconnect` for
        // that room must not wipe the legitimately-cached policy.
        self.session_room.remove(session_id);

        // Track whether THIS removal drained the room to empty. We read the
        // count from `room_members` — the in-memory, actor-synchronous presence
        // map — which is the same authoritative source the host-leave→end path
        // uses. Because the chat_server actor processes one message at a time,
        // exactly one `leave_rooms` call can observe the Vec transition from
        // non-empty to empty: during a mass-disconnect (reconnection wave) the
        // N departures are serialized, and only the last one sees
        // `members.is_empty()`. This is what makes the empty→idle NATS event
        // fire ONCE per room-becomes-empty rather than once per disconnect, so
        // there is no O(n) NATS storm.
        let mut room_became_empty = false;
        if let Some(room_id) = room {
            if let Some(members) = self.room_members.get_mut(room_id) {
                members.retain(|m| m.session != *session_id);
                room_became_empty = members.is_empty();
            }
            self.forget_room_if_empty(room_id);

            // Publish-side suppression teardown + restore (#1108, Stage 3).
            // The departing session may have been BOTH a publisher (a source
            // with its own debounce state) AND a constraining receiver (its
            // recorded preference held some other publisher's union DOWN).
            //   1. Reap the departing session's own per-source hint state so the
            //      `layer_hint_state` map cannot leak entries for a left source.
            //   2. If the room still has members, recompute room-wide: with this
            //      receiver gone its constraint disappears, so a publisher whose
            //      remaining receivers all now (or already) want the full ladder
            //      must get a RESTORE-eager hint. We message the actor rather
            //      than recomputing inline to avoid borrowing `room_members`
            //      while the recompute mutates `layer_hint_state`. This is
            //      already excluded from the union scan above (the member was
            //      retained-out), so the recompute sees post-departure demand.
            //
            //      #1203: this DEPARTURE recompute is COALESCED behind a trailing
            //      debounce instead of fired immediately. A departure only ever
            //      RAISES a remaining publisher's fail-open union (restore) and
            //      nobody is actively waiting on it, so collapsing a reconnection
            //      / meeting-end disconnect burst into ONE recompute over settled
            //      membership avoids the O(n) per-connection storm. (JOINs stay
            //      immediate — a real viewer waits on their tile.)
            self.forget_layer_hint_state_for_source(room_id, *session_id);
            if !room_became_empty {
                let room_owned = room_id.to_string();
                self.schedule_coalesced_recompute(&room_owned, actor_ctx);
            }
        }

        // End session using SessionManager
        if let (Some(room_id), Some(uid)) = (room, user_id) {
            let room_id = room_id.to_string();
            let user_id = uid.to_string();
            let display_name = display_name.unwrap_or(uid).to_string();
            let is_guest = self.session_is_guest.remove(session_id).unwrap_or(false);
            let session_manager = self.session_manager.clone();
            let nc = self.nats_connection.clone();
            let session_id_val = *session_id;

            // Observer sessions (waiting room) should not publish PARTICIPANT_LEFT
            // since they were never real participants in the meeting.
            if observer {
                info!(
                    "Observer session {} for {} leaving room {} - skipping PARTICIPANT_LEFT",
                    session_id_val, user_id, room_id
                );
                tokio::spawn(async move {
                    if let Err(e) = session_manager.end_session(&room_id, &user_id).await {
                        error!("Error ending observer session for room {}: {}", room_id, e);
                    }
                });
                return;
            }

            if let Some(state) = self.connection_states.get(session_id) {
                if *state != ConnectionState::Active {
                    info!(
                        "Skipping PARTICIPANT_LEFT for non-active session {}",
                        session_id
                    );
                    return;
                }
            }

            tokio::spawn(async move {
                // Check host-leave behavior first: if the host is leaving and
                // end_on_host_leave is set, end the meeting for all participants.
                if is_host && effective_end_on_host_leave {
                    info!(
                        "Host {} left room {} - ending meeting for all",
                        user_id, room_id
                    );
                    let subject = format!("room.{}.system", room_id.replace(' ', "_"));
                    // First emit PARTICIPANT_LEFT so clients remove the host's video tile
                    // before the MEETING_ENDED overlay renders. Without this, the host's
                    // tile remains as a ghost until teardown ordering resolves it.
                    let left_bytes = SessionManager::build_peer_left_packet(
                        &room_id,
                        &user_id,
                        session_id_val,
                        &display_name,
                        is_guest,
                    );
                    if let Err(e) = nc.publish(subject.clone(), left_bytes.into()).await {
                        error!("Error publishing PARTICIPANT_LEFT for host: {}", e);
                    }
                    // Then end the meeting for all remaining participants.
                    let ended_bytes = SessionManager::build_meeting_ended_packet(
                        &room_id,
                        "The host has ended the meeting",
                    );
                    if let Err(e) = nc.publish(subject, ended_bytes.into()).await {
                        error!("Error publishing MEETING_ENDED: {}", e);
                    }
                    if let Err(e) = session_manager.end_session(&room_id, &user_id).await {
                        error!("Error ending host session for room {}: {}", room_id, e);
                    }
                    // Notify meeting-api so it can transition the meeting's
                    // DB row to `state='ended'`. This mirrors the REST
                    // POST /leave flow's `db_meetings::end_meeting` call so
                    // the meetings list stays consistent with the
                    // MEETING_ENDED broadcast clients just received. We
                    // only fire this on the legitimate broadcast path —
                    // never when `effective_end_on_host_leave=false` —
                    // and the reconnect grace period is already honored
                    // because this code only runs from
                    // `ExecutePendingDeparture` (or explicit `Leave`),
                    // both of which are cancelled by a timely reconnect.
                    let payload = MeetingEndedByHostPayload {
                        room_id: room_id.clone(),
                    };
                    match serde_json::to_vec(&payload) {
                        Ok(json) => {
                            if let Err(e) =
                                nc.publish(MEETING_ENDED_BY_HOST_SUBJECT, json.into()).await
                            {
                                error!(
                                    "Failed to publish {} for room {}: {}",
                                    MEETING_ENDED_BY_HOST_SUBJECT, room_id, e
                                );
                            }
                        }
                        Err(e) => {
                            error!("Failed to serialize MeetingEndedByHostPayload: {}", e);
                        }
                    }
                } else {
                    // Normal participant departure
                    match session_manager.end_session(&room_id, &user_id).await {
                        Ok(SessionEndResult::HostEndedMeeting) => {
                            // SessionManager indicated host ended meeting
                            // (future-proofing for server-side tracking)
                            info!(
                                "Host {} left room {} - ending meeting for all (via SessionManager)",
                                user_id, room_id
                            );
                            let subject = format!("room.{}.system", room_id.replace(' ', "_"));
                            // Emit PARTICIPANT_LEFT first so clients clean up the host's tile.
                            let left_bytes = SessionManager::build_peer_left_packet(
                                &room_id,
                                &user_id,
                                session_id_val,
                                &display_name,
                                is_guest,
                            );
                            if let Err(e) = nc.publish(subject.clone(), left_bytes.into()).await {
                                error!("Error publishing PARTICIPANT_LEFT for host: {}", e);
                            }
                            let ended_bytes = SessionManager::build_meeting_ended_packet(
                                &room_id,
                                "The host has ended the meeting",
                            );
                            if let Err(e) = nc.publish(subject, ended_bytes.into()).await {
                                error!("Error publishing MEETING_ENDED: {}", e);
                            }
                        }
                        Ok(SessionEndResult::LastParticipantLeft) => {
                            info!("Last participant {} left room {}", user_id, room_id);
                        }
                        Ok(SessionEndResult::MeetingContinues { remaining_count }) => {
                            info!(
                                "Participant {} left room {}, {} remaining",
                                user_id, room_id, remaining_count
                            );
                            // Notify remaining peers about the departed session
                            let bytes = SessionManager::build_peer_left_packet(
                                &room_id,
                                &user_id,
                                session_id_val,
                                &display_name,
                                is_guest,
                            );
                            let subject = format!("room.{}.system", room_id.replace(' ', "_"));
                            if let Err(e) = nc.publish(subject, bytes.into()).await {
                                error!("Error publishing PARTICIPANT_LEFT: {}", e);
                            }
                        }
                        Err(e) => {
                            error!("Error ending session for room {}: {}", room_id, e);
                        }
                    }

                    // Presence-driven empty→idle transition (everyone left a
                    // meeting that did NOT end). We only reach here on the
                    // normal-departure path — NOT the host-leave-ends-meeting
                    // path above, where END must win and emitting an idle event
                    // would be wrong. A non-ending host leave
                    // (`end_on_host_leave=false`) flows through here too and is
                    // treated as a normal departure that contributes to the
                    // empty→idle transition, exactly as required.
                    //
                    // `room_became_empty` was computed synchronously in the
                    // actor BEFORE this spawn, from the `room_members` count
                    // reaching zero, so this publishes ONCE per
                    // room-becomes-empty (not once per disconnect). meeting-api
                    // resolves room_id->meeting and calls `set_idle`, which
                    // no-ops on an already-ended meeting — so even if a stray
                    // END races this idle event, ended (terminal) still wins.
                    //
                    // Multi-replica note: `room_members` is per-replica, the
                    // same assumption the host-leave→end detection already
                    // makes. "Empty" here means "empty on this replica". We do
                    // not introduce a stronger cross-replica guarantee than the
                    // existing host-leave path has.
                    if room_became_empty {
                        info!(
                            "Room {} became empty after {} left - notifying meeting-api (empty->idle)",
                            room_id, user_id
                        );
                        let payload = MeetingBecameEmptyPayload {
                            room_id: room_id.clone(),
                        };
                        match serde_json::to_vec(&payload) {
                            Ok(json) => {
                                if let Err(e) =
                                    nc.publish(MEETING_BECAME_EMPTY_SUBJECT, json.into()).await
                                {
                                    error!(
                                        "Failed to publish {} for room {}: {}",
                                        MEETING_BECAME_EMPTY_SUBJECT, room_id, e
                                    );
                                }
                            }
                            Err(e) => {
                                error!("Failed to serialize MeetingBecameEmptyPayload: {}", e);
                            }
                        }
                    }
                }
            });
        }
    }

    /// Get the session manager (for use by chat_session)
    pub fn session_manager(&self) -> &SessionManager {
        &self.session_manager
    }

    /// Evict a stale session for the given `instance_id`, if present locally.
    ///
    /// Returns `true` if a session was evicted.
    /// `skip_session_id` is the new session's ID — used as a self-delivery guard
    /// to prevent the publishing server from double-evicting.
    fn evict_stale_session(
        &mut self,
        instance_id: &str,
        room: &str,
        user_id: &str,
        skip_session_id: SessionId,
        ctx: &mut Context<Self>,
    ) -> bool {
        let prev_sid = match self.instance_index.get(instance_id) {
            Some(&sid) => sid,
            None => return false,
        };

        if prev_sid == skip_session_id {
            return false;
        }

        let user_matches = self
            .room_members
            .get(room)
            .map(|members| {
                members
                    .iter()
                    .any(|m| m.session == prev_sid && m.user_id == user_id)
            })
            .unwrap_or(false);

        if !user_matches {
            return false;
        }

        info!(
            "Evicting stale session {} for instance {} (user {} in room {}) \
             in favour of session {}",
            prev_sid, instance_id, user_id, room, skip_session_id
        );

        self.forget_session(prev_sid, room, user_id, ctx);

        true
    }

    /// Drop the cached `room_policy` entry for `room` when the room has been
    /// drained to empty. Centralises the empty-room cleanup rule shared by
    /// `leave_rooms` and `ExecutePendingDeparture`'s `was_active=false` branch.
    ///
    /// Policy eviction fires only when `room_members.get(room)` exists and is
    /// empty — i.e. we (or our caller) just removed the last member. If the
    /// room has no `room_members` entry at all, that means the cache was
    /// seeded by a `MEETING_SETTINGS_UPDATE_SUBJECT` event before any
    /// JoinRoom (the `UpdateRoomPolicy` handler accepts events for empty
    /// rooms). Wiping the legitimately-cached policy in that window would be
    /// a regression, so the helper is a no-op for the "no entry" case.
    fn forget_room_if_empty(&mut self, room: &str) {
        if let Some(members) = self.room_members.get(room) {
            if members.is_empty() {
                self.room_members.remove(room);
                self.room_policy.remove(room);
                // Bound room-labeled relay series to LIVE rooms (issue #996):
                // remove every `{room=...}` CounterVec/GaugeVec series for this
                // drained room (was previously just the #988 viewport gauge).
                // CounterVec series otherwise persist for the process lifetime,
                // so each distinct meeting would leak a permanent series. We
                // keep the `room` label (the meeting-investigation dashboard and
                // RelayPacketDrops alert depend on it) and instead expire it on
                // room drain — see `metrics::forget_room_metrics`.
                crate::metrics::forget_room_metrics(room);
            }
        }
    }

    /// Tear down all per-session state for `session_id` and cancel any
    /// pending departure for this session's `(room, instance_key)`. Shared
    /// by both eviction paths so the cleanup surface stays in lockstep — if
    /// a new per-session HashMap is added to [`ChatServer`], it MUST be
    /// cleaned up here.
    ///
    /// `user_id` is retained as a parameter for log readability only — the
    /// pending-departure key is **not** derived from it (see
    /// [`Self::pending_departures`]).
    ///
    /// Does NOT broadcast `PARTICIPANT_LEFT` — both eviction paths are
    /// silent (the new session that triggered the eviction will set
    /// [`Self::suppress_join_broadcast`] for itself so peers see neither
    /// a leave nor a redundant join — they see continuity).
    fn forget_session(
        &mut self,
        session_id: SessionId,
        room: &str,
        user_id: &str,
        ctx: &mut Context<Self>,
    ) {
        // Resolve the pending-departure key BEFORE removing session_instance
        // so the lookup still finds this session's instance_id.
        let instance_key = self.pending_departure_instance_key(session_id);

        let mut room_still_populated = false;
        if let Some(members) = self.room_members.get_mut(room) {
            members.retain(|m| m.session != session_id);
            if members.is_empty() {
                self.room_members.remove(room);
                // Mirror forget_room_if_empty: release ALL per-room relay series
                // so the eviction teardown path also cannot leak room-labeled
                // series for a drained room (HCL #988 + #996).
                crate::metrics::forget_room_metrics(room);
            } else {
                room_still_populated = true;
            }
        }

        if let Some(task) = self.active_subs.remove(&session_id) {
            task.abort();
        }

        let _ = self.sessions.remove(&session_id);
        let _ = self.connection_states.remove(&session_id);
        let _ = self.suppress_join_broadcast.remove(&session_id);
        let _ = self.session_is_guest.remove(&session_id);
        let _ = self.session_desired_streams.remove(&session_id);
        // Drop the per-session layer-preference map (#989, Phase 1b). Mirrors
        // the `leave_rooms` cleanup; both teardown paths must release it.
        let _ = self.session_layer_prefs.remove(&session_id);
        let _ = self.session_room.remove(&session_id);

        // Publish-side suppression teardown + restore (#1108, Stage 3) — the
        // eviction analog of the `leave_rooms` trigger. An evicted session may
        // have been a publisher (reap its own per-source hint state) and/or a
        // constraining receiver (its departure can RAISE a remaining publisher's
        // union → restore-eager). Recompute room-wide via the actor only while
        // the room still has members. NOTE: in the common reconnection case the
        // replacement session re-joins under a fresh session_id and re-sends its
        // LAYER_PREFERENCE, which independently re-triggers a recompute; this
        // path additionally covers a pure eviction with no replacement.
        //
        // #1203: COALESCED behind the trailing debounce, same as the
        // `leave_rooms` departure trigger above — eviction is a departure and a
        // reconnection wave evicts many stale sessions in a burst.
        self.forget_layer_hint_state_for_source(room, session_id);
        if room_still_populated {
            self.schedule_coalesced_recompute(room, ctx);
        }

        // Cancel any deferred PARTICIPANT_LEFT for this session's tab. If we
        // evict them while a departure is pending, we want the new session to
        // pick up cleanly without the old one's deferred broadcast firing.
        let departure_key = (room.to_string(), instance_key);
        if let Some(pending) = self.pending_departures.remove(&departure_key) {
            ctx.cancel_future(pending.spawn_handle);
            info!(
                "Cancelled pending departure during forget_session for user {} in room {}",
                user_id, room
            );
        }

        // instance_index uses instance_id as the key, so we look it up via
        // the reverse map and only delete the forward entry if it still
        // points at this session (it may have been replaced concurrently
        // by an eviction in flight).
        if let Some(iid) = self.session_instance.remove(&session_id) {
            if self.instance_index.get(&iid).copied() == Some(session_id) {
                self.instance_index.remove(&iid);
            }
        }
    }

    /// Build the second component of the [`Self::pending_departures`] key for
    /// a session that has registered an `instance_id` (the common path for
    /// every client coming through `JoinRoom` with a fresh sessionStorage
    /// UUID). Falls back to a per-session sentinel when no `instance_id` is
    /// known — this guarantees that even legacy clients without an
    /// `instance_id` cannot collide on the key with sibling sessions of the
    /// same `user_id`.
    fn pending_departure_instance_key(&self, session: SessionId) -> String {
        match self.session_instance.get(&session) {
            Some(iid) => iid.clone(),
            None => format!("__session__:{session}"),
        }
    }

    // -----------------------------------------------------------------------
    // Publish-side layer suppression (#1108, Stage 3 — LAYER_HINT)
    // -----------------------------------------------------------------------

    /// Compute the per-source layer UNION for one `(source, media_kind)` over
    /// every receiver currently in `room` (#1108, Stage 3).
    ///
    /// Thin `&self` wrapper that resolves the room's member session ids and
    /// delegates the actual (pure, fail-open, DoS-bounded) max computation to
    /// [`compute_max_requested_layer`]. Returns the full-ladder sentinel when the
    /// room is unknown (fail-open: nothing to suppress).
    fn max_requested_layer(&self, room: &str, source: SessionId, kind: i32) -> u32 {
        let Some(members) = self.room_members.get(room) else {
            // Unknown room → no actionable union; fail-open.
            return LAYER_HINT_FULL_LADDER_SENTINEL;
        };
        let receiver_ids: Vec<SessionId> = members.iter().map(|m| m.session).collect();
        compute_max_requested_layer(&receiver_ids, &self.session_layer_prefs, source, kind)
    }

    /// Recompute the layer union for a SINGLE source across all media kinds and
    /// emit / schedule / skip a LAYER_HINT per the debounce policy (#1108).
    ///
    /// Collects every kind whose debounce decision is `Emit` into a single
    /// `LayerHintPacket` and publishes it once to the publisher's self-subject.
    /// Kinds that need the suppress-lazy window cause one deferred
    /// `notify_later` re-check to be scheduled (idempotent — the re-check simply
    /// recomputes the then-current union).
    fn recompute_layer_hints_for_source(
        &mut self,
        room: &str,
        source: SessionId,
        ctx: &mut Context<Self>,
    ) {
        // Only emit hints for a source that is an actual current publisher in the
        // room. If it has already left (no member row), there is nothing to hint
        // and its debounce state was/will be reaped by the teardown path.
        let is_member = self
            .room_members
            .get(room)
            .is_some_and(|members| members.iter().any(|m| m.session == source));
        if !is_member {
            return;
        }

        let now = std::time::Instant::now();
        let window = std::time::Duration::from_millis(LAYER_HINT_SUPPRESS_DEBOUNCE_MS);

        let mut entries: Vec<LayerHintEntry> = Vec::new();
        let mut emit_directions: Vec<LayerHintDirection> = Vec::new();
        let mut schedule_recheck = false;

        for &kind in LAYER_HINT_MEDIA_KINDS.iter() {
            let union = self.max_requested_layer(room, source, kind);
            let key = (room.to_string(), source, kind);
            let prev = self.layer_hint_state.get(&key).copied();

            match decide_layer_hint(prev, union, now, window) {
                LayerHintDecision::Emit { value, direction } => {
                    // Record the emission: clear any pending downgrade and store
                    // the value we are about to tell the publisher.
                    self.layer_hint_state.insert(
                        key,
                        LayerHintEmitState {
                            last_emitted: value,
                            pending_lower_since: None,
                        },
                    );
                    let mut entry = LayerHintEntry::new();
                    entry.media_kind = layer_hint_media_kind(kind).into();
                    entry.max_requested_layer = value;
                    entries.push(entry);
                    emit_directions.push(direction);
                }
                LayerHintDecision::ScheduleRecheck { .. } => {
                    // Mark the pending downgrade so the (eventual) re-check knows
                    // the window is already counting, and arrange exactly one
                    // deferred recompute for this source.
                    let entry = self
                        .layer_hint_state
                        .entry(key)
                        .or_insert(LayerHintEmitState {
                            last_emitted: LAYER_HINT_FULL_LADDER_SENTINEL,
                            pending_lower_since: None,
                        });
                    if entry.pending_lower_since.is_none() {
                        entry.pending_lower_since = Some(now);
                        schedule_recheck = true;
                    }
                }
                LayerHintDecision::SkipClearPending => {
                    // Demand returned to the last-emitted level: cancel a pending
                    // downgrade so a future drop starts a fresh debounce window
                    // (otherwise a stale `pending_lower_since` would let a much
                    // later drop bypass the suppress-lazy delay).
                    if let Some(state) = self.layer_hint_state.get_mut(&key) {
                        state.pending_lower_since = None;
                    }
                }
                LayerHintDecision::SkipKeepPending => {
                    // Still counting down toward the original deadline — leave the
                    // pending timestamp untouched.
                }
            }
        }

        if !entries.is_empty() {
            self.emit_layer_hint(room, source, entries, &emit_directions);
        }

        if schedule_recheck {
            // Deferred suppress-lazy re-check: re-evaluate this source's unions
            // after the debounce window. Re-running is safe/idempotent — the
            // decision is a pure function of persisted state + the then-current
            // union, so a flapping receiver that has since restored demand simply
            // yields a skip (or an eager restore) at the deadline instead of the
            // suppress.
            ctx.notify_later(
                RecomputeLayerHints {
                    room: room.to_string(),
                    source: Some(source),
                },
                window,
            );
        }
    }

    /// Build a `LayerHintPacket` from `entries`, wrap it in a relay-authored
    /// `PacketWrapper { packet_type: LAYER_HINT, .. }`, and publish it to the
    /// publisher's OWN per-session NATS subject `room.{room}.{publisher}` — the
    /// same self-subject delivery the CONGESTION self-packet uses (#1108).
    ///
    /// `directions` is parallel to `entries` and used only to attribute each
    /// emission on the `relay_layer_hint_emitted_total` metric.
    fn emit_layer_hint(
        &self,
        room: &str,
        publisher: SessionId,
        entries: Vec<LayerHintEntry>,
        directions: &[LayerHintDirection],
    ) {
        // Resolve the publisher's user_id from room_members for the wrapper's
        // `user_id` field (cosmetic / consistency with other self-packets; the
        // delivery subject is what scopes the hint). Empty if unknown.
        let user_id = self
            .room_members
            .get(room)
            .and_then(|members| members.iter().find(|m| m.session == publisher))
            .map(|m| m.user_id.clone())
            .unwrap_or_default();

        let mut inner = LayerHintPacket::new();
        inner.entries = entries;

        let data = match inner.write_to_bytes() {
            Ok(b) => b,
            Err(e) => {
                warn!(
                    "Failed to serialize LayerHintPacket for publisher {} in room {}: {}",
                    publisher, room, e
                );
                return;
            }
        };

        let mut wrapper = PacketWrapper::new();
        wrapper.packet_type = PacketType::LAYER_HINT.into();
        wrapper.session_id = publisher;
        wrapper.user_id = user_id.into_bytes();
        wrapper.data = data;

        let bytes = match wrapper.write_to_bytes() {
            Ok(b) => b,
            Err(e) => {
                warn!(
                    "Failed to serialize LAYER_HINT PacketWrapper for publisher {} in room {}: {}",
                    publisher, room, e
                );
                return;
            }
        };

        // The publisher's own self-subject, sanitized identically to every other
        // room subject (room ids match `^[a-zA-Z0-9_-]*$`, the session is a
        // u64). This is exactly the subject the publisher subscribes to and the
        // same one the server's CONGESTION/system self-packets are published on.
        let subject = format!("room.{room}.{publisher}").replace(' ', "_");

        // Account every emitted entry by its direction BEFORE the async publish
        // so the metric reflects the decision even if the publish later fails.
        let room_label = room.to_string();
        for dir in directions {
            RELAY_LAYER_HINT_EMITTED_TOTAL
                .with_label_values(&[&room_label, dir.as_label()])
                .inc();
        }

        debug!(
            "Emitting LAYER_HINT to publisher {} in room {} on {} ({} entr{})",
            publisher,
            room,
            subject,
            directions.len(),
            if directions.len() == 1 { "y" } else { "ies" }
        );

        let nc = self.nats_connection.clone();
        tokio::spawn(async move {
            if let Err(e) = nc.publish(subject, bytes.into()).await {
                warn!(
                    "Failed to publish LAYER_HINT for publisher {} in room {}: {}",
                    publisher, room_label, e
                );
            }
        });
    }

    /// Drop all LAYER_HINT debounce state for a departed publisher `source` in
    /// `room` (#1108). Called from the teardown paths so the
    /// `layer_hint_state` map cannot leak entries for sessions that have left.
    fn forget_layer_hint_state_for_source(&mut self, room: &str, source: SessionId) {
        self.layer_hint_state
            .retain(|(r, s, _kind), _| !(r == room && *s == source));
    }

    /// Coalesce a DEPARTURE-driven (leave/evict) room-wide LAYER_HINT recompute
    /// for `room` behind the [`LAYER_HINT_RECOMPUTE_COALESCE_MS`] trailing
    /// debounce (#1203), instead of `do_send`-ing a recompute immediately.
    ///
    /// Records `room` in [`pending_recompute_rooms`] and arms ONE
    /// [`FlushPendingRecomputes`] timer if none is already in flight. A re-arm
    /// while a timer is pending is a deliberate NO-OP: the existing trailing
    /// deadline still fires and will drain whatever rooms accumulated, so a burst
    /// of N departures across the window produces exactly ONE flush (and one
    /// recompute per distinct affected room) — TRAILING coalescing, not
    /// per-event. This is the correct direction for departures: a departure can
    /// only RAISE a remaining publisher's fail-open union (restore), and nobody is
    /// actively waiting on it, so computing the FINAL union once the burst settles
    /// is both cheaper and more correct than recomputing over transient
    /// intermediate membership. See [`LAYER_HINT_RECOMPUTE_COALESCE_MS`] for why
    /// JOIN / per-LAYER_PREFERENCE recomputes intentionally bypass this path.
    fn schedule_coalesced_recompute(&mut self, room: &str, ctx: &mut Context<Self>) {
        self.pending_recompute_rooms.insert(room.to_string());
        if self.recompute_coalesce_handle.is_none() {
            let handle = ctx.notify_later(
                FlushPendingRecomputes,
                std::time::Duration::from_millis(LAYER_HINT_RECOMPUTE_COALESCE_MS),
            );
            self.recompute_coalesce_handle = Some(handle);
        }
    }

    /// Refresh the DEMAND-side gauge `relay_layer_preference_sessions{room, kind,
    /// layer_id}` for every live room (#1170 item 2).
    ///
    /// READ-ONLY: takes each session's `LayerPrefs` read lock and never a write
    /// lock; mutates no actor state. Cost is O(rooms × min(sessions, 256)) —
    /// per-room session scans are bounded by [`LAYER_HINT_MAX_RECEIVERS_SCANNED`]
    /// exactly as the union scan is, so a pathological room cannot stall the
    /// actor. For each active room it SETs all
    /// `LAYER_PREFERENCE_GAUGE_KINDS.len() × RELAY_LAYER_ID_BUCKETS.len()` cells
    /// (explicit zeros included) so a bucket whose demand vanished reads `0`
    /// rather than a stale count. Drained rooms are not iterated here (they are
    /// gone from `room_members`); their series are removed by
    /// [`crate::metrics::forget_room_metrics`] at drain time.
    fn sweep_layer_preference_gauge(&self) {
        for (room, members) in &self.room_members {
            // Tally counts: [kind_idx][bucket_idx]. Indexed parallel to
            // LAYER_PREFERENCE_GAUGE_KINDS and RELAY_LAYER_ID_BUCKETS.
            let mut counts =
                [[0u64; RELAY_LAYER_ID_BUCKETS.len()]; LAYER_PREFERENCE_GAUGE_KINDS.len()];

            for member in members.iter().take(LAYER_HINT_MAX_RECEIVERS_SCANNED) {
                let Some(prefs) = self.session_layer_prefs.get(&member.session) else {
                    // No prefs handle for this session at all = fail-open
                    // (expressed no demand) → contributes nothing, like an empty
                    // map. Matches the union scan's `None` arm.
                    continue;
                };
                // Lock-free short-circuit: an empty prefs map (no LAYER_PREFERENCE
                // recorded yet) expresses no demand for any kind, so skip the read
                // lock entirely — keeps the simulcast-off / no-prefs case free.
                if !prefs.has_any() {
                    continue;
                }
                // Poisoned lock = fail-open (uncounted), mirroring the forward
                // filter's `unwrap_or`.
                let Ok(guard) = prefs.state.read() else {
                    continue;
                };
                let buckets = classify_session_max_layer_buckets(&guard.layers);
                drop(guard);
                for (kind_idx, bucket) in buckets.iter().enumerate() {
                    // `None` = no preference for this kind = fail-open, not counted.
                    if let Some(bucket) = bucket {
                        let bucket_idx = RELAY_LAYER_ID_BUCKETS
                            .iter()
                            .position(|b| b == bucket)
                            .expect("layer_id_bucket only returns RELAY_LAYER_ID_BUCKETS values");
                        counts[kind_idx][bucket_idx] += 1;
                    }
                }
            }

            // SET every cell for this active room, including zeros, so a bucket
            // that lost all demand since the last sweep reads 0 not a stale value.
            for (kind_idx, (_, kind_label)) in LAYER_PREFERENCE_GAUGE_KINDS.iter().enumerate() {
                for (bucket_idx, bucket_label) in RELAY_LAYER_ID_BUCKETS.iter().enumerate() {
                    RELAY_LAYER_PREFERENCE_SESSIONS
                        .with_label_values(&[room, kind_label, bucket_label])
                        .set(counts[kind_idx][bucket_idx] as f64);
                }
            }
        }
    }
}

impl Actor for ChatServer {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        info!(
            "ChatServer started — subscribing to {} and {}",
            EVICT_INSTANCE_SUBJECT, MEETING_SETTINGS_UPDATE_SUBJECT
        );

        // Subscribe to the cross-server eviction subject.
        let nc_evict = self.nats_connection.clone();
        let addr_evict = ctx.address();
        tokio::spawn(async move {
            loop {
                match nc_evict.subscribe(EVICT_INSTANCE_SUBJECT).await {
                    Ok(mut sub) => {
                        while let Some(msg) = sub.next().await {
                            match serde_json::from_slice::<EvictInstancePayload>(&msg.payload) {
                                Ok(payload) => {
                                    addr_evict.do_send(EvictInstance(payload));
                                }
                                Err(e) => {
                                    warn!("Failed to deserialize evict_instance payload: {}", e);
                                }
                            }
                        }
                        warn!(
                            "{} subscription stream ended, re-subscribing in 1s",
                            EVICT_INSTANCE_SUBJECT
                        );
                    }
                    Err(e) => {
                        error!(
                            "Failed to subscribe to {}: {}, retrying in 1s",
                            EVICT_INSTANCE_SUBJECT, e
                        );
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });

        // Subscribe to per-meeting policy updates from meeting-api so the
        // `room_policy` cache stays fresh after PATCH /meetings. Each
        // chat_server instance subscribes independently (no queue group)
        // because every server holds its own room_policy cache and must
        // receive every update — this is the same fan-out semantics as
        // EVICT_INSTANCE_SUBJECT.
        let nc_settings = self.nats_connection.clone();
        let addr_settings = ctx.address();
        tokio::spawn(async move {
            loop {
                match nc_settings.subscribe(MEETING_SETTINGS_UPDATE_SUBJECT).await {
                    Ok(mut sub) => {
                        while let Some(msg) = sub.next().await {
                            match serde_json::from_slice::<MeetingSettingsUpdatePayload>(
                                &msg.payload,
                            ) {
                                Ok(payload) => {
                                    addr_settings.do_send(UpdateRoomPolicy(payload));
                                }
                                Err(e) => {
                                    warn!(
                                        "Failed to deserialize {} payload: {}",
                                        MEETING_SETTINGS_UPDATE_SUBJECT, e
                                    );
                                }
                            }
                        }
                        warn!(
                            "{} subscription stream ended, re-subscribing in 1s",
                            MEETING_SETTINGS_UPDATE_SUBJECT
                        );
                    }
                    Err(e) => {
                        error!(
                            "Failed to subscribe to {}: {}, retrying in 1s",
                            MEETING_SETTINGS_UPDATE_SUBJECT, e
                        );
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });

        // Arm the periodic DEMAND-side gauge sweep (#1170 item 2). `run_interval`
        // invokes the closure inside the actor with `&mut self` access, so the
        // sweep runs on the actor thread and reads `room_members` /
        // `session_layer_prefs` directly — no cross-thread snapshot needed. We
        // delegate to a `do_send` of `SweepLayerPreferenceGauge` (rather than
        // inlining) so the sweep body lives in one Handler that is reachable from
        // tests and keeps each tick a discrete, short mailbox message rather than
        // work spliced into the timer driver.
        ctx.run_interval(LAYER_PREFERENCE_SESSIONS_SWEEP_INTERVAL, |_act, ctx| {
            ctx.address().do_send(SweepLayerPreferenceGauge);
        });
    }

    /// Cancel the in-flight #1203 coalescing timer on actor stop so a stopping
    /// `ChatServer` leaks no `SpawnHandle` (mirrors the `pending_departures`
    /// `cancel_future` cleanup elsewhere). Dropping any not-yet-flushed pending
    /// rooms is correct: a stopping relay has no publishers left to hint.
    fn stopping(&mut self, ctx: &mut Self::Context) -> actix::Running {
        if let Some(handle) = self.recompute_coalesce_handle.take() {
            ctx.cancel_future(handle);
        }
        self.pending_recompute_rooms.clear();
        actix::Running::Stop
    }
}

impl Handler<Connect> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: Connect, _ctx: &mut Self::Context) -> Self::Result {
        let Connect { id, addr } = msg;
        self.sessions.insert(id, addr);
        self.connection_states.insert(id, ConnectionState::Testing);
    }
}

impl Handler<Disconnect> for ChatServer {
    type Result = ();

    fn handle(
        &mut self,
        Disconnect {
            session,
            room,
            user_id,
            display_name,
            is_guest,
            observer,
            is_host,
            end_on_host_leave,
        }: Disconnect,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        // Persist the authoritative guest flag so leave_rooms can populate
        // PARTICIPANT_LEFT consistently even if the original JoinRoom entry
        // was evicted. `is_guest` for a session never changes mid-lifetime.
        self.session_is_guest.entry(session).or_insert(is_guest);
        // If the session was already evicted (by a reconnecting instance_id),
        // its entries in sessions/connection_states were removed during eviction.
        // Ignore the stale Disconnect so it doesn't clobber a newer session's
        // pending departure for the same (room, user_id) key.
        let was_present = self.sessions.remove(&session).is_some();
        if !was_present {
            info!(
                "Disconnect for already-evicted session {} (user {} in room {}) — ignoring",
                session, user_id, room
            );
            return;
        }

        // Clean up session-level state immediately — the transport is gone.
        // Capture whether the session was Active before removing the state,
        // so we can store it in PendingDepartureState for the grace period.
        let was_active = self
            .connection_states
            .get(&session)
            .map(|s| *s == ConnectionState::Active)
            .unwrap_or(false);
        let _ = self.connection_states.remove(&session);
        let _ = self.suppress_join_broadcast.remove(&session);

        // Observers and non-active sessions bypass the grace period — they
        // never triggered PARTICIPANT_JOINED, so there is nothing to defer.
        if observer {
            self.leave_rooms(
                LeaveContext {
                    session_id: &session,
                    room: Some(&room),
                    user_id: Some(&user_id),
                    display_name: Some(&display_name),
                    observer: true,
                    is_host: false,
                    end_on_host_leave: true,
                },
                ctx,
            );
            return;
        }

        // Remove the NATS subscription task immediately so the old session
        // stops receiving media. Keep room_members intact for now — they
        // will be cleaned up either on reconnection or when the grace
        // period expires.
        if let Some(task) = self.active_subs.remove(&session) {
            task.abort();
        }

        // If there is already a pending departure for THIS tab / client
        // instance (same `(room, instance_key)`), cancel the old timer and
        // replace it. This handles the edge case of rapid
        // disconnect-reconnect-disconnect cycles within a single tab.
        //
        // BUG FIX (introduced by 0844f062 / batch merge of PRs #793 et al.):
        // The original code cancelled the old timer but did NOT remove the
        // replaced session from room_members. During RTT election, N candidate
        // connections all call JoinRoom (adding N room_members entries for the
        // same user_id). When the N-1 losers disconnect in rapid succession,
        // each Disconnect replaces the previous pending departure — but only
        // the *last* replacement's session gets cleaned up when the grace
        // period expires. The earlier sessions become permanent orphans in
        // room_members, appearing as phantom peers that trigger PLI storms
        // and freeze real participants' video.
        //
        // BUG FIX (issue #852, absorbed into #851): the key used to be
        // `(room, user_id)`, which collided across distinct sessions of the
        // same `user_id` after PR #851 allowed multi-session-per-user. A
        // disconnect of session B then incorrectly cancelled session A's
        // grace timer and silently dropped A from `room_members`. The key is
        // now `(room, instance_key)` where `instance_key` is the per-tab
        // identifier — sibling sessions of the same user get distinct
        // entries, but same-tab refresh still collapses cleanly.
        let instance_key = self.pending_departure_instance_key(session);
        let key = (room.clone(), instance_key.clone());
        if let Some(old) = self.pending_departures.remove(&key) {
            ctx.cancel_future(old.spawn_handle);
            // Clean up the replaced session's room_members entry to prevent
            // orphaned phantom peers.
            if let Some(members) = self.room_members.get_mut(&room) {
                members.retain(|m| m.session != old.old_session);
            }
            info!(
                "Replaced existing pending departure for instance {} (user {}) in \
                 room {} (old session {})",
                instance_key, user_id, room, old.old_session
            );
        }

        info!(
            "Deferring PARTICIPANT_LEFT for instance {} (user {}, session {}) in \
             room {} — grace period {:?}",
            instance_key, user_id, session, room, RECONNECT_GRACE_PERIOD
        );

        let handle = ctx.notify_later(
            ExecutePendingDeparture {
                session,
                room: room.clone(),
                user_id: user_id.clone(),
                instance_key: instance_key.clone(),
                display_name,
                is_host,
                end_on_host_leave,
            },
            RECONNECT_GRACE_PERIOD,
        );

        self.pending_departures.insert(
            key,
            PendingDepartureState {
                spawn_handle: handle,
                old_session: session,
                was_active,
            },
        );
    }
}

impl Handler<Leave> for ChatServer {
    type Result = ();

    fn handle(
        &mut self,
        Leave {
            session,
            room,
            user_id,
        }: Leave,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        // Cancel any pending departure for THIS session's tab / client
        // instance (`(room, instance_key)`) to avoid a duplicate
        // PARTICIPANT_LEFT when the grace-period timer fires later.
        // We don't need ctx.cancel_future() because ExecutePendingDeparture::handle
        // already checks whether the entry exists in pending_departures — once
        // removed, the timer becomes a no-op.
        //
        // BUG FIX (issue #852, absorbed into #851): keying by
        // `(room, user_id)` here used to let an explicit Leave from session A
        // silently cancel session B's pending grace timer when both sessions
        // belonged to the same `user_id`. The key now uses the per-tab
        // `instance_key` so an explicit Leave only cancels its own session's
        // pending state, never a sibling's.
        let instance_key = self.pending_departure_instance_key(session);
        let key = (room.clone(), instance_key.clone());
        if self.pending_departures.remove(&key).is_some() {
            info!(
                "Cancelled pending departure for instance {} (user {}) in room {} — \
                 explicit Leave received",
                instance_key, user_id, room
            );
        }

        // Look up is_host, end_on_host_leave, and display_name from room_members.
        // The Leave message carries no host info; we must resolve it from the
        // in-memory member table so the host-leave path in leave_rooms fires
        // correctly when the host explicitly leaves.
        let (is_host, end_on_host_leave, display_name) = self
            .room_members
            .get(&room)
            .and_then(|members| members.iter().find(|m| m.session == session))
            .map(|m| (m.is_host, m.end_on_host_leave, Some(m.display_name.clone())))
            .unwrap_or((false, true, None));

        // Leave is always a real participant, never an observer.
        self.leave_rooms(
            LeaveContext {
                session_id: &session,
                room: Some(&room),
                user_id: Some(&user_id),
                display_name: display_name.as_deref(),
                observer: false,
                is_host,
                end_on_host_leave,
            },
            ctx,
        );
    }
}

impl Handler<ActivateConnection> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: ActivateConnection, ctx: &mut Self::Context) -> Self::Result {
        let ActivateConnection { session } = msg;
        let was_testing = if let Some(state) = self.connection_states.get_mut(&session) {
            if *state == ConnectionState::Testing {
                *state = ConnectionState::Active;
                info!("Session {} activated (Testing -> Active)", session);
                true
            } else {
                false
            }
        } else {
            self.connection_states
                .insert(session, ConnectionState::Active);
            info!(
                "Session {} activated (state was missing, created as Active)",
                session
            );
            // Treat missing state as a Testing -> Active transition so we
            // still broadcast PARTICIPANT_JOINED.
            true
        };

        // Resolve this session's (room, user_id) ONCE via the O(1) `session_room`
        // reverse index over every room's members.
        // Shared by the local same-instance eviction and the cross-server
        // eviction broadcast below; only the elected (activating) connection
        // needs it.
        let room_user: Option<(String, String)> = if was_testing {
            self.session_room.get(&session).and_then(|room_id| {
                self.room_members.get(room_id).and_then(|members| {
                    members
                        .iter()
                        .find(|m| m.session == session)
                        .map(|m| (room_id.clone(), m.user_id.clone()))
                })
            })
        } else {
            None
        };

        // --- Local same-instance eviction ---
        // Now that this session is elected (Testing → Active), evict any
        // same-instance sibling that is still in room_members. This is the
        // losing RTT-election candidate (WS or WT) whose JoinRoom ran BEFORE
        // ours and whose entry is now stale.
        if was_testing {
            if let Some(iid) = self.session_instance.get(&session).cloned() {
                if let Some((room_id, user_id)) = &room_user {
                    let evicted = self.evict_stale_session(&iid, room_id, user_id, session, ctx);
                    if evicted {
                        self.suppress_join_broadcast.insert(session);
                    }
                }
                // Claim the forward mapping now that we are the elected winner.
                self.instance_index.insert(iid, session);
            }
        }

        // --- Cross-server eviction broadcast ---
        // Deferred from JoinRoom to here so that only the elected connection
        // (the winner of RTT election) publishes. Testing connections that
        // lose the election never trigger a NATS eviction message.
        if was_testing {
            if let Some(iid) = self.session_instance.get(&session).cloned() {
                if let Some((room_id, user_id)) = &room_user {
                    let payload = EvictInstancePayload {
                        instance_id: iid,
                        room: room_id.clone(),
                        user_id: user_id.clone(),
                        new_session_id: session,
                    };
                    match serde_json::to_vec(&payload) {
                        Ok(json) => {
                            let nc = self.nats_connection.clone();
                            let fut = async move {
                                if let Err(e) =
                                    nc.publish(EVICT_INSTANCE_SUBJECT, json.into()).await
                                {
                                    error!("Failed to publish eviction to NATS: {}", e);
                                }
                            };
                            let fut = actix::fut::wrap_future::<_, Self>(fut);
                            ctx.spawn(fut);
                        }
                        Err(e) => {
                            error!("Failed to serialize EvictInstancePayload: {}", e);
                        }
                    }
                }
            }
        }

        // Broadcast PARTICIPANT_JOINED now that this connection is confirmed
        // as the elected/active one. During JoinRoom, the broadcast was deferred
        // to avoid ghost join events from Testing connections (e.g., the losing
        // connection during RTT election).
        //
        // Skip the broadcast for sessions marked in suppress_join_broadcast
        // (reconnection sessions and observer sessions).
        let suppressed = self.suppress_join_broadcast.remove(&session);
        if was_testing && !suppressed {
            // Look up the session's room, user_id, and display_name from room_members.
            let mut found: Option<(String, String, String)> = None;
            for (room_id, members) in &self.room_members {
                for m in members {
                    if m.session == session {
                        found = Some((room_id.clone(), m.user_id.clone(), m.display_name.clone()));
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }

            if let Some((room_id, user_id, display_name)) = found {
                let is_guest = self
                    .session_is_guest
                    .get(&session)
                    .copied()
                    .unwrap_or(false);
                let bytes = SessionManager::build_peer_joined_packet(
                    &room_id,
                    &user_id,
                    session,
                    &display_name,
                    is_guest,
                );
                let subject = format!("room.{}.system", room_id.replace(' ', "_"));
                info!(
                    "Publishing deferred PARTICIPANT_JOINED for {} (display={}, session={}) to {}",
                    user_id, display_name, session, subject
                );
                let nc = self.nats_connection.clone();
                let fut = async move {
                    if let Err(e) = nc.publish(subject, bytes.into()).await {
                        error!("Error publishing deferred PARTICIPANT_JOINED: {}", e);
                    }
                };
                let fut = actix::fut::wrap_future::<_, Self>(fut);
                ctx.spawn(fut);
            } else {
                // This can happen for observer sessions (not tracked in room_members)
                // or if the session was cleaned up before activation. Not an error.
                info!(
                    "Session {} activated but not found in room_members — \
                     skipping PARTICIPANT_JOINED (likely observer or already cleaned up)",
                    session
                );
            }
        }
    }
}

impl Handler<RebroadcastPresence> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: RebroadcastPresence, ctx: &mut Self::Context) -> Self::Result {
        let found = self
            .session_room
            .get(&msg.session)
            .and_then(|room_id| self.room_members.get(room_id).map(|m| (room_id.clone(), m)))
            .and_then(|(room_id, members)| {
                members
                    .iter()
                    .find(|m| m.session == msg.session)
                    .map(|m| (room_id, m.user_id.clone(), m.display_name.clone()))
            });

        if let Some((room_id, user_id, display_name)) = found {
            let responder_is_active =
                self.connection_states.get(&msg.session).copied() == Some(ConnectionState::Active);
            // The requester is "local" if this instance tracks its connection;
            // then the in-memory existing-member replay already delivered the
            // PARTICIPANT_JOINED, so a NATS reply would duplicate it.
            let requester_is_local = self.connection_states.contains_key(&msg.requester_session);
            let is_guest = self
                .session_is_guest
                .get(&msg.session)
                .copied()
                .unwrap_or(false);

            if let Some((subject, bytes)) = SessionManager::rebroadcast_reply_publication(
                &room_id,
                &user_id,
                &display_name,
                msg.session,
                msg.requester_session,
                is_guest,
                responder_is_active,
                requester_is_local,
            ) {
                info!(
                    "RebroadcastPresence: re-publishing PARTICIPANT_JOINED for {} (session={}) to requester {} via {}",
                    user_id, msg.session, msg.requester_session, subject
                );
                let nc = self.nats_connection.clone();
                let fut = async move {
                    if let Err(e) = nc.publish(subject, bytes.into()).await {
                        error!(
                            "RebroadcastPresence: failed to publish PARTICIPANT_JOINED: {}",
                            e
                        );
                    }
                };
                ctx.spawn(actix::fut::wrap_future::<_, Self>(fut));
            } else {
                debug!(
                    "RebroadcastPresence: no reply for session {} (active={}, requester {} local={})",
                    msg.session, responder_is_active, msg.requester_session, requester_is_local
                );
            }
        }
    }
}

/// Handle in-memory display-name updates triggered by NATS
/// PARTICIPANT_DISPLAY_NAME_CHANGED events.
///
/// Two paths:
///
/// * **Session-scoped (`msg.session_id != 0`, HCL issue #828):** rename the
///   single `room_members` row whose `(session, user_id)` pair matches.
///   Sibling sessions sharing the same `user_id` (e.g. another browser tab of
///   the same authenticated account) keep their existing names. When no row
///   matches — stale, forged, or cross-user `session_id` — the handler logs a
///   `warn!` and no-ops; it never falls through to the user-id-wide path,
///   because doing so would let an untrusted session_id control whose name
///   gets rewritten.
///
/// * **Legacy / user-id-wide (`msg.session_id == 0`):** preserves the pre-#828
///   behaviour for clients that haven't been updated yet — rename every row
///   matching `user_id`. This is the proto-3 default sentinel.
impl Handler<UpdateMemberDisplayName> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: UpdateMemberDisplayName, _ctx: &mut Self::Context) -> Self::Result {
        let validated_name = match validate_display_name(&msg.display_name) {
            Ok(name) => name,
            Err(e) => {
                warn!(
                    "UpdateMemberDisplayName: rejecting invalid display name from NATS for user {} in room {}: {}",
                    msg.user_id, msg.room_id, e
                );
                return;
            }
        };
        let Some(members) = self.room_members.get_mut(&msg.room_id) else {
            return;
        };

        if msg.session_id != 0 {
            // Session-scoped rename. Require the `(session, user_id)` pair to
            // exist in this room — otherwise the session_id is stale, forged,
            // or belongs to a different user, and we must not fall through to
            // the user-id-wide path.
            let mut updated = false;
            for member in members.iter_mut() {
                if member.session == msg.session_id && member.user_id == msg.user_id {
                    member.display_name.clone_from(&validated_name);
                    updated = true;
                    break;
                }
            }
            if !updated {
                warn!(
                    "UpdateMemberDisplayName: session_id {} does not belong to user {} in room {}, ignoring rename (stale, forged, or cross-user session_id)",
                    msg.session_id, msg.user_id, msg.room_id
                );
            }
        } else {
            // Legacy pre-#828 path: rename every session of this user. Kept
            // for backward compatibility with clients that don't yet supply
            // `session_id` in the REST request body.
            for member in members.iter_mut() {
                if member.user_id == msg.user_id {
                    member.display_name.clone_from(&validated_name);
                }
            }
        }
    }
}

/// Recompute per-source layer unions and emit LAYER_HINT packets (#1108,
/// Stage 3 — publish-side layer suppression).
///
/// Runs entirely in the actor because the union is an inverted query over the
/// receiver-keyed `session_layer_prefs` map (see [`RecomputeLayerHints`]). A
/// `Some(source)` recompute targets a single publisher; a `None` recompute fans
/// out over every current publisher in the room (used on join/leave, which can
/// shift many sources' fail-open unions at once).
///
/// SECURITY: there is intentionally NO inbound LAYER_HINT path — this handler is
/// the ONLY producer of LAYER_HINT, and it is driven exclusively by trusted
/// relay lifecycle events (subject-authoritative LAYER_PREFERENCE recording, and
/// join/leave). It never parses or trusts a client-sent LAYER_HINT.
/// Test-only counter of how many times `Handler<RecomputeLayerHints>` has been
/// entered (#1203). Lets the coalescing tests assert "N departures → exactly ONE
/// recompute" by counting real handler invocations rather than a mock. Compiled
/// only under `cfg(test)`, so there is zero production cost.
#[cfg(test)]
pub(crate) static RECOMPUTE_LAYER_HINTS_INVOCATIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

impl Handler<RecomputeLayerHints> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: RecomputeLayerHints, ctx: &mut Self::Context) -> Self::Result {
        #[cfg(test)]
        RECOMPUTE_LAYER_HINTS_INVOCATIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match msg.source {
            Some(source) => self.recompute_layer_hints_for_source(&msg.room, source, ctx),
            None => {
                // Room-wide recompute: snapshot the current publisher sessions so
                // we are not holding an immutable borrow of `room_members` while
                // `recompute_layer_hints_for_source` mutates `layer_hint_state`.
                let sources: Vec<SessionId> = match self.room_members.get(&msg.room) {
                    Some(members) => members.iter().map(|m| m.session).collect(),
                    None => return,
                };
                for source in sources {
                    self.recompute_layer_hints_for_source(&msg.room, source, ctx);
                }
            }
        }
    }
}

/// Trailing-debounce flush for coalesced DEPARTURE-driven recomputes (#1203).
///
/// Drains [`ChatServer::pending_recompute_rooms`] and runs ONE room-wide
/// recompute per distinct room that saw a departure during the coalesce window,
/// then clears the in-flight timer handle so the NEXT departure burst arms a
/// fresh trailing timer. Reusing the existing `RecomputeLayerHints { source:
/// None }` room-wide branch keeps the union computation in one place; the only
/// thing #1203 changes is WHEN/HOW OFTEN that branch runs for departures.
///
/// A room whose membership drained to empty during the window self-clears: the
/// room-wide branch returns early when `room_members` has no entry, so a stale
/// pending room id costs at most one no-op map lookup, never a leaked recompute.
impl Handler<FlushPendingRecomputes> for ChatServer {
    type Result = ();

    fn handle(&mut self, _msg: FlushPendingRecomputes, ctx: &mut Self::Context) -> Self::Result {
        // Clear the in-flight handle FIRST so a recompute-triggered re-arm (none
        // today, but defensive) would schedule a fresh timer rather than no-op
        // against a handle we are about to consume.
        self.recompute_coalesce_handle = None;
        let rooms: Vec<String> = self.pending_recompute_rooms.drain().collect();
        for room in rooms {
            // Reuse the room-wide recompute path (source: None). It is a no-op if
            // the room drained to empty during the window (early return on a
            // missing `room_members` entry).
            self.handle(RecomputeLayerHints { room, source: None }, ctx);
        }
    }
}

impl Handler<SweepLayerPreferenceGauge> for ChatServer {
    type Result = ();

    fn handle(
        &mut self,
        _msg: SweepLayerPreferenceGauge,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        self.sweep_layer_preference_gauge();
    }
}

/// Test-only message driving the REAL #1203 departure-coalescing path through
/// the REAL `schedule_coalesced_recompute` + `notify_later` timer. Returns the
/// `(pending_rooms_len, timer_armed)` state immediately after scheduling so a
/// test can assert dedup (one timer for N calls) WITHOUT reaching into private
/// fields from outside the module.
#[cfg(test)]
#[derive(ActixMessage)]
#[rtype(result = "(usize, bool)")]
struct TestScheduleCoalescedRecompute {
    room: String,
}

#[cfg(test)]
impl Handler<TestScheduleCoalescedRecompute> for ChatServer {
    type Result = MessageResult<TestScheduleCoalescedRecompute>;

    fn handle(
        &mut self,
        msg: TestScheduleCoalescedRecompute,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        self.schedule_coalesced_recompute(&msg.room, ctx);
        MessageResult((
            self.pending_recompute_rooms.len(),
            self.recompute_coalesce_handle.is_some(),
        ))
    }
}

/// Test-only message reporting the current coalescing state
/// (`pending_rooms_len`, `timer_armed`) so a test can assert the flush has
/// drained everything after the debounce window elapsed.
#[cfg(test)]
#[derive(ActixMessage)]
#[rtype(result = "(usize, bool)")]
struct TestCoalesceState;

#[cfg(test)]
impl Handler<TestCoalesceState> for ChatServer {
    type Result = MessageResult<TestCoalesceState>;

    fn handle(&mut self, _msg: TestCoalesceState, _ctx: &mut Self::Context) -> Self::Result {
        MessageResult((
            self.pending_recompute_rooms.len(),
            self.recompute_coalesce_handle.is_some(),
        ))
    }
}

/// Handle per-room policy updates fanned out by `meeting-api` over
/// [`MEETING_SETTINGS_UPDATE_SUBJECT`].
///
/// Refreshes `room_policy[room_id]` so subsequent host disconnects read the
/// post-toggle authoritative `end_on_host_leave` (and the other three flags)
/// without a DB round-trip. We also mirror the new `end_on_host_leave` into
/// every member's per-session `RoomMemberInfo.end_on_host_leave` so legacy
/// code paths that still consult the per-member field (e.g. the `Disconnect`
/// handler's lookup for a session that has already been removed from
/// `connection_states`) see the fresh value too.
///
/// We accept the event even when no room members are currently tracked: a
/// PATCH may arrive in the brief window between meeting creation and the
/// first JoinRoom, and dropping it would re-introduce the very staleness
/// this handler exists to fix once members do join. The cache entry is
/// reaped lazily by [`ChatServer::leave_rooms`] when the room becomes empty.
impl Handler<UpdateRoomPolicy> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: UpdateRoomPolicy, _ctx: &mut Self::Context) -> Self::Result {
        let MeetingSettingsUpdatePayload {
            room_id,
            end_on_host_leave,
            admitted_can_admit,
            waiting_room_enabled,
            allow_guests,
        } = msg.0;

        // Defensive bounds — payload comes off the NATS wire from meeting-api,
        // which is trusted, but we still cap the room_id length to match
        // the EvictInstance handler's posture and avoid accidental memory
        // pressure from a misconfigured publisher.
        if room_id.is_empty() || room_id.len() > 256 {
            warn!(
                "Ignoring {} with invalid room_id length: {}",
                MEETING_SETTINGS_UPDATE_SUBJECT,
                room_id.len()
            );
            return;
        }

        info!(
            "Refreshing room_policy for {} (end_on_host_leave={}, admitted_can_admit={}, \
             waiting_room_enabled={}, allow_guests={})",
            room_id, end_on_host_leave, admitted_can_admit, waiting_room_enabled, allow_guests
        );

        self.room_policy.insert(
            room_id.clone(),
            RoomPolicy {
                end_on_host_leave,
                admitted_can_admit,
                waiting_room_enabled,
                allow_guests,
            },
        );

        // Mirror the freshest `end_on_host_leave` onto every existing room
        // member so callers that still read the per-member field (notably
        // the `Leave` handler's `room_members` lookup at the bottom of the
        // file) see the updated value too. This is belt-and-suspenders:
        // `leave_rooms` itself reads `room_policy` first, but keeping the
        // two views consistent prevents future regressions if someone
        // adds a new code path that consults `RoomMemberInfo` directly.
        if let Some(members) = self.room_members.get_mut(&room_id) {
            for member in members.iter_mut() {
                member.end_on_host_leave = end_on_host_leave;
            }
        }
    }
}

/// Handle cross-server eviction requests received via NATS.
impl Handler<EvictInstance> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: EvictInstance, ctx: &mut Self::Context) -> Self::Result {
        let EvictInstancePayload {
            instance_id,
            room,
            user_id,
            new_session_id,
        } = msg.0;

        // Validate inbound fields — the JoinRoom path sanitizes instance_id to
        // max 64 chars, but NATS messages come from untrusted peers.
        if instance_id.is_empty()
            || instance_id.len() > 64
            || room.len() > 256
            || user_id.len() > 256
        {
            warn!(
                "Ignoring eviction with invalid field lengths (instance_id={}, room={}, user_id={})",
                instance_id.len(),
                room.len(),
                user_id.len()
            );
            return;
        }

        if self.evict_stale_session(&instance_id, &room, &user_id, new_session_id, ctx) {
            info!(
                "Cross-server eviction completed: instance {} (user {} in room {}) — \
                 new session {} is on another server",
                instance_id, user_id, room, new_session_id
            );
        }
    }
}

/// Handler for deferred departure execution.
/// Runs after [`RECONNECT_GRACE_PERIOD`] unless cancelled by a reconnection.
impl Handler<ExecutePendingDeparture> for ChatServer {
    type Result = ();

    fn handle(
        &mut self,
        ExecutePendingDeparture {
            session,
            room,
            user_id,
            instance_key,
            display_name,
            is_host,
            end_on_host_leave,
        }: ExecutePendingDeparture,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        // Use the instance_key captured at Disconnect time, NOT the user_id —
        // see the doc comment on `ChatServer::pending_departures` for why
        // user_id is no longer unique enough to identify a pending entry.
        let key = (room.clone(), instance_key);

        // Only execute if this departure is still pending. It may have been
        // cancelled by a reconnection or replaced by a newer disconnect.
        if let Some(pending) = self.pending_departures.remove(&key) {
            if pending.old_session != session {
                // A newer disconnect replaced this one — do nothing, the newer
                // timer will handle it.
                info!(
                    "Stale pending departure for user {} in room {} (session {} != {}), skipping",
                    user_id, room, session, pending.old_session
                );
                // Re-insert the newer pending state.
                self.pending_departures.insert(key, pending);
                return;
            }

            // Only broadcast PARTICIPANT_LEFT if the session was Active when it
            // disconnected. Testing sessions (e.g., the losing connection during
            // RTT election) never had PARTICIPANT_JOINED broadcast, so emitting
            // PARTICIPANT_LEFT would cause ghost leave events for other participants.
            if !pending.was_active {
                info!(
                    "Grace period expired for user {} (session {}) in room {} — \
                     skipping PARTICIPANT_LEFT (session was never activated)",
                    user_id, session, room
                );
                // Still clean up room_members and instance_index for the old
                // session. Use the shared `forget_room_if_empty` helper so the
                // empty-room policy-cache eviction rule stays in lockstep with
                // the `leave_rooms` path (both paths must drop `room_policy`
                // when, and only when, this removal drained the room to empty).
                //
                // We still check empty→idle here even though this never-active
                // session never broadcast a JOIN: if it was the LAST member it
                // could be draining the room to empty while an earlier active
                // participant already left (that earlier departure saw this
                // testing session still present, so it did NOT emit the
                // empty event). Without this branch the meeting could stay
                // `active` despite being empty. We do NOT emit on the
                // host-leave-ends path here because a never-active session is
                // never the host's ending session (that goes through
                // `leave_rooms`). meeting-api's `set_idle` guards on
                // `state='active'`, so if the meeting was never activated this
                // is a harmless no-op.
                let mut room_became_empty = false;
                if let Some(members) = self.room_members.get_mut(&room) {
                    members.retain(|m| m.session != session);
                    room_became_empty = members.is_empty();
                }
                self.forget_room_if_empty(&room);
                if let Some(iid) = self.session_instance.remove(&session) {
                    if self.instance_index.get(&iid).copied() == Some(session) {
                        self.instance_index.remove(&iid);
                    }
                }
                if room_became_empty {
                    let nc = self.nats_connection.clone();
                    let room_id = room.clone();
                    tokio::spawn(async move {
                        info!(
                            "Room {} became empty after a never-activated session expired - \
                             notifying meeting-api (empty->idle)",
                            room_id
                        );
                        let payload = MeetingBecameEmptyPayload {
                            room_id: room_id.clone(),
                        };
                        match serde_json::to_vec(&payload) {
                            Ok(json) => {
                                if let Err(e) =
                                    nc.publish(MEETING_BECAME_EMPTY_SUBJECT, json.into()).await
                                {
                                    error!(
                                        "Failed to publish {} for room {}: {}",
                                        MEETING_BECAME_EMPTY_SUBJECT, room_id, e
                                    );
                                }
                            }
                            Err(e) => {
                                error!("Failed to serialize MeetingBecameEmptyPayload: {}", e);
                            }
                        }
                    });
                }
                return;
            }

            info!(
                "Grace period expired for user {} (session {}) in room {} — \
                 executing PARTICIPANT_LEFT",
                user_id, session, room
            );
            // Observer sessions bypass the grace period entirely (handled
            // directly in Disconnect), so this path is always non-observer.
            self.leave_rooms(
                LeaveContext {
                    session_id: &session,
                    room: Some(&room),
                    user_id: Some(&user_id),
                    display_name: Some(&display_name),
                    observer: false,
                    is_host,
                    end_on_host_leave,
                },
                ctx,
            );
        } else {
            info!(
                "Pending departure for user {} in room {} already cancelled (reconnected)",
                user_id, room
            );
        }
    }
}

impl Handler<ClientMessage> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: ClientMessage, ctx: &mut Self::Context) -> Self::Result {
        let ClientMessage {
            session,
            room,
            msg,
            user: _,
        } = msg;
        trace!("got message in server room {room} session {session}");

        // Check connection state - only publish to NATS if Active
        let connection_state = self
            .connection_states
            .get(&session)
            .copied()
            .unwrap_or(ConnectionState::Testing);

        if connection_state != ConnectionState::Active {
            trace!(
                "Skipping NATS publish for session {} in Testing state",
                session
            );
            return; // Don't publish during Testing state
        }

        let nc = self.nats_connection.clone();
        let subject = format!("room.{room}.{session}");
        let subject = subject.replace(' ', "_");

        let packet_bytes =
            if let Ok(mut packet_wrapper) = PacketWrapper::parse_from_bytes(&msg.data) {
                if packet_wrapper.session_id == 0 {
                    packet_wrapper.session_id = session;
                }
                match packet_wrapper.write_to_bytes() {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        error!("Failed to serialize PacketWrapper with session_id: {}", e);
                        msg.data.to_vec()
                    }
                }
            } else {
                msg.data.to_vec()
            };

        let b = bytes::Bytes::from(packet_bytes);
        let fut = async move {
            let start = std::time::Instant::now();
            match nc.publish(subject.clone(), b).await {
                Ok(_) => {
                    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                    RELAY_NATS_PUBLISH_LATENCY_MS.observe(elapsed_ms);
                    trace!("published message to {subject}");
                }
                Err(e) => error!("error publishing message to {subject}: {e}"),
            }
        };
        let fut = actix::fut::wrap_future::<_, Self>(fut);
        ctx.spawn(fut);
    }
}

impl Handler<JoinRoom> for ChatServer {
    type Result = MessageResult<JoinRoom>;

    fn handle(
        &mut self,
        JoinRoom {
            session,
            room,
            user_id,
            display_name,
            is_guest,
            observer,
            instance_id,
            is_host,
            end_on_host_leave,
            transport,
        }: JoinRoom,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        // Validate user_id synchronously BEFORE spawning async task.
        // This ensures we return an error to the client if validation fails,
        // rather than returning Ok and silently failing in the spawned task.
        if user_id == SYSTEM_USER_ID {
            return MessageResult(Err("Cannot use reserved system user ID".into()));
        }

        // Persist the server-authoritative guest flag so downstream
        // handlers (ActivateConnection, Disconnect, leave_rooms, and the
        // "existing members" broadcast in this handler) can retrieve it
        // by session_id without widening the `room_members` tuple.
        self.session_is_guest.insert(session, is_guest);

        if self.active_subs.contains_key(&session) {
            return MessageResult(Ok(()));
        }

        // Sanitize instance_id: reject oversized values to prevent memory abuse.
        let instance_id = instance_id.filter(|iid| !iid.is_empty() && iid.len() <= 64);

        // Record session→iid now; the forward index and eviction are deferred to
        // ActivateConnection. During RTT election both WS and WT candidates share
        // one instance_id — evicting at JoinRoom would kill the earlier candidate's
        // NATS task before we know which one wins, breaking PARTICIPANT_LIST_REQUEST
        // responses.
        if let Some(ref iid) = instance_id {
            self.session_instance.insert(session, iid.clone());
        }

        // --- Multi-session-per-user is allowed (issue #828) ---
        // Policy: the same `user_id` may have multiple concurrent sessions in
        // a room. Different browser tabs, devices, or instances of the same
        // authenticated user appear as distinct participants — each gets its
        // own tile, audio stream, and PARTICIPANT_JOINED/LEFT broadcast.
        //
        // The instance_id-based eviction above (around the call to
        // `evict_stale_session`) still de-duplicates **same-tab refresh /
        // back-button** cases: the `instance_id` is stored in per-tab
        // sessionStorage, so a refresh keeps it and the prior session is
        // evicted silently. A new tab generates a fresh `instance_id` via
        // `generate_instance_id` in the client, and therefore lands here
        // with no instance-id match — under the previous policy
        // (`evict_same_user_session`, now removed) we would have collapsed
        // those into a single session, which is the bug fixed by #828.
        //
        // Downstream invariants that previously assumed `(room, user_id)`
        // uniqueness:
        //   - `pending_departures` is now keyed by `(room, instance_key)`
        //     where `instance_key` is the per-tab `instance_id` (or a
        //     per-session sentinel when none is supplied). This rekeying
        //     (issue #852, absorbed into #851) prevents distinct sibling
        //     sessions of the same `user_id` from colliding on the key.
        //   - `PARTICIPANT_JOINED` / `PARTICIPANT_LEFT` packets carry
        //     `session_id` (see `SessionManager::build_peer_joined_packet`),
        //     so the client can already key on session and render distinct
        //     tiles for the same user_id.

        // --- Reconnection grace period: cancel pending departure ---
        // If the SAME TAB (same `instance_id`) is reconnecting to the same
        // room within the grace window, suppress both PARTICIPANT_LEFT
        // (already deferred) and the PARTICIPANT_JOINED that would normally
        // follow. A fresh second session of the same user (different
        // `instance_id`) will NOT match this lookup — it is a real new
        // participant and must be announced.
        //
        // Joins with no `instance_id` cannot be classified as a reconnection
        // because they have no stable identity that could match a prior
        // session's pending entry — they always fall through as fresh joins.
        let is_reconnection = if let Some(ref iid) = instance_id {
            let departure_key = (room.clone(), iid.clone());
            if let Some(pending) = self.pending_departures.remove(&departure_key) {
                ctx.cancel_future(pending.spawn_handle);

                // Clean up stale room_members entry from the old session
                if let Some(members) = self.room_members.get_mut(&room) {
                    members.retain(|m| m.session != pending.old_session);
                }

                info!(
                    "Reconnection detected for instance {} (user {}) in room {} — cancelled \
                     pending PARTICIPANT_LEFT (old session {}, new session {})",
                    iid, user_id, room, pending.old_session, session
                );
                true
            } else {
                false
            }
        } else {
            false
        };

        // Mark reconnection and observer sessions so ActivateConnection does not
        // broadcast PARTICIPANT_JOINED for them. Reconnection sessions never
        // "left" from peers' perspective; observers are never announced.
        // Instance_id-based eviction suppression is NOT set here — eviction is
        // deferred to ActivateConnection, which adds the elected session to
        // `suppress_join_broadcast` itself if it evicted a predecessor.
        if is_reconnection || observer {
            self.suppress_join_broadcast.insert(session);
        }

        let room_clone = room.clone();
        let user_id_clone = user_id.clone();
        let display_name_clone = display_name.clone();
        let session_id = session;
        let nc = self.nats_connection.clone();

        let session_str = session.to_string();
        let (subject, queue) = build_subject_and_queue(&room, &session_str);
        let session_recipient = match self.sessions.get(&session) {
            Some(addr) => addr.clone(),
            None => {
                return MessageResult(Err("Session not found".into()));
            }
        };

        // Allocate this session's viewport / "desired streams" set (HCL issue
        // #988). It starts empty = fail-open (forward all video) and is shared
        // with the NATS subscription task below: the task's VIEWPORT
        // interceptor writes it and `handle_msg` reads it on the forwarding
        // path. A reconnection runs `JoinRoom` under a fresh `session_id`, so a
        // new empty set is allocated and the client re-sends its viewport — no
        // stale viewport state survives a reconnect.
        let desired_streams: DesiredStreams = Default::default();
        self.session_desired_streams
            .insert(session, desired_streams.clone());

        // Allocate the per-session layer-preference map (#989, Phase 1b),
        // shared with the NATS subscription task below exactly like
        // `desired_streams`: the task's LAYER_PREFERENCE interceptor writes it
        // and `handle_msg` reads it on the forwarding path. A reconnection runs
        // `JoinRoom` under a fresh `session_id`, so a new empty map is allocated
        // and the client re-sends its preferences — no stale state survives a
        // reconnect. Empty map = no-op (forward all layers).
        let layer_prefs: LayerPrefs = Default::default();
        self.session_layer_prefs
            .insert(session, layer_prefs.clone());

        // Collect existing non-observer room members for notifying the new joiner.
        // On reconnection, we still send the existing member list so the
        // reconnecting client knows who is in the room.
        //
        // Snapshot `is_guest` per existing session here (inside the handler,
        // where `self` is in scope) so the spawned task can build accurate
        // PARTICIPANT_JOINED packets without needing to re-enter the actor.
        // The tuple preserves the full RoomMemberInfo (needed for host-leave
        // tracking) alongside the server-authoritative guest flag.
        let existing_members: Vec<(RoomMemberInfo, bool)> = if !observer {
            self.room_members
                .get(&room)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|m| {
                    let is_guest = self
                        .session_is_guest
                        .get(&m.session)
                        .copied()
                        .unwrap_or(false);
                    (m, is_guest)
                })
                .collect()
        } else {
            Vec::new()
        };

        // True when the room had no non-observer participants before this join.
        // Used to gate the NATS MEETING_STARTED broadcast (the transport actors
        // already send MEETING_STARTED directly to every connecting client).
        let is_first_in_room = existing_members.is_empty() && !observer;

        // Track this session in room_members (only for non-observers)
        if !observer {
            self.session_room.insert(session, room.clone());
            self.room_members
                .entry(room.clone())
                .or_default()
                .push(RoomMemberInfo {
                    session,
                    user_id: user_id.clone(),
                    display_name: display_name.clone(),
                    is_host,
                    end_on_host_leave,
                });

            // Seed the room_policy cache with the JWT-time `end_on_host_leave`
            // if we have nothing fresher. A subsequent
            // `MEETING_SETTINGS_UPDATE_SUBJECT` event from meeting-api
            // overwrites this; until then the JWT value is the best
            // approximation we have. The other three flags default to the
            // `meetings` table defaults (waiting_room_enabled=true,
            // admitted_can_admit=false, allow_guests=false) — these are not
            // currently consulted by chat_server, but seeding them keeps the
            // shape of the cache consistent so future readers don't see
            // partially-populated entries.
            //
            // We use `entry().or_insert_with(...)` so a previously-pushed
            // policy update is NOT clobbered by a later joiner whose JWT
            // still carries the pre-update flag value. Without this, a
            // toggle that landed before the second participant joined
            // would silently regress.
            self.room_policy
                .entry(room.clone())
                .or_insert_with(|| RoomPolicy {
                    end_on_host_leave,
                    admitted_can_admit: false,
                    waiting_room_enabled: true,
                    allow_guests: false,
                });

            // Publish-side suppression restore (#1108, Stage 3). A newly-joined
            // receiver has NO recorded layer preference yet, so under the
            // fail-open contract it wants the FULL ladder from EVERY existing
            // publisher. That can RAISE one or more sources' per-source unions
            // (e.g. a publisher previously suppressed to base because its only
            // receiver wanted base must now restore full for the new viewer).
            // Recompute room-wide so each publisher gets a restore-eager hint.
            // Routed through the actor (`do_send`) rather than computed inline to
            // avoid borrowing `room_members` while the recompute mutates
            // `layer_hint_state`; the new joiner is already in `room_members`
            // above, so the recompute sees its (fail-open) demand. The joiner's
            // own session has no debounce state yet, and it is a publisher of
            // nothing until it sends media, so a `None` (room-wide) recompute is
            // correct and idempotent.
            ctx.address().do_send(RecomputeLayerHints {
                room: room.clone(),
                source: None,
            });
        }

        // Clone the recipient so we can send existing member info directly to the new joiner
        let new_joiner_recipient = session_recipient.clone();

        let nc2 = self.nats_connection.clone();
        let session_clone = session;
        let server_addr = ctx.address();
        // Recipient for publish-side layer-hint recomputes (#1108, Stage 3). The
        // LAYER_PREFERENCE interceptor in this session's NATS loop messages the
        // ACTOR (it cannot compute the cross-receiver union itself); a typed
        // `Recipient<RecomputeLayerHints>` keeps the interceptor decoupled from
        // the full `ChatServer` address and trivially mockable in unit tests.
        let recompute_recipient = server_addr.clone().recipient::<RecomputeLayerHints>();
        // Shared viewport set for this session's NATS loop (HCL issue #988).
        let desired_streams_for_loop = desired_streams.clone();
        // Shared layer-preference map for this session's NATS loop (#989).
        let layer_prefs_for_loop = layer_prefs.clone();
        // Receiver transport for the per-session NATS loop's `handle_msg`, so an
        // inbound actor-mailbox overflow can be attributed to the right
        // transport on `relay_inbound_mailbox_drops_total` (Tier B #2 / #1057).
        let transport_for_loop = transport.clone();

        let handle = tokio::spawn(async move {
            // start_session is called by the transport actors (ws_chat_session /
            // wt_chat_session) in their started() method, which blocks with
            // ctx.wait() before this JoinRoom handler runs. We do NOT call it
            // again here to avoid double-counting if SessionManager ever
            // acquires stateful tracking (room capacity, DB records, etc.).
            //
            // The reserved-user-ID check is performed synchronously at the top
            // of this handler, so we can proceed directly to NATS setup.

            let start_time_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);

            info!(
                "JoinRoom task running for user {} in room {} (session {})",
                user_id_clone, room_clone, session_id,
            );

            // SESSION_ASSIGNED is sent by ws_chat_session / wt_chat_session
            // in their started() method before this JoinRoom handler runs.

            // Only broadcast MEETING_STARTED via NATS for the first
            // participant. The transport actors already send it directly
            // to every connecting client, so subsequent joins would just
            // produce redundant events for existing participants.
            if is_first_in_room {
                send_meeting_info(&nc, &room_clone, start_time_ms, &user_id_clone).await;
            }

            // PARTICIPANT_JOINED broadcast is deferred until
            // ActivateConnection is received. This prevents ghost join
            // events from Testing connections during RTT election — only
            // the elected (activated) connection announces itself.
            //
            // Reconnection joins also skip the broadcast (the user never
            // "left" from peers' perspective), and observer joins are
            // never broadcast either.
            if is_reconnection {
                info!(
                    "Suppressing PARTICIPANT_JOINED for reconnecting user {} in room {} \
                     (deferred broadcast also skipped)",
                    user_id_clone, room_clone
                );
            } else if observer {
                info!(
                    "Skipping PARTICIPANT_JOINED for observer {} in room {}",
                    user_id_clone, room_clone
                );
            } else {
                info!(
                    "Deferring PARTICIPANT_JOINED for {} (display={}) in room {} \
                     until ActivateConnection (session {})",
                    user_id_clone, display_name_clone, room_clone, session_id
                );
            }

            // Send PARTICIPANT_JOINED for each existing member directly to the new joiner.
            // This ensures the new joiner learns about all participants already in the room.
            for (member, is_guest) in &existing_members {
                let existing_bytes = SessionManager::build_peer_joined_packet(
                    &room_clone,
                    &member.user_id,
                    member.session,
                    &member.display_name,
                    *is_guest,
                );
                info!(
                    "Sending existing PARTICIPANT_JOINED for {} (display={}) to new joiner {}",
                    member.user_id, member.display_name, user_id_clone
                );
                // Priority attribution (#1145) deliberately NOT applied: this
                // forwards a single PARTICIPANT_JOINED (`PacketType::MEETING`,
                // Critical) during join setup — never a sheddable-media drop.
                // (It also predates the mailbox-drop counters and intentionally
                // stays a plain warn on this one-shot setup path.)
                if let Err(e) = new_joiner_recipient.try_send(Message {
                    // `build_peer_joined_packet` returns an owned `Vec<u8>`;
                    // wrap it in `Bytes` (a one-time move into the refcounted
                    // buffer, no copy) to match `Message.msg`'s type (#1063).
                    msg: bytes::Bytes::from(existing_bytes),
                    session: member.session,
                }) {
                    warn!(
                        "Failed to send existing PARTICIPANT_JOINED for {} to new joiner {}: {}",
                        member.user_id, user_id_clone, e
                    );
                }
            }

            match nc2.queue_subscribe(subject, queue).await {
                Ok(mut sub) => {
                    // Build the forwarding closure once; it is cheap to call
                    // per packet and avoids re-cloning the config on every
                    // message.
                    let forward = handle_msg(
                        session_recipient.clone(),
                        room_clone.clone(),
                        session_clone,
                        observer,
                        user_id_clone.clone(),
                        desired_streams_for_loop.clone(),
                        layer_prefs_for_loop.clone(),
                        transport_for_loop.clone(),
                    );
                    let self_subject =
                        format!("room.{room_clone}.{session_clone}").replace(' ', "_");

                    // Publish PARTICIPANT_LIST_REQUEST to the room system subject
                    // so peers on other servers re-broadcast their PARTICIPANT_JOINED.
                    // Those peers receive this request in their NATS loops and call
                    // RebroadcastPresence on their local ChatServer, which replies
                    // on this joiner's per-session subject. This joiner is now
                    // subscribed and will receive the reply.
                    if let Some((subject_req, request_bytes)) =
                        SessionManager::participant_list_request_publication(
                            observer,
                            &room_clone,
                            session_clone,
                        )
                    {
                        if let Err(e) = nc2.publish(subject_req, request_bytes.into()).await {
                            warn!(
                                "Failed to publish PARTICIPANT_LIST_REQUEST for {} in {}: {}",
                                user_id_clone, room_clone, e
                            );
                        }
                    }
                    while let Some(msg) = sub.next().await {
                        // Parse the PacketWrapper EXACTLY ONCE per packet and
                        // share the result with every consumer below. This is
                        // the relay's hottest path (every media frame × every
                        // receiver × every room), so the previous
                        // parse-in-each-interceptor approach doubled the
                        // protobuf decode + allocation cost. Unparseable
                        // payloads yield `None`, which every consumer treats
                        // as its fail-closed/fall-through default.
                        let parsed = PacketWrapper::parse_from_bytes(&msg.payload).ok();

                        if try_intercept_display_name_change(
                            &msg,
                            parsed.as_ref(),
                            &room_clone,
                            session_clone,
                            &session_recipient,
                            &server_addr,
                            &transport_for_loop,
                        ) {
                            continue;
                        }

                        // VIEWPORT control packets (HCL issue #988) are
                        // consumed by the relay and never re-broadcast. A
                        // VIEWPORT is recorded ONLY when it arrived on THIS
                        // session's own publish subject (the trustworthy
                        // identity bound to the authenticated connection at
                        // JoinRoom); any other VIEWPORT is dropped without
                        // mutating state. Either way we `continue` so it never
                        // reaches `handle_msg`.
                        if try_intercept_viewport(
                            &msg,
                            parsed.as_ref(),
                            &self_subject,
                            session_clone,
                            &desired_streams_for_loop,
                            &room_clone,
                        ) {
                            continue;
                        }

                        // LAYER_PREFERENCE control packets (#989, Phase 1b) are
                        // consumed by the relay and never re-broadcast, exactly
                        // like VIEWPORT above. A preference is recorded ONLY
                        // when it arrived on THIS session's own publish subject
                        // (subject-authoritative ownership); any other one is
                        // dropped without mutating state. Either way we
                        // `continue` so it never reaches `handle_msg`.
                        if try_intercept_layer_preference(
                            &msg,
                            parsed.as_ref(),
                            &self_subject,
                            &layer_prefs_for_loop,
                            &room_clone,
                            &|m| recompute_recipient.do_send(m),
                            session_clone,
                        ) {
                            continue;
                        }

                        // PARTICIPANT_LIST_REQUEST: a joiner asking existing
                        // peers to re-announce themselves.
                        // Consumed by the relay; never forwarded to clients.
                        if try_intercept_participant_list_request(
                            &msg,
                            parsed.as_ref(),
                            session_clone,
                            &server_addr,
                        ) {
                            continue;
                        }

                        if let Err(e) = forward(msg, parsed.as_ref()) {
                            error!("Error handling message: {}", e);
                            break;
                        }
                    }
                }
                Err(e) => {
                    error!("{}", e)
                }
            }
        });

        self.active_subs.insert(session, handle);

        MessageResult(Ok(()))
    }
}

async fn send_meeting_info(
    nc: &async_nats::client::Client,
    room: &str,
    start_time_ms: u64,
    creator_id: &str,
) {
    let packet_bytes =
        SessionManager::build_meeting_started_packet(room, start_time_ms, creator_id);

    let subject = format!("room.{}.system", room.replace(' ', "_"));
    match nc.publish(subject.clone(), packet_bytes.into()).await {
        Ok(_) => info!("Sent meeting start time {} to {}", start_time_ms, subject),
        Err(e) => error!("Failed to send meeting info to room {}: {}", room, e),
    }
}

/// Checks whether `msg` is a VIEWPORT control packet (HCL issue #988) and, if
/// so, intercepts it so it is NEVER re-broadcast to other peers.
///
/// `parsed` is the already-decoded `PacketWrapper` for `msg` (parsed once per
/// packet in the NATS loop and shared with every consumer); `None` means the
/// payload was unparseable.
///
/// Returns `true` when the packet was a VIEWPORT (caller must `continue` the
/// NATS loop) and `false` when the caller should fall through to `handle_msg`.
///
/// # Ownership / security (HCL #988 hardening)
///
/// The room's NATS fan-out delivers every published packet — including each
/// session's own VIEWPORT — to every session loop. A receiver may only mutate
/// its OWN viewport. Ownership is decided by the NATS SUBJECT the packet
/// arrived on, NOT by any payload field:
///
/// * A VIEWPORT is "mine" iff it arrived on this session's own publish subject
///   `self_subject` == `room.{room}.{receiver_session}`. The subject is set by
///   the relay from the authenticated connection (`Handler<ClientMessage>`
///   publishes to `room.{room}.{session}`), so it cannot be forged by a peer.
/// * Any VIEWPORT arriving on a DIFFERENT subject is dropped WITHOUT mutating
///   state. This closes the cross-session tampering vector where an attacker
///   crafts `PacketWrapper{ session_id: victim, .. }`: the payload
///   `session_id` is attacker-controllable (it is only stamped when the client
///   sends 0) and is therefore NEVER consulted for ownership here.
///
/// The receiver's own session_id is filtered out of the recorded set (a
/// session does not render itself), so a lone `[self]` viewport collapses to
/// "empty = fail-open" rather than dropping all remote video.
///
/// Accepted updates are bounded ([`VIEWPORT_MAX_SESSION_IDS`]) and rate-limited
/// ([`VIEWPORT_MIN_UPDATE_INTERVAL`]) to blunt DoS via oversized / spammed
/// lists.
///
/// The viewport set is **subtract-only** and consumed purely for VIDEO drop
/// decisions in `handle_msg`. It never widens authorization and is not logged
/// at info level (it is "who is watching whom" metadata).
fn try_intercept_viewport(
    msg: &async_nats::Message,
    parsed: Option<&PacketWrapper>,
    self_subject: &str,
    receiver_session: SessionId,
    desired_streams: &DesiredStreams,
    room: &str,
) -> bool {
    // Unparseable payloads are not our concern; let `handle_msg` apply its own
    // fail-closed handling.
    let wrapper = match parsed {
        Some(w) => w,
        None => return false,
    };

    if wrapper.packet_type != PacketType::VIEWPORT.into() {
        return false;
    }

    // From here on the packet IS a VIEWPORT and must never be forwarded:
    // every return path below yields `true`.

    // Ownership is established by the SUBJECT, not the payload. A peer can
    // forge `wrapper.session_id`, but it cannot publish onto another session's
    // subject — the relay derives the subject from the authenticated
    // connection. If this VIEWPORT did not arrive on our own subject it is not
    // ours: drop it without touching our set. This is expected for normal NATS
    // fan-out: every other receiver sees the owner's VIEWPORT and ignores it.
    if msg.subject.as_str() != self_subject {
        // Normal fan-out / other-subject VIEWPORT — dropped without mutating
        // state. A forged payload is handled by the same subject-authoritative
        // path, but this label intentionally does not imply an attack.
        RELAY_VIEWPORT_UPDATES_TOTAL
            .with_label_values(&[room, "ignored_other_subject"])
            .inc();
        return true;
    }

    if let Ok(viewport) =
        videocall_types::protos::viewport_packet::ViewportPacket::parse_from_bytes(&wrapper.data)
    {
        // DoS bound: cap the number of session_ids we will process. Truncate
        // rather than reject so an over-long list still applies its first N
        // entries (fail-open on the excess).
        let raw_len = viewport.session_ids.len();
        let next: std::collections::HashSet<u64> = viewport
            .session_ids
            .into_iter()
            .take(VIEWPORT_MAX_SESSION_IDS)
            // A session never renders itself; dropping it keeps a lone
            // `[self]` viewport from collapsing into "drop everything".
            .filter(|sid| *sid != receiver_session)
            .collect();
        // The cap fired silently before #988 (G3): record truncation so the
        // DoS guard is observable. Counted in addition to `accepted` below
        // when the (capped) update is also applied.
        if raw_len > VIEWPORT_MAX_SESSION_IDS {
            RELAY_VIEWPORT_UPDATES_TOTAL
                .with_label_values(&[room, "truncated"])
                .inc();
        }

        // Overwrite (not merge): the latest VIEWPORT is the full current
        // visible set. `write()` only fails on a poisoned lock, which would
        // mean another holder panicked; in that case we leave the previous
        // set untouched (fail-open relative to the new, smaller set).
        match desired_streams.write() {
            Ok(mut guard) => {
                // Rate-limit: ignore updates that arrive sooner than
                // VIEWPORT_MIN_UPDATE_INTERVAL after the last accepted one.
                let now = std::time::Instant::now();
                let too_soon = guard
                    .last_update
                    .is_some_and(|last| now.duration_since(last) < VIEWPORT_MIN_UPDATE_INTERVAL);
                if too_soon {
                    // Rate limit fired silently before #988 (G3).
                    RELAY_VIEWPORT_UPDATES_TOTAL
                        .with_label_values(&[room, "rate_limited"])
                        .inc();
                } else {
                    guard.ids = next;
                    guard.last_update = Some(now);
                    let set_size = guard.ids.len();
                    // Drop the lock before touching metrics.
                    drop(guard);
                    RELAY_VIEWPORT_UPDATES_TOTAL
                        .with_label_values(&[room, "accepted"])
                        .inc();
                    // Per-room set-size gauge (G2): a collapse toward 0/1 while
                    // peers still publish is the wrongly-dropping signature.
                    // Last-writer-wins across receivers in the room (no
                    // per-session label — cardinality). Cleaned up when the
                    // room drains (forget_room_if_empty / forget_session).
                    RELAY_VIEWPORT_SET_SIZE
                        .with_label_values(&[room])
                        .set(set_size as f64);
                }
            }
            Err(_) => {
                warn!(
                    "Viewport set lock poisoned for session {}; keeping previous set",
                    receiver_session
                );
            }
        }
    }
    // Malformed inner payload: still consume the packet (drop it) but leave
    // the existing set unchanged — fail-open to the prior behaviour.

    true
}

/// Checks whether `msg` is a LAYER_PREFERENCE control packet (#989, Phase 1b)
/// and, if so, intercepts it so it is NEVER re-broadcast to other peers.
///
/// `parsed` is the already-decoded `PacketWrapper` for `msg` (parsed once per
/// packet in the NATS loop and shared with every consumer); `None` means the
/// payload was unparseable.
///
/// Returns `true` when the packet was a LAYER_PREFERENCE (caller must
/// `continue` the NATS loop) and `false` when the caller should fall through to
/// `handle_msg`.
///
/// # Ownership / security (mirrors `try_intercept_viewport`)
///
/// This is the enforcement point for the field-5 trust boundary (Tony's #993
/// note). The cleartext `simulcast_layer_id` and this control packet both live
/// OUTSIDE the AEAD seal, so a peer could forge either. Ownership of a layer
/// preference is therefore decided by the NATS SUBJECT the packet arrived on,
/// NOT by any payload field:
///
/// * A LAYER_PREFERENCE is "mine" iff it arrived on this session's own publish
///   subject `self_subject` == `room.{room}.{receiver_session}`. The subject is
///   set by the relay from the authenticated connection and cannot be forged by
///   a peer.
/// * Any LAYER_PREFERENCE arriving on a DIFFERENT subject is dropped WITHOUT
///   mutating state. This is the guarantee that a forged value only
///   self-degrades the forger's OWN view (it changes only the forger's own
///   layer map); it can NEVER affect another receiver's preferences.
///
/// Accepted updates are bounded ([`LAYER_PREFERENCE_MAX_ENTRIES`]) and
/// rate-limited ([`LAYER_PREFERENCE_MIN_UPDATE_INTERVAL`]) to blunt DoS via
/// oversized / spammed maps. The map is **subtract-only** and consumed purely
/// for the layer-drop decision in `handle_msg`; it never widens authorization.
fn try_intercept_layer_preference(
    msg: &async_nats::Message,
    parsed: Option<&PacketWrapper>,
    self_subject: &str,
    layer_prefs: &LayerPrefs,
    room: &str,
    // Sink for per-source recompute requests (#1108, Stage 3). The interceptor
    // runs in a per-session NATS task and CANNOT compute the cross-receiver
    // union itself, so it hands each affected source to this sink, which in
    // production forwards to the actor via `do_send`. Taking a closure (rather
    // than the actor `Addr`/`Recipient` directly) keeps the interceptor a pure,
    // synchronously-unit-testable function: tests pass a recording/no-op closure
    // and never need a running actix system.
    on_recompute: &dyn Fn(RecomputeLayerHints),
    receiver_session: SessionId,
) -> bool {
    // Unparseable payloads are not our concern; let `handle_msg` apply its own
    // fail-closed handling.
    let wrapper = match parsed {
        Some(w) => w,
        None => return false,
    };

    if wrapper.packet_type != PacketType::LAYER_PREFERENCE.into() {
        return false;
    }

    // From here on the packet IS a LAYER_PREFERENCE and must never be
    // forwarded: every return path below yields `true`.

    // Ownership is established by the SUBJECT, not the payload — see the doc
    // comment. A LAYER_PREFERENCE arriving on any subject other than our own is
    // normal NATS fan-out (every receiver sees the owner's packet) and is
    // dropped without mutating our map.
    if msg.subject.as_str() != self_subject {
        RELAY_LAYER_PREFERENCE_UPDATES_TOTAL
            .with_label_values(&[room, "ignored_other_subject"])
            .inc();
        return true;
    }

    if let Ok(prefs) =
        videocall_types::protos::layer_preference_packet::LayerPreferencePacket::parse_from_bytes(
            &wrapper.data,
        )
    {
        // DoS bound: cap the number of entries we will process. Truncate rather
        // than reject so an over-long list still applies its first N entries
        // (fail-open on the excess), matching the viewport interceptor.
        let raw_len = prefs.entries.len();
        let mut bounded_any = false;
        // Key by (session_id, normalized media_kind) (issue #989, Phase 3). The
        // Entry's media_kind enum shares numbering with the wire MediaKind;
        // `normalize_pref_media_kind` maps UNSPECIFIED(0)→VIDEO(1) for
        // back-compat with pre-Phase-3 clients.
        //
        // DEFENSE-IN-DEPTH (#1082): clamp the VALUE RANGE of `desired_layer`.
        // The relay is layer-count-agnostic (it never learns how many layers a
        // source produces — see "AVAILABILITY NOT VALIDATED" on the forwarding
        // path), so without this a forged/garbage entry could stuff an arbitrary
        // `u32` into the per-source map. Such an id never matches a real packet,
        // so the source's non-base layers all drop and the forger self-degrades
        // to base — but the relay should not retain nonsense state. Entries
        // exceeding `LAYER_PREFERENCE_MAX_LAYER_ID` are SKIPPED (`filter_map`),
        // not clamped: skipping is fail-open per source (no recorded preference
        // for that (source, kind) → the existing forwarding path forwards every
        // layer = base-and-up), whereas clamping would invent a selection the
        // receiver never asked for. This is O(1) per entry and adds no
        // allocation. NOTE: this is NOT the real layer count — it is purely a
        // forged-id bound (see the const doc).
        let next: HashMap<(u64, i32), u32> = prefs
            .entries
            .into_iter()
            .take(LAYER_PREFERENCE_MAX_ENTRIES)
            .filter_map(|e| {
                if e.desired_layer > LAYER_PREFERENCE_MAX_LAYER_ID {
                    bounded_any = true;
                    return None;
                }
                Some((
                    (
                        e.session_id,
                        normalize_pref_media_kind(e.media_kind.value()),
                    ),
                    e.desired_layer,
                ))
            })
            .collect();
        // These two SHAPING-step outcomes are emitted BEFORE the rate-limit /
        // write-lock decision below, so they record malformed-shape *attempts*
        // and are NOT conditioned on the packet being applied (#1069). A packet
        // can therefore record `truncated`/`layer_id_out_of_bound` and then be
        // `rate_limited` rather than `accepted` — the metric doc on
        // `RELAY_LAYER_PREFERENCE_UPDATES_TOTAL` spells out this co-occurrence.
        // Left here (rather than moved into the accepted branch) deliberately:
        // relocating them onto the post-lock path would change behaviour on a
        // DoS-sensitive hot path for no observability gain, and "attempt rate"
        // is the more useful guard signal.
        if raw_len > LAYER_PREFERENCE_MAX_ENTRIES {
            RELAY_LAYER_PREFERENCE_UPDATES_TOTAL
                .with_label_values(&[room, "truncated"])
                .inc();
        }
        if bounded_any {
            RELAY_LAYER_PREFERENCE_UPDATES_TOTAL
                .with_label_values(&[room, "layer_id_out_of_bound"])
                .inc();
        }

        // Overwrite (not merge): the latest LAYER_PREFERENCE is the full
        // current per-source layer map. `write()` only fails on a poisoned
        // lock; in that case we leave the previous map untouched (fail-open
        // relative to the new map).
        match layer_prefs.state.write() {
            Ok(mut guard) => {
                let now = std::time::Instant::now();
                let too_soon = guard.last_update.is_some_and(|last| {
                    now.duration_since(last) < LAYER_PREFERENCE_MIN_UPDATE_INTERVAL
                });
                if too_soon {
                    RELAY_LAYER_PREFERENCE_UPDATES_TOTAL
                        .with_label_values(&[room, "rate_limited"])
                        .inc();
                } else {
                    let now_non_empty = !next.is_empty();
                    // Determine which SOURCE sessions had their desired layer
                    // changed by this update, BEFORE the overwrite moves `next`
                    // into the guard (#1108, Stage 3). A source "changed" if, for
                    // ANY media kind, the recorded value differs between the old
                    // and new maps — including a source whose entry was ADDED or
                    // REMOVED (removal flips it back to the fail-open full ladder,
                    // which can raise the per-source union, so a restore hint may
                    // be owed). We only message the actor for sources that
                    // actually changed, keeping the per-source recompute O(changed)
                    // rather than O(room) on every preference packet.
                    let changed_sources = changed_pref_sources(&guard.layers, &next);
                    guard.layers = next;
                    guard.last_update = Some(now);
                    drop(guard);
                    // Update the lock-free hot-path hint while still on the
                    // writer side. Stored with Relaxed ordering: the only
                    // consumer (`has_any` on the forwarding path) treats it as
                    // a hint and re-checks under the lock, so no
                    // happens-before relationship with the map contents is
                    // required for correctness — a stale `true` costs at most
                    // one lock that fails open, and a stale `false` cannot
                    // occur because we only ever raise the hint here (an empty
                    // overwrite lowers it, which can only cause an extra
                    // forward = fail-open).
                    layer_prefs
                        .non_empty
                        .store(now_non_empty, std::sync::atomic::Ordering::Relaxed);
                    RELAY_LAYER_PREFERENCE_UPDATES_TOTAL
                        .with_label_values(&[room, "accepted"])
                        .inc();

                    // Publish-side suppression trigger (#1108, Stage 3): this
                    // receiver's demand for one or more sources changed, so ask
                    // the ACTOR to recompute each affected source's per-source
                    // union and hint its publisher. This MUST go through the
                    // actor — the union is an inverted query over ALL receivers'
                    // prefs, and this interceptor runs in a per-session NATS task
                    // that can see only this one receiver's map. `do_send`
                    // mirrors the display-name-change trigger: low-priority,
                    // never blocks the NATS loop; if the actor mailbox is full the
                    // recompute is simply skipped (the next preference change, or
                    // a join/leave, will re-trigger it).
                    for src in changed_sources {
                        on_recompute(RecomputeLayerHints {
                            room: room.to_string(),
                            source: Some(src),
                        });
                    }
                    debug!(
                        "Recorded LAYER_PREFERENCE for receiver session {} in room {}; triggered per-source layer-hint recompute",
                        receiver_session, room
                    );
                }
            }
            Err(_) => {
                warn!("Layer-preference lock poisoned for self_subject {self_subject}; keeping previous map");
            }
        }
    }
    // Malformed inner payload: still consume the packet (drop it) but leave the
    // existing map unchanged — fail-open to the prior behaviour.

    true
}

/// Checks whether `msg` is a `PARTICIPANT_LIST_REQUEST` system event.
/// If so, asks the local ChatServer (via `RebroadcastPresence`) to re-publish
/// this session's own PARTICIPANT_JOINED addressed to the requesting joiner, so
/// a cross-server joiner — whose NATS subscription was established after the
/// original deferred PARTICIPANT_JOINED was published — learns about this peer.
///
/// `parsed` is the `PacketWrapper` decoded once per packet in the NATS loop.
/// Returns `true` when intercepted (caller must `continue`); `false` otherwise.
fn try_intercept_participant_list_request(
    msg: &async_nats::Message,
    parsed: Option<&PacketWrapper>,
    own_session: SessionId,
    server: &Addr<ChatServer>,
) -> bool {
    // The request is broadcast on the room system subject only.
    if !msg.subject.ends_with(".system") {
        return false;
    }

    let wrapper = match parsed {
        Some(w) => w,
        None => return false,
    };

    if wrapper.packet_type != PacketType::MEETING.into() {
        return false;
    }

    if wrapper.user_id != SYSTEM_USER_ID.as_bytes() {
        return false;
    }

    let inner = match MeetingPacket::parse_from_bytes(&wrapper.data) {
        Ok(p) => p,
        Err(_) => return false,
    };

    if inner.event_type != MeetingEventType::PARTICIPANT_LIST_REQUEST.into() {
        return false;
    }

    // Ignore our own request (we published it; no need to answer ourselves).
    if inner.session_id == own_session {
        return true;
    }

    debug!(
        "PARTICIPANT_LIST_REQUEST received (requester session={}), re-broadcasting own presence \
         for session {}",
        inner.session_id, own_session
    );
    server.do_send(RebroadcastPresence {
        session: own_session,
        requester_session: inner.session_id,
    });
    true
}

/// Checks whether `msg` is a `PARTICIPANT_DISPLAY_NAME_CHANGED` system event.
/// If so, validates and sanitises the packet, updates actor state via `server`,
/// and forwards the rebuilt packet to `recipient`. Returns `true` when the
/// message has been intercepted and the caller must `continue` the NATS loop;
/// `false` when the caller should fall through to `handle_msg`.
fn try_intercept_display_name_change(
    msg: &async_nats::Message,
    parsed: Option<&PacketWrapper>,
    room_id: &str,
    session: SessionId,
    recipient: &Recipient<Message>,
    server: &Addr<ChatServer>,
    transport: &str,
) -> bool {
    if !msg.subject.ends_with(".system") {
        return false;
    }

    // Reuse the wrapper parsed once in the NATS loop. An unparseable payload
    // falls through to `handle_msg` exactly as before.
    let wrapper = match parsed {
        Some(w) => w,
        None => return false,
    };

    if wrapper.packet_type != PacketType::MEETING.into() {
        return false;
    }

    let mut inner = match MeetingPacket::parse_from_bytes(&wrapper.data) {
        Ok(p) => p,
        Err(_) => return false,
    };

    if inner.event_type != MeetingEventType::PARTICIPANT_DISPLAY_NAME_CHANGED.into() {
        return false;
    }

    let target = match String::from_utf8(std::mem::take(&mut inner.target_user_id)) {
        Ok(s) => s,
        Err(_) => {
            warn!("UpdateMemberDisplayName: non-UTF-8 target_user_id in NATS packet, dropping");
            return true;
        }
    };
    let new_name = match String::from_utf8(std::mem::take(&mut inner.display_name)) {
        Ok(s) => s,
        Err(_) => {
            warn!("UpdateMemberDisplayName: non-UTF-8 display_name in NATS packet, dropping");
            return true;
        }
    };

    if target.is_empty() || new_name.is_empty() {
        return false;
    }

    let validated_name = match validate_display_name(&new_name) {
        Ok(name) => name,
        Err(e) => {
            warn!(
                "NATS PARTICIPANT_DISPLAY_NAME_CHANGED: rejecting invalid display name for user {} in room {}: {}",
                target, room_id, e
            );
            return true;
        }
    };

    let room_mismatch = !inner.room_id.is_empty() && inner.room_id != room_id;
    if room_mismatch {
        warn!(
            "UpdateMemberDisplayName: protobuf room_id '{}' differs from subscription room '{}', sanitizing before forwarding",
            inner.room_id, room_id
        );
    }

    // Capture the wire-level `session_id` before re-serialising so the
    // chat_server in-memory rename uses the same scoping the broadcast
    // packet carries. `0` is the proto-3 default and means "legacy /
    // user-id-wide rename"; non-zero values are interpreted by the handler
    // as session-scoped renames and validated against `room_members`.
    let packet_session_id = inner.session_id;

    // do_send is intentional here: display-name changes are rare
    // and low-priority. Mailbox backpressure on the actor is
    // unlikely; if it occurs the rename is silently skipped rather
    // than blocking the NATS subscription loop. The forwarded
    // client packet (below) is handled separately via try_send
    // with RELAY_PACKET_DROPS_TOTAL accounting.
    server.do_send(UpdateMemberDisplayName {
        room_id: room_id.to_string(),
        user_id: target.clone(),
        display_name: validated_name.clone(),
        session_id: packet_session_id,
    });

    // Always rebuild the packet with the authoritative room_id and validated
    // display_name so raw NATS payloads are never forwarded verbatim. The
    // `session_id` field is preserved as-published by `meeting-api` so peers
    // can route the rename to the correct per-session tile.
    inner.room_id = room_id.to_string();
    inner.target_user_id = target.into_bytes();
    inner.display_name = validated_name.into_bytes();
    let patched = inner;

    let forwarded = patched.write_to_bytes().and_then(|ib| {
        // Clone the shared wrapper only on this rare rename path so we can
        // rebuild it with the sanitized inner payload. The common case
        // (non-`.system` packets) never reaches here.
        let mut pw = wrapper.clone();
        pw.data = ib;
        pw.write_to_bytes()
    });

    match forwarded {
        Ok(sanitized) => {
            let message = Message {
                // `sanitized` is a freshly-serialized `Vec<u8>` (rare rename
                // path); move it into `Bytes` to match `Message.msg` (#1063).
                msg: bytes::Bytes::from(sanitized),
                session,
            };
            if let Err(e) = recipient.try_send(message) {
                // PRIORITY ATTRIBUTION DELIBERATELY NOT APPLIED HERE (#1145).
                // Unlike the main fan-out hop (`handle_msg`), this sibling
                // `try_send` site forwards exactly ONE packet type — a
                // sanitized PARTICIPANT_DISPLAY_NAME_CHANGED, which is a
                // `PacketType::MEETING` packet. `MEETING` is Critical in the
                // shed taxonomy (`priority_drop::OutboundPriority::classify_*`),
                // so it is NEVER a sheddable-media drop: running the classifier
                // here would always return Critical and the `drop_reason` would
                // always be `mailbox_full`. The label is therefore left as the
                // constant `mailbox_full` rather than adding a provably-constant
                // classify call on this rare (rename-only) path.
                RELAY_PACKET_DROPS_TOTAL
                    .with_label_values(&[room_id, "nats_delivery", "mailbox_full"])
                    .inc();
                // Same inbound-mailbox overflow signature as the main fan-out
                // site, attributed to the receiver's transport (Tier B #2 / #1057).
                RELAY_INBOUND_MAILBOX_DROPS_TOTAL
                    .with_label_values(&[transport])
                    .inc();
                warn!(
                    "Dropping sanitized PARTICIPANT_DISPLAY_NAME_CHANGED for session {}: {}",
                    session, e
                );
            }
        }
        Err(e) => {
            warn!(
                "Failed to re-serialize sanitized PARTICIPANT_DISPLAY_NAME_CHANGED, dropping forward: {}",
                e
            );
        }
    }

    true
}

/// Server-authoritative packet filter for observer (waiting-room) sessions.
///
/// # Enforcement Model
///
/// Audio/video isolation for waiting-room participants is enforced at three layers:
///
/// 1. **Server outbound (this function)** — Authoritative. Observer sessions only
///    receive MEETING and SESSION_ASSIGNED packets. All other packet types,
///    including MEDIA, are dropped. This is fail-closed: unparseable packets
///    and unknown packet types are also dropped.
///
/// 2. **Server inbound (`SessionLogic::handle_inbound`)** — Observer sessions
///    cannot publish MEDIA or KEYFRAME_REQUEST packets to the room.
///
/// 3. **Client-side (`decode_media = false`)** — Defense-in-depth only. The client
///    drops MEDIA packets when in observer mode, but this is bypassable by a
///    modified client and MUST NOT be the sole enforcement mechanism.
///
/// A modified client cannot bypass isolation because the server never sends
/// MEDIA packets to observer sessions in the first place.
// The per-session forwarding closure legitimately needs all of these inputs
// (recipient, room, session id, observer flag, user id, viewport set, layer
// prefs, and now the receiver transport for mailbox-drop attribution). Grouping
// them into a struct would not improve clarity for a single internal builder.
#[allow(clippy::too_many_arguments)]
fn handle_msg(
    session_recipient: Recipient<Message>,
    room: String,
    session: SessionId,
    observer: bool,
    receiver_user_id: String,
    desired_streams: DesiredStreams,
    layer_prefs: LayerPrefs,
    transport: String,
) -> impl Fn(async_nats::Message, Option<&PacketWrapper>) -> Result<(), std::io::Error> {
    // `parsed` is the PacketWrapper decoded ONCE per packet by the NATS loop
    // and shared with every consumer (display-name interceptor, viewport
    // interceptor, and this closure). Decoding here as well would double the
    // protobuf parse on the relay's hottest path. Unparseable payloads arrive
    // as `None`, which every downstream check treats as its "fail-closed"
    // default.
    move |msg, parsed| {
        let is_congestion = parsed
            .map(|pw| pw.packet_type == PacketType::CONGESTION.into())
            .unwrap_or(false);

        // LAYER_HINT is, like CONGESTION, a RELAY-authored self-addressed
        // control packet: the relay emits it on the publisher's OWN self-subject
        // (`room.{room}.{publisher}`) with the publisher's `session_id` stamped
        // (see `emit_layer_hint`). It must survive the self-echo guard below for
        // the same reason CONGESTION does. (#1108 delivery gap.)
        let is_layer_hint = parsed
            .map(|pw| pw.packet_type == PacketType::LAYER_HINT.into())
            .unwrap_or(false);

        let is_meeting = parsed
            .as_ref()
            .map(|pw| pw.packet_type == PacketType::MEETING.into())
            .unwrap_or(false);

        // Self-skip prevents echo of our own broadcasts. We treat a packet
        // as "from this session" if EITHER:
        //
        //   (a) the NATS subject equals our own publish subject, or
        //   (b) the embedded `PacketWrapper.session_id` matches our session.
        //
        // (a) catches the common in-actor echo where the publish subject
        // and the receiver's session line up exactly.
        //
        // (b) catches the post-reconnect window where a stale subscription
        // can deliver a packet whose subject differs from the receiver's
        // current session but whose embedded `session_id` (stamped by
        // `Handler<ClientMessage>`) still belongs to this connection.
        // This was the leak that surfaced in the 2026-05-08 production
        // meeting — the reporter received 5224 self-DIAGNOSTICS packets
        // back from the relay despite the subject-only filter being in
        // place. Applying the filter uniformly to every packet type
        // (with the CONGESTION and LAYER_HINT carve-outs below) closes the
        // leak.
        //
        // CONGESTION and LAYER_HINT are intentionally exempted: both are
        // RELAY-authored, self-ADDRESSED control packets. The relay's
        // per-source layer aggregator (LAYER_HINT, `emit_layer_hint`) publishes
        // onto the target publisher's OWN subject with that publisher's
        // session_id embedded, and the hint MUST still reach the publisher so
        // the client can cap its encoded simulcast ladder. The CONGESTION
        // carve-out is the same self-echo exemption: historically a congested
        // receiver's downlink overflow drove the relay to author a sender-keyed
        // CONGESTION here, but #1219 removed that emit (it collapsed the
        // publisher's encoder for the WHOLE room on a single slow receiver).
        // The carve-out itself stays — it still must pass any legitimately
        // relay-authored CONGESTION through to the target.
        // Without the LAYER_HINT carve-out the publish-side layer suppression
        // built in #1108 is inert: the hint is generated but the self-echo
        // guard drops it before it leaves the relay (the #1108 delivery gap).
        //
        // NOTE: the carve-out below trusts the packet TYPE only because the
        // relay authors these packets itself. Hardening against a *forged*
        // client-sent CONGESTION/LAYER_HINT (anti-reflection) is a separate,
        // still-open concern tracked in #1119 and is orthogonal to delivery —
        // do not conflate the two here.
        let subject_self = msg.subject == format!("room.{room}.{session}").replace(' ', "_").into();
        // N.B. `session_id` inside the packet is partially attacker-controlled;
        // this field is only safe for self-echo suppression, not for identity verification.
        let inner_session_self = parsed
            .map(|pw| pw.session_id != 0 && pw.session_id == session)
            .unwrap_or(false);

        // MEETING packets are server-authoritative — clients never publish them
        // (`classify_packet` drops client MEETING packets). So a MEETING packet
        // arriving on our OWN per-session subject is never an echo of our own
        // traffic; it is a server message addressed to us (a PARTICIPANT_JOINED
        // reply to our PARTICIPANT_LIST_REQUEST). MEETING packets therefore
        // bypass the subject-based self-skip, but are still dropped by
        // `inner_session_self` when they announce our own session.
        let drop_self_echo = if is_meeting {
            inner_session_self
        } else {
            subject_self || inner_session_self
        };
        if drop_self_echo && !is_congestion && !is_layer_hint {
            return Ok(());
        }

        // Unicast MEETING reply filter (see `RebroadcastPresence`): a
        // PARTICIPANT_JOINED sent in reply to a PARTICIPANT_LIST_REQUEST is
        // addressed to a single requester by publishing on that requester's
        // per-session subject (`room.{room}.{N}`). Every session receives it via
        // the room wildcard, so drop it unless it targets us. Broadcast MEETING
        // events (PARTICIPANT_JOINED at activation, PARTICIPANT_LEFT, etc.) use
        // the `room.{room}.system` subject and are left untouched.
        if is_meeting && !subject_self {
            let targets_other_session = msg
                .subject
                .as_str()
                .rsplit('.')
                .next()
                .is_some_and(|token| token.parse::<u64>().is_ok());
            if targets_other_session {
                return Ok(());
            }
        }

        // Unicast CONGESTION filter (#1220). Historically, relay-authored
        // CONGESTION was a self-addressed control packet published onto the
        // target sender's OWN per-session subject (`room.{room}.{sender_sid}`)
        // with that sender's `session_id` embedded. It was meaningful to
        // exactly ONE session — the targeted sender — yet the NATS room
        // wildcard (`room.{room}.*`) delivered it to EVERY session in the
        // room. Before this filter, every NON-target receiver forwarded
        // CONGESTION all the way to its transport, where the client discarded
        // it (`video_call_client.rs`: the client matches `session_id` against
        // its own and ignores otherwise). For a 20-person room that was
        // ~19/20 = 95% of CONGESTION deliveries wasted on the relay→transport
        // hop (serialize + channel enqueue + wire bytes) only to be dropped
        // client-side. The sender-keyed relay emit was removed in #1219, but
        // the filter remains valid defense-in-depth for any injected or
        // transitional CONGESTION packets.
        //
        // Model: the MEETING-unicast filter directly above. We drop CONGESTION
        // here unless this session is the target. "Target" is `subject_self ||
        // inner_session_self` — the SAME two conditions the self-echo carve-out
        // at line ~3708 relies on to let CONGESTION reach the targeted sender:
        //   * `subject_self`: normal case — the packet is on our own subject.
        //   * `inner_session_self`: post-reconnect case — the subject points at a
        //     stale session but the embedded `session_id` (stamped by the relay
        //     to `sender_sid`) still belongs to this connection.
        // Both are subject/relay-authoritative for delivery scoping (the
        // self-echo note above documents that `session_id` is safe for self-skip,
        // which is exactly the scoping decision made here — NOT identity auth).
        //
        // This does NOT touch the LAYER_HINT self-echo carve-out (the hard-won
        // #1108 delivery fix); LAYER_HINT is intentionally left alone (#1220).
        if is_congestion && !subject_self && !inner_session_self {
            RELAY_CONGESTION_FILTERED_TOTAL
                .with_label_values(&[&room])
                .inc();
            return Ok(());
        }

        // PEER_EVENT packets are unicast at the application layer: the
        // publisher addresses one specific peer via `target_peer_id` in the
        // inner `PeerEvent` payload. Because the NATS subject fan-out
        // delivers every published packet to every session in the room, we
        // filter here so that only the addressed session sees the event.
        //
        // Drop policy:
        //   - Unparseable PacketWrapper: drop (already-failed self-skip
        //     branch above handles `None` parse via the default `false`).
        //   - Unparseable inner PeerEvent: drop (cannot determine target).
        //   - target_peer_id != receiver_user_id: drop.
        // PEER_EVENT is additive — its absence from the observer allowlist
        // below means observers never see it, which is the desired
        // fail-closed default.
        let is_peer_event = parsed
            .map(|pw| pw.packet_type == PacketType::PEER_EVENT.into())
            .unwrap_or(false);
        if is_peer_event {
            // Parse failure or target mismatch is silently dropped; the
            // publisher does not need an error path because this is best-
            // effort confirmation feedback.
            let target_match = parsed
                .and_then(|pw| {
                    videocall_types::protos::peer_event::PeerEvent::parse_from_bytes(&pw.data).ok()
                })
                .map(|pe| pe.target_peer_id.as_slice() == receiver_user_id.as_bytes())
                .unwrap_or(false);
            if !target_match {
                trace!(
                    "Dropping PEER_EVENT for session {} (room {}): target_peer_id does not match",
                    session,
                    room
                );
                return Ok(());
            }
        }

        // Observer sessions (waiting room) only need meeting-control packets.
        // The client-side `decode_media: false` check is bypassable (WASM
        // patching, raw WebSocket capture, custom client), so the server
        // enforces this as the authoritative filter.
        //
        // Allowlist: only MEETING and SESSION_ASSIGNED are forwarded.
        // Everything else — including any future packet types — is dropped.
        // This is fail-closed by default: new PacketTypes must be explicitly
        // added here to reach observer sessions.
        if observer {
            let allowed = parsed
                .map(|pw| {
                    matches!(
                        pw.packet_type.enum_value(),
                        Ok(PacketType::MEETING) | Ok(PacketType::SESSION_ASSIGNED)
                    )
                })
                .unwrap_or(false); // unparseable → drop (fail-closed)

            if !allowed {
                trace!(
                    "Dropping non-allowed packet for observer session {} in room {}",
                    session,
                    room
                );
                return Ok(());
            }
        }

        // Viewport-aware VIDEO filtering (HCL issue #988).
        //
        // This is a SUBTRACT-ONLY filter that runs strictly AFTER the observer
        // authorization allowlist above. It can only reduce what an
        // already-authorized session receives; it never grants access and
        // never short-circuits the observer gate. JWT/JoinRoom membership +
        // the observer allowlist remain the sole authorization boundary.
        //
        // Drop the packet iff ALL of the following hold:
        //   1. the cleartext envelope `media_kind` is VIDEO, AND
        //   2. this receiver has a non-empty desired-streams (viewport) set, AND
        //   3. the SUBJECT-DERIVED source session (NOT the forgeable payload
        //      `session_id`) is NOT in that set.
        //
        // Everything else forwards. In particular:
        //   - AUDIO is NEVER filtered (off-screen speakers must be heard;
        //     keying audio off a client-supplied list would let a client
        //     silence a target — a security non-starter).
        //   - SCREEN is NEVER filtered by the camera-tile viewport (a separate
        //     future signal governs screen-share).
        //   - HEARTBEAT / RTT / KEYFRAME_REQUEST and every non-MEDIA packet
        //     type are NEVER filtered (control/liveness).
        //   - `media_kind` UNSPECIFIED (0) fails OPEN (forwards) so older
        //     clients and any packet without the discriminator are unaffected.
        //   - An empty desired set fails OPEN (no viewport signal yet).
        //
        // NOTE: this filter intentionally inspects ONLY the cleartext outer
        // `media_kind`. It never parses the (possibly E2EE-sealed) inner
        // MediaPacket, so it is correct whether or not E2EE is enabled.
        //
        // The viewport filter (#988) and the layer filter (#989) BOTH key off
        // the SUBJECT-DERIVED source session and run only for VIDEO packets, so
        // we resolve `source` ONCE here and share it across both — the source
        // parse is on the relay's hottest path (every media frame × every
        // receiver × every room) and was previously computed twice per VIDEO
        // packet.
        //
        // SOURCE IDENTITY MUST come from the NATS subject, NEVER from the
        // payload `pw.session_id`. The wrapper `session_id` is
        // attacker-controllable (ingress only stamps it when the client sends 0;
        // see the self-echo note above), so a modified client could forge it to
        // a receiver-VISIBLE peer's id and smuggle off-screen VIDEO past these
        // filters. The subject — `room.{room}.{publisher_session}` — is set by
        // the relay from the authenticated connection and cannot be forged by a
        // peer, exactly as relied on for `subject_self` and VIEWPORT ownership.
        // Room IDs match `^[a-zA-Z0-9_-]*$` (no dots) and the session is a
        // pure-digit u64, so the part after the LAST `.` is the publisher
        // session. If it does not parse (shouldn't happen for normal media),
        // FAIL OPEN — never drop on an unparseable source.
        if let Some(pw) = parsed {
            use videocall_types::protos::packet_wrapper::packet_wrapper::MediaKind;
            let wire_media_kind = pw.media_kind.enum_value();
            let is_video = wire_media_kind == Ok(MediaKind::VIDEO);
            // Layer filtering (issue #989, Phase 3) applies to VIDEO, SCREEN,
            // AND AUDIO — each addressed independently via its media kind. The
            // viewport filter remains VIDEO-only (it answers "is this sender's
            // CAMERA wanted on screen"; screen/audio are never viewport-gated).
            let is_layer_filterable = matches!(
                wire_media_kind,
                Ok(MediaKind::VIDEO) | Ok(MediaKind::SCREEN) | Ok(MediaKind::AUDIO)
            );

            // Resolve the SUBJECT-derived source ONCE, shared by both filters
            // (source identity must come from the subject, never the forgeable
            // payload session_id — see the block comment above).
            let source = if is_video || is_layer_filterable {
                msg.subject
                    .as_str()
                    .rsplit('.')
                    .next()
                    .and_then(|tok| tok.parse::<u64>().ok())
            } else {
                None
            };

            if is_video {
                // ----- Viewport filter (#988): "is this SENDER wanted?" -----
                //
                // Read the viewport set ONCE: derive both the drop decision and
                // the current set size from a single guard so the debug log on
                // the drop path costs no extra RwLock read (the drop path runs
                // at near-full inbound VIDEO rate per receiver during a viewport
                // collapse — exactly when this log matters most). A poisoned
                // lock fails OPEN (forward); an unparseable source (`None`) also
                // fails OPEN.
                let (drop_video, viewport_len) = match source {
                    Some(src) => desired_streams
                        .read()
                        .map(|st| (!st.ids.is_empty() && !st.ids.contains(&src), st.ids.len()))
                        .unwrap_or((false, 0)),
                    None => (false, 0),
                };
                if drop_video {
                    // Intentional, viewport-driven drop — accounted on a
                    // DEDICATED counter so it never pollutes the backpressure
                    // (mailbox-full) drop metric / its dashboards & alerts.
                    RELAY_VIEWPORT_FILTERED_TOTAL
                        .with_label_values(&[&room])
                        .inc();
                    // DEBUG (not trace) so a SCOPED `RUST_LOG=...chat_server=debug`
                    // — not global trace — can reconstruct who-dropped-what-from-whom
                    // for one room: receiver session, the SUBJECT-derived source
                    // (#994: derived from the NATS subject, not the forgeable payload
                    // session_id), and the current viewport set size (a collapse
                    // toward 0/1 is the wrongly-dropping signature). Per-source
                    // forensics live HERE, never in metric labels (cardinality).
                    // `viewport_len` was captured from the single decision read
                    // above — no second lock on the hot drop path.
                    debug!(
                        "Viewport drop: off-screen VIDEO from subject-derived source {:?} for receiver session {} in room {} (viewport set size {})",
                        source, session, room, viewport_len
                    );
                    return Ok(());
                }
                // Forwarded VIDEO — the denominator complement of the filtered
                // counter (HCL #988). Mutually exclusive with the drop branch
                // above; together they cover every VIDEO packet at the filter.
                RELAY_VIEWPORT_FORWARDED_TOTAL
                    .with_label_values(&[&room])
                    .inc();
            }

            // ----- Layer filter (#989): "which LAYER of a wanted sender?" -----
            //
            // Phase 3: applies to VIDEO, SCREEN, AND AUDIO, each addressed
            // independently by its media kind. For VIDEO this runs strictly
            // AFTER the viewport filter above (a VIDEO packet only reaches here
            // if the viewport forwarded it). SCREEN/AUDIO skip the viewport
            // filter entirely.
            //
            // NO-OP-FIRST / fail-open. Drop iff ALL hold:
            //   1. the cleartext `simulcast_layer_id` is non-zero, AND
            //   2. this receiver has a recorded layer preference for the
            //      (subject-derived source session, this media kind), AND
            //   3. that preference selects a DIFFERENT layer.
            // Everything else FORWARDS. In particular:
            //   - No recorded preference for this (source, kind) (or empty map)
            //     → FORWARD. The no-op gate: with no LAYER_PREFERENCE recorded
            //     the path is byte-identical to pre-#989 behaviour.
            //   - `simulcast_layer_id == 0` (base / un-upgraded publisher) →
            //     always FORWARD.
            //   - Non-media kinds are excluded by `is_layer_filterable`.
            //
            // The (source, media_kind) key (Phase 3) is what lets a receiver
            // request a low SCREEN layer while keeping full camera VIDEO from
            // the SAME source: the camera packet keys (src, VIDEO) and the
            // screen packet keys (src, SCREEN), matched against the receiver's
            // per-kind recorded preference.
            //
            // EMPTY-PREFS FAST PATH: `layer_prefs.has_any()` is a lock-free
            // AtomicBool hint that is `false` until this receiver records its
            // first LAYER_PREFERENCE, short-circuiting WITHOUT the read lock. A
            // spurious `true` only costs a read lock that fails open (never a
            // wrong drop) — see the `LayerPrefs` type doc.
            //
            // TRUST BOUNDARY (#993): both `simulcast_layer_id` (field 5) and the
            // LAYER_PREFERENCE that populated `layer_prefs` live OUTSIDE the AEAD
            // seal. A forged value only self-degrades the FORGER's OWN view.
            // Source identity comes from the NATS SUBJECT, never the forgeable
            // payload `session_id`.
            //
            // AVAILABILITY NOT VALIDATED: the relay does NOT check that the
            // requested layer is actually produced by the source; a client
            // requesting an absent layer black-tiles ITSELF (client-side clamp
            // is the mitigation, see the receiver chooser).
            if is_layer_filterable && pw.simulcast_layer_id != 0 && layer_prefs.has_any() {
                // The wire media kind, normalized to the preference-map key
                // discriminant (UNSPECIFIED→VIDEO etc.).
                let kind_key =
                    normalize_pref_media_kind(wire_media_kind.map(|k| k as i32).unwrap_or(0));
                // Drop iff there is a recorded preference for this
                // (source, kind) AND it selects a different layer. No entry →
                // fail-open (forward). A poisoned lock fails OPEN (forward). An
                // unparseable source (`None`) fails OPEN (forward).
                let drop_layer = match source {
                    Some(src) => layer_prefs
                        .state
                        .read()
                        .map(|st| {
                            st.layers
                                .get(&(src, kind_key))
                                .is_some_and(|&want| want != pw.simulcast_layer_id)
                        })
                        .unwrap_or(false),
                    None => false,
                };

                if drop_layer {
                    RELAY_LAYER_FILTERED_TOTAL.with_label_values(&[&room]).inc();
                    debug!(
                        "Layer drop: simulcast layer {} (kind {}) from subject-derived source {:?} not selected by receiver session {} in room {}",
                        pw.simulcast_layer_id, kind_key, source, session, room
                    );
                    return Ok(());
                }
                // Forwarded simulcast media — denominator complement of the
                // layer-filtered counter (#989).
                RELAY_LAYER_FORWARDED_TOTAL
                    .with_label_values(&[&room])
                    .inc();
            }

            // Per-LAYER distribution (#1105). Counts EVERY filterable media
            // packet that SURVIVED both filters above — i.e. every drop path
            // (`viewport`, `layer`) has already `return`ed, so reaching here
            // means this packet is about to be forwarded. Unlike
            // RELAY_LAYER_FORWARDED_TOTAL (the non-base, has-prefs denominator),
            // this covers ALL forwarded layers including base 0 and the
            // no-prefs fail-open case, giving the true layer MIX per room.
            //
            // `layer_id_bucket` clamps the forgeable wire `simulcast_layer_id`
            // (#993) into one of exactly 4 bounded buckets (0|1|2|other) BEFORE
            // it becomes a label — the cardinality bound is enforced there.
            if is_layer_filterable {
                RELAY_LAYER_FORWARDED_BY_LAYER_TOTAL
                    .with_label_values(&[&room, layer_id_bucket(pw.simulcast_layer_id)])
                    .inc();
            }
        }

        let message = Message {
            // FAN-OUT HOT PATH (#1063): `msg.payload` is an `async_nats`
            // `bytes::Bytes`. Cloning the handle is an O(1) atomic refcount
            // bump that SHARES the single NATS payload allocation across every
            // receiver in this room's fan-out — replacing the previous
            // `.to_vec()` that deep-copied the multi-KB frame once per
            // recipient. The per-receiver materialization back to owned bytes
            // (for the outbound channel) still happens at most once, later, in
            // `SessionLogic::handle_outbound`; delivery is byte-identical.
            msg: msg.payload.clone(),
            session,
        };

        if let Err(e) = session_recipient.try_send(message) {
            // PRIORITY-AWARE ATTRIBUTION on inbound fan-out overflow (#1145).
            //
            // HONEST CONTRACT — read before "improving" this: the actix
            // mailbox exposes NO capacity/length probe and NO preemption API
            // on `Recipient<Message>` (only `try_send`/`do_send`, verified
            // against actix 0.13.5). So when `try_send` returns `Full` the
            // packet simply CANNOT be enqueued — we do NOT, and CANNOT, evict
            // a queued packet to make room for a higher-priority one. The
            // value this block adds is therefore (a) correct ATTRIBUTION of
            // WHICH KIND was sacrificed on overflow (video vs audio vs
            // lifecycle), so dashboards/alerts see "video shed under fan-out
            // burst" rather than an undifferentiated `mailbox_full`, and
            // (b) it pairs with the mailbox HEADROOM bump (#1144) that gives
            // the burst room to land in the mailbox and then spill onto the
            // policy-aware outbound channel (which DOES shed video-first and
            // record drops for metrics / keyframe-relax). Shedding alone
            // can't save a critical packet on a full mailbox; the headroom is
            // what actually prevents the drop.
            //
            // We classify off the OUTER cleartext wrapper that was already
            // parsed ONCE per packet (`parsed`) — `packet_type` + the outer
            // `media_kind` (field 5) — NEVER the inner `MediaPacket`, which is
            // AES-sealed under E2EE. This is the SAME data the #988/#989
            // filters above already read, so the added per-receiver work here
            // is O(1): two enum reads on already-decoded fields, no parse, no
            // allocation, no lock. Fail-open: an unparseable wrapper
            // (`parsed == None`) or UNSPECIFIED/unknown media_kind classifies
            // as Control and is attributed `mailbox_full` (never preferentially
            // blamed as a media shed).
            //
            // We also distinguish `Full` (transient backpressure — the fan-out
            // burst case) from `Closed` (the receiver actor is gone). Only
            // `Full` is a shed scenario; a `Closed` drop keeps the plain
            // `mailbox_full` label and the warn, exactly as before.
            let is_full = matches!(e, SendError::Full(_));
            let priority = match parsed {
                Some(pw) => OutboundPriority::classify_outer(
                    true,
                    pw.packet_type
                        .enum_value()
                        .unwrap_or(PacketType::PACKET_TYPE_UNKNOWN),
                    pw.media_kind.enum_value().unwrap_or(
                        videocall_types::protos::packet_wrapper::packet_wrapper::MediaKind::MEDIA_KIND_UNSPECIFIED,
                    ),
                ),
                // Unparseable outer wrapper → fail-open Control (never a media shed).
                None => OutboundPriority::Control,
            };

            // Pick the per-room `drop_reason` label. On a `Full` mailbox a
            // droppable media kind (VIDEO/SCREEN → `priority_drop_video`,
            // AUDIO → `priority_drop_audio`) attributes the sacrifice to that
            // kind; everything else (Critical/Control, or a `Closed` mailbox)
            // keeps the legacy `mailbox_full` label. The labels mirror the
            // OUTBOUND taxonomy documented on `OUTBOUND_CHANNEL_DROPS_TOTAL`
            // (`metrics.rs`), so a single dashboard query spans both hops.
            let drop_reason = match (is_full, priority.priority_drop_label()) {
                (true, Some(label)) => label,
                _ => "mailbox_full",
            };

            // Room-tagged forensic series (kept for per-room drill-down). The
            // `transport="nats_delivery"` here is the publish-side identity, not
            // the receiver's transport.
            RELAY_PACKET_DROPS_TOTAL
                .with_label_values(&[&room, "nats_delivery", drop_reason])
                .inc();
            // Low-cardinality fleet-alerting sibling (Tier B #2 / #1057): the
            // room-wide-freeze signature, labeled by the RECEIVER's transport so
            // an SRE can rate() it without scraping per-room series and can tell
            // which transport's mailbox is overflowing. This counts EVERY
            // inbound-mailbox drop regardless of attributed kind, so the #1057
            // freeze signature (sum over transport) is unchanged by the new
            // per-room `drop_reason` split.
            RELAY_INBOUND_MAILBOX_DROPS_TOTAL
                .with_label_values(&[&transport])
                .inc();
            // The `Dropping inbound message for session <id> ... (mailbox full)`
            // line is a STABLE CONTRACT consumed by
            // `scripts/parse_meeting_console_logs.sh` (`--relay-ws`), which
            // greps it per session to reconstruct mailbox-drop counts. It is
            // kept VERBATIM (same text + WARN level) for every drop so the
            // analyzer is not silently corrupted — the priority attribution
            // added by this change lives entirely on the `drop_reason` metric
            // label above, not in the log line. Do NOT edge-trigger or demote
            // this line without updating the parse script in lock-step.
            warn!(
                "Dropping inbound message for session {}: {} (mailbox full — subscription continues)",
                session, e
            );
        }
        Ok(())
    }
}

// ==========================================================================
// Unit Tests for ChatServer
// ==========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use actix::Actor;
    use serial_test::serial;

    /// Test helper: create a database pool for integration tests.
    /// Kept for future JWT flow testing (create meeting -> get JWT -> connect via WS/WT).
    #[allow(dead_code)]
    async fn get_test_pool() -> sqlx::PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
        sqlx::PgPool::connect(&database_url)
            .await
            .expect("Failed to connect to test database")
    }

    // ==========================================================================
    // TEST: JoinRoom rejects reserved system user ID synchronously
    // ==========================================================================
    // This test verifies the fix for the race condition where JoinRoom would
    // spawn an async task and immediately return Ok(()), even if validation
    // would fail inside the task. Now validation happens synchronously.
    #[actix_rt::test]
    #[serial]
    async fn test_join_room_rejects_system_user_id_synchronously() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        // Start the ChatServer actor
        let chat_server = ChatServer::new(nats_client).await.start();

        // Create a mock session recipient
        // We need a real actor to receive messages, so we use a simple dummy
        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1001u64;

        // Register the session first
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Attempt to join with the reserved system user ID
        // This should return an error SYNCHRONOUSLY (not Ok then fail async)
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room".to_string(),
                user_id: SYSTEM_USER_ID.to_string(),
                display_name: SYSTEM_USER_ID.to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        // The key assertion: JoinRoom should return Err immediately
        assert!(
            result.is_err(),
            "JoinRoom with system user ID should return Err, not Ok"
        );

        let error_msg = result.unwrap_err();
        assert!(
            error_msg.contains("reserved system user ID"),
            "Error should mention reserved system user ID, got: {error_msg}"
        );
    }

    // ==========================================================================
    // TEST: JoinRoom succeeds with valid user_id
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_join_room_succeeds_with_valid_user() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1002u64;

        // Register the session
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Join with a valid user_id - should succeed
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-valid".to_string(),
                user_id: "valid-user@example.com".to_string(),
                display_name: "valid-user@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result.is_ok(),
            "JoinRoom with valid user should return Ok, got: {result:?}"
        );
    }

    // ==========================================================================
    // TEST: JoinRoom fails if session not registered
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_join_room_fails_without_session() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        // Try to join WITHOUT registering the session first
        let result = chat_server
            .send(JoinRoom {
                session: 9999u64,
                room: "test-room".to_string(),
                user_id: "valid-user@example.com".to_string(),
                display_name: "valid-user@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result.is_err(),
            "JoinRoom without registered session should return Err"
        );
        assert!(
            result.unwrap_err().contains("Session not found"),
            "Error should mention session not found"
        );
    }

    // ==========================================================================
    // TEST: Duplicate join with same session returns Ok
    // ==========================================================================
    // Verifies that a second JoinRoom for the same session_id returns Ok
    // immediately because the session is already tracked in active_subs.
    #[actix_rt::test]
    #[serial]
    async fn test_duplicate_join_same_session_returns_ok() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1003u64;

        // Register the session
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // First join attempt - should succeed (returns Ok immediately,
        // spawns async task which will also succeed with valid user)
        let result1 = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-cleanup".to_string(),
                user_id: "valid-user@example.com".to_string(),
                display_name: "valid-user@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        assert!(result1.is_ok(), "First join should succeed");

        // Second join attempt with same session - should return Ok
        // immediately because session is already in active_subs
        let result2 = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-cleanup".to_string(),
                user_id: "valid-user@example.com".to_string(),
                display_name: "valid-user@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result2.is_ok(),
            "Second join with same session should return Ok (already active)"
        );
    }

    // ==========================================================================
    // TEST: Two clients with same user_id get unique session_id values
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_same_user_id_unique_session_ids() {
        use crate::actors::session_logic::SessionLogic;
        use crate::server_diagnostics::{TrackerMessage, TrackerSender};
        use crate::session_manager::SessionManager;
        use tokio::sync::mpsc;

        let _pool = get_test_pool().await;
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        let (tx, _rx) = mpsc::unbounded_channel::<TrackerMessage>();
        let tracker_sender: TrackerSender = tx;
        let session_manager = SessionManager::new();

        // Create two sessions with the same user_id
        let user_id = "same-user@example.com".to_string();
        let room = "test-room-unique".to_string();

        let session1 = SessionLogic::new(
            chat_server.clone(),
            room.clone(),
            user_id.clone(),
            user_id.clone(), // display_name fallback
            false,           // is_guest
            nats_client.clone(),
            tracker_sender.clone(),
            session_manager.clone(),
            false,
            None, // no instance_id
            "websocket",
            false, // is_host
            false, // end_on_host_leave
        );

        let session2 = SessionLogic::new(
            chat_server.clone(),
            room.clone(),
            user_id.clone(),
            user_id.clone(), // display_name fallback
            false,           // is_guest
            nats_client.clone(),
            tracker_sender.clone(),
            session_manager.clone(),
            false,
            None, // no instance_id
            "websocket",
            false, // is_host
            false, // end_on_host_leave
        );

        // Verify they have different session IDs
        assert_ne!(
            session1.id, session2.id,
            "Two sessions with same user_id should have different session_id values"
        );
        assert!(session1.id != 0, "Session ID should not be zero");
        assert!(session2.id != 0, "Session ID should not be zero");
    }

    // ==========================================================================
    // TEST: ConnectionState transitions - Testing does not publish to NATS
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_connection_state_testing_does_not_publish() {
        use crate::messages::server::Packet;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let _pool = get_test_pool().await;
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1004u64;
        let room = "test-room-state".to_string();

        // Register session - starts in Testing state
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        let subject = format!("room.{room}.{session_id}").replace(' ', "_");
        let published = Arc::new(AtomicBool::new(false));
        let published_clone = published.clone();
        let mut sub = nats_client
            .subscribe(subject.clone())
            .await
            .expect("Failed to subscribe");

        tokio::spawn(async move {
            if let Ok(Some(_msg)) =
                tokio::time::timeout(Duration::from_millis(500), sub.next()).await
            {
                published_clone.store(true, Ordering::Relaxed);
            }
        });

        // Send message while in Testing state - should NOT publish
        chat_server
            .send(ClientMessage {
                session: session_id,
                room: room.clone(),
                msg: Packet {
                    data: Arc::new(b"test data".to_vec()),
                },
                user: "test@example.com".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        // Wait a bit to ensure no publish happened
        sleep(Duration::from_millis(600)).await;

        assert!(
            !published.load(Ordering::Relaxed),
            "Message should NOT be published while in Testing state"
        );
    }

    // ==========================================================================
    // TEST: ConnectionState transitions - Active publishes to NATS
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_connection_state_active_publishes() {
        use crate::messages::server::Packet;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let _pool = get_test_pool().await;
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1005u64;
        let room = "test-room-active".to_string();

        // Register session - starts in Testing state
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Activate the connection
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");

        let subject = format!("room.{room}.{session_id}").replace(' ', "_");
        let published = Arc::new(AtomicBool::new(false));
        let published_clone = published.clone();
        let mut sub = nats_client
            .subscribe(subject.clone())
            .await
            .expect("Failed to subscribe");

        tokio::spawn(async move {
            if let Ok(Some(_msg)) =
                tokio::time::timeout(Duration::from_millis(500), sub.next()).await
            {
                published_clone.store(true, Ordering::Relaxed);
            }
        });

        // Send message while in Active state - should publish
        chat_server
            .send(ClientMessage {
                session: session_id,
                room: room.clone(),
                msg: Packet {
                    data: Arc::new(b"test data".to_vec()),
                },
                user: "test@example.com".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        // Wait for publish
        sleep(Duration::from_millis(600)).await;

        assert!(
            published.load(Ordering::Relaxed),
            "Message should be published while in Active state"
        );
    }

    // ==========================================================================
    // TEST: ActivateConnection handler is idempotent
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_activate_connection_idempotent() {
        let _pool = get_test_pool().await;
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1006u64;

        // Register session - starts in Testing state
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // First activation - should transition Testing -> Active
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");

        // Verify state is Active
        let state1 = chat_server
            .send(GetConnectionState {
                session: session_id,
            })
            .await
            .expect("GetConnectionState should succeed")
            .expect("GetConnectionState should return Ok");
        assert_eq!(
            state1,
            ConnectionState::Active,
            "State should be Active after first activation"
        );

        // Second activation - should remain Active (idempotent)
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");

        // Verify state is still Active
        let state2 = chat_server
            .send(GetConnectionState {
                session: session_id,
            })
            .await
            .expect("GetConnectionState should succeed")
            .expect("GetConnectionState should return Ok");
        assert_eq!(
            state2,
            ConnectionState::Active,
            "State should remain Active after second activation (idempotent)"
        );
    }

    // ==========================================================================
    // TEST: JoinRoom broadcasts MEETING_STARTED via NATS (no session_id)
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_join_room_broadcasts_meeting_started() {
        use std::sync::{Arc, Mutex};
        use tokio::time::{sleep, Duration};
        use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        // `Message.msg` is now `bytes::Bytes` (#1063); capture the shared
        // handles directly. `&Bytes` derefs to `&[u8]` for the parse below.
        let received: Arc<Mutex<Vec<bytes::Bytes>>> = Arc::new(Mutex::new(Vec::new()));

        struct CapturingSession {
            received: Arc<Mutex<Vec<bytes::Bytes>>>,
        }
        impl Actor for CapturingSession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for CapturingSession {
            type Result = ();
            fn handle(&mut self, msg: Message, _ctx: &mut Self::Context) {
                self.received.lock().unwrap().push(msg.msg);
            }
        }

        let capturing = CapturingSession {
            received: received.clone(),
        }
        .start();
        let session_id = 1007u64;

        chat_server
            .send(Connect {
                id: session_id,
                addr: capturing.recipient(),
            })
            .await
            .expect("Connect should succeed");

        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-broadcast".to_string(),
                user_id: "alice@example.com".to_string(),
                display_name: "alice@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        assert!(result.is_ok(), "JoinRoom should succeed");

        // Wait for the spawned async task to complete and NATS subscription to deliver
        sleep(Duration::from_millis(500)).await;

        let msgs = received.lock().unwrap();
        // The session should NOT receive SESSION_ASSIGNED from ChatServer
        // (that's the transport layer's job). It may receive MEETING_STARTED
        // via NATS if the subscription was set up in time.
        for msg_bytes in msgs.iter() {
            if let Ok(wrapper) = <PacketWrapper as ProtobufMessage>::parse_from_bytes(msg_bytes) {
                assert_ne!(
                    wrapper.packet_type,
                    PacketType::SESSION_ASSIGNED.into(),
                    "ChatServer JoinRoom should NOT send SESSION_ASSIGNED directly"
                );
                if wrapper.packet_type == PacketType::MEETING.into() {
                    assert_eq!(
                        wrapper.session_id, 0,
                        "MEETING_STARTED must not carry session_id"
                    );
                }
            }
        }
    }

    // ==========================================================================
    // TEST: Observer JoinRoom does NOT publish PARTICIPANT_JOINED
    // ==========================================================================
    // When an observer (waiting room user) joins a room, the server should NOT
    // publish a PARTICIPANT_JOINED event to NATS. Only real participants trigger
    // this notification.
    #[actix_rt::test]
    #[serial]
    async fn test_observer_join_does_not_publish_participant_joined() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 2001u64;
        let room = "test-room-observer-join";

        // Subscribe to the system subject for this room BEFORE join
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let participant_joined_received = Arc::new(AtomicBool::new(false));
        let flag = participant_joined_received.clone();
        let mut sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");

        tokio::spawn(async move {
            use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
            use videocall_types::protos::meeting_packet::MeetingPacket;

            while let Ok(Some(msg)) =
                tokio::time::timeout(Duration::from_millis(1500), sub.next()).await
            {
                if let Ok(wrapper) =
                    <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
                {
                    if let Ok(inner) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                        if inner.event_type == MeetingEventType::PARTICIPANT_JOINED.into() {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Register session
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Join as observer - should NOT publish PARTICIPANT_JOINED
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "observer-user@example.com".to_string(),
                display_name: "observer-user@example.com".to_string(),
                is_guest: false,
                observer: true,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        assert!(result.is_ok(), "Observer JoinRoom should succeed");

        // Wait long enough for any NATS publish to arrive
        sleep(Duration::from_millis(1000)).await;

        assert!(
            !participant_joined_received.load(Ordering::Relaxed),
            "Observer join should NOT publish PARTICIPANT_JOINED to NATS"
        );
    }

    // ==========================================================================
    // TEST: Non-observer JoinRoom + ActivateConnection publishes PARTICIPANT_JOINED
    // ==========================================================================
    // When a real participant joins a room and their connection is activated,
    // the server should publish a PARTICIPANT_JOINED event to NATS so other
    // peers are notified. The broadcast is deferred from JoinRoom to
    // ActivateConnection to avoid ghost join events during RTT election.
    #[actix_rt::test]
    #[serial]
    async fn test_non_observer_join_publishes_participant_joined() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 2002u64;
        let room = "test-room-non-observer-join";

        // Subscribe to the system subject for this room BEFORE join
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let participant_joined_received = Arc::new(AtomicBool::new(false));
        let flag = participant_joined_received.clone();
        let mut sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");

        tokio::spawn(async move {
            use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
            use videocall_types::protos::meeting_packet::MeetingPacket;

            while let Ok(Some(msg)) =
                tokio::time::timeout(Duration::from_millis(1500), sub.next()).await
            {
                if let Ok(wrapper) =
                    <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
                {
                    if let Ok(inner) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                        if inner.event_type == MeetingEventType::PARTICIPANT_JOINED.into() {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Register session
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Join as non-observer
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "real-user@example.com".to_string(),
                display_name: "real-user@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        assert!(result.is_ok(), "Non-observer JoinRoom should succeed");

        // Activate the connection — this triggers the deferred PARTICIPANT_JOINED
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");

        // Wait for the async task to publish PARTICIPANT_JOINED
        sleep(Duration::from_millis(1000)).await;

        assert!(
            participant_joined_received.load(Ordering::Relaxed),
            "Non-observer join + activate SHOULD publish PARTICIPANT_JOINED to NATS"
        );
    }

    // ==========================================================================
    // TEST: JoinRoom without ActivateConnection does NOT publish PARTICIPANT_JOINED
    // ==========================================================================
    // When a connection joins but is never activated (e.g., the losing connection
    // during RTT election), PARTICIPANT_JOINED should NOT be broadcast.
    #[actix_rt::test]
    #[serial]
    async fn test_join_without_activate_does_not_publish_participant_joined() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 3001u64;
        let room = "test-room-no-activate";

        // Subscribe to the system subject for this room BEFORE join
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let participant_joined_received = Arc::new(AtomicBool::new(false));
        let flag = participant_joined_received.clone();
        let mut sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");

        tokio::spawn(async move {
            use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
            use videocall_types::protos::meeting_packet::MeetingPacket;

            while let Ok(Some(msg)) =
                tokio::time::timeout(Duration::from_millis(1500), sub.next()).await
            {
                if let Ok(wrapper) =
                    <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
                {
                    if let Ok(inner) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                        if inner.event_type == MeetingEventType::PARTICIPANT_JOINED.into() {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Register session
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Join as non-observer but do NOT activate
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "testing-user@example.com".to_string(),
                display_name: "testing-user@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        assert!(result.is_ok(), "JoinRoom should succeed");

        // Wait — no ActivateConnection sent
        sleep(Duration::from_millis(1500)).await;

        assert!(
            !participant_joined_received.load(Ordering::Relaxed),
            "JoinRoom without ActivateConnection should NOT publish PARTICIPANT_JOINED"
        );
    }

    // ==========================================================================
    // TEST: Testing session disconnect does NOT publish PARTICIPANT_LEFT
    // ==========================================================================
    // When a Testing session disconnects (e.g., the losing connection during
    // RTT election), PARTICIPANT_LEFT should NOT be broadcast because
    // PARTICIPANT_JOINED was never broadcast for it.
    #[actix_rt::test]
    #[serial]
    async fn test_testing_session_disconnect_does_not_publish_participant_left() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 3002u64;
        let room = "test-room-testing-dc";

        // Register and join (Testing state, never activated)
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "testing-dc@example.com".to_string(),
                display_name: "testing-dc@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed");
        assert!(result.is_ok(), "JoinRoom should succeed");

        // Wait for session setup
        sleep(Duration::from_millis(300)).await;

        // Subscribe to system subject to watch for PARTICIPANT_LEFT
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let participant_left_received = Arc::new(AtomicBool::new(false));
        let flag = participant_left_received.clone();
        let mut sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");

        tokio::spawn(async move {
            use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
            use videocall_types::protos::meeting_packet::MeetingPacket;

            while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(6), sub.next()).await
            {
                if let Ok(wrapper) =
                    <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
                {
                    if let Ok(inner) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                        if inner.event_type == MeetingEventType::PARTICIPANT_LEFT.into() {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Disconnect while still in Testing state (never activated)
        chat_server
            .send(Disconnect {
                session: session_id,
                room: room.to_string(),
                user_id: "testing-dc@example.com".to_string(),
                display_name: "testing-dc@example.com".to_string(),
                is_guest: false,
                observer: false,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Disconnect should succeed");

        // Wait for grace period to expire plus buffer
        sleep(Duration::from_secs(4)).await;

        assert!(
            !participant_left_received.load(Ordering::Relaxed),
            "Testing session disconnect should NOT publish PARTICIPANT_LEFT \
             (was_active=false prevents ghost leave event)"
        );
    }

    // ==========================================================================
    // TEST: Observer Disconnect does NOT publish PARTICIPANT_LEFT
    // ==========================================================================
    // When an observer session disconnects (e.g., waiting room user admitted),
    // the server should NOT publish a PARTICIPANT_LEFT event. The user was never
    // a real participant in the meeting.
    #[actix_rt::test]
    #[serial]
    async fn test_observer_disconnect_does_not_publish_participant_left() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 2003u64;
        let room = "test-room-observer-disconnect";

        // Register and join as observer first
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "observer-dc@example.com".to_string(),
                display_name: "observer-dc@example.com".to_string(),
                is_guest: false,
                observer: true,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed");
        assert!(result.is_ok(), "Observer JoinRoom should succeed");

        // Wait for session to be fully set up
        sleep(Duration::from_millis(300)).await;

        // Now subscribe to system subject to watch for PARTICIPANT_LEFT
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let participant_left_received = Arc::new(AtomicBool::new(false));
        let flag = participant_left_received.clone();
        let mut sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");

        tokio::spawn(async move {
            use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
            use videocall_types::protos::meeting_packet::MeetingPacket;

            while let Ok(Some(msg)) =
                tokio::time::timeout(Duration::from_millis(1500), sub.next()).await
            {
                if let Ok(wrapper) =
                    <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
                {
                    if let Ok(inner) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                        if inner.event_type == MeetingEventType::PARTICIPANT_LEFT.into() {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Disconnect as observer - should NOT publish PARTICIPANT_LEFT
        chat_server
            .send(Disconnect {
                session: session_id,
                room: room.to_string(),
                user_id: "observer-dc@example.com".to_string(),
                display_name: "observer-dc@example.com".to_string(),
                is_guest: false,
                observer: true,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Disconnect should succeed");

        // Wait long enough for any NATS publish to arrive
        sleep(Duration::from_millis(1000)).await;

        assert!(
            !participant_left_received.load(Ordering::Relaxed),
            "Observer disconnect should NOT publish PARTICIPANT_LEFT to NATS"
        );
    }

    // ==========================================================================
    // TEST: Non-observer Disconnect publishes PARTICIPANT_LEFT after grace period
    // ==========================================================================
    // When a real participant disconnects, the server defers the PARTICIPANT_LEFT
    // broadcast by RECONNECT_GRACE_PERIOD. If no reconnection occurs, the event
    // is published after the grace period expires. This test uses
    // ExecutePendingDeparture directly to avoid waiting for the full grace period.
    #[actix_rt::test]
    #[serial]
    async fn test_non_observer_disconnect_publishes_event() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 2004u64;
        let room = "test-room-non-observer-disconnect";

        // Register and join as real participant
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "real-dc@example.com".to_string(),
                display_name: "real-dc@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed");
        assert!(result.is_ok(), "Non-observer JoinRoom should succeed");

        // Activate the connection so the state is Active before disconnect
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");

        // Wait for session setup
        sleep(Duration::from_millis(300)).await;

        // Subscribe to system subject to watch for any meeting events
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let meeting_event_received = Arc::new(AtomicBool::new(false));
        let flag = meeting_event_received.clone();
        let mut sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");

        // Use a longer timeout to accommodate the reconnect grace period.
        // The NATS subscriber waits up to 6s (grace period is 2s + buffer).
        tokio::spawn(async move {
            use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
            use videocall_types::protos::meeting_packet::MeetingPacket;

            while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(6), sub.next()).await
            {
                if let Ok(wrapper) =
                    <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
                {
                    if let Ok(inner) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                        // Accept any meeting lifecycle event (PARTICIPANT_LEFT or MEETING_ENDED)
                        // depending on how end_session categorizes this session
                        if inner.event_type == MeetingEventType::PARTICIPANT_LEFT.into()
                            || inner.event_type == MeetingEventType::MEETING_ENDED.into()
                        {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Disconnect as non-observer — the departure is deferred by
        // RECONNECT_GRACE_PERIOD (3s). The PARTICIPANT_LEFT event will
        // not be published until the grace period expires.
        chat_server
            .send(Disconnect {
                session: session_id,
                room: room.to_string(),
                user_id: "real-dc@example.com".to_string(),
                display_name: "real-dc@example.com".to_string(),
                is_guest: false,
                observer: false,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Disconnect should succeed");

        // Wait for the grace period to expire plus some buffer.
        // RECONNECT_GRACE_PERIOD is 3s, we wait 4s to give the deferred
        // execution and NATS publish time to complete.
        sleep(Duration::from_secs(4)).await;

        // The non-observer path should have attempted to publish via the full
        // end_session flow after the grace period expired.
        assert!(
            meeting_event_received.load(Ordering::Relaxed),
            "Non-observer disconnect should publish a meeting event after grace period \
             (PARTICIPANT_LEFT or MEETING_ENDED)"
        );
    }

    // ==========================================================================
    // TEST: Observer JoinRoom succeeds and session is tracked
    // ==========================================================================
    // Verify that observer sessions are accepted and registered just like normal
    // sessions - the only difference is in event publishing behavior.
    #[actix_rt::test]
    #[serial]
    async fn test_observer_join_room_succeeds() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 2005u64;

        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Join as observer - should succeed (same as non-observer)
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-observer-ok".to_string(),
                user_id: "observer@example.com".to_string(),
                display_name: "observer@example.com".to_string(),
                is_guest: false,
                observer: true,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result.is_ok(),
            "Observer JoinRoom should succeed, got: {result:?}"
        );

        // Joining again with same session should return Ok (already in active_subs)
        let result2 = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-observer-ok".to_string(),
                user_id: "observer@example.com".to_string(),
                display_name: "observer@example.com".to_string(),
                is_guest: false,
                observer: true,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result2.is_ok(),
            "Second observer JoinRoom should return Ok (already active)"
        );
    }

    // ======================================================================
    // Test helper messages for inspecting ChatServer internal state
    // ======================================================================

    #[derive(ActixMessage)]
    #[rtype(result = "Result<ConnectionState, ()>")]
    struct GetConnectionState {
        session: SessionId,
    }

    impl Handler<GetConnectionState> for ChatServer {
        type Result = Result<ConnectionState, ()>;

        fn handle(&mut self, msg: GetConnectionState, _ctx: &mut Self::Context) -> Self::Result {
            Ok(self
                .connection_states
                .get(&msg.session)
                .copied()
                .unwrap_or(ConnectionState::Testing))
        }
    }

    // ==========================================================================
    // Unit tests for `handle_msg` observer filtering
    // ==========================================================================
    //
    // These tests exercise the closure returned by `handle_msg` in isolation.
    // They do NOT require a NATS connection or `#[serial]` — they only need an
    // actix runtime to start the RecordingSession actor.

    /// Actor that records how many `Message`s it receives.
    struct RecordingSession {
        count: Arc<AtomicUsize>,
    }

    impl Actor for RecordingSession {
        type Context = actix::Context<Self>;
    }

    impl Handler<Message> for RecordingSession {
        type Result = ();
        fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Build a minimal `async_nats::Message` with the given subject and payload.
    fn make_nats_message(subject: &str, payload: Vec<u8>) -> async_nats::Message {
        async_nats::Message {
            subject: subject.into(),
            payload: payload.into(),
            reply: None,
            headers: None,
            status: None,
            description: None,
            length: 0,
        }
    }

    /// Serialize a `PacketWrapper` with the given `PacketType`.
    fn make_packet_bytes(packet_type: PacketType) -> Vec<u8> {
        let mut pw = PacketWrapper::new();
        pw.packet_type = packet_type.into();
        pw.user_id = b"test-user".to_vec();
        pw.write_to_bytes()
            .expect("PacketWrapper serialization should succeed")
    }

    /// Serialize a `PacketWrapper` with an explicit `session_id`. Used by the
    /// post-reconnect self-skip tests to reproduce the leak where the NATS
    /// subject differs from the receiver's current session but the embedded
    /// `session_id` still belongs to this connection.
    fn make_packet_bytes_with_session(packet_type: PacketType, session_id: u64) -> Vec<u8> {
        let mut pw = PacketWrapper::new();
        pw.packet_type = packet_type.into();
        pw.user_id = b"test-user".to_vec();
        pw.session_id = session_id;
        pw.write_to_bytes()
            .expect("PacketWrapper serialization should succeed")
    }

    #[actix_rt::test]
    async fn test_handle_msg_observer_drops_media_packet() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "room1".to_string(),
            9001,
            true, // observer
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.room1.other_session",
            make_packet_bytes(PacketType::MEDIA),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "Observer must NOT receive MEDIA packets"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_observer_drops_aes_key_packet() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "room2".to_string(),
            9002,
            true, // observer
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.room2.other_session",
            make_packet_bytes(PacketType::AES_KEY),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "Observer must NOT receive AES_KEY packets"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_observer_drops_rsa_pub_key_packet() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "room3".to_string(),
            9003,
            true, // observer
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.room3.other_session",
            make_packet_bytes(PacketType::RSA_PUB_KEY),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "Observer must NOT receive RSA_PUB_KEY packets"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_observer_allows_meeting_packet() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "room4".to_string(),
            9004,
            true, // observer
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.room4.other_session",
            make_packet_bytes(PacketType::MEETING),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "Observer MUST receive MEETING packets"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_observer_drops_unparseable_packet() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "room5".to_string(),
            9005,
            true, // observer
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        // Send garbage bytes that cannot be parsed as a PacketWrapper.
        let nats_msg = make_nats_message(
            "room.room5.other_session",
            vec![0xFF, 0xFE, 0xFD, 0x00, 0x01],
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "Observer must NOT receive unparseable packets (fail-closed)"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_observer_allows_session_assigned_packet() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "room7".to_string(),
            9007,
            true, // observer
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.room7.other_session",
            make_packet_bytes(PacketType::SESSION_ASSIGNED),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "Observer MUST receive SESSION_ASSIGNED packets"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_observer_drops_connection_packet() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "room8".to_string(),
            9008,
            true, // observer
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.room8.other_session",
            make_packet_bytes(PacketType::CONNECTION),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "Observer must NOT receive CONNECTION packets (not in allowlist)"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_non_observer_forwards_media_packet() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "room6".to_string(),
            9006,
            false, // NOT an observer
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.room6.other_session",
            make_packet_bytes(PacketType::MEDIA),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "Non-observer MUST receive MEDIA packets"
        );
    }

    // ======================================================================
    // Self-skip filter — uniform across packet types (issue: 2026-05-08
    // production meeting saw 5224 self-DIAGNOSTICS leak back to the
    // reporter despite the subject-only filter being in place).
    //
    // Self-skip fires when EITHER the NATS subject matches the receiver's
    // own publish subject OR the embedded `PacketWrapper.session_id` matches
    // the receiver's current session. CONGESTION is the only carve-out.
    // ======================================================================

    #[actix_rt::test]
    async fn test_handle_msg_skips_self_diagnostics_via_subject() {
        // The simple in-actor echo: NATS subject for THIS receiver's own
        // publishes lands back on its subscription. DIAGNOSTICS is a
        // non-CONGESTION packet type and must be filtered uniformly with
        // MEDIA — the leak that triggered this test was DIAGNOSTICS-specific.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "selfskip-room".to_string(),
            7777,
            false, // not an observer
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.selfskip-room.7777",
            make_packet_bytes(PacketType::DIAGNOSTICS),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "Self-published DIAGNOSTICS on receiver's own subject must be skipped"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_skips_self_diagnostics_via_inner_session_id() {
        // Post-reconnect leak window: the publish subject differs from the
        // receiver's current session (e.g., a stale subscription survives a
        // reconnect briefly), but the embedded `session_id` still belongs to
        // this connection. The subject-only filter would let this pass; the
        // inner-session-id check closes the leak.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "reconnect-room".to_string(),
            8888,
            false,
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        // Subject points at a DIFFERENT session id; payload session_id is
        // ours. Pre-fix, this packet was forwarded.
        let nats_msg = make_nats_message(
            "room.reconnect-room.999999",
            make_packet_bytes_with_session(PacketType::DIAGNOSTICS, 8888),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "Self-DIAGNOSTICS identified by inner session_id must be skipped \
             even when the NATS subject points at a different session"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_congestion_passes_self_filter_via_subject() {
        // CONGESTION carve-out: a relay-authored, self-addressed CONGESTION
        // packet rides the throttled sender's own subject so the sender can
        // step down its quality tier; the subject-self match must NOT block
        // it. (Historically the relay AUTHORED that packet on the
        // receiver-downlink-overflow path; #1219 removed that sender-keyed
        // emit. The carve-out itself stays — it still must pass any
        // legitimately relay-authored CONGESTION through to the target.)
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "congestion-room".to_string(),
            5555,
            false,
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.congestion-room.5555",
            make_packet_bytes(PacketType::CONGESTION),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "CONGESTION must pass the subject-self filter (existing behaviour)"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_congestion_passes_self_filter_via_inner_session_id() {
        // CONGESTION carve-out applied to the new inner-session check too.
        // Legacy relay-authored CONGESTION stamped the throttled sender's
        // session into the packet. The inner-session self-skip must NOT
        // suppress CONGESTION just because the inner session_id matches.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "congestion-inner-room".to_string(),
            6666,
            false,
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.congestion-inner-room.99",
            make_packet_bytes_with_session(PacketType::CONGESTION, 6666),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "CONGESTION must pass the inner-session-id self-filter so the \
             throttle signal still reaches the sender after a reconnect"
        );
    }

    /// #1220 — a NON-target receiver must NOT have CONGESTION forwarded to its
    /// transport. CONGESTION is published on the TARGET sender's own subject
    /// (`room.{room}.{sender_sid}`); the NATS room wildcard delivers it to every
    /// session, but only the targeted sender should actually receive it on its
    /// transport. A bystander session (different subject, different inner
    /// session_id) must drop it at the relay AND bump
    /// `relay_congestion_filtered_total`.
    ///
    /// MUTATION PROOF: reverting #1220 (removing the
    /// `if is_congestion && !subject_self && !inner_session_self { return }`
    /// filter) forwards the packet to the bystander → `count` becomes 1 and the
    /// `== 0` assert FAILS (and the metric-delta assert also fails, since the
    /// filter never ran).
    #[actix_rt::test]
    async fn test_handle_msg_congestion_dropped_for_non_target_receiver() {
        let room = "congestion-nontarget-room-1220";
        let before = RELAY_CONGESTION_FILTERED_TOTAL
            .with_label_values(&[room])
            .get();

        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        // This receiver is session 7777. The CONGESTION targets sender 5555
        // (published on `room.{room}.5555` with inner session_id 5555).
        let handler = handle_msg(
            actor.recipient(),
            room.to_string(),
            7777, // bystander receiver — NOT the target
            false,
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.congestion-nontarget-room-1220.5555",
            make_packet_bytes_with_session(PacketType::CONGESTION, 5555),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "#1220: CONGESTION targeting sender 5555 must NOT be forwarded to \
             bystander receiver 7777"
        );

        let after = RELAY_CONGESTION_FILTERED_TOTAL
            .with_label_values(&[room])
            .get();
        assert_eq!(
            after - before,
            1.0,
            "#1220: dropping a non-target CONGESTION must increment \
             relay_congestion_filtered_total exactly once"
        );

        // Leave no residual series for the #996 GC guard / other tests.
        crate::metrics::forget_room_metrics(room);
    }

    #[actix_rt::test]
    async fn test_handle_msg_layer_hint_passes_self_filter_via_subject() {
        // #1108 delivery-gap carve-out. `emit_layer_hint` publishes the
        // relay-authored LAYER_HINT onto the publisher's OWN self-subject
        // (`room.{room}.{publisher}`) so the publisher can cap its encoded
        // simulcast ladder. The subject-self match must NOT block it — exactly
        // like CONGESTION. Before the `&& !is_layer_hint` carve-out this packet
        // was dropped as a self-echo, leaving the #1108 publish-side suppression
        // inert.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "layerhint-room".to_string(),
            5151,
            false,
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        // Subject == the receiver's own self-subject, mirroring the real
        // `emit_layer_hint` publish target (`room.{room}.{publisher}`).
        let nats_msg = make_nats_message(
            "room.layerhint-room.5151",
            make_packet_bytes(PacketType::LAYER_HINT),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "LAYER_HINT must pass the subject-self filter so the relay-authored \
             hint reaches the publisher (#1108 delivery gap)"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_layer_hint_passes_self_filter_via_inner_session_id() {
        // #1108 carve-out applied to the inner-session check too.
        // `emit_layer_hint` stamps `wrapper.session_id = publisher` (the
        // publisher's own session) onto the LAYER_HINT packet. The
        // inner-session self-skip must NOT suppress it just because the inner
        // session_id matches — otherwise a post-reconnect window (stale
        // subscription whose subject differs from the current session) would
        // swallow the hint even though the embedded session_id targets us.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "layerhint-inner-room".to_string(),
            6161,
            false,
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        // Subject points at a DIFFERENT session id; embedded session_id is ours
        // (the publisher the relay addressed). Must still be delivered.
        let nats_msg = make_nats_message(
            "room.layerhint-inner-room.424242",
            make_packet_bytes_with_session(PacketType::LAYER_HINT, 6161),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "LAYER_HINT must pass the inner-session-id self-filter so the \
             relay-authored hint still reaches the publisher after a reconnect"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_self_media_still_dropped_alongside_layer_hint() {
        // Regression guard paired with the two LAYER_HINT carve-out tests
        // above: the carve-out must be TYPE-scoped. A self-addressed plain
        // MEDIA packet — same self-subject AND same embedded session_id as the
        // LAYER_HINT cases — must STILL be dropped as a self-echo. If this ever
        // forwards, the carve-out has leaked into ordinary media traffic
        // (re-opening the 2026-05-08 self-echo leak).
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "layerhint-room".to_string(),
            5151,
            false,
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        // Identical addressing to test_handle_msg_layer_hint_passes_self_filter_*
        // (self-subject + own embedded session_id) but packet_type == MEDIA.
        let nats_msg = make_nats_message(
            "room.layerhint-room.5151",
            make_packet_bytes_with_session(PacketType::MEDIA, 5151),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "Self-addressed MEDIA must STILL be dropped — the LAYER_HINT \
             carve-out must not leak into ordinary media traffic"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_skips_self_media_via_inner_session_id() {
        // Apply the same uniform check to MEDIA. Pre-fix, MEDIA was filtered
        // by subject only; this lock-in test guarantees that future churn in
        // the filter cannot regress MEDIA back to subject-only matching.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "media-self-room".to_string(),
            4242,
            false,
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.media-self-room.999999",
            make_packet_bytes_with_session(PacketType::MEDIA, 4242),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "Self-MEDIA identified by inner session_id must be skipped \
             (parity with self-DIAGNOSTICS)"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_zero_inner_session_id_does_not_match() {
        // Defensive: `session_id == 0` is the unstamped/default case
        // (`Handler<ClientMessage>` stamps it to the sender's session before
        // publishing, but a malformed/raw packet may land with 0). A 0 must
        // NOT match an arbitrary receiver session — that would erroneously
        // suppress every packet from any anonymous publish path.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "zero-session-room".to_string(),
            0,
            false,
            "recv-user".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.zero-session-room.peer",
            make_packet_bytes_with_session(PacketType::MEDIA, 0),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "Zero session_id must NOT trigger the inner-session self-filter"
        );
    }

    /// Build a `PacketWrapper` carrying a serialized `PeerEvent` whose
    /// `target_peer_id` is set to `target`. Used by the PEER_EVENT routing
    /// tests below.
    fn make_peer_event_packet_bytes(target: &str) -> Vec<u8> {
        let mut pe = videocall_types::protos::peer_event::PeerEvent::new();
        pe.source_peer_id = b"some-source".to_vec();
        pe.target_peer_id = target.as_bytes().to_vec();
        pe.event_type = videocall_types::PEER_EVENT_SCREEN_DECODE_STARTED.to_string();
        let inner = pe
            .write_to_bytes()
            .expect("PeerEvent serialization should succeed");

        let mut pw = PacketWrapper::new();
        pw.packet_type = PacketType::PEER_EVENT.into();
        pw.user_id = b"some-source".to_vec();
        pw.data = inner;
        pw.write_to_bytes()
            .expect("PacketWrapper serialization should succeed")
    }

    #[actix_rt::test]
    async fn test_handle_msg_peer_event_delivered_to_target() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "peer-event-room".to_string(),
            1111,
            false,
            "alice".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.peer-event-room.peer",
            make_peer_event_packet_bytes("alice"),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "PEER_EVENT addressed to receiver MUST be delivered"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_peer_event_dropped_for_non_target() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "peer-event-room".to_string(),
            2222,
            false,
            "bob".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.peer-event-room.peer",
            make_peer_event_packet_bytes("alice"),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "PEER_EVENT addressed to another peer MUST be dropped"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_peer_event_observer_drops() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "peer-event-room".to_string(),
            3333,
            true,
            "alice".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.peer-event-room.peer",
            make_peer_event_packet_bytes("alice"),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "PEER_EVENT must never reach observer sessions, even when targeted"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_peer_event_unparseable_inner_dropped() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "peer-event-room".to_string(),
            4444,
            false,
            "alice".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let mut pw = PacketWrapper::new();
        pw.packet_type = PacketType::PEER_EVENT.into();
        pw.user_id = b"some-source".to_vec();
        pw.data = vec![0xff, 0xfe, 0xfd];
        let nats_msg = make_nats_message(
            "room.peer-event-room.peer",
            pw.write_to_bytes()
                .expect("PacketWrapper serialization should succeed"),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "PEER_EVENT with unparseable inner payload MUST be dropped"
        );
    }

    // ======================================================================
    // MEETING unicast reply filter (cross-server peer discovery)
    // ======================================================================
    //
    // A PARTICIPANT_JOINED published in reply to a PARTICIPANT_LIST_REQUEST is
    // addressed to ONE requester by publishing on that requester's per-session
    // subject (`room.{room}.{N}`). Every session receives it via the room
    // wildcard, so `handle_msg` must forward it ONLY to the targeted session N
    // and drop it for everyone else (a presence info-leak boundary). Broadcast
    // MEETING events use `room.{room}.system` and must still reach everyone.

    /// A MEETING packet on the receiver's OWN per-session subject is the
    /// targeted reply and must be forwarded. (`make_packet_bytes` leaves the
    /// inner session_id at 0, so the self-echo `inner_session_self` guard does
    /// not fire — the packet is delivered, not dropped.)
    #[actix_rt::test]
    async fn test_handle_msg_meeting_targeted_reply_forwarded_to_requester() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "disc-room".to_string(),
            7001,
            false,
            "requester".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.disc-room.7001",
            make_packet_bytes(PacketType::MEETING),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "targeted MEETING reply on our own subject MUST be forwarded"
        );
    }

    /// A MEETING packet on ANOTHER session's per-session subject is a reply
    /// addressed to a different requester and MUST be dropped for us — this is
    /// the presence info-leak boundary.
    #[actix_rt::test]
    async fn test_handle_msg_meeting_targeted_reply_dropped_for_non_requester() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "disc-room".to_string(),
            7001,
            false,
            "bystander".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        // Reply addressed to session 9999, not us (7001).
        let nats_msg = make_nats_message(
            "room.disc-room.9999",
            make_packet_bytes(PacketType::MEETING),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "targeted MEETING reply on another session's subject MUST be dropped"
        );
    }

    /// A broadcast MEETING event (PARTICIPANT_JOINED at activation,
    /// PARTICIPANT_LEFT, etc.) uses the `.system` subject and MUST reach every
    /// session, including ones that are not the targeted requester.
    #[actix_rt::test]
    async fn test_handle_msg_meeting_system_broadcast_forwarded() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "disc-room".to_string(),
            7001,
            false,
            "peer".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.disc-room.system",
            make_packet_bytes(PacketType::MEETING),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "broadcast MEETING event on .system subject MUST be forwarded to all"
        );
    }

    // ======================================================================
    // Viewport-aware VIDEO filtering (HCL issue #988)
    // ======================================================================
    //
    // These exercise the cleartext-`media_kind` drop check in `handle_msg`
    // and the `try_intercept_viewport` control-packet interceptor in
    // isolation. None of them require NATS.

    use videocall_types::protos::packet_wrapper::packet_wrapper::MediaKind;
    use videocall_types::protos::viewport_packet::ViewportPacket;

    /// Build a MEDIA `PacketWrapper` with an explicit cleartext `media_kind`
    /// and source `session_id`, serialized to bytes.
    fn make_media_packet_bytes(media_kind: MediaKind, source_session: u64) -> Vec<u8> {
        let mut pw = PacketWrapper::new();
        pw.packet_type = PacketType::MEDIA.into();
        pw.user_id = b"sender".to_vec();
        pw.session_id = source_session;
        pw.media_kind = media_kind.into();
        pw.write_to_bytes()
            .expect("PacketWrapper serialization should succeed")
    }

    /// Build a `DesiredStreams` pre-populated with the given session_ids.
    fn desired_streams_with(ids: &[u64]) -> DesiredStreams {
        Arc::new(RwLock::new(ViewportState {
            ids: ids.iter().copied().collect(),
            last_update: None,
        }))
    }

    /// An empty `LayerPrefs` — the no-op default used by every handle_msg test
    /// that does not exercise the #989 layer filter. With an empty map the
    /// layer filter forwards everything (byte-identical to pre-#989 behaviour).
    fn empty_layer_prefs() -> LayerPrefs {
        LayerPrefs::default()
    }

    /// Build a `LayerPrefs` pre-populated with the given (source_session,
    /// desired_layer) entries, keyed as VIDEO (the pre-Phase-3 default). Sets
    /// the lock-free `non_empty` hint to match the map so the forwarding hot
    /// path's fast-path check is exercised faithfully.
    fn layer_prefs_with(entries: &[(u64, u32)]) -> LayerPrefs {
        let kinded: Vec<(u64, i32, u32)> = entries
            .iter()
            .map(|&(s, l)| (s, 1 /* VIDEO */, l))
            .collect();
        layer_prefs_with_kinds(&kinded)
    }

    /// Build a `LayerPrefs` keyed by (source_session, media_kind, desired_layer)
    /// (issue #989, Phase 3). `media_kind` is the normalized wire discriminant
    /// (VIDEO=1, AUDIO=2, SCREEN=3).
    fn layer_prefs_with_kinds(entries: &[(u64, i32, u32)]) -> LayerPrefs {
        let layers: HashMap<(u64, i32), u32> =
            entries.iter().map(|&(s, k, l)| ((s, k), l)).collect();
        let non_empty = !layers.is_empty();
        LayerPrefs {
            state: Arc::new(RwLock::new(LayerPrefsState {
                layers,
                last_update: None,
            })),
            non_empty: Arc::new(std::sync::atomic::AtomicBool::new(non_empty)),
        }
    }

    /// Build a MEDIA `PacketWrapper` with explicit cleartext `media_kind`,
    /// source `session_id`, and `simulcast_layer_id`, serialized to bytes.
    /// Used by the #989 layer-filter tests.
    fn make_media_packet_bytes_with_layer(
        media_kind: MediaKind,
        source_session: u64,
        layer: u32,
    ) -> Vec<u8> {
        let mut pw = PacketWrapper::new();
        pw.packet_type = PacketType::MEDIA.into();
        pw.user_id = b"sender".to_vec();
        pw.session_id = source_session;
        pw.media_kind = media_kind.into();
        pw.simulcast_layer_id = layer;
        pw.write_to_bytes()
            .expect("PacketWrapper serialization should succeed")
    }

    /// Build a LAYER_PREFERENCE `PacketWrapper` whose wire `session_id` is
    /// `owner` (ownership is decided by SUBJECT, not this field — `owner` is
    /// retained so tests can deliberately forge it to prove it is ignored)
    /// carrying the given (source_session, desired_layer) entries.
    fn make_layer_preference_packet_bytes(owner: u64, entries: &[(u64, u32)]) -> Vec<u8> {
        use videocall_types::protos::layer_preference_packet::layer_preference_packet::Entry;
        use videocall_types::protos::layer_preference_packet::LayerPreferencePacket;
        let inner = LayerPreferencePacket {
            entries: entries
                .iter()
                .map(|&(session_id, desired_layer)| Entry {
                    session_id,
                    desired_layer,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let mut pw = PacketWrapper::new();
        pw.packet_type = PacketType::LAYER_PREFERENCE.into();
        pw.session_id = owner;
        pw.data = inner
            .write_to_bytes()
            .expect("LayerPreferencePacket serialization should succeed");
        pw.write_to_bytes()
            .expect("PacketWrapper serialization should succeed")
    }

    /// Phase 3: build a LAYER_PREFERENCE carrying (source, EntryMediaKind, layer)
    /// entries so tests can exercise per-(source,kind) recording.
    fn make_layer_preference_packet_bytes_kinded(
        owner: u64,
        entries: &[(u64, i32, u32)],
    ) -> Vec<u8> {
        use videocall_types::protos::layer_preference_packet::layer_preference_packet::Entry;
        use videocall_types::protos::layer_preference_packet::LayerPreferencePacket;
        let inner = LayerPreferencePacket {
            entries: entries
                .iter()
                .map(|&(session_id, kind, desired_layer)| Entry {
                    session_id,
                    desired_layer,
                    media_kind: ::protobuf::EnumOrUnknown::from_i32(kind),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let mut pw = PacketWrapper::new();
        pw.packet_type = PacketType::LAYER_PREFERENCE.into();
        pw.session_id = owner;
        pw.data = inner
            .write_to_bytes()
            .expect("LayerPreferencePacket serialization should succeed");
        pw.write_to_bytes()
            .expect("PacketWrapper serialization should succeed")
    }

    /// Parse the payload of an `async_nats::Message` into an optional
    /// `PacketWrapper`, mirroring the single-parse the NATS loop performs.
    fn parse_pw(msg: &async_nats::Message) -> Option<PacketWrapper> {
        PacketWrapper::parse_from_bytes(&msg.payload).ok()
    }

    /// Build a VIEWPORT `PacketWrapper` whose wire `session_id` is `owner`
    /// (NOTE: ownership is decided by SUBJECT, not this field — `owner` is
    /// retained so tests can deliberately forge it to prove it is ignored)
    /// listing `visible` session_ids.
    fn make_viewport_packet_bytes(owner: u64, visible: &[u64]) -> Vec<u8> {
        let inner = ViewportPacket {
            session_ids: visible.to_vec(),
            ..Default::default()
        };
        let mut pw = PacketWrapper::new();
        pw.packet_type = PacketType::VIEWPORT.into();
        pw.session_id = owner;
        pw.data = inner
            .write_to_bytes()
            .expect("ViewportPacket serialization should succeed");
        pw.write_to_bytes()
            .expect("PacketWrapper serialization should succeed")
    }

    #[actix_rt::test]
    async fn test_handle_msg_drops_off_screen_video() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        // Receiver renders sessions {200, 300}; video from 999 is off-screen.
        let handler = handle_msg(
            actor.recipient(),
            "vp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            desired_streams_with(&[200, 300]),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.vp-room.999",
            make_media_packet_bytes(MediaKind::VIDEO, 999),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "VIDEO from a session NOT in the viewport set MUST be dropped"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_forged_media_session_id_does_not_bypass_filter() {
        // P1 regression (PR #994): the VIDEO source identity used for the
        // viewport containment test MUST come from the NATS subject, never the
        // forgeable payload `session_id`. Here the receiver renders {200, 300}.
        // A malicious publisher (real session 999, off-screen) forges the
        // payload `session_id` to 200 (a VISIBLE peer) hoping the filter sees
        // "200 is in the set → forward". Because the packet necessarily arrives
        // on the attacker's OWN subject `room.vp-room.999`, the subject-derived
        // source is 999 (not in the set), so it MUST still be DROPPED.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "vp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            desired_streams_with(&[200, 300]),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        // Forged payload session_id = 200 (visible), but arrives on 999's subject.
        let nats_msg = make_nats_message(
            "room.vp-room.999",
            make_media_packet_bytes(MediaKind::VIDEO, 200),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "forged payload session_id MUST NOT bypass the viewport filter; \
             subject-derived source (999) governs and is off-screen"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_unparseable_subject_source_fails_open() {
        // Defensive: if the subject's trailing token is not a u64 (should never
        // happen for normal media), the source is unknown and we FAIL OPEN
        // (forward) rather than drop. Receiver renders {200} but the source
        // cannot be derived from a non-numeric subject token.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "vp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            desired_streams_with(&[200]),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.vp-room.not-a-number",
            make_media_packet_bytes(MediaKind::VIDEO, 999),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "an unparseable subject-derived source MUST fail open (forward)"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_forwards_on_screen_video() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "vp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            desired_streams_with(&[200, 300]),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        // Subject-derived source = 200 (in the viewport set).
        let nats_msg = make_nats_message(
            "room.vp-room.200",
            make_media_packet_bytes(MediaKind::VIDEO, 200),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "VIDEO from a session IN the viewport set MUST be forwarded"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_empty_viewport_forwards_all_video() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        // Empty set = no viewport signal yet = fail-open.
        let handler = handle_msg(
            actor.recipient(),
            "vp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.vp-room.999",
            make_media_packet_bytes(MediaKind::VIDEO, 999),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "Empty viewport set MUST fail open (forward all VIDEO)"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_never_filters_audio() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        // Off-screen sender, but AUDIO must NEVER be filtered (security:
        // off-screen speakers must be heard).
        let handler = handle_msg(
            actor.recipient(),
            "vp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            desired_streams_with(&[200]),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.vp-room.999",
            make_media_packet_bytes(MediaKind::AUDIO, 999),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "AUDIO MUST NEVER be filtered by the viewport set"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_never_filters_screen() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        // SCREEN is not governed by the camera-tile viewport.
        let handler = handle_msg(
            actor.recipient(),
            "vp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            desired_streams_with(&[200]),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.vp-room.999",
            make_media_packet_bytes(MediaKind::SCREEN, 999),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "SCREEN MUST NOT be filtered by the camera-tile viewport"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_unspecified_media_kind_fails_open() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        // UNSPECIFIED (e.g. an older client) must fail open even with an
        // active viewport set and an off-screen source.
        let handler = handle_msg(
            actor.recipient(),
            "vp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            desired_streams_with(&[200]),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.vp-room.999",
            make_media_packet_bytes(MediaKind::MEDIA_KIND_UNSPECIFIED, 999),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "UNSPECIFIED media_kind MUST fail open (forward)"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_viewport_does_not_affect_observer_gate() {
        // The viewport filter runs AFTER the observer allowlist. An observer
        // must still be denied MEDIA regardless of viewport set contents
        // (the allowlist drops it first).
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "vp-room".to_string(),
            100,
            true, // observer
            "recv".to_string(),
            // Even a viewport set that "includes" the source must not let an
            // observer receive MEDIA.
            desired_streams_with(&[999]),
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.vp-room.999",
            make_media_packet_bytes(MediaKind::VIDEO, 999),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "Observer gate MUST take precedence; viewport set cannot grant MEDIA"
        );
    }

    // Ownership in `try_intercept_viewport` is decided by the SUBJECT the
    // packet arrived on. The receiver's own subject is `room.{room}.{session}`
    // with spaces replaced by `_` — these tests build the matching subject.
    fn self_subject_for(room: &str, session: u64) -> String {
        format!("room.{room}.{session}").replace(' ', "_")
    }

    #[test]
    fn test_intercept_viewport_records_own_set() {
        // A VIEWPORT arriving on the receiver's OWN subject updates the set.
        let desired = DesiredStreams::default();
        let self_subject = self_subject_for("r", 100);
        let msg = make_nats_message(&self_subject, make_viewport_packet_bytes(100, &[200, 300]));
        let parsed = parse_pw(&msg);
        let intercepted =
            try_intercept_viewport(&msg, parsed.as_ref(), &self_subject, 100, &desired, "r");
        assert!(intercepted, "VIEWPORT packet must be intercepted (dropped)");
        let st = desired.read().unwrap();
        assert_eq!(st.ids.len(), 2);
        assert!(st.ids.contains(&200) && st.ids.contains(&300));
    }

    #[test]
    fn test_intercept_viewport_excludes_self_session() {
        // The receiver's own session_id is never added to its set, so a
        // lone `[self]` viewport collapses to empty = fail-open.
        let desired = DesiredStreams::default();
        let self_subject = self_subject_for("r", 100);
        let msg = make_nats_message(&self_subject, make_viewport_packet_bytes(100, &[100]));
        let parsed = parse_pw(&msg);
        assert!(try_intercept_viewport(
            &msg,
            parsed.as_ref(),
            &self_subject,
            100,
            &desired,
            "r"
        ));
        assert!(
            desired.read().unwrap().ids.is_empty(),
            "self session_id must be filtered out of the viewport set"
        );
    }

    #[test]
    fn test_intercept_viewport_other_subject_does_not_mutate() {
        // A VIEWPORT that arrived on a DIFFERENT publisher's subject is
        // dropped but must NOT mutate this receiver's set.
        let desired = desired_streams_with(&[200]);
        let self_subject = self_subject_for("r", 100);
        // Packet arrived on session 555's subject, not ours (100).
        let msg = make_nats_message("room.r.555", make_viewport_packet_bytes(555, &[999]));
        let parsed = parse_pw(&msg);
        let intercepted =
            try_intercept_viewport(&msg, parsed.as_ref(), &self_subject, 100, &desired, "r");
        assert!(
            intercepted,
            "another session's VIEWPORT must still be intercepted (never re-broadcast)"
        );
        let st = desired.read().unwrap();
        assert_eq!(st.ids.len(), 1);
        assert!(
            st.ids.contains(&200),
            "receiver's own set must be untouched by another subject's VIEWPORT"
        );
    }

    #[test]
    fn test_intercept_viewport_forged_payload_session_id_does_not_mutate() {
        // HIGH/BLOCKING regression (HCL #988 security): an attacker forges the
        // PAYLOAD `session_id` to equal the victim's session (100), but the
        // packet necessarily arrives on the ATTACKER's subject (room.r.555),
        // not the victim's. Ownership is decided by subject, so the victim's
        // set MUST NOT be mutated. This is the vector the old payload-based
        // ownership check missed.
        let desired = desired_streams_with(&[200]);
        let self_subject = self_subject_for("r", 100); // victim's subject
                                                       // Forged payload session_id = 100 (victim) but arrives on 555's subject.
        let msg = make_nats_message("room.r.555", make_viewport_packet_bytes(100, &[999]));
        let parsed = parse_pw(&msg);
        let intercepted =
            try_intercept_viewport(&msg, parsed.as_ref(), &self_subject, 100, &desired, "r");
        assert!(
            intercepted,
            "forged cross-session VIEWPORT must still be intercepted (never re-broadcast)"
        );
        let st = desired.read().unwrap();
        assert_eq!(
            st.ids.len(),
            1,
            "victim's set MUST NOT be mutated by a forged payload session_id"
        );
        assert!(
            st.ids.contains(&200) && !st.ids.contains(&999),
            "attacker-controlled session_ids must never reach the victim's set"
        );
    }

    #[test]
    fn test_intercept_viewport_ignores_non_viewport() {
        // A non-VIEWPORT packet must fall through (return false) so the loop
        // hands it to handle_msg.
        let desired = DesiredStreams::default();
        let self_subject = self_subject_for("r", 100);
        let msg = make_nats_message(&self_subject, make_packet_bytes(PacketType::MEDIA));
        let parsed = parse_pw(&msg);
        assert!(
            !try_intercept_viewport(&msg, parsed.as_ref(), &self_subject, 100, &desired, "r"),
            "non-VIEWPORT packets must fall through to handle_msg"
        );
    }

    #[test]
    fn test_intercept_viewport_unparseable_falls_through() {
        // An unparseable payload (`parsed == None`) must fall through so
        // handle_msg's fail-closed logic governs it.
        let desired = DesiredStreams::default();
        let self_subject = self_subject_for("r", 100);
        let msg = make_nats_message(&self_subject, vec![0xff, 0xfe, 0xfd]);
        assert!(
            !try_intercept_viewport(&msg, None, &self_subject, 100, &desired, "r"),
            "unparseable payloads must fall through to handle_msg"
        );
    }

    #[test]
    fn test_intercept_viewport_overwrites_previous_set() {
        // The latest VIEWPORT replaces (does not merge with) the prior set.
        let desired = desired_streams_with(&[1, 2, 3]);
        let self_subject = self_subject_for("r", 100);
        let msg = make_nats_message(&self_subject, make_viewport_packet_bytes(100, &[7]));
        let parsed = parse_pw(&msg);
        assert!(try_intercept_viewport(
            &msg,
            parsed.as_ref(),
            &self_subject,
            100,
            &desired,
            "r"
        ));
        let st = desired.read().unwrap();
        assert_eq!(st.ids.len(), 1);
        assert!(st.ids.contains(&7) && !st.ids.contains(&1));
    }

    #[test]
    fn test_intercept_viewport_caps_session_ids() {
        // A VIEWPORT with more than VIEWPORT_MAX_SESSION_IDS entries is
        // truncated to the cap (DoS bound).
        let desired = DesiredStreams::default();
        let self_subject = self_subject_for("r", 100);
        // Oversized list of distinct session_ids (none == receiver 100).
        let big: Vec<u64> = (1000..1000 + (VIEWPORT_MAX_SESSION_IDS as u64) + 50).collect();
        let msg = make_nats_message(&self_subject, make_viewport_packet_bytes(100, &big));
        let parsed = parse_pw(&msg);
        assert!(try_intercept_viewport(
            &msg,
            parsed.as_ref(),
            &self_subject,
            100,
            &desired,
            "r"
        ));
        let st = desired.read().unwrap();
        assert_eq!(
            st.ids.len(),
            VIEWPORT_MAX_SESSION_IDS,
            "accepted session_ids must be capped at VIEWPORT_MAX_SESSION_IDS"
        );
    }

    #[test]
    fn test_intercept_viewport_rate_limited() {
        // A second VIEWPORT arriving immediately after an accepted one is
        // consumed (dropped, returns true) but must NOT mutate the set.
        let desired = DesiredStreams::default();
        let self_subject = self_subject_for("r", 100);

        let msg1 = make_nats_message(&self_subject, make_viewport_packet_bytes(100, &[200, 300]));
        let parsed1 = parse_pw(&msg1);
        assert!(try_intercept_viewport(
            &msg1,
            parsed1.as_ref(),
            &self_subject,
            100,
            &desired,
            "r"
        ));
        assert_eq!(desired.read().unwrap().ids.len(), 2);

        // Immediate second update (well within VIEWPORT_MIN_UPDATE_INTERVAL).
        let msg2 = make_nats_message(&self_subject, make_viewport_packet_bytes(100, &[999]));
        let parsed2 = parse_pw(&msg2);
        assert!(
            try_intercept_viewport(&msg2, parsed2.as_ref(), &self_subject, 100, &desired, "r"),
            "rate-limited VIEWPORT must still be intercepted (never re-broadcast)"
        );
        let st = desired.read().unwrap();
        assert_eq!(
            st.ids.len(),
            2,
            "a too-soon VIEWPORT update must be ignored (set unchanged)"
        );
        assert!(
            st.ids.contains(&200) && !st.ids.contains(&999),
            "rate-limited update must not replace the set"
        );
    }

    #[test]
    fn test_intercept_viewport_accepts_update_after_interval() {
        // After VIEWPORT_MIN_UPDATE_INTERVAL has elapsed, a new VIEWPORT is
        // accepted. Simulate elapsed time by rewinding `last_update`.
        let desired = desired_streams_with(&[200, 300]);
        if let Ok(mut guard) = desired.write() {
            guard.last_update =
                Some(std::time::Instant::now() - (VIEWPORT_MIN_UPDATE_INTERVAL * 2));
        }
        let self_subject = self_subject_for("r", 100);
        let msg = make_nats_message(&self_subject, make_viewport_packet_bytes(100, &[7]));
        let parsed = parse_pw(&msg);
        assert!(try_intercept_viewport(
            &msg,
            parsed.as_ref(),
            &self_subject,
            100,
            &desired,
            "r"
        ));
        let st = desired.read().unwrap();
        assert_eq!(st.ids.len(), 1);
        assert!(
            st.ids.contains(&7),
            "an update after the min interval must be accepted"
        );
    }

    /// Returns all RoomMemberInfo entries for a given room.
    #[derive(ActixMessage)]
    #[rtype(result = "Vec<RoomMemberInfo>")]
    struct GetRoomMembers {
        room: String,
    }

    impl Handler<GetRoomMembers> for ChatServer {
        type Result = MessageResult<GetRoomMembers>;

        fn handle(&mut self, msg: GetRoomMembers, _ctx: &mut Self::Context) -> Self::Result {
            MessageResult(
                self.room_members
                    .get(&msg.room)
                    .cloned()
                    .unwrap_or_default(),
            )
        }
    }

    /// Check whether a session is in the suppress_join_broadcast set.
    #[derive(ActixMessage)]
    #[rtype(result = "bool")]
    struct IsSuppressedJoinBroadcast {
        session: SessionId,
    }

    impl Handler<IsSuppressedJoinBroadcast> for ChatServer {
        type Result = bool;

        fn handle(
            &mut self,
            msg: IsSuppressedJoinBroadcast,
            _ctx: &mut Self::Context,
        ) -> Self::Result {
            self.suppress_join_broadcast.contains(&msg.session)
        }
    }

    /// Check whether a session is registered in the sessions map.
    #[derive(ActixMessage)]
    #[rtype(result = "bool")]
    struct HasSession {
        session: SessionId,
    }

    impl Handler<HasSession> for ChatServer {
        type Result = bool;

        fn handle(&mut self, msg: HasSession, _ctx: &mut Self::Context) -> Self::Result {
            self.sessions.contains_key(&msg.session)
        }
    }

    /// Check whether a session has an active NATS subscription.
    #[derive(ActixMessage)]
    #[rtype(result = "bool")]
    struct HasActiveSub {
        session: SessionId,
    }

    impl Handler<HasActiveSub> for ChatServer {
        type Result = bool;

        fn handle(&mut self, msg: HasActiveSub, _ctx: &mut Self::Context) -> Self::Result {
            self.active_subs.contains_key(&msg.session)
        }
    }

    // ======================================================================
    // Helper: connect + join a session, returning Ok or panicking
    // ======================================================================
    async fn connect_and_join(
        chat_server: &actix::Addr<ChatServer>,
        session_id: SessionId,
        room: &str,
        user_id: &str,
        addr: actix::Recipient<Message>,
        instance_id: Option<String>,
    ) {
        chat_server
            .send(Connect {
                id: session_id,
                addr,
            })
            .await
            .expect("Connect should succeed");

        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: user_id.to_string(),
                display_name: user_id.to_string(),
                is_guest: false,
                observer: false,
                instance_id,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result.is_ok(),
            "JoinRoom should succeed for session {session_id}, got: {result:?}"
        );
    }

    // ======================================================================
    // Session eviction via instance_id — integration tests
    // ======================================================================

    // ------------------------------------------------------------------
    // TEST 1: Basic eviction — Session B evicts Session A (same instance_id)
    // ------------------------------------------------------------------
    // Session A joins a room with instance_id="inst-1". Session B joins the
    // same room with the same user_id and instance_id="inst-1". Verify:
    //   - Session A is removed from room_members
    //   - Session B is present in room_members
    //   - PARTICIPANT_JOINED is suppressed for Session B
    #[actix_rt::test]
    #[serial]
    async fn test_eviction_basic_same_user() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let room = "eviction-basic";
        let user_id = "alice@example.com";
        let instance_id = "inst-alice-1".to_string();
        let session_a: SessionId = 5001;
        let session_b: SessionId = 5002;

        // Session A joins with instance_id
        let dummy_a = DummySession.start();
        connect_and_join(
            &chat_server,
            session_a,
            room,
            user_id,
            dummy_a.recipient(),
            Some(instance_id.clone()),
        )
        .await;

        // Allow the async JoinRoom task to start the NATS subscription
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Activate session A so it is registered in instance_index. Eviction
        // during B's ActivateConnection needs the forward mapping to find A.
        chat_server
            .send(ActivateConnection { session: session_a })
            .await
            .expect("ActivateConnection A should succeed");

        // Verify Session A is in room_members
        let members = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(members.len(), 1, "Room should have exactly 1 member");
        assert_eq!(
            members[0].session, session_a,
            "Session A should be in the room"
        );

        // Session B joins with same instance_id (same user reconnecting)
        let dummy_b = DummySession.start();
        connect_and_join(
            &chat_server,
            session_b,
            room,
            user_id,
            dummy_b.recipient(),
            Some(instance_id),
        )
        .await;

        // Allow the async JoinRoom task to complete
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Activate session B — this triggers eviction of session A.
        chat_server
            .send(ActivateConnection { session: session_b })
            .await
            .expect("ActivateConnection B should succeed");

        // Verify Session A is evicted and Session B is in room_members
        let members = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(
            members.len(),
            1,
            "Room should have exactly 1 member after eviction"
        );
        assert_eq!(
            members[0].session, session_b,
            "Session B should be the sole member"
        );

        // Verify Session A's internal state was cleaned up
        let has_session_a = chat_server
            .send(HasSession { session: session_a })
            .await
            .expect("HasSession should succeed");
        assert!(
            !has_session_a,
            "Session A should be removed from sessions map"
        );

        let has_sub_a = chat_server
            .send(HasActiveSub { session: session_a })
            .await
            .expect("HasActiveSub should succeed");
        assert!(
            !has_sub_a,
            "Session A should have no active NATS subscription"
        );

        // PARTICIPANT_JOINED suppression is consumed inside ActivateConnection
        // (the flag is removed when the broadcast is correctly skipped).
    }

    // ------------------------------------------------------------------
    // TEST 2: Different user_id — no eviction
    // ------------------------------------------------------------------
    // Session A joins (user "alice") with instance_id="inst-1". Session B
    // joins (user "bob") with the same instance_id="inst-1". Session A must
    // NOT be evicted because the user_id does not match.
    #[actix_rt::test]
    #[serial]
    async fn test_eviction_different_user_no_eviction() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let room = "eviction-diff-user";
        let instance_id = "inst-shared".to_string();
        let session_a: SessionId = 6001;
        let session_b: SessionId = 6002;

        // Session A joins as "alice" with instance_id
        let dummy_a = DummySession.start();
        connect_and_join(
            &chat_server,
            session_a,
            room,
            "alice@example.com",
            dummy_a.recipient(),
            Some(instance_id.clone()),
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Session B joins as "bob" with the same instance_id
        // (should NOT evict because user_id differs)
        let dummy_b = DummySession.start();
        connect_and_join(
            &chat_server,
            session_b,
            room,
            "bob@example.com",
            dummy_b.recipient(),
            Some(instance_id),
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Both sessions should be in room_members — alice was NOT evicted
        let members = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(
            members.len(),
            2,
            "Room should have 2 members (no eviction across different user_ids)"
        );

        let session_ids: Vec<SessionId> = members.iter().map(|m| m.session).collect();
        assert!(
            session_ids.contains(&session_a),
            "Session A (alice) should still be in room_members"
        );
        assert!(
            session_ids.contains(&session_b),
            "Session B (bob) should be in room_members"
        );

        // Session A should still be registered (not evicted)
        let has_session_a = chat_server
            .send(HasSession { session: session_a })
            .await
            .expect("HasSession should succeed");
        assert!(
            has_session_a,
            "Session A should NOT be removed when user_id does not match"
        );

        // Session B should NOT have suppress_join_broadcast (normal join, not eviction)
        let suppressed = chat_server
            .send(IsSuppressedJoinBroadcast { session: session_b })
            .await
            .expect("IsSuppressedJoinBroadcast should succeed");
        assert!(
            !suppressed,
            "Session B (different user) should NOT suppress PARTICIPANT_JOINED"
        );
    }

    // ------------------------------------------------------------------
    // TEST 3: No instance_id — normal join flow
    // ------------------------------------------------------------------
    // Session joins with instance_id = None. Verify it follows
    // the normal PARTICIPANT_JOINED flow (not suppressed).
    #[actix_rt::test]
    #[serial]
    async fn test_no_instance_id_normal_join() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let room = "eviction-no-prev";
        let session_id: SessionId = 7001;

        let dummy = DummySession.start();
        connect_and_join(
            &chat_server,
            session_id,
            room,
            "carol@example.com",
            dummy.recipient(),
            None,
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Session should be in room_members
        let members = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(members.len(), 1, "Room should have exactly 1 member");
        assert_eq!(
            members[0].session, session_id,
            "The session should be in room_members"
        );

        // PARTICIPANT_JOINED should NOT be suppressed (normal first join)
        let suppressed = chat_server
            .send(IsSuppressedJoinBroadcast {
                session: session_id,
            })
            .await
            .expect("IsSuppressedJoinBroadcast should succeed");
        assert!(
            !suppressed,
            "Normal join (no instance_id) should NOT suppress PARTICIPANT_JOINED"
        );
    }

    // ------------------------------------------------------------------
    // TEST 4: New instance_id (no prior session) — normal join
    // ------------------------------------------------------------------
    // Session joins with an instance_id that has no prior entry in the
    // instance_index. Verify no panic and the join proceeds normally.
    #[actix_rt::test]
    #[serial]
    async fn test_new_instance_id_normal_join() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let room = "eviction-new-instance";
        let session_id: SessionId = 8001;

        let dummy = DummySession.start();
        connect_and_join(
            &chat_server,
            session_id,
            room,
            "dave@example.com",
            dummy.recipient(),
            Some("inst-brand-new".to_string()), // instance_id with no prior session
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Session should be in room_members (first-time join with this instance_id)
        let members = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(
            members.len(),
            1,
            "Room should have exactly 1 member after first join with new instance_id"
        );
        assert_eq!(
            members[0].session, session_id,
            "The new session should be in room_members"
        );

        // No eviction occurred, so PARTICIPANT_JOINED should NOT be suppressed
        let suppressed = chat_server
            .send(IsSuppressedJoinBroadcast {
                session: session_id,
            })
            .await
            .expect("IsSuppressedJoinBroadcast should succeed");
        assert!(
            !suppressed,
            "First join with new instance_id should NOT suppress PARTICIPANT_JOINED"
        );
    }

    // ------------------------------------------------------------------
    // TEST 5: Multi-device safe — same user, different instance_ids
    // ------------------------------------------------------------------
    // Session A joins (user "alice", instance_id="inst-tab1"). Session B
    // joins (user "alice", instance_id="inst-tab2"). Both should coexist
    // in room_members because different instance_ids mean different
    // browser tabs / devices.
    //
    #[actix_rt::test]
    #[serial]
    async fn test_multi_device_safe_coexistence() {
        // Policy (#828): same user_id may have multiple concurrent sessions
        // with different instance_ids — they coexist as distinct
        // participants (real multi-tab / multi-device use case).
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let room = "eviction-multidevice";
        let user_id = "alice@example.com";
        let session_a: SessionId = 9001;
        let session_b: SessionId = 9002;

        // Session A joins with instance_id for tab 1
        let dummy_a = DummySession.start();
        connect_and_join(
            &chat_server,
            session_a,
            room,
            user_id,
            dummy_a.recipient(),
            Some("inst-tab1".to_string()),
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Session B joins with a different instance_id for tab 2.
        // Different instance_ids: both must coexist.
        let dummy_b = DummySession.start();
        connect_and_join(
            &chat_server,
            session_b,
            room,
            user_id,
            dummy_b.recipient(),
            Some("inst-tab2".to_string()),
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Both sessions should be in room_members.
        let members = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(
            members.len(),
            2,
            "Room should have 2 members (same user, distinct sessions coexist)"
        );

        let session_ids: Vec<SessionId> = members.iter().map(|m| m.session).collect();
        assert!(
            session_ids.contains(&session_a),
            "Session A should still be in room_members (not evicted)"
        );
        assert!(
            session_ids.contains(&session_b),
            "Session B should be in room_members"
        );

        // Session A should still be registered.
        let has_session_a = chat_server
            .send(HasSession { session: session_a })
            .await
            .expect("HasSession should succeed");
        assert!(
            has_session_a,
            "Session A should still be registered (multi-session-per-user allowed)"
        );

        // Session B should NOT suppress PARTICIPANT_JOINED — it is a real,
        // distinct join event from peers' point of view.
        let suppressed_b = chat_server
            .send(IsSuppressedJoinBroadcast { session: session_b })
            .await
            .expect("IsSuppressedJoinBroadcast should succeed");
        assert!(
            !suppressed_b,
            "Session B should NOT suppress PARTICIPANT_JOINED (new distinct session)"
        );
    }

    // Test helper: inspect instance_index
    #[derive(ActixMessage)]
    #[rtype(result = "Option<SessionId>")]
    struct GetInstanceSession {
        instance_id: String,
    }

    impl Handler<GetInstanceSession> for ChatServer {
        type Result = Option<SessionId>;

        fn handle(&mut self, msg: GetInstanceSession, _ctx: &mut Self::Context) -> Self::Result {
            self.instance_index.get(&msg.instance_id).copied()
        }
    }

    // ==========================================================================
    // TEST: EvictInstance handler evicts the correct session
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_evict_instance_evicts_correct_session() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 10001u64;
        let instance_id = "cross-server-evict-test".to_string();
        let room = "test-room-cross-evict".to_string();
        let user_id = "alice@example.com".to_string();

        // Register and join with instance_id
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.clone(),
                user_id: user_id.clone(),
                display_name: user_id.clone(),
                is_guest: false,
                observer: false,
                instance_id: Some(instance_id.clone()),
                is_host: false,
                end_on_host_leave: false,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed");
        assert!(result.is_ok(), "JoinRoom should succeed");

        // Activate the session so instance_index is populated (eviction and the
        // reverse lookup are deferred from JoinRoom to ActivateConnection).
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");

        // Verify session is tracked
        let stored = chat_server
            .send(GetInstanceSession {
                instance_id: instance_id.clone(),
            })
            .await
            .expect("Query should succeed");
        assert_eq!(stored, Some(session_id));

        // Send EvictInstance with a DIFFERENT new_session_id (simulating cross-server)
        chat_server
            .send(EvictInstance(EvictInstancePayload {
                instance_id: instance_id.clone(),
                room: room.clone(),
                user_id: user_id.clone(),
                new_session_id: 99999,
            }))
            .await
            .expect("EvictInstance should succeed");

        // Verify session was evicted from instance_index
        let stored = chat_server
            .send(GetInstanceSession {
                instance_id: instance_id.clone(),
            })
            .await
            .expect("Query should succeed");
        assert_eq!(stored, None, "Instance should be removed after eviction");

        // Verify session is removed from sessions map
        let has = chat_server
            .send(HasSession {
                session: session_id,
            })
            .await
            .expect("Query should succeed");
        assert!(!has, "Session should be removed from sessions map");
    }

    // ==========================================================================
    // TEST: EvictInstance is no-op for unknown instance_id
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_evict_instance_noop_for_unknown() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 10002u64;
        let instance_id = "known-inst".to_string();
        let room = "test-room-noop".to_string();
        let user_id = "bob@example.com".to_string();

        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.clone(),
                user_id: user_id.clone(),
                display_name: user_id.clone(),
                is_guest: false,
                observer: false,
                instance_id: Some(instance_id.clone()),
                is_host: false,
                end_on_host_leave: false,
                transport: "websocket".to_string(),
            })
            .await
            .expect("JoinRoom should succeed")
            .expect("JoinRoom should return Ok");

        // Activate so instance_index is populated (deferred from JoinRoom).
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");

        // Send eviction for UNKNOWN instance_id
        chat_server
            .send(EvictInstance(EvictInstancePayload {
                instance_id: "unknown-inst".to_string(),
                room: room.clone(),
                user_id: user_id.clone(),
                new_session_id: 99999,
            }))
            .await
            .expect("EvictInstance should succeed");

        // Original session should still be present
        let stored = chat_server
            .send(GetInstanceSession {
                instance_id: instance_id.clone(),
            })
            .await
            .expect("Query should succeed");
        assert_eq!(
            stored,
            Some(session_id),
            "Known instance should be unaffected"
        );

        let has = chat_server
            .send(HasSession {
                session: session_id,
            })
            .await
            .expect("Query should succeed");
        assert!(has, "Session should still exist");
    }

    // ==========================================================================
    // TEST: Self-delivery guard prevents double eviction
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_evict_instance_self_delivery_guard() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 10003u64;
        let instance_id = "self-delivery-test".to_string();
        let room = "test-room-self".to_string();
        let user_id = "charlie@example.com".to_string();

        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.clone(),
                user_id: user_id.clone(),
                display_name: user_id.clone(),
                is_guest: false,
                observer: false,
                instance_id: Some(instance_id.clone()),
                is_host: false,
                end_on_host_leave: false,
                transport: "websocket".to_string(),
            })
            .await
            .expect("JoinRoom should succeed")
            .expect("JoinRoom should return Ok");

        // Activate so instance_index is populated (deferred from JoinRoom).
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");

        // Send eviction with new_session_id == the SAME session (self-delivery)
        chat_server
            .send(EvictInstance(EvictInstancePayload {
                instance_id: instance_id.clone(),
                room: room.clone(),
                user_id: user_id.clone(),
                new_session_id: session_id, // SAME as stored — self-delivery
            }))
            .await
            .expect("EvictInstance should succeed");

        // Session should NOT be evicted (self-delivery guard)
        let stored = chat_server
            .send(GetInstanceSession {
                instance_id: instance_id.clone(),
            })
            .await
            .expect("Query should succeed");
        assert_eq!(
            stored,
            Some(session_id),
            "Self-delivery should not evict the session"
        );

        let has = chat_server
            .send(HasSession {
                session: session_id,
            })
            .await
            .expect("Query should succeed");
        assert!(has, "Session should still exist after self-delivery");
    }

    // ==========================================================================
    // TEST: EvictInstance verifies user_id ownership
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_evict_instance_verifies_user_ownership() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 10004u64;
        let instance_id = "ownership-test".to_string();
        let room = "test-room-ownership".to_string();
        let user_id = "alice@example.com".to_string();

        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.clone(),
                user_id: user_id.clone(),
                display_name: user_id.clone(),
                is_guest: false,
                observer: false,
                instance_id: Some(instance_id.clone()),
                is_host: false,
                end_on_host_leave: false,
                transport: "websocket".to_string(),
            })
            .await
            .expect("JoinRoom should succeed")
            .expect("JoinRoom should return Ok");

        // Activate so instance_index is populated (deferred from JoinRoom).
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");

        // Send eviction with DIFFERENT user_id (attacker scenario)
        chat_server
            .send(EvictInstance(EvictInstancePayload {
                instance_id: instance_id.clone(),
                room: room.clone(),
                user_id: "mallory@example.com".to_string(), // WRONG user
                new_session_id: 99999,
            }))
            .await
            .expect("EvictInstance should succeed");

        // Session should NOT be evicted (user_id mismatch)
        let stored = chat_server
            .send(GetInstanceSession {
                instance_id: instance_id.clone(),
            })
            .await
            .expect("Query should succeed");
        assert_eq!(
            stored,
            Some(session_id),
            "Session should not be evicted by different user"
        );

        let has = chat_server
            .send(HasSession {
                session: session_id,
            })
            .await
            .expect("Query should succeed");
        assert!(has, "Session should still exist (user_id mismatch)");
    }

    // ==========================================================================
    // BUG FIX (#502): host disconnect + end_on_host_leave cache staleness
    // ==========================================================================
    //
    // The chat_server caches `end_on_host_leave` at JoinRoom time from the JWT
    // claim. Mid-meeting PATCH /meetings updates the DB but the cached value
    // stayed stale until the host reconnected, so back-navigating after
    // toggling to `false` still kicked everyone out.
    //
    // The fix introduces a per-room `room_policy` cache refreshed by the
    // `internal.meeting_settings_updated` NATS event. The host-disconnect
    // path now reads `room_policy[room]` instead of the per-session JWT
    // capture. The legitimate broadcast path also publishes
    // `internal.meeting_ended_by_host` so meeting-api can mark the DB
    // `state='ended'` to mirror what clients see.
    //
    // The five tests below cover:
    //   1. Stable end_on_host_leave=true, host disconnects -> MEETING_ENDED + DB event.
    //   2. Stable end_on_host_leave=false, host disconnects -> no broadcast, no DB event.
    //   3. Mid-meeting toggle (true -> false), host disconnects -> no broadcast.
    //   4. Reconnect within grace period -> no broadcast, no DB event.
    //   5. Mid-meeting toggle, non-host disconnects -> host's cached policy is fresh.
    // ==========================================================================

    /// Test fixture: dummy session actor that ignores all messages.
    struct EohlDummySession;
    impl Actor for EohlDummySession {
        type Context = actix::Context<Self>;
    }
    impl Handler<Message> for EohlDummySession {
        type Result = ();
        fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
    }

    /// Drain a NATS subscriber with a deadline, recording whether each
    /// well-known event type arrived. Returns `(saw_meeting_ended,
    /// saw_participant_left)`.
    async fn collect_meeting_events(
        sub: &mut async_nats::Subscriber,
        deadline: tokio::time::Duration,
    ) -> (bool, bool) {
        use std::time::Instant;
        use tokio::time::timeout;
        use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
        use videocall_types::protos::meeting_packet::MeetingPacket;

        let start = Instant::now();
        let mut saw_ended = false;
        let mut saw_left = false;
        while start.elapsed() < deadline {
            let remaining = deadline.saturating_sub(start.elapsed());
            match timeout(remaining, sub.next()).await {
                Ok(Some(msg)) => {
                    if let Ok(wrapper) =
                        <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
                    {
                        if let Ok(inner) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                            if inner.event_type == MeetingEventType::MEETING_ENDED.into() {
                                saw_ended = true;
                            } else if inner.event_type == MeetingEventType::PARTICIPANT_LEFT.into()
                            {
                                saw_left = true;
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        (saw_ended, saw_left)
    }

    /// Wait up to `deadline` for at least one message on `sub`. Returns
    /// `Some(payload_bytes)` on the first hit, `None` on timeout.
    async fn wait_for_first(
        sub: &mut async_nats::Subscriber,
        deadline: tokio::time::Duration,
    ) -> Option<Vec<u8>> {
        match tokio::time::timeout(deadline, sub.next()).await {
            Ok(Some(msg)) => Some(msg.payload.to_vec()),
            _ => None,
        }
    }

    /// Drain `sub` for the full `deadline`, returning every payload received.
    /// Unlike [`wait_for_first`] this does NOT stop at the first message, so it
    /// can prove an event fired *exactly once* (and not once-per-disconnect).
    async fn drain_all(
        sub: &mut async_nats::Subscriber,
        deadline: tokio::time::Duration,
    ) -> Vec<Vec<u8>> {
        use std::time::Instant;
        use tokio::time::timeout;
        let start = Instant::now();
        let mut out = Vec::new();
        while start.elapsed() < deadline {
            let remaining = deadline.saturating_sub(start.elapsed());
            match timeout(remaining, sub.next()).await {
                Ok(Some(msg)) => out.push(msg.payload.to_vec()),
                _ => break,
            }
        }
        out
    }

    // ──────────────────────────────────────────────────────────────────────
    // TEST 1: Stable end_on_host_leave=true, host disconnects ->
    //         MEETING_ENDED broadcast AND internal.meeting_ended_by_host
    //         payload. Verifies the legitimate broadcast path still works
    //         after the refactor.
    // ──────────────────────────────────────────────────────────────────────
    #[actix_rt::test]
    #[serial]
    async fn test_host_disconnect_eohl_true_broadcasts_and_publishes_db_event() {
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();
        let dummy = EohlDummySession.start();
        let session_id = 9_500u64;
        let room = "test-eohl-true-broadcast";

        // Subscribe BEFORE the disconnect runs so we don't miss the publish.
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let mut system_sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");
        let mut db_sub = nats_client
            .subscribe(MEETING_ENDED_BY_HOST_SUBJECT)
            .await
            .expect("Failed to subscribe to internal subject");

        // Connect + JoinRoom as host with end_on_host_leave=true.
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");
        chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "host-true@example.com".to_string(),
                display_name: "host-true@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: true,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom should return Ok");
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");
        sleep(Duration::from_millis(300)).await;

        // Disconnect the host. The broadcast happens after the grace period.
        chat_server
            .send(Disconnect {
                session: session_id,
                room: room.to_string(),
                user_id: "host-true@example.com".to_string(),
                display_name: "host-true@example.com".to_string(),
                is_guest: false,
                observer: false,
                is_host: true,
                end_on_host_leave: true,
            })
            .await
            .expect("Disconnect should succeed");

        // Grace is 3s; allow 5s for publish to land on both subscribers.
        let (saw_ended, _saw_left) =
            collect_meeting_events(&mut system_sub, Duration::from_secs(5)).await;
        assert!(
            saw_ended,
            "MEETING_ENDED must be broadcast when end_on_host_leave=true and host disconnects"
        );

        let db_payload = wait_for_first(&mut db_sub, Duration::from_secs(2)).await;
        let db_payload = db_payload.expect(
            "internal.meeting_ended_by_host must be published when MEETING_ENDED is broadcast",
        );
        let parsed: MeetingEndedByHostPayload =
            serde_json::from_slice(&db_payload).expect("Payload should deserialize");
        assert_eq!(parsed.room_id, room);
    }

    // ──────────────────────────────────────────────────────────────────────
    // TEST 2: Stable end_on_host_leave=false, host disconnects ->
    //         no MEETING_ENDED, no DB event. Verifies the policy gate
    //         still suppresses the broadcast for hosts who never wanted
    //         end-on-leave.
    // ──────────────────────────────────────────────────────────────────────
    #[actix_rt::test]
    #[serial]
    async fn test_host_disconnect_eohl_false_no_broadcast_no_db_event() {
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();
        let dummy = EohlDummySession.start();
        let session_id = 9_501u64;
        let room = "test-eohl-false-no-broadcast";

        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let mut system_sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");
        let mut db_sub = nats_client
            .subscribe(MEETING_ENDED_BY_HOST_SUBJECT)
            .await
            .expect("Failed to subscribe to internal subject");

        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");
        chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "host-false@example.com".to_string(),
                display_name: "host-false@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: true,
                end_on_host_leave: false,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom should return Ok");
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");
        sleep(Duration::from_millis(300)).await;

        chat_server
            .send(Disconnect {
                session: session_id,
                room: room.to_string(),
                user_id: "host-false@example.com".to_string(),
                display_name: "host-false@example.com".to_string(),
                is_guest: false,
                observer: false,
                is_host: true,
                end_on_host_leave: false,
            })
            .await
            .expect("Disconnect should succeed");

        let (saw_ended, _saw_left) =
            collect_meeting_events(&mut system_sub, Duration::from_secs(5)).await;
        assert!(
            !saw_ended,
            "MEETING_ENDED must NOT be broadcast when end_on_host_leave=false"
        );

        let db_payload = wait_for_first(&mut db_sub, Duration::from_secs(1)).await;
        assert!(
            db_payload.is_none(),
            "internal.meeting_ended_by_host must NOT be published when no broadcast fires"
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // EMPTY->IDLE TEST A: two non-host participants leave a meeting.
    //         The became-empty event must fire EXACTLY ONCE — only after the
    //         LAST participant leaves (room_members reaches zero), NOT once per
    //         disconnect. This guards against an O(n) NATS storm on a mass
    //         disconnect / reconnection wave.
    // ──────────────────────────────────────────────────────────────────────
    #[actix_rt::test]
    #[serial]
    async fn test_room_empty_publishes_idle_event_exactly_once() {
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();
        let dummy = EohlDummySession.start();
        let room = "test-empty-idle-once";
        let s1 = 9_700u64;
        let s2 = 9_701u64;

        let mut empty_sub = nats_client
            .subscribe(MEETING_BECAME_EMPTY_SUBJECT)
            .await
            .expect("Failed to subscribe to became-empty subject");

        // Two participants join and activate.
        for (sid, uid) in [(s1, "p1@example.com"), (s2, "p2@example.com")] {
            chat_server
                .send(Connect {
                    id: sid,
                    addr: dummy.clone().recipient(),
                })
                .await
                .expect("Connect should succeed");
            chat_server
                .send(JoinRoom {
                    session: sid,
                    room: room.to_string(),
                    user_id: uid.to_string(),
                    display_name: uid.to_string(),
                    is_guest: false,
                    observer: false,
                    instance_id: None,
                    is_host: false,
                    end_on_host_leave: false,
                    transport: "websocket".to_string(),
                })
                .await
                .expect("Message delivery should succeed")
                .expect("JoinRoom should return Ok");
            chat_server
                .send(ActivateConnection { session: sid })
                .await
                .expect("ActivateConnection should succeed");
        }
        sleep(Duration::from_millis(300)).await;

        // First participant leaves — room is NOT yet empty (s2 remains), so no
        // became-empty event must fire.
        chat_server
            .send(Disconnect {
                session: s1,
                room: room.to_string(),
                user_id: "p1@example.com".to_string(),
                display_name: "p1@example.com".to_string(),
                is_guest: false,
                observer: false,
                is_host: false,
                end_on_host_leave: false,
            })
            .await
            .expect("Disconnect should succeed");

        // Wait past the grace period for s1's departure to execute, then assert
        // NO empty event yet.
        let early = drain_all(&mut empty_sub, Duration::from_secs(5)).await;
        assert!(
            early.is_empty(),
            "became-empty must NOT fire while a participant remains; got {} event(s)",
            early.len()
        );

        // Second (last) participant leaves — room becomes empty now.
        chat_server
            .send(Disconnect {
                session: s2,
                room: room.to_string(),
                user_id: "p2@example.com".to_string(),
                display_name: "p2@example.com".to_string(),
                is_guest: false,
                observer: false,
                is_host: false,
                end_on_host_leave: false,
            })
            .await
            .expect("Disconnect should succeed");

        // Drain the full window; expect exactly one became-empty event.
        let events = drain_all(&mut empty_sub, Duration::from_secs(6)).await;
        assert_eq!(
            events.len(),
            1,
            "became-empty must fire EXACTLY ONCE when the room drains to empty, not once per \
             disconnect; got {} event(s)",
            events.len()
        );
        let parsed: MeetingBecameEmptyPayload =
            serde_json::from_slice(&events[0]).expect("Payload should deserialize");
        assert_eq!(parsed.room_id, room);
    }

    // ──────────────────────────────────────────────────────────────────────
    // EMPTY->IDLE TEST B: host leaves with end_on_host_leave=true. END must
    //         win — the meeting ends, so the became-empty (idle) event must
    //         NOT fire even though the room drained to empty.
    // ──────────────────────────────────────────────────────────────────────
    #[actix_rt::test]
    #[serial]
    async fn test_host_leave_eohl_true_does_not_emit_idle_event() {
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();
        let dummy = EohlDummySession.start();
        let room = "test-empty-idle-eohl-true";
        let session_id = 9_702u64;

        let mut empty_sub = nats_client
            .subscribe(MEETING_BECAME_EMPTY_SUBJECT)
            .await
            .expect("Failed to subscribe to became-empty subject");

        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");
        chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "host@example.com".to_string(),
                display_name: "host@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: true,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom should return Ok");
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");
        sleep(Duration::from_millis(300)).await;

        chat_server
            .send(Disconnect {
                session: session_id,
                room: room.to_string(),
                user_id: "host@example.com".to_string(),
                display_name: "host@example.com".to_string(),
                is_guest: false,
                observer: false,
                is_host: true,
                end_on_host_leave: true,
            })
            .await
            .expect("Disconnect should succeed");

        // END wins: the host-leave path fires MEETING_ENDED + the ended-by-host
        // event, NOT the became-empty idle event.
        let events = drain_all(&mut empty_sub, Duration::from_secs(6)).await;
        assert!(
            events.is_empty(),
            "became-empty (idle) must NOT fire when end_on_host_leave=true ends the meeting; \
             got {} event(s)",
            events.len()
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // TEST 3: Mid-meeting toggle (true at JoinRoom, then NATS-pushed false)
    //         + host disconnects -> no MEETING_ENDED. This is the actual
    //         bug from discussion #502.
    // ──────────────────────────────────────────────────────────────────────
    #[actix_rt::test]
    #[serial]
    async fn test_mid_meeting_toggle_to_false_suppresses_broadcast() {
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();
        let dummy = EohlDummySession.start();
        let session_id = 9_502u64;
        let room = "test-eohl-toggle-to-false";

        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let mut system_sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");
        let mut db_sub = nats_client
            .subscribe(MEETING_ENDED_BY_HOST_SUBJECT)
            .await
            .expect("Failed to subscribe to internal subject");

        // Host joins with end_on_host_leave=true (the JWT-time value).
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");
        chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "host-toggle@example.com".to_string(),
                display_name: "host-toggle@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: true,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom should return Ok");
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");
        sleep(Duration::from_millis(300)).await;

        // Simulate meeting-api publishing the post-toggle policy snapshot.
        // The chat_server's `started()` task is already subscribed to this
        // subject; we publish here and wait long enough for the actor to
        // process the inbound NATS message and apply the UpdateRoomPolicy.
        let toggle_payload = MeetingSettingsUpdatePayload {
            room_id: room.to_string(),
            end_on_host_leave: false,
            admitted_can_admit: false,
            waiting_room_enabled: true,
            allow_guests: false,
        };
        let toggle_bytes =
            serde_json::to_vec(&toggle_payload).expect("Should serialize toggle payload");
        nats_client
            .publish(MEETING_SETTINGS_UPDATE_SUBJECT, toggle_bytes.into())
            .await
            .expect("Should publish toggle");
        // Allow time for the NATS message to be received by chat_server's
        // subscription loop and for the UpdateRoomPolicy actor message to
        // be handled. 800ms is well above NATS RTT on a healthy bus.
        sleep(Duration::from_millis(800)).await;

        // Now disconnect. The Disconnect message itself still carries
        // end_on_host_leave=true (the stale JWT-time capture), but
        // leave_rooms should consult the freshest room_policy entry
        // and decide NOT to broadcast.
        chat_server
            .send(Disconnect {
                session: session_id,
                room: room.to_string(),
                user_id: "host-toggle@example.com".to_string(),
                display_name: "host-toggle@example.com".to_string(),
                is_guest: false,
                observer: false,
                is_host: true,
                end_on_host_leave: true, // STALE — must NOT win
            })
            .await
            .expect("Disconnect should succeed");

        let (saw_ended, _saw_left) =
            collect_meeting_events(&mut system_sub, Duration::from_secs(5)).await;
        assert!(
            !saw_ended,
            "MEETING_ENDED must NOT be broadcast after mid-meeting toggle to end_on_host_leave=false"
        );

        let db_payload = wait_for_first(&mut db_sub, Duration::from_secs(1)).await;
        assert!(
            db_payload.is_none(),
            "internal.meeting_ended_by_host must NOT fire when policy was toggled off"
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // TEST 4: Reconnect within grace period -> no broadcast, no DB event.
    //         The reconnect cancels the deferred ExecutePendingDeparture
    //         before it can call leave_rooms, so neither the MEETING_ENDED
    //         packet nor the internal DB-write event should be observed.
    // ──────────────────────────────────────────────────────────────────────
    #[actix_rt::test]
    #[serial]
    async fn test_reconnect_within_grace_skips_broadcast_and_db_event() {
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();
        let dummy = EohlDummySession.start();
        let session_id_1 = 9_503u64;
        let session_id_2 = 9_504u64;
        let room = "test-eohl-reconnect-grace";
        let user = "host-reconnect@example.com";

        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let mut system_sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");
        let mut db_sub = nats_client
            .subscribe(MEETING_ENDED_BY_HOST_SUBJECT)
            .await
            .expect("Failed to subscribe to internal subject");

        // First session: connect + join + activate.
        // Carries a stable `instance_id` so the new (room, instance_id)
        // pending-departure key (issue #852) can match the reconnecting
        // session below — modelling real client behaviour where
        // sessionStorage preserves the UUID across an in-tab reconnect.
        chat_server
            .send(Connect {
                id: session_id_1,
                addr: dummy.clone().recipient(),
            })
            .await
            .expect("Connect should succeed");
        chat_server
            .send(JoinRoom {
                session: session_id_1,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                instance_id: Some("iid-reconnect-grace".to_string()),
                is_host: true,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom should return Ok");
        chat_server
            .send(ActivateConnection {
                session: session_id_1,
            })
            .await
            .expect("ActivateConnection should succeed");
        sleep(Duration::from_millis(300)).await;

        // Disconnect — schedules ExecutePendingDeparture for grace period later.
        chat_server
            .send(Disconnect {
                session: session_id_1,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                is_host: true,
                end_on_host_leave: true,
            })
            .await
            .expect("Disconnect should succeed");

        // Reconnect with the SAME instance_id BEFORE the grace period
        // elapses (same-tab refresh). Grace is 3s; we wait 1s to be safely
        // inside the window.
        sleep(Duration::from_secs(1)).await;
        chat_server
            .send(Connect {
                id: session_id_2,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");
        chat_server
            .send(JoinRoom {
                session: session_id_2,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                instance_id: Some("iid-reconnect-grace".to_string()),
                is_host: true,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom should return Ok");
        chat_server
            .send(ActivateConnection {
                session: session_id_2,
            })
            .await
            .expect("ActivateConnection should succeed");

        // Wait beyond the grace period — even though it's elapsed by now,
        // the reconnect should have cancelled ExecutePendingDeparture, so
        // no broadcast and no DB event should arrive.
        let (saw_ended, _saw_left) =
            collect_meeting_events(&mut system_sub, Duration::from_secs(5)).await;
        assert!(
            !saw_ended,
            "Reconnect within grace period must cancel deferred MEETING_ENDED broadcast"
        );

        let db_payload = wait_for_first(&mut db_sub, Duration::from_secs(1)).await;
        assert!(
            db_payload.is_none(),
            "Reconnect within grace period must cancel internal.meeting_ended_by_host"
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // TEST 5: Mid-meeting toggle, NON-host disconnect -> the host's
    //         RoomMemberInfo.end_on_host_leave field gets updated by the
    //         settings-update consumer. Locks in the cache-update path
    //         independently from the broadcast logic, so a future change
    //         to the leave_rooms gate can't silently resurrect the bug.
    // ──────────────────────────────────────────────────────────────────────
    #[actix_rt::test]
    #[serial]
    async fn test_settings_update_refreshes_room_policy_for_existing_host() {
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();
        let dummy = EohlDummySession.start();
        let host_id = 9_505u64;
        let other_id = 9_506u64;
        let room = "test-eohl-toggle-other-leaves";

        // Host joins with end_on_host_leave=true.
        chat_server
            .send(Connect {
                id: host_id,
                addr: dummy.clone().recipient(),
            })
            .await
            .expect("Connect should succeed");
        chat_server
            .send(JoinRoom {
                session: host_id,
                room: room.to_string(),
                user_id: "host-multi@example.com".to_string(),
                display_name: "host-multi@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: true,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom should return Ok");
        chat_server
            .send(ActivateConnection { session: host_id })
            .await
            .expect("ActivateConnection should succeed");

        // A non-host participant joins.
        chat_server
            .send(Connect {
                id: other_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");
        chat_server
            .send(JoinRoom {
                session: other_id,
                room: room.to_string(),
                user_id: "other-multi@example.com".to_string(),
                display_name: "other-multi@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom should return Ok");
        chat_server
            .send(ActivateConnection { session: other_id })
            .await
            .expect("ActivateConnection should succeed");
        sleep(Duration::from_millis(300)).await;

        // Toggle end_on_host_leave to false via the internal NATS event.
        let toggle = MeetingSettingsUpdatePayload {
            room_id: room.to_string(),
            end_on_host_leave: false,
            admitted_can_admit: false,
            waiting_room_enabled: true,
            allow_guests: false,
        };
        nats_client
            .publish(
                MEETING_SETTINGS_UPDATE_SUBJECT,
                serde_json::to_vec(&toggle).unwrap().into(),
            )
            .await
            .expect("Should publish toggle");
        sleep(Duration::from_millis(800)).await;

        // Inspect the host's per-member end_on_host_leave field via a
        // synthetic actor message we add only for tests below.
        let observed = chat_server
            .send(GetRoomMemberEndOnHostLeave {
                room: room.to_string(),
                session: host_id,
            })
            .await
            .expect("Query should succeed");
        assert_eq!(
            observed,
            Some(false),
            "UpdateRoomPolicy must mirror end_on_host_leave onto every member of the room"
        );

        // Sanity: also the room_policy cache itself should reflect the toggle.
        let policy = chat_server
            .send(GetRoomPolicyEndOnHostLeave {
                room: room.to_string(),
            })
            .await
            .expect("Query should succeed");
        assert_eq!(
            policy,
            Some(false),
            "room_policy cache must hold the post-toggle end_on_host_leave value"
        );

        // Now disconnect the non-host. This exercises leave_rooms for a
        // non-host (no MEETING_ENDED expected regardless of policy) and
        // confirms the host's cache survives.
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let mut system_sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");
        chat_server
            .send(Disconnect {
                session: other_id,
                room: room.to_string(),
                user_id: "other-multi@example.com".to_string(),
                display_name: "other-multi@example.com".to_string(),
                is_guest: false,
                observer: false,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Disconnect should succeed");

        let (saw_ended, _saw_left) =
            collect_meeting_events(&mut system_sub, Duration::from_secs(5)).await;
        assert!(
            !saw_ended,
            "Non-host disconnect must not broadcast MEETING_ENDED regardless of policy"
        );

        // Final sanity: the host's policy still reads false after the
        // other participant left.
        let still = chat_server
            .send(GetRoomMemberEndOnHostLeave {
                room: room.to_string(),
                session: host_id,
            })
            .await
            .expect("Query should succeed");
        assert_eq!(
            still,
            Some(false),
            "Host's cached end_on_host_leave must persist after a non-host departure"
        );
    }

    // ======================================================================
    // Same-user-id multi-session coexistence tests (issue #828)
    // ======================================================================
    //
    // Previously (under the now-removed `evict_same_user_session` helper),
    // a JoinRoom from the same `user_id` would silently evict any prior
    // session for that user in the same room. That collapsed legitimate
    // multi-tab / multi-device / multi-instance scenarios into a single
    // visible participant — the symptom reported in HCL issue #828.
    //
    // New policy: same `user_id` may have multiple concurrent sessions in
    // a room. Each session is a distinct participant from the peers' point
    // of view. The instance-id-based eviction (`evict_stale_session`) is
    // kept for same-tab refresh / back-button — that path keys on the
    // per-tab `instance_id` from sessionStorage, which survives a refresh
    // but differs across tabs / devices, so it correctly distinguishes
    // "same tab, reloaded" from "different tab, same user".

    // ------------------------------------------------------------------
    // TEST: Same user, different sessions coexist in the room (#828)
    // ------------------------------------------------------------------
    // Session A joins with instance_id="inst-A". Session B joins the same
    // room with the SAME user_id but a FRESH instance_id="inst-B"
    // (simulating a second tab / device / instance of the same logged-in
    // user). The instance-id eviction cannot match different instance_ids,
    // and the old `evict_same_user_session` fallthrough is gone, so BOTH
    // sessions must coexist. Verify:
    //   - Both RoomMemberInfo entries are present (same user_id, distinct
    //     session ids).
    //   - Session A's per-session state was NOT torn down.
    //   - Session B does NOT have PARTICIPANT_JOINED suppression set —
    //     peers should see a real join event for the new session.
    //   - No spurious PARTICIPANT_LEFT or MEETING_ENDED for session A.
    #[actix_rt::test]
    #[serial]
    async fn same_user_different_sessions_coexist_in_room() {
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();
        let dummy = EohlDummySession.start();
        let session_a: SessionId = 9_700;
        let session_b: SessionId = 9_701;
        let room = "issue-828-coexist";
        let user = "antonio@example.com";

        // Subscribe to the system subject so we can confirm no spurious
        // PARTICIPANT_LEFT / MEETING_ENDED is broadcast for session A.
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let mut system_sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");

        // Session A: connect + join with instance_id="inst-A".
        chat_server
            .send(Connect {
                id: session_a,
                addr: dummy.clone().recipient(),
            })
            .await
            .expect("Connect should succeed");
        chat_server
            .send(JoinRoom {
                session: session_a,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                instance_id: Some("inst-A".to_string()),
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom A should return Ok");

        chat_server
            .send(ActivateConnection { session: session_a })
            .await
            .expect("ActivateConnection should succeed");
        sleep(Duration::from_millis(200)).await;

        // Session B: connect + join with a FRESH instance_id, same user_id.
        // Under the new policy this must NOT evict session A.
        chat_server
            .send(Connect {
                id: session_b,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");
        chat_server
            .send(JoinRoom {
                session: session_b,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                instance_id: Some("inst-B".to_string()),
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom B should return Ok");

        sleep(Duration::from_millis(200)).await;

        // Both members must be present with the same user_id but distinct sessions.
        let members = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(
            members.len(),
            2,
            "Same user with different sessions must coexist (got {} members)",
            members.len()
        );
        let sessions: std::collections::HashSet<_> = members.iter().map(|m| m.session).collect();
        assert!(
            sessions.contains(&session_a),
            "Session A must still be in room_members"
        );
        assert!(
            sessions.contains(&session_b),
            "Session B must be in room_members"
        );
        assert!(
            members.iter().all(|m| m.user_id == user),
            "All surviving members should share the user_id"
        );

        // Session A's per-session state must NOT have been torn down.
        let has_session_a = chat_server
            .send(HasSession { session: session_a })
            .await
            .expect("HasSession should succeed");
        assert!(
            has_session_a,
            "Session A must remain in the sessions map (not evicted by B's JoinRoom)"
        );
        let has_sub_a = chat_server
            .send(HasActiveSub { session: session_a })
            .await
            .expect("HasActiveSub should succeed");
        assert!(has_sub_a, "Session A must still hold its NATS subscription");

        // Session B must NOT be suppressed — peers should see its
        // PARTICIPANT_JOINED as a real, distinct join event.
        let suppressed_b = chat_server
            .send(IsSuppressedJoinBroadcast { session: session_b })
            .await
            .expect("IsSuppressedJoinBroadcast should succeed");
        assert!(
            !suppressed_b,
            "Session B must NOT suppress PARTICIPANT_JOINED — multi-session same-user is a real join"
        );

        // Activate B and confirm no spurious PARTICIPANT_LEFT / MEETING_ENDED
        // was broadcast for session A. (PARTICIPANT_JOINED for B is allowed
        // and expected — `collect_meeting_events` only watches LEFT/ENDED.)
        chat_server
            .send(ActivateConnection { session: session_b })
            .await
            .expect("ActivateConnection should succeed");

        let (saw_ended, saw_left) =
            collect_meeting_events(&mut system_sub, Duration::from_millis(800)).await;
        assert!(
            !saw_left,
            "Multi-session coexistence must not broadcast PARTICIPANT_LEFT for session A"
        );
        assert!(
            !saw_ended,
            "Multi-session coexistence must not broadcast MEETING_ENDED"
        );
    }

    // ------------------------------------------------------------------
    // TEST: A peer joining sees BOTH same-user sessions (#828)
    // ------------------------------------------------------------------
    // Same setup as above: two same-user sessions A and B coexist. A third,
    // distinct user C joins the room. Verify that the existing-members
    // broadcast from `JoinRoom`'s handler reports BOTH A and B back to C —
    // i.e. C must learn about both sessions of the duplicated user, not
    // just the latest.
    #[actix_rt::test]
    #[serial]
    async fn peer_sees_both_same_user_sessions_on_join() {
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();
        let dummy = EohlDummySession.start();
        let session_a: SessionId = 9_740;
        let session_b: SessionId = 9_741;
        let session_c: SessionId = 9_742;
        let room = "issue-828-peer-view";
        let user_x = "tony@example.com";
        let user_c = "carol@example.com";

        // Two same-user sessions, distinct instance_ids.
        chat_server
            .send(Connect {
                id: session_a,
                addr: dummy.clone().recipient(),
            })
            .await
            .expect("Connect A should succeed");
        chat_server
            .send(JoinRoom {
                session: session_a,
                room: room.to_string(),
                user_id: user_x.to_string(),
                display_name: user_x.to_string(),
                is_guest: false,
                observer: false,
                instance_id: Some("inst-laptop".to_string()),
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom A should return Ok");
        sleep(Duration::from_millis(100)).await;

        chat_server
            .send(Connect {
                id: session_b,
                addr: dummy.clone().recipient(),
            })
            .await
            .expect("Connect B should succeed");
        chat_server
            .send(JoinRoom {
                session: session_b,
                room: room.to_string(),
                user_id: user_x.to_string(),
                display_name: user_x.to_string(),
                is_guest: false,
                observer: false,
                instance_id: Some("inst-phone".to_string()),
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom B should return Ok");
        sleep(Duration::from_millis(100)).await;

        // Sanity: both same-user sessions are in the room.
        let members_before = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(
            members_before.len(),
            2,
            "Pre-condition: both same-user sessions must be visible before C joins"
        );

        // A third, distinct user joins. The existing-members broadcast in
        // `JoinRoom` (chat_server.rs around line 1644) iterates
        // `room_members` and reports every existing entry to the new joiner.
        // We assert via `GetRoomMembers` after C joins: the membership
        // snapshot must contain three entries (A, B, C) — confirming that
        // the existing-members iteration sees both same-user sessions.
        chat_server
            .send(Connect {
                id: session_c,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect C should succeed");
        chat_server
            .send(JoinRoom {
                session: session_c,
                room: room.to_string(),
                user_id: user_c.to_string(),
                display_name: user_c.to_string(),
                is_guest: false,
                observer: false,
                instance_id: Some("inst-carol".to_string()),
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom C should return Ok");
        sleep(Duration::from_millis(150)).await;

        let members_after = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(
            members_after.len(),
            3,
            "Peer C must see both same-user sessions plus itself (got {})",
            members_after.len()
        );
        let same_user_count = members_after.iter().filter(|m| m.user_id == user_x).count();
        assert_eq!(
            same_user_count, 2,
            "Both sessions of {user_x} must be present in the post-join roster"
        );
    }

    // ------------------------------------------------------------------
    // TEST: Multi-tab same user, no instance_id — both sessions coexist (#828)
    // ------------------------------------------------------------------
    // Two distinct sessions, same user_id, same room, neither carries an
    // instance_id. Under the new policy (no `evict_same_user_session`),
    // both must coexist as distinct participants.
    #[actix_rt::test]
    #[serial]
    async fn multi_tab_dup_no_instance_id_coexists() {
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();
        let dummy = EohlDummySession.start();
        let session_a: SessionId = 9_710;
        let session_b: SessionId = 9_711;
        let room = "issue-828-multi-tab";
        let user = "carol@example.com";

        // Tab A: no instance_id.
        chat_server
            .send(Connect {
                id: session_a,
                addr: dummy.clone().recipient(),
            })
            .await
            .expect("Connect should succeed");
        chat_server
            .send(JoinRoom {
                session: session_a,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom A should return Ok");
        sleep(Duration::from_millis(150)).await;

        // Tab B: no instance_id, same user_id and room.
        chat_server
            .send(Connect {
                id: session_b,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");
        chat_server
            .send(JoinRoom {
                session: session_b,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom B should return Ok");
        sleep(Duration::from_millis(150)).await;

        let members = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(
            members.len(),
            2,
            "Multi-tab same-user must coexist as two distinct sessions"
        );
        let sessions: std::collections::HashSet<_> = members.iter().map(|m| m.session).collect();
        assert!(sessions.contains(&session_a));
        assert!(sessions.contains(&session_b));
    }

    // ------------------------------------------------------------------
    // TEST: Same-tab refresh (same instance_id) still evicts the prior session
    // ------------------------------------------------------------------
    // Regression check for #828: the instance-id eviction path is the
    // sole de-duplication mechanism after `evict_same_user_session` was
    // removed. When a tab refreshes / the back-button is used, the client
    // re-uses its sessionStorage `instance_id`; `evict_stale_session` must
    // still collapse session A -> session B. This test locks in that the
    // multi-session-per-user fix did NOT regress the same-tab-refresh path.
    #[actix_rt::test]
    #[serial]
    async fn same_user_same_instance_still_evicts_on_refresh() {
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();
        let dummy = EohlDummySession.start();
        let session_a: SessionId = 9_720;
        let session_b: SessionId = 9_721;
        let room = "issue-502-iid-path";
        let user = "dave@example.com";
        let iid = "inst-stable".to_string();

        // Session A with instance_id.
        chat_server
            .send(Connect {
                id: session_a,
                addr: dummy.clone().recipient(),
            })
            .await
            .expect("Connect should succeed");
        chat_server
            .send(JoinRoom {
                session: session_a,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                instance_id: Some(iid.clone()),
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom A should return Ok");
        sleep(Duration::from_millis(150)).await;

        // Activate session A so it is registered in instance_index. Eviction
        // during B's ActivateConnection needs the forward mapping to find A.
        chat_server
            .send(ActivateConnection { session: session_a })
            .await
            .expect("ActivateConnection A should succeed");

        // Session B with the SAME instance_id (in-tab transport reconnect).
        chat_server
            .send(Connect {
                id: session_b,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");
        chat_server
            .send(JoinRoom {
                session: session_b,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                instance_id: Some(iid),
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom B should return Ok");
        sleep(Duration::from_millis(150)).await;

        // Activate session B — this triggers eviction of session A.
        chat_server
            .send(ActivateConnection { session: session_b })
            .await
            .expect("ActivateConnection B should succeed");

        let members = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(
            members.len(),
            1,
            "In-tab reconnect (same instance_id) must still leave one member"
        );
        assert_eq!(members[0].session, session_b);
        // PARTICIPANT_JOINED suppression is consumed inside ActivateConnection
        // (the flag is removed when the broadcast is correctly skipped).
    }

    // ------------------------------------------------------------------
    // TEST: Reconnection grace window cancels pending departure
    // ------------------------------------------------------------------
    // Sequence: session A joins + activates as host, then disconnects
    // (schedules ExecutePendingDeparture for the grace window), then
    // session B joins as host with the SAME `instance_id` BEFORE the grace
    // expires (modelling a same-tab refresh / back-navigation that
    // restores `instance_id` from sessionStorage). The
    // reconnection-grace-period path (matching on `(room, instance_id)`
    // in `pending_departures`) cancels the pending departure so the
    // deferred MEETING_ENDED + PARTICIPANT_LEFT broadcasts NEVER fire —
    // peers see a seamless host presence.
    //
    // Note (#828 / #852): the grace path was rekeyed from `(room, user_id)`
    // to `(room, instance_id)` so that distinct sibling sessions of the
    // same `user_id` (different tabs) are no longer misclassified as
    // reconnections. Same-tab refresh keeps the instance_id and therefore
    // still matches — that's the path this test exercises.
    #[actix_rt::test]
    #[serial]
    async fn reconnection_grace_cancels_pending_departure() {
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();
        let dummy = EohlDummySession.start();
        let session_a: SessionId = 9_730;
        let session_b: SessionId = 9_731;
        let room = "issue-502-cancel-departure";
        let user = "erin-host@example.com";

        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let mut system_sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");
        let mut db_sub = nats_client
            .subscribe(MEETING_ENDED_BY_HOST_SUBJECT)
            .await
            .expect("Failed to subscribe to internal subject");

        // Session A: host, activates, then disconnects (defers departure).
        chat_server
            .send(Connect {
                id: session_a,
                addr: dummy.clone().recipient(),
            })
            .await
            .expect("Connect should succeed");
        chat_server
            .send(JoinRoom {
                session: session_a,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                instance_id: Some("iid-A".to_string()),
                is_host: true,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom A should return Ok");
        chat_server
            .send(ActivateConnection { session: session_a })
            .await
            .expect("ActivateConnection should succeed");
        sleep(Duration::from_millis(250)).await;

        chat_server
            .send(Disconnect {
                session: session_a,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                is_host: true,
                end_on_host_leave: true,
            })
            .await
            .expect("Disconnect should succeed");

        // Wait briefly inside the grace window, then session B rejoins
        // with a FRESH instance_id (simulating back-navigation + rejoin).
        sleep(Duration::from_millis(800)).await;
        chat_server
            .send(Connect {
                id: session_b,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");
        chat_server
            .send(JoinRoom {
                session: session_b,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                // Same instance_id as session A — models a same-tab refresh
                // / back-navigation, the only path that legitimately matches
                // a pending departure after the #852 rekey.
                instance_id: Some("iid-A".to_string()),
                is_host: true,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Message delivery should succeed")
            .expect("JoinRoom B should return Ok");
        chat_server
            .send(ActivateConnection { session: session_b })
            .await
            .expect("ActivateConnection should succeed");

        // Wait beyond the grace period. Even though it elapsed, the
        // pending departure should have been cancelled by the
        // (room, instance_id) match during JoinRoom B — so no broadcast
        // and no DB event should land.
        let (saw_ended, saw_left) =
            collect_meeting_events(&mut system_sub, Duration::from_secs(5)).await;
        assert!(
            !saw_ended,
            "Same-iid rejoin within grace must cancel deferred MEETING_ENDED"
        );
        assert!(
            !saw_left,
            "Same-iid rejoin within grace must cancel deferred PARTICIPANT_LEFT"
        );

        let db_payload = wait_for_first(&mut db_sub, Duration::from_secs(1)).await;
        assert!(
            db_payload.is_none(),
            "Same-iid rejoin within grace must cancel internal.meeting_ended_by_host"
        );

        // Single survivor in room_members — session B.
        let members = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].session, session_b);
    }

    // ──────────────────────────────────────────────────────────────────────
    // Tests for issue #852 — pending_departures rekey from (room, user_id)
    // to (room, instance_id). Each test below fails on the pre-fix code
    // (where two sessions of the same user_id collide on the HashMap key)
    // and passes after the rekey.
    // ──────────────────────────────────────────────────────────────────────

    /// Test-only helper: report the number of `pending_departures` entries
    /// for a given `room`. Used by the #852 multi-session tests to confirm
    /// that two sibling sessions of the same user each get an independent
    /// entry instead of overwriting one another.
    #[derive(ActixMessage)]
    #[rtype(result = "usize")]
    struct CountPendingDeparturesForRoom {
        room: String,
    }

    impl Handler<CountPendingDeparturesForRoom> for ChatServer {
        type Result = usize;
        fn handle(
            &mut self,
            msg: CountPendingDeparturesForRoom,
            _ctx: &mut Self::Context,
        ) -> Self::Result {
            self.pending_departures
                .keys()
                .filter(|(r, _)| r == &msg.room)
                .count()
        }
    }

    // ------------------------------------------------------------------
    // TEST: Disconnect of one same-user session does not remove the
    //       sibling session from room_members (#852, site A).
    // ------------------------------------------------------------------
    // Pre-fix behaviour: under the `(room, user_id)` key, A's Disconnect
    // registered a pending entry for `(room, user)`; B's Disconnect then
    // *replaced* that entry, and the replacement path used
    // `members.retain(|m| m.session != old.old_session)` to silently
    // drop A from `room_members`. With the new `(room, instance_id)`
    // key, A and B get independent pending entries — B's disconnect
    // cannot reach A's room_members row.
    #[actix_rt::test]
    #[serial]
    async fn disconnect_of_one_session_does_not_remove_sibling_same_user_session() {
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();
        let dummy = EohlDummySession.start();
        let session_a: SessionId = 9_810;
        let session_b: SessionId = 9_811;
        let room = "issue-852-disconnect-sibling";
        let user = "tony@example.com";

        // Both sessions of the same user join with distinct instance_ids.
        chat_server
            .send(Connect {
                id: session_a,
                addr: dummy.clone().recipient(),
            })
            .await
            .expect("Connect A");
        chat_server
            .send(JoinRoom {
                session: session_a,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                instance_id: Some("iid-laptop".to_string()),
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Delivery A")
            .expect("JoinRoom A");
        chat_server
            .send(ActivateConnection { session: session_a })
            .await
            .expect("Activate A");

        chat_server
            .send(Connect {
                id: session_b,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect B");
        chat_server
            .send(JoinRoom {
                session: session_b,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                instance_id: Some("iid-phone".to_string()),
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Delivery B")
            .expect("JoinRoom B");
        chat_server
            .send(ActivateConnection { session: session_b })
            .await
            .expect("Activate B");
        sleep(Duration::from_millis(150)).await;

        // Session B disconnects mid-call. With the pre-fix key, this would
        // cancel A's (nonexistent) pending entry by colliding on user_id,
        // and worse: if A had also disconnected first, A's row in
        // room_members would be retain-removed here. We model the simpler
        // case (only B disconnects) to assert that A is untouched.
        chat_server
            .send(Disconnect {
                session: session_b,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Disconnect B");

        // A must still be present in room_members and in the sessions map.
        let members = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers");
        let sessions: std::collections::HashSet<_> = members.iter().map(|m| m.session).collect();
        assert!(
            sessions.contains(&session_a),
            "Session A must remain in room_members after sibling B disconnects \
             (got sessions {sessions:?})"
        );
        let has_a = chat_server
            .send(HasSession { session: session_a })
            .await
            .expect("HasSession A");
        assert!(has_a, "Session A's transport entry must be preserved");
        let has_sub_a = chat_server
            .send(HasActiveSub { session: session_a })
            .await
            .expect("HasActiveSub A");
        assert!(
            has_sub_a,
            "Session A's NATS subscription must NOT have been aborted by B's Disconnect"
        );
    }

    // ------------------------------------------------------------------
    // TEST: Two same-user disconnects produce two independent grace
    //       entries (#852, site A).
    // ------------------------------------------------------------------
    // Pre-fix: A's Disconnect inserts one entry; B's Disconnect REPLACES
    // it (same `(room, user_id)`) — leaving exactly one entry and only
    // one grace timer. After the rekey, both disconnects produce
    // independent `(room, instance_id)` entries and each fires its own
    // PARTICIPANT_LEFT broadcast.
    #[actix_rt::test]
    #[serial]
    async fn disconnect_of_both_same_user_sessions_creates_two_independent_grace_entries() {
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();
        let dummy = EohlDummySession.start();
        let session_a: SessionId = 9_820;
        let session_b: SessionId = 9_821;
        let room = "issue-852-two-grace-entries";
        let user = "tony@example.com";

        for (sid, iid) in [(session_a, "iid-A"), (session_b, "iid-B")] {
            chat_server
                .send(Connect {
                    id: sid,
                    addr: dummy.clone().recipient(),
                })
                .await
                .expect("Connect");
            chat_server
                .send(JoinRoom {
                    session: sid,
                    room: room.to_string(),
                    user_id: user.to_string(),
                    display_name: user.to_string(),
                    is_guest: false,
                    observer: false,
                    instance_id: Some(iid.to_string()),
                    is_host: false,
                    end_on_host_leave: true,
                    transport: "websocket".to_string(),
                })
                .await
                .expect("Delivery")
                .expect("JoinRoom");
            chat_server
                .send(ActivateConnection { session: sid })
                .await
                .expect("Activate");
        }
        sleep(Duration::from_millis(150)).await;

        // Both same-user sessions disconnect in rapid succession.
        for sid in [session_a, session_b] {
            chat_server
                .send(Disconnect {
                    session: sid,
                    room: room.to_string(),
                    user_id: user.to_string(),
                    display_name: user.to_string(),
                    is_guest: false,
                    observer: false,
                    is_host: false,
                    end_on_host_leave: true,
                })
                .await
                .expect("Disconnect");
        }

        let count = chat_server
            .send(CountPendingDeparturesForRoom {
                room: room.to_string(),
            })
            .await
            .expect("CountPendingDeparturesForRoom");
        assert_eq!(
            count, 2,
            "Two same-user disconnects must produce two independent pending_departures \
             entries (got {count}); on the pre-fix code this returned 1 because the \
             second Disconnect collided on (room, user_id) and replaced the first."
        );
    }

    // ------------------------------------------------------------------
    // TEST: Explicit Leave of one same-user session does not cancel a
    //       sibling's pending grace (#852, site B).
    // ------------------------------------------------------------------
    // Pre-fix: B's POST /leave executed
    //   `pending_departures.remove(&(room, user_id))`
    // which could yank A's pending entry instead of B's. After the
    // rekey, Leave looks up its own session's instance_id and only
    // removes its own entry.
    #[actix_rt::test]
    #[serial]
    async fn explicit_leave_of_one_session_does_not_cancel_sibling_grace() {
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();
        let dummy = EohlDummySession.start();
        let session_a: SessionId = 9_830;
        let session_b: SessionId = 9_831;
        let room = "issue-852-leave-sibling";
        let user = "tony@example.com";

        for (sid, iid) in [(session_a, "iid-A"), (session_b, "iid-B")] {
            chat_server
                .send(Connect {
                    id: sid,
                    addr: dummy.clone().recipient(),
                })
                .await
                .expect("Connect");
            chat_server
                .send(JoinRoom {
                    session: sid,
                    room: room.to_string(),
                    user_id: user.to_string(),
                    display_name: user.to_string(),
                    is_guest: false,
                    observer: false,
                    instance_id: Some(iid.to_string()),
                    is_host: false,
                    end_on_host_leave: true,
                    transport: "websocket".to_string(),
                })
                .await
                .expect("Delivery")
                .expect("JoinRoom");
            chat_server
                .send(ActivateConnection { session: sid })
                .await
                .expect("Activate");
        }
        sleep(Duration::from_millis(150)).await;

        // A disconnects -> A enters grace.
        chat_server
            .send(Disconnect {
                session: session_a,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Disconnect A");
        // Sanity: one pending entry exists for the room.
        let before = chat_server
            .send(CountPendingDeparturesForRoom {
                room: room.to_string(),
            })
            .await
            .expect("Count before");
        assert_eq!(before, 1, "Pre-condition: A's grace entry must be present");

        // B explicitly leaves. Pre-fix, this `remove(&(room, user_id))`
        // would silently consume A's pending entry. Post-fix, B's leave
        // lookup uses B's own instance_id and finds nothing.
        chat_server
            .send(Leave {
                session: session_b,
                room: room.to_string(),
                user_id: user.to_string(),
            })
            .await
            .expect("Leave B");

        let after = chat_server
            .send(CountPendingDeparturesForRoom {
                room: room.to_string(),
            })
            .await
            .expect("Count after");
        assert_eq!(
            after, 1,
            "B's explicit Leave must NOT cancel A's pending grace entry \
             (got {after} pending entries; pre-fix yielded 0 because the \
             (room, user_id) lookup collided on A's entry)."
        );
    }

    // ------------------------------------------------------------------
    // TEST: A fresh second session of the same user is NOT misclassified
    //       as a reconnection (#852, site C).
    // ------------------------------------------------------------------
    // Pre-fix: B's JoinRoom matched A's pending entry at `(room,
    // user_id)`, cancelled A's grace timer, removed A from
    // room_members, and suppressed B's own PARTICIPANT_JOINED. With
    // the new `(room, instance_id)` lookup, B's fresh instance_id
    // does not match A's entry — B is announced as a real join and
    // A's pending state is left untouched.
    #[actix_rt::test]
    #[serial]
    async fn fresh_second_session_of_same_user_is_not_misclassified_as_reconnection() {
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();
        let dummy = EohlDummySession.start();
        let session_a: SessionId = 9_840;
        let session_b: SessionId = 9_841;
        let room = "issue-852-fresh-second";
        let user = "tony@example.com";

        // A: active in the room. We disconnect A so it enters grace —
        // this is the state under which the bug fires (B's JoinRoom
        // matches A's pending entry on the `(room, user_id)` key).
        chat_server
            .send(Connect {
                id: session_a,
                addr: dummy.clone().recipient(),
            })
            .await
            .expect("Connect A");
        chat_server
            .send(JoinRoom {
                session: session_a,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                instance_id: Some("iid-A".to_string()),
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Delivery A")
            .expect("JoinRoom A");
        chat_server
            .send(ActivateConnection { session: session_a })
            .await
            .expect("Activate A");
        sleep(Duration::from_millis(150)).await;

        chat_server
            .send(Disconnect {
                session: session_a,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Disconnect A");

        // Inside the grace window, B (fresh instance_id, same user) joins.
        chat_server
            .send(Connect {
                id: session_b,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect B");
        chat_server
            .send(JoinRoom {
                session: session_b,
                room: room.to_string(),
                user_id: user.to_string(),
                display_name: user.to_string(),
                is_guest: false,
                observer: false,
                instance_id: Some("iid-B-fresh".to_string()),
                is_host: false,
                end_on_host_leave: true,
                transport: "websocket".to_string(),
            })
            .await
            .expect("Delivery B")
            .expect("JoinRoom B");
        sleep(Duration::from_millis(150)).await;

        // B must NOT be suppressed — peers should see a real
        // PARTICIPANT_JOINED for this distinct session.
        let suppressed_b = chat_server
            .send(IsSuppressedJoinBroadcast { session: session_b })
            .await
            .expect("IsSuppressedJoinBroadcast B");
        assert!(
            !suppressed_b,
            "B's fresh instance_id must NOT trigger the reconnection-grace \
             suppression path — it is a real new session, not a reconnect."
        );

        // A's pending grace entry must still be present (B did not consume it).
        let count = chat_server
            .send(CountPendingDeparturesForRoom {
                room: room.to_string(),
            })
            .await
            .expect("CountPendingDeparturesForRoom");
        assert_eq!(
            count, 1,
            "A's pending grace must survive B's join (got {count} entries; \
             pre-fix dropped to 0 because B's JoinRoom matched A's entry on \
             (room, user_id))."
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // UpdateMemberDisplayName — HCL issue #828 follow-up
    // ──────────────────────────────────────────────────────────────────────
    // These tests pin the session-scoped rename semantics introduced after
    // PR #851. The bug being fixed: a user with two tabs in the same meeting
    // (same `user_id`, distinct `session` ids) renames Tab A, and Tab B's
    // name also changes because the chat_server iterated all `room_members`
    // matching `msg.user_id`. The fix keys the rename on `(session, user_id)`
    // when `msg.session_id != 0`, and falls back to the legacy user-id-wide
    // path only when the wire packet carried `session_id == 0` (the proto-3
    // default emitted by older clients).
    //
    // Test helper: spin up two sessions of the same `user_id`, send a single
    // `UpdateMemberDisplayName`, and assert exactly which row(s) changed.

    /// Convenience helper for the four rename tests below.
    /// Starts a ChatServer, registers `sessions` against it (each as a
    /// distinct `DummySession`), and returns the actor address plus a vec
    /// of the joined session ids in input order.
    async fn setup_chat_with_sessions(
        room: &str,
        sessions: &[(SessionId, &str)], // (session_id, user_id) pairs
    ) -> actix::Addr<ChatServer> {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");
        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        for (sid, uid) in sessions {
            let dummy = DummySession.start();
            connect_and_join(&chat_server, *sid, room, uid, dummy.recipient(), None).await;
        }
        // Allow async JoinRoom tasks to populate room_members.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        chat_server
    }

    async fn get_members(chat_server: &actix::Addr<ChatServer>, room: &str) -> Vec<RoomMemberInfo> {
        chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed")
    }

    /// Two sessions of the same `user_id` join the same room. A rename
    /// scoped to session A's id MUST update only A's row — B keeps the
    /// display_name it had at JoinRoom time.
    #[actix_rt::test]
    #[serial]
    async fn rename_one_session_only_updates_that_sessions_room_members_row() {
        let room = "rename-scoped-room";
        let user_id = "twin-tabs@example.com";
        let session_a: SessionId = 9001;
        let session_b: SessionId = 9002;

        let chat_server =
            setup_chat_with_sessions(room, &[(session_a, user_id), (session_b, user_id)]).await;

        let pre = get_members(&chat_server, room).await;
        assert_eq!(pre.len(), 2, "both sessions must be tracked");
        let pre_b_name = pre
            .iter()
            .find(|m| m.session == session_b)
            .map(|m| m.display_name.clone())
            .expect("B must be present");

        // Rename A only.
        chat_server
            .send(UpdateMemberDisplayName {
                room_id: room.to_string(),
                user_id: user_id.to_string(),
                display_name: "Tab A renamed".to_string(),
                session_id: session_a,
            })
            .await
            .expect("UpdateMemberDisplayName delivery");

        let post = get_members(&chat_server, room).await;
        let a = post
            .iter()
            .find(|m| m.session == session_a)
            .expect("A still present");
        let b = post
            .iter()
            .find(|m| m.session == session_b)
            .expect("B still present");
        assert_eq!(
            a.display_name, "Tab A renamed",
            "session A's display_name must reflect the rename"
        );
        assert_eq!(
            b.display_name, pre_b_name,
            "session B's display_name must NOT be touched by a session-scoped \
             rename targeting A (HCL issue #828)"
        );
    }

    /// When the wire packet carries `session_id == 0` (legacy/older client),
    /// the handler must fall back to the pre-#828 behaviour and rename every
    /// row matching `user_id`. This is the regression lock-in for older
    /// clients that haven't been updated yet.
    #[actix_rt::test]
    #[serial]
    async fn rename_with_session_id_zero_falls_back_to_user_id_wide_update() {
        let room = "rename-legacy-room";
        let user_id = "legacy-twin@example.com";
        let session_a: SessionId = 9101;
        let session_b: SessionId = 9102;

        let chat_server =
            setup_chat_with_sessions(room, &[(session_a, user_id), (session_b, user_id)]).await;

        chat_server
            .send(UpdateMemberDisplayName {
                room_id: room.to_string(),
                user_id: user_id.to_string(),
                display_name: "Legacy Renamed".to_string(),
                session_id: 0,
            })
            .await
            .expect("UpdateMemberDisplayName delivery");

        let post = get_members(&chat_server, room).await;
        assert!(
            post.iter().all(|m| m.display_name == "Legacy Renamed"),
            "session_id=0 must rename every row matching user_id, got: {:?}",
            post.iter()
                .map(|m| (m.session, m.display_name.clone()))
                .collect::<Vec<_>>()
        );
    }

    /// A stale or invalid `session_id` (one that doesn't exist in the room)
    /// must produce a no-op — NOT a silent fallback to the user-id-wide
    /// path. This is the defence against a forged session_id changing the
    /// authenticated user's name.
    #[actix_rt::test]
    #[serial]
    async fn rename_with_stale_session_id_no_ops_with_warn() {
        let room = "rename-stale-room";
        let user_id = "stale-twin@example.com";
        let session_a: SessionId = 9201;
        let session_b: SessionId = 9202;

        let chat_server =
            setup_chat_with_sessions(room, &[(session_a, user_id), (session_b, user_id)]).await;
        let pre = get_members(&chat_server, room).await;
        let pre_snapshot: Vec<(SessionId, String)> = pre
            .iter()
            .map(|m| (m.session, m.display_name.clone()))
            .collect();

        // session_id that doesn't exist in the room at all.
        chat_server
            .send(UpdateMemberDisplayName {
                room_id: room.to_string(),
                user_id: user_id.to_string(),
                display_name: "Should Be Ignored".to_string(),
                session_id: 7777,
            })
            .await
            .expect("UpdateMemberDisplayName delivery");

        let post = get_members(&chat_server, room).await;
        let post_snapshot: Vec<(SessionId, String)> = post
            .iter()
            .map(|m| (m.session, m.display_name.clone()))
            .collect();
        assert_eq!(
            pre_snapshot, post_snapshot,
            "stale session_id must NOT change any room_members row — silent \
             no-op (with warn log) is mandatory; falling through to the \
             user-id-wide path would let a forged session_id rewrite the \
             authenticated user's name"
        );
    }

    /// A `session_id` that exists in the room but belongs to a *different*
    /// user (not the one in `msg.user_id`) must also produce a no-op. This
    /// defends against a request that smuggled a session_id from another
    /// participant — neither the smuggling user's name nor the unrelated
    /// participant's name may change.
    #[actix_rt::test]
    #[serial]
    async fn rename_with_session_id_belonging_to_different_user_no_ops() {
        let room = "rename-cross-user-room";
        let user_alice = "alice@example.com";
        let user_bob = "bob@example.com";
        let session_alice: SessionId = 9301;
        let session_bob: SessionId = 9302;

        let chat_server = setup_chat_with_sessions(
            room,
            &[(session_alice, user_alice), (session_bob, user_bob)],
        )
        .await;

        let pre = get_members(&chat_server, room).await;
        let pre_snapshot: Vec<(SessionId, String, String)> = pre
            .iter()
            .map(|m| (m.session, m.user_id.clone(), m.display_name.clone()))
            .collect();

        // Alice submits the rename but smuggles in Bob's session_id.
        chat_server
            .send(UpdateMemberDisplayName {
                room_id: room.to_string(),
                user_id: user_alice.to_string(),
                display_name: "Hijack Attempt".to_string(),
                session_id: session_bob,
            })
            .await
            .expect("UpdateMemberDisplayName delivery");

        let post = get_members(&chat_server, room).await;
        let post_snapshot: Vec<(SessionId, String, String)> = post
            .iter()
            .map(|m| (m.session, m.user_id.clone(), m.display_name.clone()))
            .collect();
        assert_eq!(
            pre_snapshot, post_snapshot,
            "cross-user session_id must NOT mutate any row — Bob's row must \
             survive untouched, and Alice's row must NOT silently fall \
             through to the user-id-wide path"
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // Test-only actor messages used to peek at chat_server internals
    // without exposing them in the production API surface.
    // ──────────────────────────────────────────────────────────────────────

    #[derive(ActixMessage)]
    #[rtype(result = "Option<bool>")]
    struct GetRoomMemberEndOnHostLeave {
        room: String,
        session: SessionId,
    }

    impl Handler<GetRoomMemberEndOnHostLeave> for ChatServer {
        type Result = MessageResult<GetRoomMemberEndOnHostLeave>;
        fn handle(
            &mut self,
            msg: GetRoomMemberEndOnHostLeave,
            _ctx: &mut Self::Context,
        ) -> Self::Result {
            MessageResult(
                self.room_members
                    .get(&msg.room)
                    .and_then(|members| members.iter().find(|m| m.session == msg.session))
                    .map(|m| m.end_on_host_leave),
            )
        }
    }

    #[derive(ActixMessage)]
    #[rtype(result = "Option<bool>")]
    struct GetRoomPolicyEndOnHostLeave {
        room: String,
    }

    impl Handler<GetRoomPolicyEndOnHostLeave> for ChatServer {
        type Result = MessageResult<GetRoomPolicyEndOnHostLeave>;
        fn handle(
            &mut self,
            msg: GetRoomPolicyEndOnHostLeave,
            _ctx: &mut Self::Context,
        ) -> Self::Result {
            MessageResult(self.room_policy.get(&msg.room).map(|p| p.end_on_host_leave))
        }
    }

    // ======================================================================
    // Per-receiver simulcast LAYER selection (#989, Phase 1b)
    // ======================================================================
    //
    // These exercise the layer-drop check in `handle_msg` (after the viewport
    // filter) and the `try_intercept_layer_preference` control-packet
    // interceptor in isolation. None require NATS.

    #[actix_rt::test]
    async fn test_handle_msg_empty_layer_prefs_is_noop_forward() {
        // NO-OP-FIRST: with NO recorded layer preference, a simulcast VIDEO
        // packet (layer != 0) is forwarded byte-identically to pre-#989.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "lp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(), // empty viewport = fail-open (forward all senders)
            empty_layer_prefs(),       // empty prefs = no-op (forward all layers)
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.lp-room.999",
            make_media_packet_bytes_with_layer(MediaKind::VIDEO, 999, 2),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "with no recorded layer preference, simulcast VIDEO MUST be forwarded (no-op)"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_drops_non_matching_layer() {
        // Receiver wants layer 1 from source 999; a layer-2 packet from 999 is
        // dropped.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "lp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(),
            layer_prefs_with(&[(999, 1)]),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.lp-room.999",
            make_media_packet_bytes_with_layer(MediaKind::VIDEO, 999, 2),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "simulcast VIDEO whose layer != the recorded preference MUST be dropped"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_forwards_matching_layer() {
        // Receiver wants layer 2 from source 999; a layer-2 packet is forwarded.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "lp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(),
            layer_prefs_with(&[(999, 2)]),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.lp-room.999",
            make_media_packet_bytes_with_layer(MediaKind::VIDEO, 999, 2),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "simulcast VIDEO whose layer matches the recorded preference MUST be forwarded"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_layer_zero_always_forwarded() {
        // simulcast_layer_id == 0 (base / un-upgraded publisher) is ALWAYS
        // forwarded even when the receiver has a (non-zero) preference for the
        // source. Layer 0 is the no-op gate on the media side.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "lp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(),
            layer_prefs_with(&[(999, 2)]),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.lp-room.999",
            make_media_packet_bytes_with_layer(MediaKind::VIDEO, 999, 0),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "layer 0 (base) MUST always be forwarded regardless of preference"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_audio_not_filtered_by_video_preference() {
        // Phase 3: layer prefs are keyed by (source, media_kind). An AUDIO
        // packet is NOT filtered by a VIDEO-kind preference for the same source
        // — the keys differ, so it fails open (forward). (Pre-Phase-3 this held
        // because audio was excluded entirely; now it holds because of the
        // per-kind key.)
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "lp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(),
            layer_prefs_with(&[(999, 1)]), // keyed (999, VIDEO)
            "websocket".to_string(),
        );

        // AUDIO layer 2 with only a VIDEO preference for 999 → forward.
        let nats_msg = make_nats_message(
            "room.lp-room.999",
            make_media_packet_bytes_with_layer(MediaKind::AUDIO, 999, 2),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "AUDIO must not be filtered by a VIDEO-kind preference (per-kind key)"
        );
    }

    // ===== Per-layer forwarded distribution counter (#1105) =====

    /// `layer_id_bucket` MUST clamp the forgeable wire id into EXACTLY the four
    /// bounded label values 0|1|2|other. This is the cardinality guarantee — if
    /// it ever returns anything else (or stops collapsing large ids), the
    /// counter's series count becomes unbounded. The asserts pin the real
    /// boundaries (2 stays "2", 3 and u32::MAX both collapse to "other"), so the
    /// test fails if the match arms are widened or the catch-all is removed.
    #[test]
    fn test_layer_id_bucket_is_bounded() {
        assert_eq!(layer_id_bucket(0), "0");
        assert_eq!(layer_id_bucket(1), "1");
        assert_eq!(layer_id_bucket(2), "2");
        // The whole point: everything above the real 0..=2 ladder — including
        // the next ladder rung (3) and a forged u32::MAX — collapses to ONE
        // bucket, so the label set can never exceed 4 distinct values.
        assert_eq!(layer_id_bucket(3), "other");
        assert_eq!(layer_id_bucket(7), "other");
        assert_eq!(layer_id_bucket(u32::MAX), "other");

        // The complete set of buckets the function can EVER emit is exactly 4.
        // Sample a wide range and prove no fifth value appears.
        let mut seen: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
        for id in 0u32..1000 {
            seen.insert(layer_id_bucket(id));
        }
        seen.insert(layer_id_bucket(u32::MAX));
        assert_eq!(
            seen.len(),
            4,
            "layer_id_bucket must emit exactly 4 bounded label values, got {seen:?}"
        );
    }

    /// A forwarded media packet increments the per-layer distribution counter on
    /// the bucket matching its `simulcast_layer_id`. Uses a UNIQUE room so the
    /// process-global lazy_static counter is isolated from other tests (we read
    /// the absolute value of a freshly-created series).
    #[actix_rt::test]
    async fn test_handle_msg_per_layer_counter_records_forwarded_layer() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        // Unique room → fresh, isolated counter series.
        let room = "per-layer-room-l2";
        let handler = handle_msg(
            actor.recipient(),
            room.to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(),
            // Receiver wants layer 2 from source 999 → a layer-2 packet matches
            // and is forwarded.
            layer_prefs_with(&[(999, 2)]),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            &format!("room.{room}.999"),
            make_media_packet_bytes_with_layer(MediaKind::VIDEO, 999, 2),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "matching-layer VIDEO must be forwarded"
        );
        // The forwarded packet was layer 2 → the "2" bucket is incremented once,
        // and the other buckets stay at zero for this room.
        assert_eq!(
            RELAY_LAYER_FORWARDED_BY_LAYER_TOTAL
                .with_label_values(&[room, "2"])
                .get() as u64,
            1,
            "forwarding a layer-2 packet must increment the layer_id=2 bucket"
        );
        assert_eq!(
            RELAY_LAYER_FORWARDED_BY_LAYER_TOTAL
                .with_label_values(&[room, "1"])
                .get() as u64,
            0,
            "layer-2 forward must NOT touch the layer_id=1 bucket"
        );
    }

    /// A LAYER-FILTERED (dropped) packet must NOT increment the per-layer
    /// distribution counter — the counter reflects only what is actually
    /// forwarded. This pins the increment to the post-filter forward path: if
    /// the increment were moved BEFORE the layer drop gate, the dropped layer's
    /// bucket would wrongly read 1 and this test would fail.
    #[actix_rt::test]
    async fn test_handle_msg_per_layer_counter_skips_filtered_layer() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let room = "per-layer-room-drop";
        let handler = handle_msg(
            actor.recipient(),
            room.to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(),
            // Receiver wants layer 0 from source 999, so a layer-2 packet is
            // DROPPED by the layer filter.
            layer_prefs_with(&[(999, 0)]),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            &format!("room.{room}.999"),
            make_media_packet_bytes_with_layer(MediaKind::VIDEO, 999, 2),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "non-matching-layer VIDEO must be dropped"
        );
        assert_eq!(
            RELAY_LAYER_FORWARDED_BY_LAYER_TOTAL
                .with_label_values(&[room, "2"])
                .get() as u64,
            0,
            "a layer-FILTERED packet must NOT increment any forwarded-by-layer bucket"
        );
    }

    /// A forwarded packet whose wire layer id is ABOVE the real 0..=2 ladder
    /// (here a forged large id) lands in the bounded "other" bucket, never in a
    /// per-id series. This is the runtime proof of the cardinality clamp on the
    /// real forwarding path (not just the unit test of `layer_id_bucket`). The
    /// receiver has no prefs (fail-open forward), so the high-id packet is
    /// forwarded and counted.
    #[actix_rt::test]
    async fn test_handle_msg_per_layer_counter_buckets_forged_layer_into_other() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let room = "per-layer-room-other";
        let handler = handle_msg(
            actor.recipient(),
            room.to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(),
            // No recorded preference → fail-open: every layer forwards, so the
            // forged-id packet reaches the per-layer counter.
            empty_layer_prefs(),
            "websocket".to_string(),
        );

        // A forged layer id well above the real ladder.
        let nats_msg = make_nats_message(
            &format!("room.{room}.999"),
            make_media_packet_bytes_with_layer(MediaKind::VIDEO, 999, u32::MAX),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "no-prefs fail-open must forward the packet"
        );
        assert_eq!(
            RELAY_LAYER_FORWARDED_BY_LAYER_TOTAL
                .with_label_values(&[room, "other"])
                .get() as u64,
            1,
            "a forged out-of-ladder layer id must land in the bounded \"other\" bucket"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_drops_non_matching_screen_layer() {
        // Phase 3: SCREEN is layer-filtered like VIDEO. Receiver wants screen
        // layer 0 from source 999; a screen layer-2 packet is dropped.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "lp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(),
            layer_prefs_with_kinds(&[(999, 3 /* SCREEN */, 0)]),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.lp-room.999",
            make_media_packet_bytes_with_layer(MediaKind::SCREEN, 999, 2),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "SCREEN whose layer != the recorded SCREEN preference MUST be dropped"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_forwards_matching_screen_layer() {
        // Phase 3: a SCREEN packet matching the recorded SCREEN preference is
        // forwarded.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "lp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(),
            layer_prefs_with_kinds(&[(999, 3 /* SCREEN */, 1)]),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.lp-room.999",
            make_media_packet_bytes_with_layer(MediaKind::SCREEN, 999, 1),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "matching SCREEN forwarded"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_drops_non_matching_audio_layer() {
        // Phase 3: AUDIO is layer-filtered when an AUDIO-kind preference exists.
        // Receiver wants audio layer 0 from 999; an audio layer-1 packet drops.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "lp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(),
            layer_prefs_with_kinds(&[(999, 2 /* AUDIO */, 0)]),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.lp-room.999",
            make_media_packet_bytes_with_layer(MediaKind::AUDIO, 999, 1),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "AUDIO whose layer != the recorded AUDIO preference MUST be dropped"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_per_kind_independence_same_source() {
        // Phase 3 core: camera VIDEO and SCREEN of the SAME source are addressed
        // independently. Receiver wants VIDEO layer 2 but SCREEN layer 0 from
        // 999. A VIDEO layer-2 packet forwards; a SCREEN layer-2 packet drops.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "lp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(),
            layer_prefs_with_kinds(&[(999, 1 /* VIDEO */, 2), (999, 3 /* SCREEN */, 0)]),
            "websocket".to_string(),
        );

        // VIDEO layer 2 matches the VIDEO pref → forward.
        let video_msg = make_nats_message(
            "room.lp-room.999",
            make_media_packet_bytes_with_layer(MediaKind::VIDEO, 999, 2),
        );
        let vparsed = parse_pw(&video_msg);
        handler(video_msg, vparsed.as_ref()).expect("ok");

        // SCREEN layer 2 does NOT match the SCREEN pref (wants 0) → drop.
        let screen_msg = make_nats_message(
            "room.lp-room.999",
            make_media_packet_bytes_with_layer(MediaKind::SCREEN, 999, 2),
        );
        let sparsed = parse_pw(&screen_msg);
        handler(screen_msg, sparsed.as_ref()).expect("ok");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "VIDEO forwarded + SCREEN dropped independently for the same source"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_unknown_source_layer_fails_open() {
        // Receiver has a preference for source 999, but a layer-2 packet for a
        // DIFFERENT source (777) has no recorded preference → fail-open
        // (forward).
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "lp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(),
            layer_prefs_with(&[(999, 1)]),
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.lp-room.777",
            make_media_packet_bytes_with_layer(MediaKind::VIDEO, 777, 2),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "a source with no recorded layer preference MUST fail open (forward)"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_screen_not_filtered_by_video_preference() {
        // #1070: per-kind independence (Phase 3). A SCREEN packet whose layer
        // does NOT match must STILL be forwarded when the receiver's only
        // recorded preference is VIDEO-keyed for the same source — the layer
        // filter keys on (source, media_kind), so a (999, VIDEO) preference can
        // never drop a (999, SCREEN) packet. (Mirrors the AUDIO-vs-VIDEO test;
        // proves SCREEN is not collaterally filtered by a camera preference. A
        // SCREEN packet IS filtered only by a SCREEN-keyed preference, which is
        // covered by `test_handle_msg_drops_non_matching_screen_layer`.)
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "lp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(),
            layer_prefs_with(&[(999, 1)]), // keyed (999, VIDEO) only
            "websocket".to_string(),
        );

        // SCREEN layer 2 with only a VIDEO preference for 999 → forward (the
        // (999, SCREEN) key has no entry, so the SCREEN packet fails open). If
        // SCREEN were ever filtered by the VIDEO key this would drop (count 0).
        let nats_msg = make_nats_message(
            "room.lp-room.999",
            make_media_packet_bytes_with_layer(MediaKind::SCREEN, 999, 2),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "SCREEN must not be filtered by a VIDEO-kind preference (per-kind key)"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_desired_layer_zero_is_base_only() {
        // #1070: a recorded preference of `desired_layer = 0` for a source means
        // "base layer only" — any NON-zero layer from that source is dropped,
        // and the layer-0 (base) packet is forwarded. This pins the exact
        // base-only contract: it is the recorded preference VALUE (0), not the
        // absence of a preference, that drives the drop of higher layers.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "lp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(),
            layer_prefs_with(&[(999, 0)]), // (999, VIDEO) → desired layer 0 (base only)
            "websocket".to_string(),
        );

        // Layer 2 from 999 must be DROPPED (preference selects base 0, layer 2 != 0).
        let layer2_msg = make_nats_message(
            "room.lp-room.999",
            make_media_packet_bytes_with_layer(MediaKind::VIDEO, 999, 2),
        );
        let l2parsed = parse_pw(&layer2_msg);
        handler(layer2_msg, l2parsed.as_ref()).expect("handler should not return Err");

        // Layer 0 from 999 must be FORWARDED (base is always forwarded; the
        // non-zero-layer gate excludes it before the preference is even read).
        let layer0_msg = make_nats_message(
            "room.lp-room.999",
            make_media_packet_bytes_with_layer(MediaKind::VIDEO, 999, 0),
        );
        let l0parsed = parse_pw(&layer0_msg);
        handler(layer0_msg, l0parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "desired_layer=0 must drop the layer-2 packet and forward only the base (layer-0) packet"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_poisoned_layer_prefs_lock_fails_open() {
        // #1070: a POISONED LayerPrefs RwLock must FAIL OPEN — a video packet
        // that the recorded preference would otherwise DROP is forwarded
        // instead, because the read-lock `.map(...).unwrap_or(false)` on the
        // forwarding path treats a lock error as "do not drop". The fast-path
        // hint (`has_any()`) is still `true` (it is a separate AtomicBool, not
        // affected by poisoning), so the forwarding path DOES take the read
        // lock — exercising exactly the `unwrap_or(false)` fail-open arm.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        // Receiver wants layer 1 from 999 → a layer-2 packet would normally drop.
        let prefs = layer_prefs_with(&[(999, 1)]);

        // Poison the prefs lock by panicking while holding the write guard.
        let poison_target = prefs.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poison_target.state.write().unwrap();
            panic!("intentional panic to poison the LayerPrefs lock");
        }));
        assert!(
            prefs.state.read().is_err(),
            "precondition: the LayerPrefs lock must be poisoned"
        );
        assert!(
            prefs.has_any(),
            "precondition: the non_empty hint must still be true so the forwarding \
             path takes the read lock and exercises the fail-open arm"
        );

        let handler = handle_msg(
            actor.recipient(),
            "lp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(),
            prefs,
            "websocket".to_string(),
        );

        // Layer 2 from 999: the preference selects layer 1, so WITHOUT the
        // poison this packet would be dropped. With the lock poisoned it must
        // fail open and be forwarded.
        let nats_msg = make_nats_message(
            "room.lp-room.999",
            make_media_packet_bytes_with_layer(MediaKind::VIDEO, 999, 2),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "a poisoned LayerPrefs lock MUST fail open (forward the packet it would otherwise drop)"
        );
    }

    #[test]
    fn test_intercept_layer_preference_records_own_map() {
        // A LAYER_PREFERENCE on the receiver's OWN subject updates the map.
        let prefs = LayerPrefs::default();
        let self_subject = self_subject_for("r", 100);
        let msg = make_nats_message(
            &self_subject,
            make_layer_preference_packet_bytes(100, &[(200, 1), (300, 2)]),
        );
        let parsed = parse_pw(&msg);
        let intercepted = try_intercept_layer_preference(
            &msg,
            parsed.as_ref(),
            &self_subject,
            &prefs,
            "r",
            &|_| {},
            100,
        );
        assert!(
            intercepted,
            "LAYER_PREFERENCE packet must be intercepted (dropped)"
        );
        // The lock-free hot-path hint must be raised once a preference is
        // recorded, so the forwarding path stops taking the empty-prefs fast
        // path and consults the map.
        assert!(
            prefs.has_any(),
            "non_empty hint must be set after an accepted update"
        );
        let st = prefs.state.read().unwrap();
        // Phase 3: entries without media_kind default to the VIDEO(1) key.
        assert_eq!(st.layers.get(&(200, 1)), Some(&1));
        assert_eq!(st.layers.get(&(300, 1)), Some(&2));
    }

    #[test]
    fn test_intercept_layer_preference_records_per_media_kind() {
        // Phase 3: an Entry's media_kind is recorded as part of the key, so the
        // SAME source can carry distinct preferences for VIDEO vs SCREEN vs
        // AUDIO, and UNSPECIFIED(0) maps to the VIDEO key (back-compat).
        let prefs = LayerPrefs::default();
        let self_subject = self_subject_for("r", 100);
        let msg = make_nats_message(
            &self_subject,
            make_layer_preference_packet_bytes_kinded(
                100,
                &[
                    (200, 0 /* UNSPECIFIED → VIDEO */, 1),
                    (200, 3 /* SCREEN */, 0),
                    (200, 2 /* AUDIO */, 0),
                ],
            ),
        );
        let parsed = parse_pw(&msg);
        assert!(try_intercept_layer_preference(
            &msg,
            parsed.as_ref(),
            &self_subject,
            &prefs,
            "r",
            &|_| {},
            100,
        ));
        let st = prefs.state.read().unwrap();
        assert_eq!(
            st.layers.get(&(200, 1)),
            Some(&1),
            "UNSPECIFIED keys as VIDEO"
        );
        assert_eq!(
            st.layers.get(&(200, 3)),
            Some(&0),
            "SCREEN keyed distinctly"
        );
        assert_eq!(st.layers.get(&(200, 2)), Some(&0), "AUDIO keyed distinctly");
        assert_eq!(st.layers.len(), 3, "three independent (source,kind) keys");
    }

    #[test]
    fn test_layer_prefs_empty_hint_is_false_by_default() {
        // The empty-prefs fast path depends on the hint being false until the
        // first accepted update — this is what keeps the no-preference interim
        // lock-free.
        let prefs = LayerPrefs::default();
        assert!(
            !prefs.has_any(),
            "a fresh LayerPrefs must report has_any() == false (empty-prefs fast path)"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_empty_prefs_fast_path_forwards_nonzero_layer() {
        // Explicit coverage of the lock-free empty-prefs early-out: a non-zero
        // simulcast layer with NO recorded preference is forwarded (no-op),
        // exercising the `has_any() == false` short-circuit.
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let prefs = empty_layer_prefs();
        assert!(!prefs.has_any(), "precondition: prefs start empty");

        let handler = handle_msg(
            actor.recipient(),
            "lp-room".to_string(),
            100,
            false,
            "recv".to_string(),
            DesiredStreams::default(),
            prefs,
            "websocket".to_string(),
        );

        let nats_msg = make_nats_message(
            "room.lp-room.999",
            make_media_packet_bytes_with_layer(MediaKind::VIDEO, 999, 3),
        );
        let parsed = parse_pw(&nats_msg);
        handler(nats_msg, parsed.as_ref()).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "empty-prefs fast path MUST forward a non-zero-layer VIDEO packet"
        );
    }

    #[test]
    fn test_intercept_layer_preference_other_subject_does_not_mutate() {
        // A LAYER_PREFERENCE that arrived on a DIFFERENT publisher's subject is
        // dropped but MUST NOT mutate this receiver's map. This is the
        // field-5 / forged-payload trust boundary (#993): a forged value can
        // only self-degrade the forger's OWN view.
        let prefs = layer_prefs_with(&[(200, 1)]);
        let self_subject = self_subject_for("r", 100);
        // Packet arrived on session 555's subject (forging owner=100 in the
        // payload), not ours (100).
        let msg = make_nats_message(
            "room.r.555",
            make_layer_preference_packet_bytes(100, &[(200, 9), (999, 3)]),
        );
        let parsed = parse_pw(&msg);
        let intercepted = try_intercept_layer_preference(
            &msg,
            parsed.as_ref(),
            &self_subject,
            &prefs,
            "r",
            &|_| {},
            100,
        );
        assert!(intercepted, "LAYER_PREFERENCE must still be consumed");
        let st = prefs.state.read().unwrap();
        assert_eq!(
            st.layers.get(&(200, 1)),
            Some(&1),
            "another session's LAYER_PREFERENCE MUST NOT overwrite our map"
        );
        assert!(
            !st.layers.contains_key(&(999, 1)),
            "another session's LAYER_PREFERENCE MUST NOT add entries to our map"
        );
    }

    #[test]
    fn test_intercept_layer_preference_non_layer_packet_falls_through() {
        // A non-LAYER_PREFERENCE packet returns false (caller falls through to
        // handle_msg) and does not touch the map.
        let prefs = LayerPrefs::default();
        let self_subject = self_subject_for("r", 100);
        let msg = make_nats_message(
            &self_subject,
            make_media_packet_bytes_with_layer(MediaKind::VIDEO, 100, 1),
        );
        let parsed = parse_pw(&msg);
        assert!(
            !try_intercept_layer_preference(
                &msg,
                parsed.as_ref(),
                &self_subject,
                &prefs,
                "r",
                &|_| {},
                100,
            ),
            "non-LAYER_PREFERENCE packet must fall through (return false)"
        );
        assert!(prefs.state.read().unwrap().layers.is_empty());
    }

    #[test]
    fn test_intercept_layer_preference_caps_entries() {
        // A LAYER_PREFERENCE with more than LAYER_PREFERENCE_MAX_ENTRIES entries
        // is truncated to the cap (DoS guard).
        let prefs = LayerPrefs::default();
        let self_subject = self_subject_for("r", 100);
        let big: Vec<(u64, u32)> = (0..(LAYER_PREFERENCE_MAX_ENTRIES as u64 + 50))
            .map(|i| (1000 + i, 1u32))
            .collect();
        let msg = make_nats_message(&self_subject, make_layer_preference_packet_bytes(100, &big));
        let parsed = parse_pw(&msg);
        assert!(try_intercept_layer_preference(
            &msg,
            parsed.as_ref(),
            &self_subject,
            &prefs,
            "r",
            &|_| {},
            100,
        ));
        assert_eq!(
            prefs.state.read().unwrap().layers.len(),
            LAYER_PREFERENCE_MAX_ENTRIES,
            "accepted entries must be capped at LAYER_PREFERENCE_MAX_ENTRIES"
        );
    }

    #[test]
    fn test_intercept_layer_preference_bounds_desired_layer() {
        // #1082 defense-in-depth: an entry whose `desired_layer` exceeds
        // LAYER_PREFERENCE_MAX_LAYER_ID is SKIPPED (not recorded), while
        // in-bound entries in the SAME packet are still recorded. Skipping is
        // fail-open per source: the out-of-bound source has no recorded
        // preference, so the forwarding path forwards all its layers (base-and-up)
        // exactly as if no preference had been sent for it.
        let prefs = LayerPrefs::default();
        let self_subject = self_subject_for("r", 100);

        // Source 200: in-bound layer (1) → recorded.
        // Source 300: at the exact bound → recorded (inclusive upper bound).
        // Source 400: one past the bound → skipped.
        // Source 500: wildly forged u32::MAX → skipped.
        let msg = make_nats_message(
            &self_subject,
            make_layer_preference_packet_bytes_kinded(
                100,
                &[
                    (200, 1 /* VIDEO */, 1),
                    (300, 1 /* VIDEO */, LAYER_PREFERENCE_MAX_LAYER_ID),
                    (400, 1 /* VIDEO */, LAYER_PREFERENCE_MAX_LAYER_ID + 1),
                    (500, 1 /* VIDEO */, u32::MAX),
                ],
            ),
        );
        let parsed = parse_pw(&msg);
        assert!(try_intercept_layer_preference(
            &msg,
            parsed.as_ref(),
            &self_subject,
            &prefs,
            "r",
            &|_| {},
            100,
        ));
        let st = prefs.state.read().unwrap();
        assert_eq!(
            st.layers.get(&(200, 1)),
            Some(&1),
            "in-bound desired_layer must be recorded"
        );
        assert_eq!(
            st.layers.get(&(300, 1)),
            Some(&LAYER_PREFERENCE_MAX_LAYER_ID),
            "desired_layer at the exact bound must be recorded (inclusive)"
        );
        assert!(
            !st.layers.contains_key(&(400, 1)),
            "desired_layer one past the bound must be SKIPPED (not recorded)"
        );
        assert!(
            !st.layers.contains_key(&(500, 1)),
            "forged u32::MAX desired_layer must be SKIPPED (not recorded)"
        );
        assert_eq!(
            st.layers.len(),
            2,
            "exactly the two in-bound entries are recorded"
        );
        assert!(
            prefs.has_any(),
            "the non-empty hint must reflect the recorded in-bound entries"
        );
    }

    #[test]
    fn test_intercept_layer_preference_all_out_of_bound_records_nothing() {
        // #1082: a packet whose every entry is out-of-bound records NOTHING and
        // leaves the session in the empty / fail-open state (the non-empty hint
        // stays false), so the forwarding path is byte-identical to no-preference.
        // Subject ownership is still honored (the packet arrived on self_subject).
        let prefs = LayerPrefs::default();
        let self_subject = self_subject_for("r", 100);
        let msg = make_nats_message(
            &self_subject,
            make_layer_preference_packet_bytes_kinded(
                100,
                &[
                    (200, 1 /* VIDEO */, LAYER_PREFERENCE_MAX_LAYER_ID + 1),
                    (300, 2 /* AUDIO */, u32::MAX),
                ],
            ),
        );
        let parsed = parse_pw(&msg);
        assert!(try_intercept_layer_preference(
            &msg,
            parsed.as_ref(),
            &self_subject,
            &prefs,
            "r",
            &|_| {},
            100,
        ));
        assert!(
            prefs.state.read().unwrap().layers.is_empty(),
            "an all-out-of-bound packet must record no entries (fail-open)"
        );
        assert!(
            !prefs.has_any(),
            "the non-empty hint must stay false when nothing is recorded"
        );
    }

    #[test]
    fn test_intercept_layer_preference_out_of_bound_other_subject_ignored() {
        // #1082 + subject-authoritative: an out-of-bound (or any) entry arriving
        // on a DIFFERENT subject than self_subject is dropped without mutating
        // state — the bound check never even runs for non-owned packets.
        let prefs = LayerPrefs::default();
        let self_subject = self_subject_for("r", 100);
        let other_subject = self_subject_for("r", 999);
        let msg = make_nats_message(
            &other_subject,
            make_layer_preference_packet_bytes_kinded(
                999,
                &[(200, 1 /* VIDEO */, LAYER_PREFERENCE_MAX_LAYER_ID + 1)],
            ),
        );
        let parsed = parse_pw(&msg);
        // Consumed (true) but never recorded — ownership decided by subject.
        assert!(try_intercept_layer_preference(
            &msg,
            parsed.as_ref(),
            &self_subject,
            &prefs,
            "r",
            &|_| {},
            100,
        ));
        assert!(
            prefs.state.read().unwrap().layers.is_empty(),
            "a LAYER_PREFERENCE on another subject must never mutate our map"
        );
    }

    #[test]
    fn test_intercept_layer_preference_rate_limited() {
        // A second update within LAYER_PREFERENCE_MIN_UPDATE_INTERVAL is
        // consumed but ignored (the map keeps the first update's contents).
        let prefs = LayerPrefs::default();
        let self_subject = self_subject_for("r", 100);

        let msg1 = make_nats_message(
            &self_subject,
            make_layer_preference_packet_bytes(100, &[(200, 1)]),
        );
        let parsed1 = parse_pw(&msg1);
        assert!(try_intercept_layer_preference(
            &msg1,
            parsed1.as_ref(),
            &self_subject,
            &prefs,
            "r",
            &|_| {},
            100,
        ));
        assert_eq!(prefs.state.read().unwrap().layers.get(&(200, 1)), Some(&1));

        // Immediate second update (well within the rate-limit window).
        let msg2 = make_nats_message(
            &self_subject,
            make_layer_preference_packet_bytes(100, &[(200, 5)]),
        );
        let parsed2 = parse_pw(&msg2);
        assert!(try_intercept_layer_preference(
            &msg2,
            parsed2.as_ref(),
            &self_subject,
            &prefs,
            "r",
            &|_| {},
            100,
        ));
        assert_eq!(
            prefs.state.read().unwrap().layers.get(&(200, 1)),
            Some(&1),
            "rate-limited update must NOT mutate the map"
        );
    }

    // ======================================================================
    // Publish-side layer suppression — per-source layer union + debounce
    // (#1108, Stage 3 — LAYER_HINT)
    // ======================================================================
    //
    // These exercise the relay-side union computation, the change-detection used
    // to trigger recomputes, the suppress-lazy / restore-eager debounce state
    // machine, and the forge-resistance guarantee (no inbound LAYER_HINT ingest).
    // They test the PURE functions directly and therefore need neither NATS nor a
    // running actor.

    /// One `(source_session, media_kind, desired_layer)` preference entry, as
    /// consumed by [`layer_prefs_with_kinds`]. Aliased to keep the union-test
    /// helper signatures under clippy's type-complexity threshold.
    type LayerEntrySpec = (u64, i32, u32);
    /// One receiver's spec: its session id plus the entries it has recorded.
    type ReceiverSpec<'a> = (SessionId, &'a [LayerEntrySpec]);

    /// Build a receiver-keyed prefs map from `(receiver, &[(source, kind, layer)])`
    /// specs, mirroring the real `session_layer_prefs` shape. A receiver listed
    /// with an EMPTY slice has a `LayerPrefs` whose map is empty (still a recorded
    /// session, but no preference for any source → fail-open per (source,kind)).
    fn receivers_map(specs: &[ReceiverSpec<'_>]) -> HashMap<SessionId, LayerPrefs> {
        specs
            .iter()
            .map(|&(receiver, entries)| (receiver, layer_prefs_with_kinds(entries)))
            .collect()
    }

    const VIDEO_KIND: i32 = 1;

    /// Build a `(source, kind) -> desired_layer` map for the demand-gauge
    /// classifier (#1170), mirroring `LayerPrefsState.layers`.
    fn layers_map(entries: &[(u64, i32, u32)]) -> HashMap<(u64, i32), u32> {
        entries.iter().map(|&(s, k, l)| ((s, k), l)).collect()
    }

    /// Resolve a kind's classified bucket from the parallel-indexed result of
    /// `classify_session_max_layer_buckets`, by the kind's gauge label.
    fn bucket_for_kind<'a>(
        classified: &[Option<&'a str>; LAYER_PREFERENCE_GAUGE_KINDS.len()],
        kind_label: &str,
    ) -> Option<&'a str> {
        let idx = LAYER_PREFERENCE_GAUGE_KINDS
            .iter()
            .position(|(_, label)| *label == kind_label)
            .expect("kind_label must be a known gauge kind");
        classified[idx]
    }

    /// Pins the #1170 demand-gauge aggregation: each receiver is classified by
    /// its MAX requested layer PER KIND, an empty map contributes nothing, and a
    /// forged layer id buckets to "other". This is the source-of-truth reduction
    /// the gauge depends on — it must fail if the max is swapped for min, if the
    /// per-kind split is broken, or if fail-open exclusion regresses.
    #[test]
    fn test_classify_session_max_layer_buckets() {
        const SCREEN_KIND: i32 = 3;

        // Receiver wants {(srcA,VIDEO):0, (srcB,VIDEO):2, (srcC,SCREEN):1}.
        // MAX over VIDEO is 2 → bucket "2"; SCREEN has a single entry 1 → "1".
        let m = layers_map(&[
            (10, VIDEO_KIND, 0),
            (11, VIDEO_KIND, 2),
            (12, SCREEN_KIND, 1),
        ]);
        let classified = classify_session_max_layer_buckets(&m);
        assert_eq!(
            bucket_for_kind(&classified, "video"),
            Some("2"),
            "VIDEO must classify by the MAX requested layer (2), not the min (0) or first-seen"
        );
        assert_eq!(
            bucket_for_kind(&classified, "screen"),
            Some("1"),
            "SCREEN is classified independently of VIDEO"
        );

        // Empty map = no demand expressed for any kind → every kind is None
        // (uncounted / fail-open), contributing nothing to the gauge.
        let empty = layers_map(&[]);
        let classified_empty = classify_session_max_layer_buckets(&empty);
        assert_eq!(bucket_for_kind(&classified_empty, "video"), None);
        assert_eq!(bucket_for_kind(&classified_empty, "screen"), None);

        // A receiver with only VIDEO prefs is fail-open (None) for SCREEN, not
        // counted as some default bucket.
        let video_only = layers_map(&[(10, VIDEO_KIND, 1)]);
        let classified_vo = classify_session_max_layer_buckets(&video_only);
        assert_eq!(bucket_for_kind(&classified_vo, "video"), Some("1"));
        assert_eq!(
            bucket_for_kind(&classified_vo, "screen"),
            None,
            "no SCREEN entry = fail-open = uncounted, NOT a default bucket"
        );

        // A forged / out-of-ladder layer id (9) must collapse to "other" so it
        // cannot create an unbounded label series. Use it as the MAX so the
        // bucketing of the chosen max is what is under test.
        let forged = layers_map(&[(10, VIDEO_KIND, 0), (11, VIDEO_KIND, 9)]);
        let classified_forged = classify_session_max_layer_buckets(&forged);
        assert_eq!(
            bucket_for_kind(&classified_forged, "video"),
            Some("other"),
            "a forged/out-of-ladder layer id (9) must bucket to \"other\""
        );

        // AUDIO has no simulcast ladder: even a (bogus) AUDIO entry must not
        // appear as a tracked kind (only VIDEO+SCREEN are gauge kinds).
        const AUDIO_KIND: i32 = 2;
        assert!(
            !LAYER_PREFERENCE_GAUGE_KINDS
                .iter()
                .any(|(k, _)| *k == AUDIO_KIND),
            "AUDIO must NOT be a demand-gauge kind (no simulcast layers)"
        );
    }

    /// The sweep's kind taxonomy ([`LAYER_PREFERENCE_GAUGE_KINDS`]) and the
    /// room-drain GC's taxonomy ([`crate::metrics::RELAY_LAYER_PREFERENCE_KINDS`])
    /// MUST list the exact same `kind` label set — otherwise the sweep would
    /// write a series the GC never removes (a leak) or vice versa. This fails if
    /// either array drifts.
    #[test]
    fn layer_preference_gauge_kinds_match_metrics_taxonomy() {
        let sweep_labels: std::collections::BTreeSet<&str> = LAYER_PREFERENCE_GAUGE_KINDS
            .iter()
            .map(|(_, label)| *label)
            .collect();
        let gc_labels: std::collections::BTreeSet<&str> =
            crate::metrics::RELAY_LAYER_PREFERENCE_KINDS
                .iter()
                .copied()
                .collect();
        assert_eq!(
            sweep_labels, gc_labels,
            "the sweep's kind labels and the room-drain GC's kind labels must match exactly"
        );
    }

    #[test]
    fn test_union_max_over_receivers() {
        // Three receivers want layers {2, 1, 2} of source 900's VIDEO. The union
        // (max) is 2.
        let prefs = receivers_map(&[
            (1, &[(900, VIDEO_KIND, 2)]),
            (2, &[(900, VIDEO_KIND, 1)]),
            (3, &[(900, VIDEO_KIND, 2)]),
        ]);
        let members = [900, 1, 2, 3];
        let union = compute_max_requested_layer(&members, &prefs, 900, VIDEO_KIND);
        assert_eq!(union, 2, "union of {{2,1,2}} must be 2");
    }

    #[test]
    fn test_union_fail_open_on_absent_entry() {
        // Receiver 1 wants base (0); receiver 2 has NO recorded entry for source
        // 900 (fail-open = wants the full ladder). The union must be the
        // full-ladder sentinel, i.e. "suppress nothing".
        let prefs = receivers_map(&[
            (1, &[(900, VIDEO_KIND, 0)]),
            (2, &[(901, VIDEO_KIND, 0)]), // a pref for a DIFFERENT source only
        ]);
        let members = [900, 1, 2];
        let union = compute_max_requested_layer(&members, &prefs, 900, VIDEO_KIND);
        assert_eq!(
            union, LAYER_HINT_FULL_LADDER_SENTINEL,
            "a receiver with no entry for (source,kind) is fail-open → full ladder"
        );
    }

    #[test]
    fn test_union_fail_open_on_missing_prefs_map() {
        // A member with NO LayerPrefs entry at all (never recorded a single
        // preference packet) is fail-open too.
        let prefs = receivers_map(&[(1, &[(900, VIDEO_KIND, 0)])]);
        // Member 2 is in the room but absent from the prefs map entirely.
        let members = [900, 1, 2];
        let union = compute_max_requested_layer(&members, &prefs, 900, VIDEO_KIND);
        assert_eq!(
            union, LAYER_HINT_FULL_LADDER_SENTINEL,
            "a member with no prefs map entry is fail-open → full ladder"
        );
    }

    #[test]
    fn test_union_all_base_is_zero() {
        // Every receiver explicitly wants base (0) → union 0 (fully suppressible).
        let prefs = receivers_map(&[
            (1, &[(900, VIDEO_KIND, 0)]),
            (2, &[(900, VIDEO_KIND, 0)]),
            (3, &[(900, VIDEO_KIND, 0)]),
        ]);
        let members = [900, 1, 2, 3];
        let union = compute_max_requested_layer(&members, &prefs, 900, VIDEO_KIND);
        assert_eq!(union, 0, "when every receiver wants base the union is 0");
    }

    #[test]
    fn test_union_skips_source_itself() {
        // The source is not its own receiver: even if a (bogus) self entry exists
        // it must not be counted. Here only the source has any entry, so the union
        // over the *other* members (none) is 0.
        let prefs = receivers_map(&[(900, &[(900, VIDEO_KIND, 2)])]);
        let members = [900];
        let union = compute_max_requested_layer(&members, &prefs, 900, VIDEO_KIND);
        assert_eq!(
            union, 0,
            "the source's own entry must be skipped (a publisher is not its own receiver)"
        );
    }

    #[test]
    fn test_union_per_kind_independent() {
        // The same source can carry distinct unions per media kind. Receivers
        // want VIDEO {2,1} but SCREEN {0,0}; the VIDEO union is 2 while the
        // SCREEN union is 0.
        const SCREEN_KIND: i32 = 3;
        let prefs = receivers_map(&[
            (1, &[(900, VIDEO_KIND, 2), (900, SCREEN_KIND, 0)]),
            (2, &[(900, VIDEO_KIND, 1), (900, SCREEN_KIND, 0)]),
        ]);
        let members = [900, 1, 2];
        assert_eq!(
            compute_max_requested_layer(&members, &prefs, 900, VIDEO_KIND),
            2,
            "VIDEO union"
        );
        assert_eq!(
            compute_max_requested_layer(&members, &prefs, 900, SCREEN_KIND),
            0,
            "SCREEN union is independent of VIDEO"
        );
    }

    #[test]
    fn test_union_disconnect_full_wanter_shrinks() {
        // Before: receivers {1: layer 1, 2: full-ladder (no entry)} → union is the
        // sentinel (fail-open). After receiver 2 (the full-ladder wanter) leaves,
        // only receiver 1 remains and the union shrinks to 1.
        let prefs = receivers_map(&[
            (1, &[(900, VIDEO_KIND, 1)]),
            (2, &[(901, VIDEO_KIND, 0)]), // no entry for 900 → fail-open for 900
        ]);
        let before = compute_max_requested_layer(&[900, 1, 2], &prefs, 900, VIDEO_KIND);
        assert_eq!(
            before, LAYER_HINT_FULL_LADDER_SENTINEL,
            "with a fail-open receiver present the union is the full-ladder sentinel"
        );
        // Receiver 2 has left → it is no longer in the member list.
        let after = compute_max_requested_layer(&[900, 1], &prefs, 900, VIDEO_KIND);
        assert_eq!(
            after, 1,
            "after the full-ladder receiver leaves, the union shrinks to the remaining max (1)"
        );
    }

    #[test]
    fn test_union_disconnect_base_wanter_unchanged() {
        // Before: receivers {1: layer 2, 2: base 0} → union 2. After receiver 2
        // (the base wanter, which was NOT the constraining max) leaves, the union
        // is still 2.
        let prefs = receivers_map(&[(1, &[(900, VIDEO_KIND, 2)]), (2, &[(900, VIDEO_KIND, 0)])]);
        let before = compute_max_requested_layer(&[900, 1, 2], &prefs, 900, VIDEO_KIND);
        assert_eq!(before, 2);
        let after = compute_max_requested_layer(&[900, 1], &prefs, 900, VIDEO_KIND);
        assert_eq!(
            after, 2,
            "removing a non-max (base) receiver leaves the union unchanged"
        );
    }

    #[test]
    fn test_union_dos_cap_truncates_fail_open() {
        // A room larger than the scan cap fails open on the remainder: even if
        // every scanned receiver wants base, the un-scanned tail is treated as
        // wanting the full ladder, so the union is the sentinel. Build cap+2
        // base-wanting receivers (ids 1..=cap+2) plus the source.
        let cap = LAYER_HINT_MAX_RECEIVERS_SCANNED;
        let specs: Vec<(SessionId, Vec<LayerEntrySpec>)> = (1..=(cap as u64 + 2))
            .map(|r| (r, vec![(900u64, VIDEO_KIND, 0u32)]))
            .collect();
        let prefs: HashMap<SessionId, LayerPrefs> = specs
            .iter()
            .map(|(r, e)| (*r, layer_prefs_with_kinds(e)))
            .collect();
        let mut members: Vec<SessionId> = vec![900];
        members.extend(1..=(cap as u64 + 2));
        let union = compute_max_requested_layer(&members, &prefs, 900, VIDEO_KIND);
        assert_eq!(
            union, LAYER_HINT_FULL_LADDER_SENTINEL,
            "scanning past the DoS cap must fail open (treat the tail as full-ladder)"
        );
    }

    #[test]
    fn test_union_poisoned_lock_fails_open() {
        // A receiver whose prefs RwLock is POISONED contributes the full-ladder
        // sentinel (fail-open), mirroring the forwarding filter's `unwrap_or`.
        let prefs = receivers_map(&[(1, &[(900, VIDEO_KIND, 0)])]);
        // Poison receiver 1's lock by panicking while holding the write guard.
        let target = prefs.get(&1).unwrap().clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = target.state.write().unwrap();
            panic!("intentional panic to poison the lock");
        }));
        assert!(
            prefs.get(&1).unwrap().state.read().is_err(),
            "precondition: receiver 1's lock is poisoned"
        );
        let union = compute_max_requested_layer(&[900, 1], &prefs, 900, VIDEO_KIND);
        assert_eq!(
            union, LAYER_HINT_FULL_LADDER_SENTINEL,
            "a poisoned receiver lock must fail open → full ladder"
        );
    }

    #[test]
    fn test_changed_pref_sources_detects_add_remove_change() {
        let mut old: HashMap<(u64, i32), u32> = HashMap::new();
        old.insert((900, VIDEO_KIND), 2);
        old.insert((901, VIDEO_KIND), 1);
        old.insert((902, VIDEO_KIND), 0);

        let mut new = old.clone();
        new.insert((900, VIDEO_KIND), 0); // changed value for 900
        new.remove(&(901, VIDEO_KIND)); // removed 901 (reverts to fail-open)
        new.insert((903, VIDEO_KIND), 1); // added 903
                                          // 902 unchanged

        let mut changed = changed_pref_sources(&old, &new);
        changed.sort_unstable();
        assert_eq!(
            changed,
            vec![900, 901, 903],
            "changed set must include value-change (900), removal (901), and add (903) — not the unchanged 902"
        );
    }

    #[test]
    fn test_changed_pref_sources_no_change_is_empty() {
        let mut map: HashMap<(u64, i32), u32> = HashMap::new();
        map.insert((900, VIDEO_KIND), 2);
        let changed = changed_pref_sources(&map, &map.clone());
        assert!(
            changed.is_empty(),
            "identical maps yield no changed sources (change-detect skip)"
        );
    }

    #[test]
    fn test_decide_layer_hint_restore_is_eager() {
        // A higher union than last emitted is emitted IMMEDIATELY (restore-eager),
        // even with a long debounce window and zero elapsed time.
        let now = std::time::Instant::now();
        let window = std::time::Duration::from_millis(LAYER_HINT_SUPPRESS_DEBOUNCE_MS);
        let prev = Some(LayerHintEmitState {
            last_emitted: 1,
            pending_lower_since: None,
        });
        let decision = decide_layer_hint(prev, 3, now, window);
        assert_eq!(
            decision,
            LayerHintDecision::Emit {
                value: 3,
                direction: LayerHintDirection::Restore
            },
            "a higher union must emit immediately (restore-eager)"
        );
    }

    #[test]
    fn test_decide_layer_hint_unchanged_is_skip() {
        let now = std::time::Instant::now();
        let window = std::time::Duration::from_millis(LAYER_HINT_SUPPRESS_DEBOUNCE_MS);
        let prev = Some(LayerHintEmitState {
            last_emitted: 2,
            pending_lower_since: None,
        });
        assert_eq!(
            decide_layer_hint(prev, 2, now, window),
            LayerHintDecision::SkipClearPending,
            "an unchanged union must be skipped (change-detect) and clear any pending downgrade"
        );
    }

    #[test]
    fn test_decide_layer_hint_lower_schedules_then_emits_after_window() {
        // Suppress-lazy: the FIRST observation of a lower union schedules a
        // re-check (no emit). Once the window has fully elapsed, the lower hint is
        // emitted as a suppress.
        let window = std::time::Duration::from_millis(LAYER_HINT_SUPPRESS_DEBOUNCE_MS);
        let t0 = std::time::Instant::now();

        // No prior state → assumed full-ladder. A concrete lower union (1) is a
        // downgrade and must first SCHEDULE.
        let first = decide_layer_hint(None, 1, t0, window);
        match first {
            LayerHintDecision::ScheduleRecheck { deadline } => {
                assert_eq!(deadline, t0 + window, "deadline is now + window");
            }
            other => panic!("first lower observation must ScheduleRecheck, got {other:?}"),
        }

        // Simulate the actor having recorded the pending timestamp at t0, then the
        // re-check firing AFTER the window: it must now EMIT (suppress).
        let pending = Some(LayerHintEmitState {
            last_emitted: LAYER_HINT_FULL_LADDER_SENTINEL,
            pending_lower_since: Some(t0),
        });
        let after = decide_layer_hint(pending, 1, t0 + window, window);
        assert_eq!(
            after,
            LayerHintDecision::Emit {
                value: 1,
                direction: LayerHintDirection::Suppress
            },
            "after the stability window the lower union emits as a suppress"
        );
    }

    #[test]
    fn test_decide_layer_hint_lower_within_window_is_skip() {
        // While a downgrade is pending but the window has NOT elapsed, decisions
        // skip (keep waiting) — this is what collapses a rapid flap into a single
        // eventual lower hint instead of one per change.
        let window = std::time::Duration::from_millis(LAYER_HINT_SUPPRESS_DEBOUNCE_MS);
        let t0 = std::time::Instant::now();
        let pending = Some(LayerHintEmitState {
            last_emitted: LAYER_HINT_FULL_LADDER_SENTINEL,
            pending_lower_since: Some(t0),
        });
        // Half a window later — still pending.
        let mid = t0 + window / 2;
        assert_eq!(
            decide_layer_hint(pending, 1, mid, window),
            LayerHintDecision::SkipKeepPending,
            "a still-pending downgrade within the window must keep waiting (debounce)"
        );
        // A DIFFERENT lower value mid-window also keeps waiting (does not reset to
        // a fresh schedule, does not emit early).
        assert_eq!(
            decide_layer_hint(pending, 0, mid, window),
            LayerHintDecision::SkipKeepPending,
            "a changed-but-still-lower value mid-window keeps waiting"
        );
    }

    #[test]
    fn test_decide_layer_hint_flap_back_up_cancels_suppress() {
        // Rapid flap: union drops (schedule), then RISES back to/above the last
        // emitted before the window elapses. The rise emits eagerly as a restore,
        // and there is no lingering suppress. This proves the eager-up path wins
        // over a pending downgrade.
        let window = std::time::Duration::from_millis(LAYER_HINT_SUPPRESS_DEBOUNCE_MS);
        let t0 = std::time::Instant::now();
        // Pending downgrade from full-ladder to 1, recorded at t0.
        let pending = Some(LayerHintEmitState {
            last_emitted: 2,
            pending_lower_since: Some(t0),
        });
        // Mid-window the union jumps to 3 (> last_emitted 2): restore-eager.
        let mid = t0 + window / 2;
        assert_eq!(
            decide_layer_hint(pending, 3, mid, window),
            LayerHintDecision::Emit {
                value: 3,
                direction: LayerHintDirection::Restore
            },
            "a mid-window rise above last_emitted emits a restore immediately"
        );
    }

    #[test]
    fn test_decide_layer_hint_first_full_ladder_is_skip() {
        // No prior state and the union is ALREADY the full ladder (the fail-open
        // default the publisher is assumed to be encoding): nothing to say → skip.
        let now = std::time::Instant::now();
        let window = std::time::Duration::from_millis(LAYER_HINT_SUPPRESS_DEBOUNCE_MS);
        assert_eq!(
            decide_layer_hint(None, LAYER_HINT_FULL_LADDER_SENTINEL, now, window),
            LayerHintDecision::SkipClearPending,
            "a first observation equal to the assumed full-ladder baseline is a no-op"
        );
    }

    #[test]
    fn test_decide_layer_hint_cancelled_downgrade_re_debounces() {
        // Regression guard for the stale-`pending_lower_since` bug: a downgrade
        // that is CANCELLED by demand returning to the emitted level must reset
        // the pending state, so a LATER downgrade starts a FRESH debounce window
        // instead of emitting a suppress immediately.
        let window = std::time::Duration::from_millis(LAYER_HINT_SUPPRESS_DEBOUNCE_MS);
        let t0 = std::time::Instant::now();

        // 1) Downgrade observed at t0 → schedule (pending set to t0 by the actor).
        assert!(matches!(
            decide_layer_hint(None, 1, t0, window),
            LayerHintDecision::ScheduleRecheck { .. }
        ));
        let pending = LayerHintEmitState {
            last_emitted: LAYER_HINT_FULL_LADDER_SENTINEL,
            pending_lower_since: Some(t0),
        };

        // 2) Mid-window the union returns to the full-ladder baseline (==
        //    last_emitted): the decision is SkipClearPending, and the actor clears
        //    `pending_lower_since`.
        let mid = t0 + window / 2;
        assert_eq!(
            decide_layer_hint(Some(pending), LAYER_HINT_FULL_LADDER_SENTINEL, mid, window),
            LayerHintDecision::SkipClearPending,
            "demand returning to baseline must cancel the pending downgrade"
        );
        let cleared = LayerHintEmitState {
            last_emitted: LAYER_HINT_FULL_LADDER_SENTINEL,
            pending_lower_since: None,
        };

        // 3) Much LATER (well beyond the original window) the union drops again.
        //    Because pending was cleared, this is a FIRST downgrade observation
        //    again → ScheduleRecheck, NOT an immediate suppress emit. This is the
        //    behaviour the bug would have broken.
        let much_later = t0 + window * 5;
        assert!(
            matches!(
                decide_layer_hint(Some(cleared), 1, much_later, window),
                LayerHintDecision::ScheduleRecheck { .. }
            ),
            "a downgrade after a cancelled one must re-debounce (not bypass the window)"
        );
    }

    #[test]
    fn test_layer_hint_forge_resistance_no_inbound_ingest() {
        // FORGE RESISTANCE (security): a LAYER_HINT packet arriving on a session
        // subject MUST NOT be interpreted as a hint and MUST NOT mutate any
        // union/preference state. The LAYER_PREFERENCE interceptor — the ONLY
        // path that records union inputs — must FALL THROUGH (return false) for a
        // LAYER_HINT, never recording anything, and never invoking the recompute
        // sink. There is no try_intercept_layer_hint at all.
        let prefs = LayerPrefs::default();
        let self_subject = self_subject_for("r", 100);

        // Craft a LAYER_HINT wrapper (as a malicious client might) carrying a
        // bogus payload, delivered on the receiver's OWN subject.
        let mut inner = LayerHintPacket::new();
        let mut entry = LayerHintEntry::new();
        entry.media_kind =
            videocall_types::protos::layer_hint_packet::layer_hint_packet::MediaKind::VIDEO.into();
        entry.max_requested_layer = 0; // "suppress everything" — must be ignored
        inner.entries.push(entry);
        let mut pw = PacketWrapper::new();
        pw.packet_type = PacketType::LAYER_HINT.into();
        pw.session_id = 100;
        pw.data = inner.write_to_bytes().unwrap();
        let bytes = pw.write_to_bytes().unwrap();
        let msg = make_nats_message(&self_subject, bytes);
        let parsed = parse_pw(&msg);

        // Track whether the recompute sink is (wrongly) invoked.
        let recompute_calls = std::cell::Cell::new(0u32);
        let intercepted = try_intercept_layer_preference(
            &msg,
            parsed.as_ref(),
            &self_subject,
            &prefs,
            "r",
            &|_| recompute_calls.set(recompute_calls.get() + 1),
            100,
        );

        assert!(
            !intercepted,
            "a LAYER_HINT must FALL THROUGH the LAYER_PREFERENCE interceptor (return false) — \
             it is not a preference and there is no inbound LAYER_HINT path"
        );
        assert!(
            prefs.state.read().unwrap().layers.is_empty(),
            "a forged LAYER_HINT must NOT record any preference / union input"
        );
        assert!(
            !prefs.has_any(),
            "a forged LAYER_HINT must not raise the recorded-prefs hint"
        );
        assert_eq!(
            recompute_calls.get(),
            0,
            "a forged LAYER_HINT must never trigger a layer-hint recompute"
        );
    }

    #[test]
    fn test_layer_preference_triggers_recompute_for_changed_source_only() {
        // A genuine LAYER_PREFERENCE on the OWN subject records the map AND invokes
        // the recompute sink once per CHANGED source — proving the per-source
        // trigger (checklist item 5) fires with the right source set.
        let prefs = LayerPrefs::default();
        let self_subject = self_subject_for("r", 100);
        let msg = make_nats_message(
            &self_subject,
            make_layer_preference_packet_bytes(100, &[(200, 1), (300, 2)]),
        );
        let parsed = parse_pw(&msg);

        let recomputed: std::cell::RefCell<Vec<Option<SessionId>>> =
            std::cell::RefCell::new(Vec::new());
        let intercepted = try_intercept_layer_preference(
            &msg,
            parsed.as_ref(),
            &self_subject,
            &prefs,
            "r",
            &|m: RecomputeLayerHints| recomputed.borrow_mut().push(m.source),
            100,
        );
        assert!(intercepted, "a LAYER_PREFERENCE must be intercepted");

        let mut sources: Vec<u64> = recomputed
            .borrow()
            .iter()
            .map(|s| s.expect("per-source trigger must carry Some(source)"))
            .collect();
        sources.sort_unstable();
        assert_eq!(
            sources,
            vec![200, 300],
            "recording two new sources must trigger a per-source recompute for each"
        );
    }

    #[test]
    fn test_layer_preference_other_subject_does_not_trigger_recompute() {
        // A LAYER_PREFERENCE on a DIFFERENT subject is dropped without mutating
        // state AND without triggering any recompute (the union must never be
        // built from a non-subject-authoritative packet).
        let prefs = LayerPrefs::default();
        let self_subject = self_subject_for("r", 100);
        let msg = make_nats_message(
            "room.r.555",
            make_layer_preference_packet_bytes(100, &[(200, 1)]),
        );
        let parsed = parse_pw(&msg);
        let recompute_calls = std::cell::Cell::new(0u32);
        let intercepted = try_intercept_layer_preference(
            &msg,
            parsed.as_ref(),
            &self_subject,
            &prefs,
            "r",
            &|_| recompute_calls.set(recompute_calls.get() + 1),
            100,
        );
        assert!(intercepted, "still consumed");
        assert_eq!(
            recompute_calls.get(),
            0,
            "a foreign-subject LAYER_PREFERENCE must NOT trigger a recompute"
        );
        assert!(prefs.state.read().unwrap().layers.is_empty());
    }

    #[test]
    fn test_layer_hint_media_kind_mapping() {
        use videocall_types::protos::layer_hint_packet::layer_hint_packet::MediaKind as HintKind;
        assert_eq!(layer_hint_media_kind(1), HintKind::VIDEO);
        assert_eq!(layer_hint_media_kind(2), HintKind::AUDIO);
        assert_eq!(layer_hint_media_kind(3), HintKind::SCREEN);
        assert_eq!(layer_hint_media_kind(0), HintKind::MEDIA_KIND_UNSPECIFIED);
        assert_eq!(layer_hint_media_kind(99), HintKind::MEDIA_KIND_UNSPECIFIED);
    }

    // =====================================================================
    // #1203 — DEPARTURE-recompute coalescing (trailing debounce)
    // =====================================================================
    //
    // These tests start a real `ChatServer` (NATS-backed) and drive the REAL
    // `schedule_coalesced_recompute` + `notify_later` timer + flush path. They
    // count actual `Handler<RecomputeLayerHints>` invocations via the
    // test-only `RECOMPUTE_LAYER_HINTS_INVOCATIONS` counter.

    async fn connect_nats_or_skip() -> Option<async_nats::client::Client> {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        match async_nats::connect(&nats_url).await {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("SKIP: NATS unavailable at {nats_url}: {e}");
                None
            }
        }
    }

    /// #1203: N DEPARTURE-driven coalesced recomputes for the SAME room within
    /// the window collapse into exactly ONE timer and ONE recompute; two
    /// distinct rooms produce exactly TWO recomputes. After the window the
    /// pending set is drained and the timer handle is cleared.
    ///
    /// MUTATION PROOF: revert #1203 (point `leave_rooms`/`forget_session` back
    /// at `do_send(RecomputeLayerHints { source: None })`, i.e. make
    /// `schedule_coalesced_recompute` recompute immediately per call instead of
    /// arming one trailing timer) and this asserts BOTH (a) the dedup state
    /// `(pending_len, armed)` after repeated same-room calls — which would no
    /// longer be `(1, true)` — and (b) `recomputes == 2`, which would balloon to
    /// 6 (5 same-room + 1 other). Either breaks the test.
    #[actix_rt::test]
    #[serial]
    async fn test_1203_departures_coalesce_into_single_recompute() {
        use std::sync::atomic::Ordering as AtomicOrdering;

        let Some(nats_client) = connect_nats_or_skip().await else {
            return;
        };
        let chat = ChatServer::new(nats_client).await.start();

        RECOMPUTE_LAYER_HINTS_INVOCATIONS.store(0, AtomicOrdering::SeqCst);

        let room_a = "coalesce-room-a-1203".to_string();
        let room_b = "coalesce-room-b-1203".to_string();

        // 5 departure-driven schedules for room A: all should dedup behind ONE
        // timer (pending_len stays 1, timer stays armed). No recompute yet.
        for i in 0..5 {
            let (pending_len, armed) = chat
                .send(TestScheduleCoalescedRecompute {
                    room: room_a.clone(),
                })
                .await
                .expect("message delivery should succeed");
            assert_eq!(
                pending_len, 1,
                "call {i}: same-room departures must dedup to ONE pending room"
            );
            assert!(armed, "call {i}: the single trailing timer must be armed");
        }

        // One departure-driven schedule for room B: pending grows to 2, still
        // ONE timer (the existing trailing deadline covers both rooms).
        let (pending_len, armed) = chat
            .send(TestScheduleCoalescedRecompute {
                room: room_b.clone(),
            })
            .await
            .expect("message delivery should succeed");
        assert_eq!(
            pending_len, 2,
            "a distinct room adds a second pending entry"
        );
        assert!(
            armed,
            "still exactly one trailing timer for the whole burst"
        );

        // Before the window elapses, NO recompute has run yet (debounced).
        assert_eq!(
            RECOMPUTE_LAYER_HINTS_INVOCATIONS.load(AtomicOrdering::SeqCst),
            0,
            "departure recomputes must be DEFERRED until the coalesce window elapses"
        );

        // Wait out the coalesce window + slack for the flush to fire.
        tokio::time::sleep(std::time::Duration::from_millis(
            LAYER_HINT_RECOMPUTE_COALESCE_MS + 250,
        ))
        .await;

        // Exactly one recompute per distinct room (2), NOT one per call (6).
        assert_eq!(
            RECOMPUTE_LAYER_HINTS_INVOCATIONS.load(AtomicOrdering::SeqCst),
            2,
            "the 6 coalesced departure schedules must yield exactly 2 recomputes \
             (one per distinct room)"
        );

        // The flush drained the set and cleared the timer handle.
        let (pending_after, armed_after) = chat
            .send(TestCoalesceState)
            .await
            .expect("message delivery should succeed");
        assert_eq!(pending_after, 0, "flush must drain the pending set");
        assert!(
            !armed_after,
            "flush must clear the in-flight timer so the next burst re-arms fresh"
        );
    }

    /// #1203: a JOIN-style recompute (`RecomputeLayerHints { source: None }`
    /// sent directly, exactly as the join site does) runs IMMEDIATELY — it is
    /// NOT subject to the departure coalesce window. Contrast: a coalesced
    /// departure schedule does NOT bump the recompute counter immediately.
    ///
    /// MUTATION PROOF: if someone routed the JOIN path through
    /// `schedule_coalesced_recompute` (the thing #1203 deliberately does NOT do
    /// for joins), the immediate-after-yield count would be 0, failing the
    /// `== 1` assert.
    #[actix_rt::test]
    #[serial]
    async fn test_1203_join_recompute_is_immediate_not_coalesced() {
        use std::sync::atomic::Ordering as AtomicOrdering;

        let Some(nats_client) = connect_nats_or_skip().await else {
            return;
        };
        let chat = ChatServer::new(nats_client).await.start();

        RECOMPUTE_LAYER_HINTS_INVOCATIONS.store(0, AtomicOrdering::SeqCst);

        // First: a coalesced DEPARTURE schedule does NOT recompute immediately.
        let _ = chat
            .send(TestScheduleCoalescedRecompute {
                room: "join-immediate-departure-room".to_string(),
            })
            .await
            .expect("message delivery should succeed");
        // Round-trip a state probe to ensure the schedule was processed; the
        // counter must still be 0 (departure is debounced).
        let _ = chat
            .send(TestCoalesceState)
            .await
            .expect("message delivery should succeed");
        assert_eq!(
            RECOMPUTE_LAYER_HINTS_INVOCATIONS.load(AtomicOrdering::SeqCst),
            0,
            "a coalesced departure must NOT recompute synchronously"
        );

        // Now: a JOIN-style direct recompute. The join site uses
        // `do_send(RecomputeLayerHints { room, source: None })`; we replicate
        // that exact message. It must run immediately (no debounce wait).
        chat.send(RecomputeLayerHints {
            room: "join-immediate-room".to_string(),
            source: None,
        })
        .await
        .expect("message delivery should succeed");

        assert_eq!(
            RECOMPUTE_LAYER_HINTS_INVOCATIONS.load(AtomicOrdering::SeqCst),
            1,
            "a JOIN-style recompute must run IMMEDIATELY (one invocation), not be \
             held behind the departure coalesce window"
        );
    }

    /// #1235: the #1203 departure coalescing is exercised through the REAL
    /// departure handlers, not just the `schedule_coalesced_recompute` primitive.
    ///
    /// The two existing #1203 tests above drive `TestScheduleCoalescedRecompute`
    /// (the primitive) directly. That pins the coalescing machinery but leaves a
    /// gap: nothing proves that the actual `leave_rooms` (explicit `Leave`) and
    /// `forget_session` (cross-server `EvictInstance`) call sites are still wired
    /// to `schedule_coalesced_recompute` rather than the pre-#1203 immediate
    /// `do_send(RecomputeLayerHints { source: None })`. This test closes that gap
    /// by driving the FULL relay lifecycle through the real handlers:
    ///
    ///   * Connect -> JoinRoom -> Leave                              (-> leave_rooms)
    ///   * Connect -> JoinRoom -> ActivateConnection -> EvictInstance (-> forget_session)
    ///
    /// Topology (every departed room keeps >=1 member so a recompute is actually
    /// SCHEDULED — `leave_rooms`/`forget_session` only schedule when the room is
    /// NOT drained to empty):
    ///   * Room A: 3 members. A1 departs via `Leave`, A2 departs via
    ///     `EvictInstance`; A3 remains. BOTH departures hit room A within the
    ///     coalesce window, so they MUST coalesce into ONE recompute.
    ///   * Room B: 2 members. B1 departs via `Leave`; B2 remains. ONE recompute.
    ///   => 2 distinct affected rooms => exactly 2 recomputes total, NOT one per
    ///      departure (which would be 3).
    ///
    /// The counter is reset AFTER all joins/activations complete. This is load-
    /// bearing: the JoinRoom handler's synchronous body does
    /// `do_send(RecomputeLayerHints { source: None })` (the #1108 Stage-3 join
    /// restore), so each join bumps the counter; resetting after a drain probe
    /// isolates the count to the departures under test. `ActivateConnection` on a
    /// session with a fresh, unshared instance_id is an eviction no-op
    /// (`instance_index.get(iid)` is None on first activation) so it schedules no
    /// recompute of its own.
    ///
    /// MUTATION PROOF: reverting EITHER departure call site to the immediate
    /// `do_send(RecomputeLayerHints { room, source: None })` breaks this test.
    ///   * Reverting `leave_rooms` fires room A's + room B's `Leave`-driven
    ///     recomputes synchronously, tripping the pre-window `== 0` assert.
    ///   * Reverting `forget_session` fires room A's `EvictInstance`-driven
    ///     recompute synchronously, tripping the pre-window `== 0` assert.
    /// Both reverts have been run and observed to fail (see the issue/PR notes).
    #[actix_rt::test]
    #[serial]
    async fn test_1235_real_departure_handlers_coalesce() {
        use std::sync::atomic::Ordering as AtomicOrdering;

        let Some(nats_client) = connect_nats_or_skip().await else {
            return;
        };
        let chat = ChatServer::new(nats_client).await.start();

        // Unique, #1235-specific room names and session/instance ids so this
        // test cannot collide with any other test's state. `#[serial]` already
        // serializes, but unique keys keep the actor state hermetic regardless.
        let room_a = "i1235-coalesce-room-a".to_string();
        let room_b = "i1235-coalesce-room-b".to_string();

        // (session_id, instance_id) for every member.
        let a1 = (1_235_001u64, "i1235-a1");
        let a2 = (1_235_002u64, "i1235-a2");
        let a3 = (1_235_003u64, "i1235-a3");
        let b1 = (1_235_004u64, "i1235-b1");
        let b2 = (1_235_005u64, "i1235-b2");

        // A no-op session recipient: every real JoinRoom needs a
        // `Recipient<Message>` registered via Connect first. DummySession never
        // sends anything, so no LAYER_PREFERENCE interceptor recompute can fire
        // from this test — the only recomputes are the join restore (which we
        // reset away) and the departures under test.
        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        // Helper: Connect + JoinRoom a non-observer member into a room.
        async fn join_member(
            chat: &Addr<ChatServer>,
            session: SessionId,
            instance_id: &str,
            room: &str,
        ) {
            let dummy = DummySession.start();
            chat.send(Connect {
                id: session,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");
            chat.send(JoinRoom {
                session,
                room: room.to_string(),
                user_id: format!("{instance_id}@example.com"),
                display_name: instance_id.to_string(),
                is_guest: false,
                observer: false,
                instance_id: Some(instance_id.to_string()),
                is_host: false,
                end_on_host_leave: false,
                transport: "websocket".to_string(),
            })
            .await
            .expect("JoinRoom delivery should succeed")
            .expect("JoinRoom should return Ok");
        }

        // --- Setup: populate both rooms via the REAL join path. ---
        join_member(&chat, a1.0, a1.1, &room_a).await;
        join_member(&chat, a2.0, a2.1, &room_a).await;
        join_member(&chat, a3.0, a3.1, &room_a).await;
        join_member(&chat, b1.0, b1.1, &room_b).await;
        join_member(&chat, b2.0, b2.1, &room_b).await;

        // A2 is the EvictInstance target. The cross-server eviction path
        // (EvictInstance -> evict_stale_session -> forget_session) requires
        // `instance_index[iid] -> prev_session`, which is populated by
        // ActivateConnection (NOT JoinRoom). Activate ONLY A2: its instance_id is
        // unique and unshared, so this first activation finds no prior
        // instance_index entry and performs no eviction itself (it only claims
        // the forward mapping + broadcasts PARTICIPANT_JOINED). No recompute.
        chat.send(ActivateConnection { session: a2.0 })
            .await
            .expect("ActivateConnection should succeed");

        // Drain every queued JoinRoom-restore recompute (one per non-observer
        // join) and the ActivateConnection work by round-tripping a state probe,
        // THEN reset the counter so it reflects ONLY the departures under test.
        let _ = chat
            .send(TestCoalesceState)
            .await
            .expect("state probe should succeed");
        RECOMPUTE_LAYER_HINTS_INVOCATIONS.store(0, AtomicOrdering::SeqCst);

        // Sanity: no departure has happened yet, and nothing should be pending /
        // armed from the join phase (joins use immediate do_send, not the
        // coalesce timer).
        let (pending_pre, armed_pre) = chat
            .send(TestCoalesceState)
            .await
            .expect("state probe should succeed");
        assert_eq!(
            pending_pre, 0,
            "joins must not leave anything in the departure coalesce set"
        );
        assert!(
            !armed_pre,
            "joins must not arm the departure coalesce timer"
        );

        // --- Drive the departure burst through the REAL handlers. ---
        // Room A, departure 1: explicit Leave -> leave_rooms. A2 + A3 remain, so
        // the room is NOT empty and a coalesced recompute IS scheduled.
        chat.send(Leave {
            session: a1.0,
            room: room_a.clone(),
            user_id: format!("{}@example.com", a1.1),
        })
        .await
        .expect("Leave delivery should succeed");

        // Room A, departure 2: cross-server EvictInstance -> evict_stale_session
        // -> forget_session. A3 remains, so the room is NOT empty and a coalesced
        // recompute IS scheduled (dedups with departure 1: same room A).
        // new_session_id differs from A2's session so the eviction is not a
        // self-skip.
        chat.send(EvictInstance(EvictInstancePayload {
            instance_id: a2.1.to_string(),
            room: room_a.clone(),
            user_id: format!("{}@example.com", a2.1),
            new_session_id: 1_235_999,
        }))
        .await
        .expect("EvictInstance delivery should succeed");

        // Room B, single departure: explicit Leave -> leave_rooms. B2 remains.
        chat.send(Leave {
            session: b1.0,
            room: room_b.clone(),
            user_id: format!("{}@example.com", b1.1),
        })
        .await
        .expect("Leave delivery should succeed");

        // Round-trip a state probe: this forces all the queued Leave/Evict
        // messages above to DRAIN before we read the counter, making the
        // pre-window assertion race-free.
        let (pending_after_burst, armed_after_burst) = chat
            .send(TestCoalesceState)
            .await
            .expect("state probe should succeed");

        // BEFORE the window elapses: NO recompute has run (all departures are
        // debounced behind the trailing timer). This is the assertion that a
        // reverted (immediate-do_send) call site trips.
        assert_eq!(
            RECOMPUTE_LAYER_HINTS_INVOCATIONS.load(AtomicOrdering::SeqCst),
            0,
            "real-handler departures must be DEFERRED until the coalesce window \
             elapses — a non-zero count here means a departure call site fired an \
             immediate recompute instead of coalescing"
        );
        // Exactly the two affected rooms are pending behind ONE armed timer.
        assert_eq!(
            pending_after_burst, 2,
            "two distinct rooms saw a departure (room A via Leave+Evict coalesced, \
             room B via Leave) => exactly two pending rooms"
        );
        assert!(
            armed_after_burst,
            "the burst must arm exactly one trailing coalesce timer"
        );

        // Wait out the coalesce window + slack for the flush to fire.
        tokio::time::sleep(std::time::Duration::from_millis(
            LAYER_HINT_RECOMPUTE_COALESCE_MS + 250,
        ))
        .await;

        // Exactly one recompute per distinct affected room (2), NOT one per
        // departure (3). Room A's two departures coalesced into one.
        assert_eq!(
            RECOMPUTE_LAYER_HINTS_INVOCATIONS.load(AtomicOrdering::SeqCst),
            2,
            "three real departures across two rooms must yield exactly two \
             recomputes (one per distinct room), not one per departure"
        );

        // The flush drained the pending set and cleared the timer handle.
        let (pending_final, armed_final) = chat
            .send(TestCoalesceState)
            .await
            .expect("state probe should succeed");
        assert_eq!(
            pending_final, 0,
            "the flush must drain the departure coalesce set"
        );
        assert!(
            !armed_final,
            "the flush must clear the timer handle so the next burst re-arms fresh"
        );
    }
}
