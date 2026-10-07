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
 */

//! Receiver-side packet quality diagnostics shared by WebSocket and WebTransport clients.
//!
//! Parses every inbound `PacketWrapper` → `MediaPacket`, tracks per-rung sequence
//! numbers, measures inter-arrival variability, and computes A/V sync drift.
//! Reports a summary line at `INFO` level every 10 seconds.

use crate::aq_controller::BotAq;
use crate::keyframe_requester::KeyframeRequester;
use crate::layer_preference_sender::{LayerPreferenceSender, PinMediaKind};
use crate::media_delay::{DelayKind, DelayWindowStats, MediaDelayTracker};
use crate::rtt_probe::RttProbeState;
use crate::viewport_sender::ViewportSender;
use protobuf::Message;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, info};
use videocall_aq::constants::{
    AUDIO_SIMULCAST_MAX_LAYERS, LAYER_AVAILABILITY_WINDOW_MS, SEQ_RESET_REANCHOR_GAP,
    SIMULCAST_MAX_LAYERS,
};
use videocall_types::protos::layer_hint_packet::{
    layer_hint_packet::MediaKind as HintMediaKind, LayerHintPacket,
};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

#[cfg(feature = "metrics")]
use crate::metrics_server::BotMetrics;

/// Per-sender bytes accumulated between DIAGNOSTICS drains.
#[derive(Default, Clone)]
pub struct SenderHealthCounters {
    pub audio_bytes: u64,
    pub video_bytes: u64,
}

/// How long a rung stays "arriving" after its last packet.
///
/// Shares its definition with `videocall-client`'s
/// `LayerAvailability::DEFAULT_WINDOW_MS` so bot and browser cannot drift on which
/// rungs a source is offering (#2206).
const RUNG_WINDOW: Duration = Duration::from_millis(LAYER_AVAILABILITY_WINDOW_MS);

/// A peer silent this long has left. The browser drops a peer after three
/// consecutive 5 s monitor ticks with no packet from it, 10–15 s
/// (`videocall-client` `Peer::check_heartbeat`); checked on the 500 ms
/// DIAGNOSTICS tick, the bot drops it after 15–15.5 s.
pub const PEER_SILENCE_EVICT: Duration = Duration::from_secs(15);

/// A media kind with its own simulcast ladder, layer-id space and sequence space.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum RungKind {
    Audio,
    Video,
}

/// Per-rung receive state. `last_seen` drives rung availability; `max_seq` is that
/// rung's own high-water mark, since every simulcast layer stamps a dense sequence of
/// its own.
struct RungState {
    last_seen: Instant,
    max_seq: Option<u64>,
}

#[derive(Default)]
struct RungAdmission {
    would_decode: bool,
}

/// Welford accumulator for the population standard deviation of consecutive
/// inter-arrival deltas — NOT RFC 3550 jitter.
#[derive(Default)]
struct InterArrival {
    last_ms: Option<f64>,
    arrivals: u64,
    deltas: u64,
    mean: f64,
    m2: f64,
}

impl InterArrival {
    fn record(&mut self, at_ms: f64) {
        self.arrivals += 1;
        if let Some(prev) = self.last_ms.replace(at_ms) {
            let delta = (at_ms - prev).abs();
            self.deltas += 1;
            let from_old_mean = delta - self.mean;
            self.mean += from_old_mean / self.deltas as f64;
            self.m2 += from_old_mean * (delta - self.mean);
        }
    }

    fn stddev_ms(&self) -> f64 {
        if self.deltas == 0 {
            return 0.0;
        }
        (self.m2 / self.deltas as f64).sqrt()
    }

    #[cfg(test)]
    fn arrivals(&self) -> u64 {
        self.arrivals
    }
}

/// `retain` and `remove` never shrink a `HashMap`; this gives the table back.
fn shrink_if_sparse<K: std::hash::Hash + Eq, V>(map: &mut HashMap<K, V>) {
    if map.capacity() > 4 * map.len().max(64) {
        map.shrink_to_fit();
    }
}

impl RungKind {
    fn ladder_len(self) -> usize {
        match self {
            RungKind::Audio => AUDIO_SIMULCAST_MAX_LAYERS,
            RungKind::Video => SIMULCAST_MAX_LAYERS,
        }
    }

    fn pin_kind(self) -> PinMediaKind {
        match self {
            RungKind::Audio => PinMediaKind::Audio,
            RungKind::Video => PinMediaKind::Video,
        }
    }
}

/// Tracks inbound packet statistics for quality-of-service diagnostics.
#[derive(Default)]
pub struct InboundStats {
    audio_packets: u64,
    video_packets: u64,
    video_keyframes: u64,
    heartbeat_packets: u64,
    other_packets: u64,
    audio_bytes: u64,
    video_bytes: u64,
    audio_seq_gaps: u64,
    video_seq_gaps: u64,
    /// Rung marks abandoned this window for silence past `RUNG_WINDOW`. Nonzero means
    /// the gap counters above under-report by an unknown amount.
    audio_rung_expiries: u64,
    video_rung_expiries: u64,
    /// Inter-arrival variability, over every arriving rung.
    video_ia: InterArrival,
    audio_ia: InterArrival,
    // A/V sync dropped: browser audio uses Date.now() ms, video uses EncodedVideoChunk µs — cross-unit delta is meaningless. Re-add when browser wire format is unified.
    parse_errors: u64,
    /// One-way delay per sender stream, from the embedded wall-clock timestamp.
    delay: MediaDelayTracker,
    audio_delay: DelayWindowStats,
    video_delay: DelayWindowStats,
    /// Arrival and sequence state per (session_id, kind, rung).
    ///
    /// Keyed by SESSION, not user: one user on two devices is two independent sources
    /// with independent per-rung sequence counters, so a user-keyed mark makes every
    /// alternation between them look like loss.
    rung_state: HashMap<(u64, RungKind, u32), RungState>,
    /// Per-sender counters drained by the DIAGNOSTICS reporter on its own cadence.
    diag_counters: HashMap<Arc<str>, SenderHealthCounters>,
    /// Total inbound packets since last health drain (all types).
    health_total_packets: u64,
    /// Last time each sender was seen — used to evict stale entries.
    last_seen: HashMap<Arc<str>, Instant>,
    /// Intern map: raw user_id bytes → the one shared name every per-sender map keys on.
    sender_names: HashMap<Vec<u8>, Arc<str>>,
    /// Relay session of each sender's latest media packet.
    sender_session: HashMap<Arc<str>, u64>,
    // NOTE(#1108): the `aq` controller handle and `diagnostics_parse_errors`
    // counter were removed — inbound DIAGNOSTICS are no longer parsed or routed
    // into the AQ (receiver FPS no longer feeds the sender AQ). The bot AQ ticks
    // on a timer in `main`.
    /// Optional RTT probe state. When set, echoed RTT packets are routed here
    /// to compute real round-trip time instead of being counted as media.
    rtt_probe: Option<Arc<RttProbeState>>,
    /// Optional keyframe requester. When set, new peers trigger a
    /// KEYFRAME_REQUEST for VIDEO the first time they are observed.
    keyframe_requester: Option<KeyframeRequester>,
    /// Optional viewport sender. When set, the source session_id of every
    /// inbound media packet is fed here so the bot can emit VIEWPORT control
    /// packets like a real client (HCL issue #988).
    viewport_sender: Option<ViewportSender>,
    /// Optional layer-preference sender. When set, the source session_id of
    /// every inbound media packet is fed here so the bot can emit
    /// LAYER_PREFERENCE control packets pinning each source to a fixed simulcast
    /// layer, like a real client that selected a quality tier (#1083-A2).
    layer_preference_sender: Option<LayerPreferenceSender>,
    /// This client's relay session (from `SESSION_ASSIGNED`); a `LAYER_HINT`
    /// for another session is ignored.
    own_session_id: Option<u64>,
    /// Receives this publisher's camera `LAYER_HINT` caps.
    layer_hint_aq: Option<Arc<BotAq>>,
    /// Optional Prometheus metrics handle. When set, every inbound packet
    /// increments `bot_packets_received_total` (labeled by media_type) and
    /// parse failures increment `bot_packets_parsed_error_total`.
    #[cfg(feature = "metrics")]
    metrics: Option<InboundMetrics>,
}

/// Label bundle for inbound packet metrics.
#[cfg(feature = "metrics")]
struct InboundMetrics {
    metrics: Arc<BotMetrics>,
    bot: String,
    meeting: String,
    delay_labels: Option<DelayLabels>,
}

/// Labels for the delay histograms: this receiver's transport and network
/// profile, plus the profile of each sender run by the same process.
#[cfg(feature = "metrics")]
pub struct DelayLabels {
    pub transport: &'static str,
    pub rx_profile: String,
    pub sender_profiles: Arc<HashMap<String, String>>,
}

impl InboundStats {
    // NOTE(#1108): `set_aq` was removed — inbound DIAGNOSTICS no longer feed the
    // AQ. The bot AQ ticks on a timer in `main`.

    /// Attach an RTT probe state. When set, echoed RTT packets from the relay
    /// are routed to `RttProbeState::record_echo` instead of being counted as
    /// generic media.
    pub fn set_rtt_probe(&mut self, state: Arc<RttProbeState>) {
        self.rtt_probe = Some(state);
    }

    /// This client's relay session, once `SESSION_ASSIGNED` has arrived.
    pub fn own_session_id(&self) -> Option<u64> {
        self.own_session_id
    }

    /// Apply self-targeted `LAYER_HINT` VIDEO caps to `aq`, as the browser
    /// feeds them to its camera encoder's controller.
    pub fn set_layer_hint_aq(&mut self, aq: Arc<BotAq>) {
        self.layer_hint_aq = Some(aq);
    }

    /// Attach a keyframe requester. When set, newly discovered peers trigger
    /// a KEYFRAME_REQUEST for VIDEO.
    pub fn set_keyframe_requester(&mut self, requester: KeyframeRequester) {
        self.keyframe_requester = Some(requester);
    }

    /// Attach a viewport sender. When set, the relay-stamped source session_id
    /// of every inbound media packet is fed to it so the bot emits VIEWPORT
    /// control packets mimicking a real client's on-screen tile set (#988).
    pub fn set_viewport_sender(&mut self, sender: ViewportSender) {
        self.viewport_sender = Some(sender);
    }

    /// Attach a layer-preference sender. When set, the relay-stamped source
    /// session_id of every inbound media packet is fed to it so the bot emits
    /// LAYER_PREFERENCE control packets pinning each source to a fixed simulcast
    /// layer, mimicking a real client that selected a quality tier (#1083-A2).
    pub fn set_layer_preference_sender(&mut self, sender: LayerPreferenceSender) {
        self.layer_preference_sender = Some(sender);
    }

    /// Install (or replace) the Prometheus metrics handle. Calls made before
    /// `set_metrics` are uncounted — we intentionally do not buffer on the
    /// hot path.
    #[cfg(feature = "metrics")]
    pub fn set_metrics(&mut self, metrics: Arc<BotMetrics>, bot: String, meeting: String) {
        self.metrics = Some(InboundMetrics {
            metrics,
            bot,
            meeting,
            delay_labels: None,
        });
    }

