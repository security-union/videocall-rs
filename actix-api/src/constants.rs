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

/// Regex pattern for validating usernames and room IDs
/// Allows alphanumeric characters, underscores, and hyphens
pub const VALID_ID_PATTERN: &str = "^[a-zA-Z0-9_-]*$";

/// Maximum incoming frame/stream size in bytes for both WebSocket and WebTransport.
///
/// 4 MB accommodates worst-case 1080p VP9 keyframes (1-2 MB raw) plus protobuf
/// wrapping overhead. The previous 1 MB limit caused session termination when a
/// participant shared a high-quality 1080p screen, because VP9 keyframes exceeded
/// the cap and triggered a protocol error that closed the entire connection.
pub const MAX_FRAME_SIZE: usize = 4_000_000;

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
/// 512 caps the unistream backlog at ~16 seconds of single-stream video
/// (512 / 30 fps), which is already generous for burst absorption, and
/// deliberately aligns the unistream bound with the already-512 datagram
/// bound ([`WT_DATAGRAM_CHANNEL_CAPACITY`]). Once the queue saturates,
/// the priority-drop policy (see `priority_drop.rs`) sheds video before
/// audio before control, and the per-sender CONGESTION feedback path
/// tells fast senders to step their quality down — both of which are the
/// *correct* response to a slow receiver, far better than hoarding stale
/// frames in a 4096-deep buffer.
///
/// The previous 4096 default was a 2026-05-11 stopgap chosen before the
/// priority-drop policy (discussion #699) and congestion feedback existed.
/// With those landed, the large buffer is no longer needed and is lowered
/// here per issue #979. Operators with an exceptional workload can still
/// raise the bound at deploy-time via `WT_OUTBOUND_CHANNEL_CAPACITY`.
pub const WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT: usize = 512;

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

/// Bounded channel capacity for WebSocket outbound relay queue.
///
/// Half the WebTransport capacity because WS frames are larger
/// (full frames vs MTU-limited datagrams). 128 slots at ~50KB avg
/// provides ~6.4MB max queue depth.
pub const WS_OUTBOUND_CHANNEL_CAPACITY: usize = 128;

/// Bounded channel capacity for the WebTransport **datagram** outbound queue.
///
/// As of the Phase 2 WT-freeze fix (discussion #756), the per-session
/// outbound channel is split into two: a unistream channel and a
/// datagram channel. Splitting the channels is the architectural change;
/// the unistream side keeps the env-tunable
/// [`WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT`] (now 512, see issue #979)
/// since it continues to absorb video + screen + oversized control
/// packets, while the datagram side is sized small on purpose:
///
/// * Datagram traffic is small (~80 audio packets/sec/sender at ~80B
///   each, plus heartbeats / RTT echoes / non-media control under MTU).
/// * Datagrams are independent: there is no QUIC flow-control coupling
///   between them, so a slow receiver cannot stall the queue.
/// * `session.send_datagram` returns immediately on the wire (UDP-style
///   semantics inside the QUIC connection), so the queue exists only to
///   absorb actor-side bursts during scheduling jitter — not to buffer
///   for receiver congestion.
///
/// 512 slots ≈ 10 seconds of headroom at 50 audio pps (the dominant
/// datagram rate per session); more than enough for actor / writer
/// scheduling jitter, and small enough that a misrouted video burst
/// (oversized audio mis-classified as datagram) would not balloon
/// per-session memory.
///
/// This value is NOT env-tunable today. If a future workload genuinely
/// needs a larger datagram queue (e.g. very chatty diagnostics), promote
/// it to an env-resolved getter mirroring [`wt_outbound_channel_capacity`].
pub const WT_DATAGRAM_CHANNEL_CAPACITY: usize = 512;

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
/// * WS: mailbox 128 → **256** (channel stays 128).
/// * WT: mailbox `unistream + datagram` (default 1024) → **2048**
///   (each channel stays 512; the deep-stale-video bound is on the channel,
///   so a 2048 mailbox does NOT create a 2048-deep stale-video buffer).
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
        // Sample values intentionally chosen so neither equals
        // `WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT` (512) — otherwise the
        // assertion would pass even if the env value were silently ignored.
        assert_eq!(resolve_wt_outbound_channel_capacity(Some("1024")), 1024);
        assert_eq!(resolve_wt_outbound_channel_capacity(Some("8192")), 8192);
    }

    #[test]
    fn wt_outbound_channel_capacity_default_is_512() {
        // Sentinel test pinning the documented fail-fast value (issue #979).
        // If this needs to change, update the doc comment on
        // `WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT` (and any helm overlays /
        // operator docs) first, then this assertion.
        assert_eq!(WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT, 512);
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
}