    /// Increment `bot_packets_received_total{media_type=…}`. No-op when
    /// metrics are off or unbound.
    #[cfg(feature = "metrics")]
    fn bump_received(&self, media_type: &str) {
        if let Some(m) = &self.metrics {
            m.metrics
                .packets_received_total
                .with_label_values(&[m.bot.as_str(), m.meeting.as_str(), media_type])
                .inc();
        }
    }

    /// Increment `bot_packets_parsed_error_total{stage=…}`.
    #[cfg(feature = "metrics")]
    fn bump_parse_error(&self, stage: &str) {
        if let Some(m) = &self.metrics {
            m.metrics
                .packets_parsed_error_total
                .with_label_values(&[m.bot.as_str(), m.meeting.as_str(), stage])
                .inc();
        }
    }

    /// Highest rung arriving from `session_id` within the window. `0` when nothing is
    /// recent — the bandwidth-safe default, and the same answer
    /// `LayerAvailability::highest_available` gives on an empty map.
    ///
    /// SCOPE, because the obvious reading is wrong: this matches an UNCAPPED receiver —
    /// 1-on-1, pinned, or screen-share. It does NOT match a browser in a multi-tile
    /// grid, where the #1256 rendered-tile lid caps the chooser one or more rungs below
    /// the top (which rung depends on tile size and the viewer's density mode). The bot
    /// also re-derives per packet where the browser only consults availability on its
    /// ~5s tick. Every residual error runs OPTIMISTIC, in exactly the many-participant
    /// regime bots simulate.
    ///
    /// Freshness is tested inside the lookup rather than pruned first, so this is at
    /// most `kind.ladder_len()` direct lookups — O(1) in publisher count — and
    /// eviction is left to [`Self::reset`].
    fn highest_arriving_rung(&self, kind: RungKind, session_id: u64, now: Instant) -> u32 {
        (0..kind.ladder_len() as u32)
            .rev()
            .find(|rung| {
                self.rung_state
                    .get(&(session_id, kind, *rung))
                    .is_some_and(|st| now.duration_since(st.last_seen) <= RUNG_WINDOW)
            })
            .unwrap_or(0)
    }

    /// The rung this bot would decode from `session_id` for `kind`.
    ///
    /// With `--pin-layer N` the receiver's exact-match guard stays on N: the relay
    /// never drops rung 0, so deriving from arrivals would fall back to 0 and count
    /// frames the browser would SKIP. So a pin wins over observation, and only a pin
    /// scoped to THIS kind: the relay keys its drop on `(source, kind)`.
    fn decoded_rung_for(&self, kind: RungKind, session_id: u64, now: Instant) -> u32 {
        if let Some(pinned) = self
            .layer_preference_sender
            .as_ref()
            .and_then(|lps| lps.pinned_layer_for(kind.pin_kind()))
        {
            return pinned;
        }
        self.highest_arriving_rung(kind, session_id, now)
    }

    /// The single enforcement point for per-rung inbound accounting: admission,
    /// availability insert, decode decision, sequence-gap tracking. Returns whether
    /// this bot would DECODE the packet. `simulcast_layer_id` is publisher-controlled
    /// cleartext, so an off-ladder value touches NO map — cycling unique ids would
    /// otherwise mint one entry per packet.
    fn admit_rung(
        &mut self,
        kind: RungKind,
        session_id: u64,
        rung: u32,
        sequence: Option<u64>,
        now: Instant,
    ) -> RungAdmission {
        if (rung as usize) >= kind.ladder_len() {
            return RungAdmission::default();
        }

        let mut gap = 0u64;
        let st = self
            .rung_state
            .entry((session_id, kind, rung))
            .or_insert(RungState {
                last_seen: now,
                max_seq: None,
            });
        // Enforced HERE, not only in `reset`'s 10s sweep, which bit or missed depending
        // on where the tick landed inside the silence.
        let expired = now.duration_since(st.last_seen) > RUNG_WINDOW;
        st.last_seen = now;
        // Frames the relay shed were never forwarded here, so they are not loss. Dropped
        // before the sequence branch: a packet carrying no sequence must still un-anchor,
        // or the next sequenced one books the whole silence against a stale mark.
        if expired {
            st.max_seq = None;
        }
        if let Some(seq) = sequence {
            match st.max_seq {
                Some(max_seen) if seq > max_seen => {
                    gap = seq - max_seen - 1;
                    st.max_seq = Some(seq);
                }
                // A restart re-initialises the encoder's sequence to 0: a mic or camera
                // toggled off then on, a device hot-plug, a page-reload rejoin, or a bot
                // pod restart. Without re-anchoring the mark wedges at its pre-restart
                // value and the loss signal goes blind until the sequence climbs back.
                Some(max_seen) if max_seen.saturating_sub(seq) >= SEQ_RESET_REANCHOR_GAP => {
                    st.max_seq = Some(seq);
                }
                Some(_) => {}
                None => st.max_seq = Some(seq),
            }
        }
        match kind {
            RungKind::Audio => {
                self.audio_seq_gaps += gap;
                self.audio_rung_expiries += u64::from(expired);
            }
            RungKind::Video => {
                self.video_seq_gaps += gap;
                self.video_rung_expiries += u64::from(expired);
            }
        }

        RungAdmission {
            would_decode: rung == self.decoded_rung_for(kind, session_id, now),
        }
    }

    // `_my_user_id` is retained in the signature (many callers pass it) but is
    // no longer used: DIAGNOSTICS filtering-by-sender moved out with the AQ
    // fan-in removal (issue #1108).
    pub fn record_packet(&mut self, _my_user_id: &str, data: &[u8]) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as f64;

        self.health_total_packets += 1;

        let Ok(wrapper) = PacketWrapper::parse_from_bytes(data) else {
            self.parse_errors += 1;
            #[cfg(feature = "metrics")]
            self.bump_parse_error("wrapper");
            return;
        };
        let session_id = wrapper.session_id;

        match wrapper.packet_type.enum_value() {
            Ok(PacketType::MEETING) => {
                if let Ok(meeting) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                    if meeting.event_type.enum_value() == Ok(MeetingEventType::PARTICIPANT_LEFT) {
                        self.remove_sender(&meeting.target_user_id, meeting.session_id);
                    }
                }
            }
            Ok(PacketType::SESSION_ASSIGNED) => self.own_session_id = Some(session_id),
            Ok(PacketType::LAYER_HINT) => self.on_layer_hint(&wrapper),
            _ => {}
        }

        // DIAGNOSTICS packets: counted for inbound-stats accounting only.
        //
        // NOTE(#1108): previously these were parsed and fed into the bot's AQ
        // controller (`aq.process_diagnostics`) so the bot reacted to receiver-
        // reported FPS like a browser. Stage 2 removed receiver FPS from the
        // sender AQ entirely, so we no longer parse or route them — the bot's AQ
        // is now a self-timer driven from `main` (see `BotAq::tick`). We still
        // account for the packet here so inbound stats stay accurate.
        if wrapper.packet_type.enum_value() == Ok(PacketType::DIAGNOSTICS) {
            self.other_packets += 1;
            #[cfg(feature = "metrics")]
            self.bump_received("diagnostics");
            return;
        }

        if wrapper.packet_type.enum_value() != Ok(PacketType::MEDIA) {
            self.other_packets += 1;
            #[cfg(feature = "metrics")]
            self.bump_received("other");
            return;
        }

        let Ok(media) = MediaPacket::parse_from_bytes(&wrapper.data) else {
            self.parse_errors += 1;
            #[cfg(feature = "metrics")]
            self.bump_parse_error("media");
            return;
        };

        // Intercept echoed RTT packets BEFORE normal media accounting.
        // The relay echoes the entire PacketWrapper back verbatim when
        // media_type == RTT, so our own probe comes back with our user_id
        // in wrapper.user_id. Route to the RTT probe state for RTT calc.
        if media.media_type.enum_value() == Ok(MediaType::RTT) {
            if let Some(ref rtt_state) = self.rtt_probe {
                rtt_state.record_echo(media.timestamp);
                debug!("RTT echo received, timestamp={:.1}", media.timestamp);
            }
            #[cfg(feature = "metrics")]
            self.bump_received("rtt");
            self.other_packets += 1;
            return;
        }

        // The relay populates wrapper.user_id but strips media.user_id,
        // so use the wrapper-level user_id for per-sender tracking.
        let sender = self.intern_sender(&wrapper.user_id);

        // MONOTONIC on purpose: a backward NTP step makes a wall-clock delta negative
        // and holds stale rungs past the window. `now_ms` is wall-clock by contrast.
        let now = Instant::now();

        // Update last-seen time for stale entry eviction.
        self.last_seen.insert(Arc::clone(&sender), now);
        self.sender_session.insert(Arc::clone(&sender), session_id);

        // Notify keyframe requester about newly seen peers.
        if let Some(ref mut kr) = self.keyframe_requester {
            kr.on_peer_seen(&sender);
        }

        // Whether THIS packet is VIDEO. The relay VIEWPORT filter applies to
        // VIDEO only (constants.rs::viewport_should_drop, gated by the is_video
        // branch in chat_server.rs), so the viewport sender uses is_video to arm
        // its reconnect re-assert exclusively on off-viewport VIDEO — the
        // observable fail-open symptom. AUDIO / SCREEN are never viewport-
        // filtered, so an off-viewport packet of those kinds is expected on a
        // healthy connection and must NOT arm the re-assert (HCL #1006). RTT is
        // already intercepted and returned above, so it never reaches here.
        //
        // NOTE: this gating is VIEWPORT-only. The LAYER_PREFERENCE filter is
        // per-(source,kind) and applies to VIDEO/SCREEN/AUDIO. Since #2206 the bot
        // DOES read `simulcast_layer_id` on inbound video, so symptom-gating this
        // one is now tractable — but it is not implemented: the signal would need to
        // distinguish "relay forgot my preference" from a publisher that legitimately
        // shed the pinned rung. The layer-preference sender therefore keeps its
        // re-assert-every-reset-window behaviour (heals fail-open within one window
        // at the cost of a periodic control packet); #1006 is VIEWPORT-only.
        let is_video = media.media_type.enum_value() == Ok(MediaType::VIDEO);

        // Feed the relay-stamped source session_id to the viewport sender so it
        // can emit VIEWPORT control packets like a real client (#988). The
        // relay stamps `wrapper.session_id` to the publisher's session on
        // forward (it is 0 only for unstamped/legacy packets, which the sender
        // ignores). This mirrors how the browser derives peers from
        // `PacketWrapper.session_id` on the decode path.
        if let Some(ref mut vs) = self.viewport_sender {
            vs.on_source_seen(wrapper.session_id, is_video);
        }

        // Feed the same relay-stamped source session_id to the layer-preference
        // sender so a "pin to layer N" bot emits a LAYER_PREFERENCE for each
        // discovered source (#1083-A2). Same fail-open/unstamped-sentinel
        // handling as the viewport sender above.
        if let Some(ref mut lps) = self.layer_preference_sender {
            lps.on_source_seen(wrapper.session_id);
        }

        match media.media_type.enum_value() {
            Ok(MediaType::AUDIO) => {
                #[cfg(feature = "metrics")]
                self.bump_received("audio");
                // #2244: count the decoded rung only, as the VIDEO arm has since #2206.
                let admission = self.admit_rung(
                    RungKind::Audio,
                    session_id,
                    wrapper.simulcast_layer_id,
                    media.audio_metadata.as_ref().map(|meta| meta.sequence),
                    now,
                );

                self.audio_bytes += media.data.len() as u64;
                self.audio_ia.record(now_ms);
                if admission.would_decode {
                    self.audio_packets += 1;
                    self.observe_delay(
                        &sender,
                        session_id,
                        DelayKind::Audio,
                        now_ms,
                        media.timestamp,
                        now,
                    );
                }

                // Accumulate health counters for this sender
                self.diag_counters
                    .entry(Arc::clone(&sender))
                    .or_default()
                    .audio_bytes += media.data.len() as u64;
            }
            Ok(MediaType::VIDEO) => {
                #[cfg(feature = "metrics")]
                self.bump_received("video");

                // #2206: count only the rung this bot would DECODE, mirroring the
                // browser's EXACT-MATCH guard. Bytes and arrivals stay unfiltered —
                // those measure what the link actually delivered, which is the
                // honest figure for a receiver the relay is fanning every rung to.
                let admission = self.admit_rung(
                    RungKind::Video,
                    session_id,
                    wrapper.simulcast_layer_id,
                    media.video_metadata.as_ref().map(|meta| meta.sequence),
                    now,
                );

                self.video_bytes += media.data.len() as u64;
                self.video_ia.record(now_ms);
                if admission.would_decode {
                    self.video_packets += 1;
                    self.observe_delay(
                        &sender,
                        session_id,
                        DelayKind::Video,
                        now_ms,
                        media.timestamp,
                        now,
                    );
                }

                // Accumulate health counters for this sender
                self.diag_counters
                    .entry(Arc::clone(&sender))
                    .or_default()
                    .video_bytes += media.data.len() as u64;

                if admission.would_decode && media.frame_type == "key" {
                    self.video_keyframes += 1;
                }
            }
            Ok(MediaType::HEARTBEAT) => {
                #[cfg(feature = "metrics")]
                self.bump_received("health");
                self.heartbeat_packets += 1;
            }
            _ => {
                #[cfg(feature = "metrics")]
                self.bump_received("other");
                self.other_packets += 1;
            }
        }
    }

    /// Record one decoded-layer packet's one-way delay (skipped, and counted,
    /// when the sender timestamp is not wall-clock ms).
    #[cfg_attr(not(feature = "metrics"), allow(unused_variables))]
    fn observe_delay(
        &mut self,
        sender: &str,
        stream: u64,
        kind: DelayKind,
        now_ms: f64,
        sender_ts_ms: f64,
        now: Instant,
    ) {
        let Some(sample) = self.delay.observe(stream, kind, now_ms, sender_ts_ms, now) else {
            #[cfg(feature = "metrics")]
            if let Some(m) = &self.metrics {
                m.metrics
                    .media_delay_implausible_total
                    .with_label_values(&[kind.as_str()])
                    .inc();
            }
            return;
        };
        match kind {
            DelayKind::Audio => self.audio_delay.record(sample),
            DelayKind::Video => self.video_delay.record(sample),
        }
        #[cfg(feature = "metrics")]
        if let Some(m) = &self.metrics {
            if let Some(l) = &m.delay_labels {
                let tx = l
                    .sender_profiles
                    .get(sender)
                    .map(String::as_str)
                    .unwrap_or("external");
                let labels = [kind.as_str(), tx, l.rx_profile.as_str(), l.transport];
                m.metrics
                    .media_owd_ms
                    .with_label_values(&labels)
                    .observe(sample.owd_ms);
                m.metrics
                    .media_excess_delay_ms
                    .with_label_values(&labels)
                    .observe(sample.excess_ms);
            }
        }
    }

    /// Delay statistics for the current reporting window: (audio, video).
    pub fn delay_window(&self) -> (DelayWindowStats, DelayWindowStats) {
        (self.audio_delay, self.video_delay)
    }

    /// Label the delay histograms (no-op until `set_metrics` has run).
    #[cfg(feature = "metrics")]
    pub fn set_delay_labels(&mut self, labels: DelayLabels) {
        if let Some(m) = &mut self.metrics {
            m.delay_labels = Some(labels);
        }
    }

    pub fn report(&self, user_id: &str) {
        let audio_iastddev = self.audio_ia.stddev_ms();
        let video_iastddev = self.video_ia.stddev_ms();

        info!(
            "[{}] RX STATS (10s): audio={} decoded-rung pkts (all rungs: {:.0} KB, \
             ia_stddev={:.1}ms, gaps={}, rung_expiries={}), video={} decoded-rung pkts \
             ({} key, all rungs: {:.0} KB, ia_stddev={:.1}ms, gaps={}, rung_expiries={}), \
             heartbeat={}, errors={}, delay audio mean/max={:.0}/{:.0}ms excess_max={:.0}ms, \
             video mean/max={:.0}/{:.0}ms excess_max={:.0}ms",
            user_id,
            self.audio_packets,
            self.audio_bytes as f64 / 1024.0,
            audio_iastddev,
            self.audio_seq_gaps,
            self.audio_rung_expiries,
            self.video_packets,
            self.video_keyframes,
            self.video_bytes as f64 / 1024.0,
            video_iastddev,
            self.video_seq_gaps,
            self.video_rung_expiries,
            self.heartbeat_packets,
            self.parse_errors,
            self.audio_delay.owd_mean_ms(),
            self.audio_delay.owd_max_ms,
            self.audio_delay.excess_max_ms,
            self.video_delay.owd_mean_ms(),
            self.video_delay.owd_max_ms,
            self.video_delay.excess_max_ms,
        );
    }

    pub fn reset(&mut self) {
        // Only the window's counters are cleared; attached senders, session
        // identity and per-sender maps persist.
        //
        // Rolling 4s availability window — dropping it on the 10s diagnostic reset
        // would repeat the ramp and inflate the next health sample (#2206). Stale
        // entries are evicted HERE rather than on the packet path: `reset` runs every
        // 10s in both pin and observation modes, so this is the one sweep that bounds
        // the map against per-reconnect `session_id` churn.
        let sweep_now = Instant::now();
        // A rung silent past the window loses its mark: what the relay shed was never sent
        // here. Counted, like the packet-path expiry, so the abandoned stretch is visible.
        let mut swept = (0u64, 0u64);
        self.rung_state.retain(|&(_, kind, _), st| {
            let keep = sweep_now.duration_since(st.last_seen) <= RUNG_WINDOW;
            if !keep {
                match kind {
                    RungKind::Audio => swept.0 += 1,
                    RungKind::Video => swept.1 += 1,
                }
            }
            keep
        });
        shrink_if_sparse(&mut self.rung_state);
        self.delay.evict_idle(sweep_now);
        self.audio_packets = 0;
        self.video_packets = 0;
        self.video_keyframes = 0;
        self.heartbeat_packets = 0;
        self.other_packets = 0;
        self.audio_bytes = 0;
        self.video_bytes = 0;
        self.audio_seq_gaps = 0;
        self.video_seq_gaps = 0;
        self.parse_errors = 0;
        self.video_ia = InterArrival::default();
        self.audio_ia = InterArrival::default();
        self.audio_delay = DelayWindowStats::default();
        self.video_delay = DelayWindowStats::default();
        // Seeded, not zeroed: `report` runs before `reset`, so a sweep's marks land in the
        // window that opens here rather than the one that just closed.
        self.audio_rung_expiries = swept.0;
        self.video_rung_expiries = swept.1;

        // Re-assert the VIEWPORT (#988 load-test fidelity). The relay drops a
        // bot's viewport subscription on disconnect, and a reconnect / re-election
        // allocates a fresh empty viewport (fail-open → the bot silently receives
        // ALL video again). The real browser client re-sends its viewport on the
        // `Connected` state edge to recover from this; the bot has no such event,
        // so we re-assert off this periodic reset hook instead.
        //
        // reset-vs-first-connect: this hook is the 10s diagnostic-window reset,
        // NOT a dedicated reconnect callback, so it also fires during a healthy
        // connection. To avoid re-asserting an unchanged viewport every window
        // (HCL #1006 — that blunts relay_viewport_updates_total{outcome=accepted}
        // as the "client re-subscribed after a flap" signal),
        // `resend_on_reconnect` is GATED on the observable fail-open symptom:
        // inbound VIDEO from a source outside the last-sent viewport (the relay
        // viewport-filters VIDEO only, so off-viewport video means its copy of
        // our subscription is gone). On a steady connection with an unchanged
        // visible set the relay forwards NO off-viewport video, so this re-asserts
        // ZERO times after the initial send. It is additionally guarded on
        // `has_sent` (a bot that has not yet rendered anyone never double-sends)
        // and rate-limited (MIN_RESEND_INTERVAL, now a secondary burst guard). The
        // `known_sources` set is preserved across reset (take/restored above), so
        // an armed re-assert reflects exactly the subset the bot was rendering.
        // Net effect: a fail-open (re-election / failover) is healed within one
        // reset window; a steady connection stays silent.
        if let Some(ref mut vs) = self.viewport_sender {
            vs.resend_on_reconnect();
        }

        // Same re-assert for the layer-preference signal (#1083-A2): the relay
        // drops a receiver's recorded layer preference on disconnect, and a
        // reconnect leaves it empty (fail-open → the bot silently receives the
        // full ladder again). `resend_on_reconnect` is idempotent, guarded on
        // `has_sent`, and rate-limited, exactly like the viewport re-assert.
        //
        // NOTE: unlike the viewport re-assert above, this is NOT gated on an
        // observed fail-open symptom. Since #2206 the per-rung arrival IS visible on
        // inbound video, so a gate is now buildable — but an arriving non-pinned rung
        // does not by itself distinguish "relay forgot my preference" from a
        // publisher that shed the pinned rung, so it retains the
        // re-assert-every-window behaviour (heals within one window at the cost of a
        // periodic control packet); #1006's symptom-gating is VIEWPORT-only.
        if let Some(ref mut lps) = self.layer_preference_sender {
            lps.resend_on_reconnect();
        }
    }

    /// The browser's `LAYER_HINT` arm (`video_call_client.rs`): only a hint
    /// stamped with our own session counts, and only its VIDEO entry is used.
    fn on_layer_hint(&mut self, wrapper: &PacketWrapper) {
        let (Some(aq), Some(own)) = (&self.layer_hint_aq, self.own_session_id) else {
            return;
        };
        if wrapper.session_id != own {
            return;
        }
        let Ok(hint) = LayerHintPacket::parse_from_bytes(&wrapper.data) else {
            return;
        };
        for entry in &hint.entries {
            if entry.media_kind.enum_value() == Ok(HintMediaKind::VIDEO) {
                aq.observe_layer_hint(entry.max_requested_layer);
            }
        }
    }

    /// Forget a peer that left (relay `PARTICIPANT_LEFT`), as the browser's
    /// `remove_peer` does, so its DIAGNOSTICS trackers stop at once.
    /// The browser removes by session, so a leave for a session other than the
    /// sender's latest (a reconnect's old one) is ignored.
    pub fn remove_sender(&mut self, raw_user_id: &[u8], session_id: u64) {
        let name = String::from_utf8_lossy(raw_user_id);
        if self
            .sender_session
            .get(name.as_ref())
            .is_some_and(|&live| live != session_id)
        {
            return;
        }
        self.sender_session.remove(name.as_ref());
        self.last_seen.remove(name.as_ref());
        self.diag_counters.remove(name.as_ref());
        self.sender_names.remove(raw_user_id);
    }

    #[cfg(test)]
    pub(crate) fn backdate_sender(&mut self, name: &str, age: Duration) {
        if let Some(seen) = self.last_seen.get_mut(name) {
            *seen = Instant::now() - age;
        }
    }

    /// Remove entries from ALL per-sender maps for senders not seen within `max_age`.
    /// Call this periodically (e.g. from the 10s reporting tick) to bound memory.
    pub fn evict_stale(&mut self, max_age: Duration) {
        let cutoff = Instant::now() - max_age;
        let stale_senders: Vec<Arc<str>> = self
            .last_seen
            .iter()
            .filter(|(_, &ts)| ts < cutoff)
            .map(|(k, _)| Arc::clone(k))
            .collect();

        for sender in &stale_senders {
            self.last_seen.remove(&**sender);
            self.sender_session.remove(&**sender);
            self.diag_counters.remove(&**sender);
        }

        // Also evict from the intern map — find Vec<u8> keys whose name value
        // matches a stale sender.
        if !stale_senders.is_empty() {
            self.sender_names
                .retain(|_, v| !stale_senders.iter().any(|s| s == v));
            shrink_if_sparse(&mut self.last_seen);
            shrink_if_sparse(&mut self.sender_names);
        }
    }

    /// Inbound packets of every type since the last call, for HEALTH.
    pub fn take_health_total(&mut self) -> u64 {
        std::mem::take(&mut self.health_total_packets)
    }

    /// Drain the per-sender bytes since the last call (DIAGNOSTICS).
    pub fn drain_diagnostics_counters(&mut self) -> HashMap<String, SenderHealthCounters> {
        std::mem::take(&mut self.diag_counters)
            .into_iter()
            .map(|(name, c)| (name.to_string(), c))
            .collect()
    }

    /// Senders currently known (not yet evicted as stale).
    pub fn known_senders(&self) -> std::collections::HashSet<String> {
        self.last_seen.keys().map(|k| k.to_string()).collect()
    }

    /// An `Arc` and not a borrow so callers can key the per-sender maps while
    /// still holding `&mut self`.
    fn intern_sender(&mut self, raw: &[u8]) -> Arc<str> {
        if let Some(name) = self.sender_names.get(raw) {
            return Arc::clone(name);
        }
        let name: Arc<str> = Arc::from(String::from_utf8_lossy(raw).as_ref());
        self.sender_names.insert(raw.to_vec(), Arc::clone(&name));
        name
    }
}

/// Wire packets for tests in this and sibling modules.
#[cfg(test)]
pub(crate) mod test_packets {
    use protobuf::Message;
    use videocall_types::protos::media_packet::media_packet::MediaType;
    use videocall_types::protos::media_packet::MediaPacket;
    use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
    use videocall_types::protos::meeting_packet::MeetingPacket;
    use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
    use videocall_types::protos::packet_wrapper::PacketWrapper;

    /// A 100-byte media packet from `sender`.
    pub(crate) fn media(sender: &str, media_type: MediaType) -> Vec<u8> {
        media_from(sender, media_type, 7)
    }

    /// A 100-byte media packet from `sender`'s relay session `session_id`.
    pub(crate) fn media_from(sender: &str, media_type: MediaType, session_id: u64) -> Vec<u8> {
        let mut media = MediaPacket::new();
        media.media_type = media_type.into();
        media.data = vec![0u8; 100];
        PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            user_id: sender.as_bytes().to_vec(),
            data: media.write_to_bytes().unwrap(),
            session_id,
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()
    }

    /// A relay packet of `packet_type` stamped with `session_id`.
    pub(crate) fn control(packet_type: PacketType, session_id: u64, data: Vec<u8>) -> Vec<u8> {
        PacketWrapper {
            packet_type: packet_type.into(),
            session_id,
            data,
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()
    }

    /// The relay's `PARTICIPANT_LEFT` for `user_id` (`session_manager.rs` `build_peer_left_packet`).
    pub(crate) fn participant_left(user_id: &str) -> Vec<u8> {
        participant_left_session(user_id, 7)
    }

    /// The relay's `PARTICIPANT_LEFT` for one session of `user_id`.
    pub(crate) fn participant_left_session(user_id: &str, session_id: u64) -> Vec<u8> {
        let meeting = MeetingPacket {
            event_type: MeetingEventType::PARTICIPANT_LEFT.into(),
            target_user_id: user_id.as_bytes().to_vec(),
            session_id,
            ..Default::default()
        };
        PacketWrapper {
            packet_type: PacketType::MEETING.into(),
            user_id: b"system".to_vec(),
            data: meeting.write_to_bytes().unwrap(),
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::Message;
    use videocall_types::protos::media_packet::media_packet::MediaType;
    use videocall_types::protos::media_packet::{AudioMetadata, MediaPacket, VideoMetadata};
    use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
    use videocall_types::protos::packet_wrapper::PacketWrapper;

    /// Build a serialized PacketWrapper containing a MediaPacket for testing.
    fn make_media_packet(sender: &str, media_type: MediaType, seq: u64, timestamp: f64) -> Vec<u8> {
        let mut media = MediaPacket::new();
        media.media_type = media_type.into();
        media.data = vec![0u8; 100]; // dummy payload
        media.timestamp = timestamp;

        match media_type {
            MediaType::AUDIO => {
                media.audio_metadata = Some(AudioMetadata {
                    sequence: seq,
                    ..Default::default()
                })
                .into();
            }
            MediaType::VIDEO => {
                media.video_metadata = Some(VideoMetadata {
                    sequence: seq,
                    ..Default::default()
                })
                .into();
            }
            _ => {}
        }

        let wrapper = PacketWrapper {
            user_id: sender.as_bytes().to_vec(),
            packet_type: PacketType::MEDIA.into(),
            data: media.write_to_bytes().unwrap(),
            ..Default::default()
        };
        wrapper.write_to_bytes().unwrap()
    }

    /// Same as [`make_media_packet`] but stamps the outer wrapper's
    /// `simulcast_layer_id` — set by the PUBLISHER per layer and forwarded verbatim
    /// by the relay, which does not rewrite or validate it.
    fn make_video_packet_on_rung(
        sender: &str,
        session_id: u64,
        seq: u64,
        timestamp: f64,
        rung: u32,
    ) -> Vec<u8> {
        let mut wrapper = PacketWrapper::parse_from_bytes(&make_media_packet(
            sender,
            MediaType::VIDEO,
            seq,
            timestamp,
        ))
        .unwrap();
        wrapper.simulcast_layer_id = rung;
        wrapper.session_id = session_id;
        wrapper.write_to_bytes().unwrap()
    }

    /// Seed a rung's arrival instant without a packet, for window-expiry tests.
    fn seed_rung(stats: &mut InboundStats, kind: RungKind, session: u64, rung: u32, at: Instant) {
        stats.rung_state.insert(
            (session, kind, rung),
            RungState {
                last_seen: at,
                max_seq: None,
            },
        );
    }

    const ALICE: u64 = 11;
    const BOB: u64 = 22;

    fn wall_now_ms() -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as f64
    }

    #[test]
    fn decoded_audio_and_video_record_one_way_delay_from_the_sender_timestamp() {
        let mut stats = InboundStats::default();
        let sent = wall_now_ms() - 150.0;
        stats.record_packet("me", &make_media_packet("alice", MediaType::AUDIO, 1, sent));
        stats.record_packet("me", &make_media_packet("alice", MediaType::VIDEO, 1, sent));
        let (audio, video) = stats.delay_window();
        assert_eq!(audio.count, 1);
        assert_eq!(video.count, 1);
        assert!(
            (150.0..1_000.0).contains(&audio.owd_max_ms),
            "audio owd {}",
            audio.owd_max_ms
        );
    }

    #[test]
    fn a_media_time_timestamp_records_no_delay() {
        let mut stats = InboundStats::default();
        // Browser video carries EncodedVideoChunk.timestamp(), not wall-clock ms.
        stats.record_packet(
            "me",
            &make_media_packet("alice", MediaType::VIDEO, 1, 33_366.0),
        );
        assert_eq!(stats.delay_window().1.count, 0);
        assert_eq!(stats.delay.implausible(), 1);
    }

    #[test]
    fn the_delay_floor_survives_the_report_reset() {
        let mut stats = InboundStats::default();
        stats.record_packet(
            "me",
            &make_media_packet("alice", MediaType::AUDIO, 1, wall_now_ms() - 50.0),
        );
        stats.reset();
        assert_eq!(stats.delay_window().0.count, 0, "window stats reset");
        stats.record_packet(
            "me",
            &make_media_packet("alice", MediaType::AUDIO, 2, wall_now_ms() - 450.0),
        );
        let (audio, _) = stats.delay_window();
        assert!(
            audio.excess_max_ms >= 350.0,
            "floor from before the reset must still apply, excess {}",
            audio.excess_max_ms
        );
    }

    #[test]
    fn participant_left_forgets_the_peer_at_once() {
        let mut stats = InboundStats::default();
        stats.record_packet("me", &test_packets::media("alice", MediaType::AUDIO));
        stats.record_packet("me", &test_packets::media("bob", MediaType::AUDIO));
        stats.record_packet("me", &test_packets::participant_left("alice"));
        assert!(!stats.known_senders().contains("alice"));
        assert!(stats.known_senders().contains("bob"));
        assert!(!stats.drain_diagnostics_counters().contains_key("alice"));
    }

    #[test]
    fn only_a_self_targeted_video_layer_hint_caps_the_published_ladder() {
        use videocall_aq::TestClock;
        use videocall_types::protos::layer_hint_packet::layer_hint_packet::Entry;
        let clock = Arc::new(TestClock::new(0));
        let aq = BotAq::new(clock.clone());
        aq.set_simulcast_layers(3);
        let mut stats = InboundStats::default();
        stats.set_layer_hint_aq(Arc::clone(&aq));
        let hint = |kind: HintMediaKind, max: u32| {
            let mut entry = Entry::new();
            entry.media_kind = kind.into();
            entry.max_requested_layer = max;
            let mut packet = LayerHintPacket::new();
            packet.entries.push(entry);
            packet.write_to_bytes().unwrap()
        };
        let mut t = 0;
        let mut tick = |aq: &BotAq| {
            t += 1_000;
            clock.set_ms(t);
            aq.tick();
            aq.simulcast_snapshot().active
        };
        stats.record_packet(
            "me",
            &test_packets::control(PacketType::SESSION_ASSIGNED, 42, vec![]),
        );
        let to = |session| {
            test_packets::control(
                PacketType::LAYER_HINT,
                session,
                hint(HintMediaKind::VIDEO, 0),
            )
        };
        stats.record_packet("me", &to(43));
        stats.record_packet(
            "me",
            &test_packets::control(PacketType::LAYER_HINT, 42, hint(HintMediaKind::AUDIO, 0)),
        );
        assert_eq!(
            tick(&aq),
            3,
            "another session's hint and AUDIO entries are ignored"
        );
        stats.reset();
        stats.record_packet("me", &to(42));
        assert_eq!(
            tick(&aq),
            1,
            "our own VIDEO hint sheds to the base layer, also after the 10 s reset"
        );
    }

    #[test]
    fn a_leave_for_an_old_session_keeps_the_reconnected_peer() {
        let mut stats = InboundStats::default();
        stats.record_packet(
            "me",
            &test_packets::media_from("alice", MediaType::AUDIO, 8),
        );
        stats.record_packet("me", &test_packets::participant_left_session("alice", 7));
        assert!(stats.known_senders().contains("alice"));
        stats.record_packet("me", &test_packets::participant_left_session("alice", 8));
        assert!(!stats.known_senders().contains("alice"));
    }

    #[test]
    fn a_peer_silent_for_three_browser_monitor_ticks_is_gone() {
        let mut stats = InboundStats::default();
        stats.record_packet("me", &test_packets::media("alice", MediaType::AUDIO));
        stats.record_packet("me", &test_packets::media("bob", MediaType::AUDIO));
        *stats.last_seen.get_mut("alice").unwrap() = Instant::now() - Duration::from_secs(16);
        *stats.last_seen.get_mut("bob").unwrap() = Instant::now() - Duration::from_secs(9);
        stats.evict_stale(PEER_SILENCE_EVICT);
        assert!(!stats.known_senders().contains("alice"));
        assert!(stats.known_senders().contains("bob"));
    }

    #[test]
    fn diagnostics_counters_drain_independently_and_survive_the_report_reset() {
        let mut stats = InboundStats::default();
        stats.record_packet("me", &make_media_packet("alice", MediaType::AUDIO, 1, 0.0));
        stats.record_packet("me", &make_media_packet("alice", MediaType::VIDEO, 1, 0.0));
        // The HEALTH drain must not consume the DIAGNOSTICS window.
        let _ = stats.take_health_total();
        stats.reset();
        let window = stats.drain_diagnostics_counters();
        assert_eq!(window["alice"].audio_bytes, 100);
        assert_eq!(window["alice"].video_bytes, 100);
        assert!(stats.drain_diagnostics_counters().is_empty());
        assert!(stats.known_senders().contains("alice"));
    }

    #[test]
    fn video_packets_counts_one_rung_not_the_ladder_sum() {
        // #2206: the relay fails open and forwards EVERY rung to a healthy
        // receiver, so an unfiltered count reads the ladder sum — an 8 fps 3-rung
        // camera measured ~52. The browser skips non-selected rungs before decode
        // and has counted DECODED frames since #2190; the bot must match or one
        // proto field means two things by producer.
        let mut stats = InboundStats::default();

        // Warm-up frame: while the top rung is not yet observed, the lower rungs ARE
        // momentarily the highest arriving one and count. Measure the steady state
        // after the ladder is known, not across it.
        //
        // Not the same shape as the browser's join ramp: `LayerChooser` is
        // unconstrained at cold start and returns `highest_available` on its first
        // window, so its ramp comes from `selected_video_layer` initialising to 0 and
        // only moving on the ~5s monitor tick — a step, not a 3-packet over-count.
        for rung in 0..3u32 {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", ALICE, 0, 1000.0, rung),
            );
        }
        let after_warmup = stats.video_packets;

        let frames = 8u64;
        for seq in 1..=frames {
            for rung in 0..3u32 {
                let data =
                    make_video_packet_on_rung("alice", ALICE, seq, 1000.0 + seq as f64, rung);
                stats.record_packet("bot", &data);
            }
        }

        assert_eq!(
            stats.video_packets - after_warmup,
            frames,
            "steady state must count ONE rung's frames, not all three"
        );
        // Bytes and arrivals stay unfiltered — they measure what the link
        // delivered, which is the honest figure for a fanned-out receiver.
        let total_packets = (frames + 1) * 3;
        assert_eq!(stats.video_ia.arrivals(), total_packets);
        assert_eq!(stats.video_bytes, total_packets * 100);
    }

    #[test]
    fn video_packets_tracks_the_top_rung_per_sender_independently() {
        // Two senders offering different ladder depths must not pool their rungs.
        let mut stats = InboundStats::default();
        // Establish alice's 3-rung ladder before measuring (see the ramp note above).
        for rung in 0..3u32 {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", ALICE, 0, 1000.0, rung),
            );
        }
        let base = stats.video_packets;

        for seq in 1..=4u64 {
            for rung in 0..3u32 {
                stats.record_packet(
                    "bot",
                    &make_video_packet_on_rung("alice", ALICE, seq, 1000.0 + seq as f64, rung),
                );
            }
            // bob publishes one layer, so rung 0 IS his top and every frame counts.
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("bob", BOB, seq, 1000.0 + seq as f64, 0),
            );
        }
        // alice contributes 4 (one per frame), bob 4 — pooling the two senders'
        // rungs would make bob's rung-0 frames fail alice's top-rung test.
        assert_eq!(stats.video_packets - base, 8);
    }

    #[test]
    fn video_packets_follows_the_top_rung_rate_not_the_base_rate() {
        // The rungs run at DIFFERENT rates (the camera ladder is 7/15/30 fps), so
        // which rung is counted changes the number — this is what distinguishes
        // "highest arriving" from "base only". A healthy unconstrained receiver
        // fails open to the best available layer, so the bot must report the TOP
        // rung's rate; reporting the base would under-state a real client.
        let mut stats = InboundStats::default();

        // Establish the ladder, then emit 12 top-rung frames for every 3 base ones
        // (a 4:1 rate ratio, the shape of 30 fps vs 7 fps).
        for rung in 0..3u32 {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", ALICE, 0, 1000.0, rung),
            );
        }
        let base = stats.video_packets;

        let top_frames = 12u64;
        for i in 1..=top_frames {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", ALICE, i, 1000.0 + i as f64, 2),
            );
            if i % 4 == 0 {
                stats.record_packet(
                    "bot",
                    &make_video_packet_on_rung("alice", ALICE, i, 1000.0 + i as f64, 0),
                );
            }
        }

        let counted = stats.video_packets - base;
        // The base rung is interleaved at 1/4 the top rung's rate, so following the
        // wrong one reads `top_frames / 4` — the equality below discriminates them.
        assert_eq!(
            counted, top_frames,
            "must follow the TOP rung's rate, not the base rung's"
        );
    }

    #[test]
    fn a_shed_top_rung_expires_so_the_new_top_is_selected() {
        // The window is load-bearing, not decoration: if a shed rung never expires
        // the bot keeps expecting a rung that stopped arriving and counts ZERO from
        // that sender — the metric freezes while frames still flow.
        //
        // Drives `highest_arriving_rung` directly rather than through
        // `record_packet`: that samples `Instant::now()`, so every packet in a unit
        // test lands on the same instant and nothing can age out.
        let mut stats = InboundStats::default();
        let t0 = Instant::now();
        for rung in 0..3u32 {
            seed_rung(&mut stats, RungKind::Video, ALICE, rung, t0);
        }
        assert_eq!(stats.highest_arriving_rung(RungKind::Video, ALICE, t0), 2);

        // Rungs 1 and 2 shed; only rung 0 keeps arriving.
        let later = t0 + RUNG_WINDOW + Duration::from_millis(1);
        seed_rung(&mut stats, RungKind::Video, ALICE, 0, later);
        assert_eq!(
            stats.highest_arriving_rung(RungKind::Video, ALICE, later),
            0,
            "once the shed rungs age out, rung 0 IS the top arriving rung"
        );

        // Aged-out entries are IGNORED by the probe (above) and swept by `reset`,
        // which is the only eviction site now that freshness is folded into the
        // lookup — see `highest_arriving_rung`.
        assert!(
            stats.rung_state.contains_key(&(ALICE, RungKind::Video, 2)),
            "the probe must not evict; that is reset's job"
        );
    }

    #[test]
    fn reset_sweeps_aged_rungs_so_the_map_cannot_grow_without_bound() {
        // The map is keyed by `session_id`, which is re-minted per transport actor, so
        // every reconnect orphans up to SIMULCAST_MAX_LAYERS entries. `reset` runs every
        // 10s in BOTH pin and observation modes and is the only thing that reclaims
        // them: `highest_arriving_rung` no longer prunes, and in pin mode
        // `decoded_rung_for` returns before ever reaching it.
        let mut stats = InboundStats::default();
        let stale = Instant::now() - (RUNG_WINDOW + Duration::from_millis(1));
        for session in 0..50u64 {
            for rung in 0..3u32 {
                seed_rung(&mut stats, RungKind::Video, session, rung, stale);
            }
        }
        let fresh_session = 999u64;
        seed_rung(
            &mut stats,
            RungKind::Video,
            fresh_session,
            1,
            Instant::now(),
        );
        assert_eq!(stats.rung_state.len(), 151);

        stats.reset();

        assert_eq!(
            stats.rung_state.len(),
            1,
            "every aged entry must be reclaimed, not merely ignored"
        );
        assert!(
            stats
                .rung_state
                .contains_key(&(fresh_session, RungKind::Video, 1)),
            "a rung still inside the window must SURVIVE reset — dropping it would \
             repeat the ramp and inflate the next health sample (#2206)"
        );
    }

    #[test]
    fn the_sweep_counts_the_marks_it_abandons() {
        // Without this the expiry figure only sees silences no 10s tick swept, so a 0
        // would not mean "the window never bit".
        let mut stats = InboundStats::default();
        let stale = Instant::now() - (RUNG_WINDOW + Duration::from_millis(1));
        seed_rung(&mut stats, RungKind::Audio, ALICE, 0, stale);
        seed_rung(&mut stats, RungKind::Video, ALICE, 0, stale);
        seed_rung(&mut stats, RungKind::Video, BOB, 1, stale);
        seed_rung(&mut stats, RungKind::Video, BOB, 0, Instant::now());

        stats.reset();

        assert_eq!(stats.audio_rung_expiries, 1);
        assert_eq!(
            stats.video_rung_expiries, 2,
            "both aged video marks must be counted; the fresh one must not"
        );
    }

    #[test]
    fn reset_gives_back_the_capacity_a_rung_storm_left_pinned() {
        let mut stats = InboundStats::default();
        let stale = Instant::now() - (RUNG_WINDOW + Duration::from_millis(1));
        for session in 0..4_000u64 {
            seed_rung(&mut stats, RungKind::Video, session, 0, stale);
        }
        for session in 0..10u64 {
            seed_rung(&mut stats, RungKind::Audio, session, 0, Instant::now());
        }
        assert!(stats.rung_state.capacity() >= 4_000);

        stats.reset();

        assert_eq!(stats.rung_state.len(), 10);
        assert!(
            stats.rung_state.capacity() <= 256,
            "capacity {} is still the storm high-water mark",
            stats.rung_state.capacity()
        );
    }

    #[test]
    fn evict_stale_gives_back_the_capacity_a_sender_storm_left_pinned() {
        let mut stats = InboundStats::default();
        let stale = Instant::now() - Duration::from_secs(120);
        for i in 0..4_000u32 {
            let name: Arc<str> = Arc::from(format!("peer-{i}").as_str());
            stats
                .sender_names
                .insert(name.as_bytes().to_vec(), Arc::clone(&name));
            stats.last_seen.insert(name, stale);
        }
        let live: Arc<str> = Arc::from("bob");
        stats
            .sender_names
            .insert(live.as_bytes().to_vec(), Arc::clone(&live));
        stats.last_seen.insert(live, Instant::now());
        assert!(stats.last_seen.capacity() >= 4_000);

        stats.evict_stale(Duration::from_secs(60));

        assert_eq!(stats.last_seen.len(), 1);
        assert!(
            stats.last_seen.capacity() <= 256 && stats.sender_names.capacity() <= 256,
            "capacities {}/{} are still the storm high-water mark",
            stats.last_seen.capacity(),
            stats.sender_names.capacity()
        );
    }

    #[test]
    fn rung_window_matches_the_client_availability_window() {
        // Both this crate and `videocall-client` derive from
        // `videocall_aq::constants::LAYER_AVAILABILITY_WINDOW_MS`, so they cannot silently
        // disagree. What remains hand-written, and is what this asserts, is the
        // ms -> Duration conversion.
        assert_eq!(
            RUNG_WINDOW,
            Duration::from_millis(LAYER_AVAILABILITY_WINDOW_MS),
            "the window must be derived from the shared constant, not redefined"
        );
    }

    /// Build an `InboundStats` whose layer-preference sender pins `layer` for `kind`.
    fn stats_pinned(layer: u32, kind: PinMediaKind) -> InboundStats {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let mut stats = InboundStats::default();
        stats.set_layer_preference_sender(LayerPreferenceSender::new(
            "bot".to_string(),
            Some(layer),
            kind,
            tx,
        ));
        stats
    }

    /// Feed `frames` frames of a full 3-rung ladder from ALICE.
    fn feed_full_ladder(stats: &mut InboundStats, frames: u64) {
        for seq in 1..=frames {
            for rung in 0..3u32 {
                stats.record_packet(
                    "bot",
                    &make_video_packet_on_rung("alice", ALICE, seq, 1000.0 + seq as f64, rung),
                );
            }
        }
    }

    #[test]
    fn a_video_pin_steers_the_video_count() {
        // The pin branch's reason for existing: the relay never drops rung 0, so
        // deriving from arrivals would fall back to base and count frames the browser
        // would SKIP. With a VIDEO pin on rung 0, only rung 0 counts.
        let mut stats = stats_pinned(0, PinMediaKind::Video);
        let frames = 5u64;
        feed_full_ladder(&mut stats, frames);
        assert_eq!(
            stats.video_packets, frames,
            "a video pin must select exactly the pinned rung"
        );
    }

    #[test]
    fn a_non_video_pin_does_not_steer_the_video_count() {
        // The relay keys its drop on (source, KIND). An audio- or screen-scoped pin
        // leaves the VIDEO ladder fully forwarded, so applying its rung to video would
        // under-report by the ladder ratio — and with a rung that never arrives (rung 2
        // against a single-rung publisher) it would report ZERO forever on a healthy
        // stream. Video must fall back to observing arrivals.
        for kind in [PinMediaKind::Audio, PinMediaKind::Screen] {
            let mut stats = stats_pinned(0, kind);
            let frames = 5u64;
            feed_full_ladder(&mut stats, frames);
            // Observation path: the top arriving rung (2) is counted once per frame,
            // plus the ladder-depth ramp on the first frame.
            assert_eq!(
                stats.video_packets,
                frames + 2,
                "{kind:?} pin must NOT pin the video count to rung 0 (would read {frames})"
            );
        }
    }

    #[test]
    fn a_pin_for_a_rung_that_never_arrives_reports_zero_not_base() {
        // The other half of the pin contract: a shed pinned rung must read as the
        // starvation it is, not fall back to base. This is why `decoded_rung_for`
        // returns the REQUESTED rung in pin mode.
        let mut stats = stats_pinned(2, PinMediaKind::Video);
        for seq in 1..=5u64 {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", ALICE, seq, 1000.0 + seq as f64, 0),
            );
        }
        assert_eq!(
            stats.video_packets, 0,
            "a pinned rung that never arrives is starvation, not base-rung health"
        );
        assert!(
            stats.video_bytes > 0,
            "bytes stay unfiltered so liveness still sees the stream"
        );
    }

    #[test]
    fn an_off_ladder_rung_is_neither_counted_nor_recorded() {
        // `simulcast_layer_id` is forgeable and unbounded. One packet claiming
        // u32::MAX must not become a map entry (cardinality inflation on a per-packet
        // scan) and must not win `max()` — which would zero every honest frame from
        // that sender for the whole availability window.
        let mut stats = InboundStats::default();
        stats.record_packet(
            "bot",
            &make_video_packet_on_rung("alice", ALICE, 0, 1000.0, u32::MAX),
        );
        assert_eq!(stats.video_packets, 0, "an off-ladder rung is undecodable");
        assert!(
            stats.rung_state.is_empty(),
            "an off-ladder rung must not create a map entry"
        );

        // The BOUNDARY, not just a far-out value: `rung == SIMULCAST_MAX_LAYERS` is the
        // first invalid id, so this is what distinguishes `<` from `<=` in the guard.
        let mut boundary = InboundStats::default();
        boundary.record_packet(
            "bot",
            &make_video_packet_on_rung("alice", ALICE, 0, 1000.0, SIMULCAST_MAX_LAYERS as u32),
        );
        assert_eq!(
            boundary.video_packets, 0,
            "rung == SIMULCAST_MAX_LAYERS is off-ladder (valid ids are 0..SIMULCAST_MAX_LAYERS)"
        );
        assert!(
            boundary.rung_state.is_empty(),
            "the boundary rung must not create a map entry either"
        );

        // And honest frames that follow are unaffected.
        for seq in 1..=4u64 {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", ALICE, seq, 1000.0 + seq as f64, 0),
            );
        }
        assert_eq!(
            stats.video_packets, 4,
            "the forged rung must not poison max()"
        );
    }

    #[test]
    fn a_single_rung_sender_is_fully_counted() {
        // The pinned / single-stream case (`--pin-layer`, or a publisher with one
        // layer): every packet is the top arriving rung, so nothing is filtered.
        let mut stats = InboundStats::default();
        for seq in 0..5 {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("carol", 33, seq, 1000.0 + seq as f64, 0),
            );
        }
        assert_eq!(stats.video_packets, 5);
    }

    #[test]
    fn test_gap_detection() {
        let mut stats = InboundStats::default();

        // Send sequential audio packets: 0, 1, 2
        for seq in 0..3 {
            let data = make_media_packet("alice", MediaType::AUDIO, seq, 1000.0 + seq as f64);
            stats.record_packet("bot", &data);
        }
        assert_eq!(
            stats.audio_seq_gaps, 0,
            "sequential packets should have no gaps"
        );

        // Skip seq 3, send seq 5 (gap of 1 missing packet: seq 4 is also missing = gap of 2)
        let data = make_media_packet("alice", MediaType::AUDIO, 5, 1005.0);
        stats.record_packet("bot", &data);
        assert_eq!(
            stats.audio_seq_gaps, 2,
            "skipping 3->5 should detect 2 gap packets"
        );

        // Same test for video
        for seq in 0..3 {
            let data = make_media_packet("alice", MediaType::VIDEO, seq, 1000.0 + seq as f64);
            stats.record_packet("bot", &data);
        }
        assert_eq!(stats.video_seq_gaps, 0);

        let data = make_media_packet("alice", MediaType::VIDEO, 5, 1005.0);
        stats.record_packet("bot", &data);
        assert_eq!(stats.video_seq_gaps, 2);
    }

    #[test]
    fn test_reorder_no_false_gap() {
        let mut stats = InboundStats::default();

        // Send video seq 0, 1, 3, 2 — the 3 before 2 is a reorder
        let data = make_media_packet("bob", MediaType::VIDEO, 0, 1000.0);
        stats.record_packet("bot", &data);

        let data = make_media_packet("bob", MediaType::VIDEO, 1, 1001.0);
        stats.record_packet("bot", &data);

        // seq 3 arrives before seq 2 — gap of 1 (seq 2 missing at this point)
        let data = make_media_packet("bob", MediaType::VIDEO, 3, 1003.0);
        stats.record_packet("bot", &data);
        assert_eq!(stats.video_seq_gaps, 1, "3 after 1 = 1 gap");

        // seq 2 arrives late (reorder) — should NOT create a false gap
        let data = make_media_packet("bob", MediaType::VIDEO, 2, 1002.0);
        stats.record_packet("bot", &data);
        assert_eq!(
            stats.video_seq_gaps, 1,
            "late seq 2 should not increase gap count"
        );

        // max_seen should still be 3
        assert_eq!(
            stats
                .rung_state
                .get(&(0, RungKind::Video, 0))
                .unwrap()
                .max_seq,
            Some(3)
        );
    }

    /// Drive a series of arrival timestamps through the production accumulator.
    fn accumulate(arrivals: &[f64]) -> InterArrival {
        let mut ia = InterArrival::default();
        for &at in arrivals {
            ia.record(at);
        }
        ia
    }

    #[test]
    fn test_interarrival_stddev() {
        // Perfectly uniform arrivals should have zero stddev
        let stddev = accumulate(&[0.0, 10.0, 20.0, 30.0, 40.0]).stddev_ms();
        assert!(
            stddev < 0.001,
            "uniform arrivals should have ~0 stddev, got {}",
            stddev
        );

        // Alternating 10ms and 20ms inter-arrival deltas
        // deltas = [10, 20, 10, 20], mean = 15, variance = 25, stddev = 5
        let stddev = accumulate(&[0.0, 10.0, 30.0, 40.0, 60.0]).stddev_ms();
        assert!(
            (stddev - 5.0).abs() < 0.01,
            "expected stddev ~5.0, got {}",
            stddev
        );

        // A backward wall-clock step (NTP) must fold to a positive delta:
        // deltas = [10, 5, 10], mean = 25/3, stddev = sqrt(50/9)
        let stddev = accumulate(&[0.0, 10.0, 5.0, 15.0]).stddev_ms();
        assert!(
            (stddev - 2.357_022_6).abs() < 0.001,
            "expected stddev ~2.3570, got {}",
            stddev
        );

        // Too few arrivals should return 0
        assert_eq!(accumulate(&[]).stddev_ms(), 0.0);
        assert_eq!(accumulate(&[42.0]).stddev_ms(), 0.0);
    }

    #[test]
    fn the_streaming_stddev_agrees_with_two_passes_over_the_retained_series() {
        // The formula the accumulator replaced — a different algorithm, so
        // agreement is evidence rather than tautology.
        fn two_pass(arrivals: &[f64]) -> f64 {
            if arrivals.len() < 2 {
                return 0.0;
            }
            let deltas: Vec<f64> = arrivals.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
            let mean = deltas.iter().sum::<f64>() / deltas.len() as f64;
            let var = deltas.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / deltas.len() as f64;
            var.sqrt()
        }

        // On the wall-clock epoch `record_packet` feeds in (~1.75e12 ms).
        let mut arrivals = Vec::new();
        let mut t = 1_755_000_000_000.0_f64;
        for i in 0..5_000u64 {
            t += 20.0 + ((i * 7919) % 23) as f64 * 0.5;
            arrivals.push(t);
        }

        let ia = accumulate(&arrivals);
        let streaming = ia.stddev_ms();
        let reference = two_pass(&arrivals);
        assert!(
            (streaming - reference).abs() < 1e-9,
            "streaming {} vs two-pass {}",
            streaming,
            reference
        );
        assert_eq!(ia.arrivals(), arrivals.len() as u64);
    }

    #[test]
    fn test_evict_stale() {
        let mut stats = InboundStats::default();

        // Record packets from two senders
        let data = make_media_packet("alice", MediaType::AUDIO, 0, 1000.0);
        stats.record_packet("bot", &data);
        let data = make_media_packet("bob", MediaType::VIDEO, 0, 1000.0);
        stats.record_packet("bot", &data);

        assert!(stats.last_seen.contains_key("alice"));
        assert!(stats.last_seen.contains_key("bob"));

        // Backdate alice's last_seen to simulate being stale
        *stats.last_seen.get_mut("alice").unwrap() = Instant::now() - Duration::from_secs(120);

        // Evict with 60s threshold — alice should be removed, bob should remain
        stats.evict_stale(Duration::from_secs(60));

        assert!(
            !stats.last_seen.contains_key("alice"),
            "alice should be evicted"
        );
        assert!(!stats.diag_counters.contains_key("alice"));

        assert!(stats.last_seen.contains_key("bob"), "bob should remain");
    }

    #[test]
    fn test_health_counters_drain() {
        let mut stats = InboundStats::default();

        // Record some packets
        for seq in 0..5 {
            let data = make_media_packet("alice", MediaType::AUDIO, seq, 1000.0 + seq as f64);
            stats.record_packet("bot", &data);
        }
        for seq in 0..3 {
            let data = make_media_packet("alice", MediaType::VIDEO, seq, 1000.0 + seq as f64);
            stats.record_packet("bot", &data);
        }

        assert_eq!(stats.health_total_packets, 8);

        // Drain
        let (counters, total) = (
            stats.drain_diagnostics_counters(),
            stats.take_health_total(),
        );
        assert_eq!(total, 8);
        let alice = counters.get("alice").expect("alice should have counters");
        assert_eq!((alice.audio_bytes, alice.video_bytes), (500, 300));

        // After drain, counters should be reset
        assert_eq!(stats.health_total_packets, 0);
        assert!(stats.diag_counters.is_empty());

        // A second drain should return empty
        let (counters2, total2) = (
            stats.drain_diagnostics_counters(),
            stats.take_health_total(),
        );
        assert_eq!(total2, 0);
        assert!(counters2.is_empty());
    }

    fn make_audio_packet_on_rung(
        sender: &str,
        session_id: u64,
        seq: u64,
        timestamp: f64,
        rung: u32,
    ) -> Vec<u8> {
        let mut wrapper = PacketWrapper::parse_from_bytes(&make_media_packet(
            sender,
            MediaType::AUDIO,
            seq,
            timestamp,
        ))
        .unwrap();
        wrapper.simulcast_layer_id = rung;
        wrapper.session_id = session_id;
        wrapper.write_to_bytes().unwrap()
    }

    fn make_keyframe_on_rung(
        sender: &str,
        session_id: u64,
        seq: u64,
        timestamp: f64,
        rung: u32,
    ) -> Vec<u8> {
        let mut wrapper = PacketWrapper::parse_from_bytes(&make_video_packet_on_rung(
            sender, session_id, seq, timestamp, rung,
        ))
        .unwrap();
        let mut media = MediaPacket::parse_from_bytes(&wrapper.data).unwrap();
        media.frame_type = "key".to_string();
        wrapper.data = media.write_to_bytes().unwrap();
        wrapper.write_to_bytes().unwrap()
    }

    #[test]
    fn audio_packets_counts_one_rung_not_the_ladder_sum() {
        let mut stats = InboundStats::default();

        // Warm-up: each rung is the top rung SEEN SO FAR, so frame 1 over-counts.
        for rung in 0..3u32 {
            stats.record_packet(
                "bot",
                &make_audio_packet_on_rung("alice", ALICE, 0, 1000.0, rung),
            );
        }
        let after_warmup = stats.audio_packets;

        let frames = 50u64;
        for seq in 1..=frames {
            for rung in 0..3u32 {
                stats.record_packet(
                    "bot",
                    &make_audio_packet_on_rung("alice", ALICE, seq, 1000.0 + seq as f64, rung),
                );
            }
        }

        assert_eq!(
            stats.audio_packets - after_warmup,
            frames,
            "steady state must count ONE rung's packets, not all three"
        );
        // Bytes and arrivals stay unfiltered.
        let total = (frames + 1) * 3;
        assert_eq!(stats.audio_ia.arrivals(), total);
        assert_eq!(stats.audio_bytes, total * 100);
    }

    #[test]
    fn per_rung_tracking_detects_single_rung_audio_loss() {
        // Equal-rate rungs keep a single per-sender mark in lockstep, so it never fires.
        let mut stats = InboundStats::default();
        let mut lost = 0u64;
        for seq in 0..50u64 {
            for rung in 0..3u32 {
                if rung == 1 && seq % 5 == 2 {
                    lost += 1;
                    continue;
                }
                stats.record_packet(
                    "bot",
                    &make_audio_packet_on_rung("alice", ALICE, seq, 1000.0 + seq as f64, rung),
                );
            }
        }
        assert_eq!(lost, 10, "fixture must actually drop packets");
        assert_eq!(
            stats.audio_seq_gaps, lost,
            "rung-1 loss must be counted, not masked by rungs 0 and 2"
        );
    }

    #[test]
    fn a_clean_multi_rung_video_publisher_reports_no_phantom_loss() {
        // One mark per sender: observing a mid-call publisher low-rung-first mints a gap.
        let mut stats = InboundStats::default();
        stats.record_packet(
            "bot",
            &make_video_packet_on_rung("alice", ALICE, 7 * 60, 1000.0, 0),
        );
        stats.record_packet(
            "bot",
            &make_video_packet_on_rung("alice", ALICE, 30 * 60, 1001.0, 2),
        );
        assert_eq!(
            stats.video_seq_gaps, 0,
            "two independent per-rung sequences are not a gap in either rung"
        );
    }

    #[test]
    fn per_rung_tracking_detects_base_rung_video_loss_after_a_shed() {
        // A shed top rung strands a shared mark above base's own sequence.
        let mut stats = InboundStats::default();
        for seq in 0..900u64 {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", ALICE, seq, 1000.0 + seq as f64, 2),
            );
        }
        assert_eq!(stats.video_seq_gaps, 0, "the top rung itself lost nothing");

        // Tracking is deliberately NOT gated on decode: tracked without being counted.
        let mut lost = 0u64;
        for seq in 0..210u64 {
            if seq % 3 == 1 {
                lost += 1;
                continue;
            }
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", ALICE, seq, 2000.0 + seq as f64, 0),
            );
        }
        assert_eq!(lost, 70, "fixture must actually drop packets");
        assert_eq!(
            stats.video_seq_gaps, lost,
            "base-rung loss must be counted against the BASE rung's own sequence"
        );
    }

    #[test]
    fn video_keyframes_counts_one_rung_not_the_ladder_sum() {
        // A GOP boundary emits one keyframe on EVERY rung.
        let mut stats = InboundStats::default();
        for rung in 0..3u32 {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", ALICE, 0, 1000.0, rung),
            );
        }
        assert_eq!(stats.video_keyframes, 0, "delta frames are not keyframes");

        for rung in 0..3u32 {
            stats.record_packet(
                "bot",
                &make_keyframe_on_rung("alice", ALICE, 1, 1001.0, rung),
            );
        }
        assert_eq!(
            stats.video_keyframes, 1,
            "one keyframe per GOP boundary, not one per rung"
        );

        // KNOWN GAP: any ladder ramp-up (join or post-shed recovery) counts one keyframe
        // per rung, because `highest_arriving_rung` rises step by step as rungs return.
        let mut joining = InboundStats::default();
        for rung in 0..3u32 {
            joining.record_packet(
                "bot",
                &make_keyframe_on_rung("alice", ALICE, 0, 1000.0, rung),
            );
        }
        assert_eq!(
            joining.video_keyframes, 3,
            "the join ramp still over-counts the first GOP by the ladder depth"
        );

        // The same ramp recurs on RECOVERY: age rungs 1-2 out of the window, then let
        // them return. Each becomes the top arriving rung in turn.
        let stale = Instant::now() - (RUNG_WINDOW + Duration::from_millis(1));
        seed_rung(&mut stats, RungKind::Video, ALICE, 1, stale);
        seed_rung(&mut stats, RungKind::Video, ALICE, 2, stale);
        let before_recovery = stats.video_keyframes;
        for rung in 0..3u32 {
            stats.record_packet(
                "bot",
                &make_keyframe_on_rung("alice", ALICE, 2, 1002.0, rung),
            );
        }
        assert_eq!(
            stats.video_keyframes - before_recovery,
            3,
            "post-shed recovery re-counts one keyframe per returning rung"
        );
    }

    #[test]
    fn an_audio_pin_steers_the_audio_count() {
        // Equal rung rates make selection unobservable by rate; the pin is the observable.
        let mut stats = stats_pinned(2, PinMediaKind::Audio);
        for seq in 0..20u64 {
            stats.record_packet(
                "bot",
                &make_audio_packet_on_rung("alice", ALICE, seq, 1000.0 + seq as f64, 0),
            );
        }
        assert_eq!(
            stats.audio_packets, 0,
            "a pinned audio rung that never arrives is starvation, not base-rung health"
        );
        assert!(
            stats.audio_bytes > 0,
            "bytes stay unfiltered so liveness still sees the stream"
        );
    }

    #[test]
    fn a_non_audio_pin_does_not_steer_the_audio_count() {
        // Another kind's pin would report ZERO forever on a single-rung publisher.
        for kind in [PinMediaKind::Video, PinMediaKind::Screen] {
            let mut stats = stats_pinned(2, kind);
            for seq in 0..20u64 {
                stats.record_packet(
                    "bot",
                    &make_audio_packet_on_rung("alice", ALICE, seq, 1000.0 + seq as f64, 0),
                );
            }
            assert_eq!(
                stats.audio_packets, 20,
                "{kind:?} pin must NOT pin the audio count to rung 2"
            );
        }
    }

    #[test]
    fn the_audio_and_video_ladders_do_not_share_availability() {
        // Pooling the kinds would let audio rung 2 zero a single-rung video publisher.
        let mut stats = InboundStats::default();
        for rung in 0..3u32 {
            stats.record_packet(
                "bot",
                &make_audio_packet_on_rung("alice", ALICE, 0, 1000.0, rung),
            );
        }
        for seq in 1..=5u64 {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", ALICE, seq, 1000.0 + seq as f64, 0),
            );
        }
        assert_eq!(
            stats.video_packets, 5,
            "audio's ladder depth must not filter video"
        );
    }

    #[test]
    fn the_audio_rung_guard_admits_the_top_rung_and_rejects_the_one_above() {
        let mut admitted = InboundStats::default();
        admitted.record_packet(
            "bot",
            &make_audio_packet_on_rung(
                "alice",
                ALICE,
                0,
                1000.0,
                AUDIO_SIMULCAST_MAX_LAYERS as u32 - 1,
            ),
        );
        assert_eq!(
            admitted.audio_packets, 1,
            "the top audio rung is on-ladder and must be admitted"
        );

        let mut rejected = InboundStats::default();
        rejected.record_packet(
            "bot",
            &make_audio_packet_on_rung(
                "alice",
                ALICE,
                0,
                1000.0,
                AUDIO_SIMULCAST_MAX_LAYERS as u32,
            ),
        );
        assert_eq!(
            rejected.audio_packets, 0,
            "the first id above the ladder is undecodable"
        );
        assert!(
            rejected.rung_state.is_empty(),
            "an off-ladder rung must not create a map entry"
        );
        assert!(
            rejected.audio_bytes > 0,
            "bytes stay unfiltered even for a rung we cannot decode"
        );
    }

    #[test]
    fn a_shed_audio_rung_expires_so_the_new_top_is_selected() {
        // Without expiry the bot keeps expecting a rung that stopped arriving.
        let mut stats = InboundStats::default();
        let t0 = Instant::now();
        for rung in 0..3u32 {
            seed_rung(&mut stats, RungKind::Audio, ALICE, rung, t0);
        }
        assert_eq!(stats.highest_arriving_rung(RungKind::Audio, ALICE, t0), 2);

        let later = t0 + RUNG_WINDOW + Duration::from_millis(1);
        seed_rung(&mut stats, RungKind::Audio, ALICE, 0, later);
        assert_eq!(
            stats.highest_arriving_rung(RungKind::Audio, ALICE, later),
            0,
            "once the shed rungs age out, rung 0 IS the top arriving rung"
        );
    }

    #[test]
    fn a_publisher_restart_reanchors_instead_of_wedging_the_mark() {
        // A mic/camera toggle off->on, device hot-plug, page-reload rejoin or bot pod
        // restart re-initialises the encoder's sequence to 0 mid-stream.
        let mut stats = InboundStats::default();
        for seq in 0..5000u64 {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", ALICE, seq, 1000.0, 0),
            );
        }
        assert_eq!(stats.video_seq_gaps, 0, "the first stream lost nothing");

        let mut lost = 0u64;
        for seq in 0..300u64 {
            if seq % 3 == 1 {
                lost += 1;
                continue;
            }
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", ALICE, seq, 2000.0, 0),
            );
        }
        assert_eq!(lost, 100, "fixture must actually drop packets");
        assert_eq!(
            stats.video_seq_gaps, lost,
            "loss after a restart must be visible, not swallowed as reorder"
        );
    }

    #[test]
    fn a_backward_jump_below_the_reanchor_gap_stays_reorder() {
        // Ordinary reordering must NOT re-anchor, or the next arrival mints a gap.
        let mut stats = InboundStats::default();
        for seq in 0..2000u64 {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", ALICE, seq, 1000.0, 0),
            );
        }
        let mark = 1999u64;
        stats.record_packet(
            "bot",
            &make_video_packet_on_rung("alice", ALICE, mark - 4, 1001.0, 0),
        );
        assert_eq!(
            stats
                .rung_state
                .get(&(ALICE, RungKind::Video, 0))
                .unwrap()
                .max_seq,
            Some(mark),
            "a sub-gap backward jump must leave the mark alone"
        );
        assert_eq!(stats.video_seq_gaps, 0);

        // The other side of the SAME guard, so this fails if the arm is deleted as well
        // as if its threshold widens: a jump at exactly the gap DOES re-anchor.
        let reanchored = mark - SEQ_RESET_REANCHOR_GAP;
        stats.record_packet(
            "bot",
            &make_video_packet_on_rung("alice", ALICE, reanchored, 1002.0, 0),
        );
        assert_eq!(
            stats
                .rung_state
                .get(&(ALICE, RungKind::Video, 0))
                .unwrap()
                .max_seq,
            Some(reanchored),
            "a jump at exactly the gap must re-anchor"
        );
    }

    #[test]
    fn two_sessions_of_one_user_do_not_fabricate_loss() {
        // Same user_id on two devices (phone + laptop, or two tabs) is two independent
        // sources with independent per-rung sequences. A user-keyed mark makes every
        // alternation between them a re-anchor plus a gap the size of their separation.
        const PHONE: u64 = 71;
        const LAPTOP: u64 = 72;
        let mut stats = InboundStats::default();
        for seq in 0..2000u64 {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", PHONE, seq, 1000.0, 0),
            );
        }
        for seq in 0..500u64 {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", LAPTOP, seq, 1000.0, 0),
            );
        }
        assert_eq!(stats.video_seq_gaps, 0, "neither device lost anything");

        // Separation is 1500, above SEQ_RESET_REANCHOR_GAP, so a pooled mark re-anchors
        // on every switch and books the difference as loss in both directions.
        for i in 0..10u64 {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", PHONE, 2000 + i, 1000.0, 0),
            );
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", LAPTOP, 500 + i, 1000.0, 0),
            );
        }
        assert_eq!(
            stats.video_seq_gaps, 0,
            "two devices of one user must not fabricate loss for each other"
        );
    }

    #[test]
    fn a_rung_returning_after_the_availability_window_books_no_loss() {
        let mut stats = InboundStats::default();
        for seq in 0..100u64 {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", ALICE, seq, 1000.0, 0),
            );
            stats.record_packet(
                "bot",
                &make_audio_packet_on_rung("alice", ALICE, seq, 1000.0, 0),
            );
        }
        assert_eq!(stats.video_seq_gaps, 0);

        // Both rungs go silent past the window, then the sweep runs.
        let stale = Instant::now() - (RUNG_WINDOW + Duration::from_millis(1));
        for kind in [RungKind::Video, RungKind::Audio] {
            stats
                .rung_state
                .get_mut(&(ALICE, kind, 0))
                .expect("the mark must exist before it can be swept")
                .last_seen = stale;
        }
        stats.reset();
        assert!(
            stats.rung_state.is_empty(),
            "the sweep must drop aged marks"
        );

        // The rungs return far ahead: the publisher kept encoding through the shed.
        stats.record_packet(
            "bot",
            &make_video_packet_on_rung("alice", ALICE, 5_000, 2000.0, 0),
        );
        stats.record_packet(
            "bot",
            &make_audio_packet_on_rung("alice", ALICE, 5_000, 2000.0, 0),
        );
        assert_eq!(
            stats.video_seq_gaps, 0,
            "frames the relay shed are not this receiver's loss"
        );
        assert_eq!(stats.audio_seq_gaps, 0, "same contract for audio");
    }

    #[test]
    fn a_rung_silent_past_the_window_rebaselines_without_waiting_for_the_sweep() {
        // #2424: before this, a silence no 10s tick landed inside kept its mark and
        // booked the publisher's whole un-forwarded run as this receiver's loss.
        let mut stats = InboundStats::default();
        let t0 = Instant::now();
        stats.admit_rung(RungKind::Video, ALICE, 0, Some(10), t0);

        let returned = t0 + RUNG_WINDOW + Duration::from_millis(1);
        stats.admit_rung(RungKind::Video, ALICE, 0, Some(5_010), returned);

        assert_eq!(
            stats.video_seq_gaps, 0,
            "a rung returning past the window must re-baseline, not book 5000 lost packets"
        );
        assert_eq!(
            stats.video_rung_expiries, 1,
            "the un-measurable stretch must be counted, or the 0 above is silent"
        );
        assert_eq!(
            stats
                .rung_state
                .get(&(ALICE, RungKind::Video, 0))
                .and_then(|st| st.max_seq),
            Some(5_010),
            "re-baselined onto the returning sequence, so the NEXT gap is measurable"
        );
    }

    #[test]
    fn an_expired_rung_un_anchors_even_when_the_packet_carries_no_sequence() {
        // Metadata-less media still ends the silence and must un-anchor with it.
        let mut stats = InboundStats::default();
        let t0 = Instant::now();
        stats.admit_rung(RungKind::Video, ALICE, 0, Some(10), t0);

        let returned = t0 + RUNG_WINDOW + Duration::from_millis(1);
        stats.admit_rung(RungKind::Video, ALICE, 0, None, returned);
        stats.admit_rung(
            RungKind::Video,
            ALICE,
            0,
            Some(5_010),
            returned + Duration::from_millis(30),
        );

        assert_eq!(stats.video_seq_gaps, 0);
        assert_eq!(stats.video_rung_expiries, 1);
    }

    #[test]
    fn a_silence_inside_the_window_still_books_its_gap() {
        let mut stats = InboundStats::default();
        let t0 = Instant::now();
        stats.admit_rung(RungKind::Audio, ALICE, 0, Some(10), t0);

        let still_inside = t0 + RUNG_WINDOW - Duration::from_millis(1);
        stats.admit_rung(RungKind::Audio, ALICE, 0, Some(60), still_inside);

        assert_eq!(stats.audio_seq_gaps, 49);
        assert_eq!(stats.audio_rung_expiries, 0);
    }

    #[test]
    fn reported_gaps_count_the_decoded_rung_not_the_ladder_sum() {
        // The browser drops a non-selected rung BEFORE its sequence tracker, so an
        // unfiltered count would read up to the ladder sum against a browser's one rung.
        let mut stats = InboundStats::default();
        for rung in 0..3u32 {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", ALICE, 0, 1000.0, rung),
            );
        }

        let mut lost = 0u64;
        for seq in 1..=60u64 {
            for rung in 0..3u32 {
                if rung == 1 && seq % 5 == 2 {
                    lost += 1;
                    continue;
                }
                stats.record_packet(
                    "bot",
                    &make_video_packet_on_rung("alice", ALICE, seq, 1000.0 + seq as f64, rung),
                );
            }
        }
        assert_eq!(lost, 12, "fixture must actually drop packets");
        assert_eq!(
            stats.video_seq_gaps, lost,
            "the diagnostic counter stays unfiltered: it measures what the link lost"
        );
    }
}
