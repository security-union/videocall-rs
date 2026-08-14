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
//! Parses every inbound `PacketWrapper` → `MediaPacket`, tracks per-sender sequence
//! numbers, measures inter-arrival variability, and computes A/V sync drift.
//! Reports a summary line at `INFO` level every 10 seconds.

use crate::keyframe_requester::KeyframeRequester;
use crate::layer_preference_sender::{LayerPreferenceSender, PinMediaKind};
use crate::rtt_probe::RttProbeState;
use crate::viewport_sender::ViewportSender;
use protobuf::Message;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, info};
use videocall_aq::constants::{LAYER_AVAILABILITY_WINDOW_MS, SIMULCAST_MAX_LAYERS};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

#[cfg(feature = "metrics")]
use crate::metrics_server::BotMetrics;

/// Per-sender counters accumulated between health report drains.
#[derive(Default, Clone)]
pub struct SenderHealthCounters {
    pub audio_packets: u64,
    pub video_packets: u64,
    pub audio_bytes: u64,
    pub video_bytes: u64,
}

/// How long a rung stays "arriving" after its last packet.
///
/// Shares its definition with `videocall-client`'s
/// `LayerAvailability::DEFAULT_WINDOW_MS` so bot and browser cannot drift on which
/// rungs a source is offering (#2206).
const RUNG_WINDOW: Duration = Duration::from_millis(LAYER_AVAILABILITY_WINDOW_MS);

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
    /// Highest audio sequence number seen per sender (for gap detection under reorder).
    max_audio_seq: HashMap<String, u64>,
    /// Highest video sequence number seen per sender (for gap detection under reorder).
    max_video_seq: HashMap<String, u64>,
    audio_seq_gaps: u64,
    video_seq_gaps: u64,
    /// Arrival times for inter-arrival variability calculation.
    video_arrivals: Vec<f64>,
    audio_arrivals: Vec<f64>,
    // A/V sync dropped: browser audio uses Date.now() ms, video uses EncodedVideoChunk µs — cross-unit delta is meaningless. Re-add when browser wire format is unified.
    parse_errors: u64,
    /// Last-seen instant per (source session_id, simulcast rung); a rung is "arriving"
    /// while its last observation is within [`RUNG_WINDOW`]. Lets `video_packets`
    /// count what the bot WOULD decode rather than every rung the relay forwards —
    /// the relay fails open, so an unfiltered count reads the ladder sum (#2206).
    ///
    /// Keyed by SESSION, not user: one user on two devices is two independent
    /// sources the browser selects layers for separately, and a `u64` key also
    /// avoids cloning the sender name on every video packet.
    rung_last_seen: HashMap<(u64, u32), Instant>,
    /// Per-sender counters for health reporting (accumulated between drains).
    health_counters: HashMap<String, SenderHealthCounters>,
    /// Total inbound packets since last health drain (all types).
    health_total_packets: u64,
    /// Snapshot of the most recently drained health-counter window, kept so
    /// secondary consumers (e.g. the diagnostics reporter) can read the same
    /// window the health reporter emitted without double-draining and zeroing
    /// the live counters between producers.
    last_drain_snapshot: HashMap<String, SenderHealthCounters>,
    /// Last time each sender was seen — used to evict stale entries.
    last_seen: HashMap<String, Instant>,
    /// Intern map: raw user_id bytes → owned String to avoid per-packet allocation.
    sender_names: HashMap<Vec<u8>, String>,
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
    /// most `SIMULCAST_MAX_LAYERS` direct lookups — O(1) in publisher count — and
    /// eviction is left to [`Self::reset`].
    fn highest_arriving_rung(&self, session_id: u64, now: Instant) -> u32 {
        (0..SIMULCAST_MAX_LAYERS as u32)
            .rev()
            .find(|rung| {
                self.rung_last_seen
                    .get(&(session_id, *rung))
                    .is_some_and(|seen| now.duration_since(*seen) <= RUNG_WINDOW)
            })
            .unwrap_or(0)
    }

    /// The rung this bot would decode from `session_id`.
    ///
    /// With `--pin-layer N` the receiver's exact-match guard stays on N: the relay
    /// never drops rung 0, so deriving from arrivals would fall back to 0 and count
    /// frames the browser would SKIP. So an explicit pin wins over observation.
    /// Only a VIDEO-scoped pin steers this count. The relay keys its drop on
    /// `(source, kind)`, so an audio- or screen-scoped pin leaves the video ladder
    /// fully forwarded — applying its rung here would under-report video by the
    /// ladder ratio while the pinned rung might never arrive at all.
    fn decoded_rung_for(&self, session_id: u64, now: Instant) -> u32 {
        if let Some(pinned) = self
            .layer_preference_sender
            .as_ref()
            .and_then(|lps| lps.pinned_layer_for(PinMediaKind::Video))
        {
            return pinned;
        }
        self.highest_arriving_rung(session_id, now)
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
        let sender = self.intern_sender(&wrapper.user_id).to_owned();

        // Update last-seen time for stale entry eviction.
        self.last_seen.insert(sender.clone(), Instant::now());

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
                self.audio_packets += 1;
                self.audio_bytes += media.data.len() as u64;
                self.audio_arrivals.push(now_ms);

                // Accumulate health counters for this sender
                let hc = self.health_counters.entry(sender.clone()).or_default();
                hc.audio_packets += 1;
                hc.audio_bytes += media.data.len() as u64;

                if let Some(meta) = media.audio_metadata.as_ref() {
                    let seq = meta.sequence;
                    if let Some(&max_seen) = self.max_audio_seq.get(&sender) {
                        if seq > max_seen + 1 {
                            // Gap: packets between max_seen and seq are missing
                            self.audio_seq_gaps += seq - max_seen - 1;
                        }
                        // Only update if this is a new high-water mark
                        if seq > max_seen {
                            self.max_audio_seq.insert(sender.clone(), seq);
                        }
                        // seq <= max_seen means reorder/duplicate — do not count as gap
                    } else {
                        // First packet from this sender
                        self.max_audio_seq.insert(sender.clone(), seq);
                    }
                }
            }
            Ok(MediaType::VIDEO) => {
                #[cfg(feature = "metrics")]
                self.bump_received("video");

                // #2206: count only the rung this bot would DECODE, mirroring the
                // browser's EXACT-MATCH guard. Bytes and arrivals stay unfiltered —
                // those measure what the link actually delivered, which is the
                // honest figure for a receiver the relay is fanning every rung to.
                // Rung freshness uses the MONOTONIC clock: a backward NTP step makes a
                // wall-clock delta negative, holding stale rungs past the window. `now_ms`
                // stays wall-clock because the arrival series it feeds reports absolute
                // times. Sampled here, not per packet — only VIDEO needs it.
                let now = Instant::now();
                let rung = wrapper.simulcast_layer_id;
                // `simulcast_layer_id` is publisher-controlled cleartext, so an
                // out-of-ladder value must never become a map entry: cycling unique
                // ids would mint one entry per packet. Off-ladder rungs are also
                // undecodable, so they are not counted.
                //
                // DIVERGES from the client's `clamp_observed_layer_id`, which collapses an
                // off-ladder id onto the top rung's availability where the bot discards
                // it, so the fleet cannot see the resulting browser freeze (#2245).
                let on_ladder = (rung as usize) < SIMULCAST_MAX_LAYERS;
                let would_decode = if on_ladder {
                    self.rung_last_seen.insert((session_id, rung), now);
                    rung == self.decoded_rung_for(session_id, now)
                } else {
                    false
                };

                self.video_bytes += media.data.len() as u64;
                self.video_arrivals.push(now_ms);
                if would_decode {
                    self.video_packets += 1;
                }

                // Accumulate health counters for this sender
                let hc = self.health_counters.entry(sender.clone()).or_default();
                hc.video_bytes += media.data.len() as u64;
                if would_decode {
                    hc.video_packets += 1;
                }

                if media.frame_type == "key" {
                    self.video_keyframes += 1;
                }

                if let Some(meta) = media.video_metadata.as_ref() {
                    let seq = meta.sequence;
                    if let Some(&max_seen) = self.max_video_seq.get(&sender) {
                        if seq > max_seen + 1 {
                            // Gap: packets between max_seen and seq are missing
                            self.video_seq_gaps += seq - max_seen - 1;
                        }
                        // Only update if this is a new high-water mark
                        if seq > max_seen {
                            self.max_video_seq.insert(sender.clone(), seq);
                        }
                        // seq <= max_seen means reorder/duplicate — do not count as gap
                    } else {
                        // First packet from this sender
                        self.max_video_seq.insert(sender.clone(), seq);
                    }
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

    /// Inter-arrival time standard deviation (not RFC 3550 jitter).
    /// Measures variability in packet arrival timing as the standard deviation
    /// of consecutive inter-arrival deltas.
    fn interarrival_stddev_ms(arrivals: &[f64]) -> f64 {
        if arrivals.len() < 2 {
            return 0.0;
        }
        let deltas: Vec<f64> = arrivals.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        let mean = deltas.iter().sum::<f64>() / deltas.len() as f64;
        let variance = deltas.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / deltas.len() as f64;
        variance.sqrt()
    }

    pub fn report(&self, user_id: &str) {
        let audio_iastddev = Self::interarrival_stddev_ms(&self.audio_arrivals);
        let video_iastddev = Self::interarrival_stddev_ms(&self.video_arrivals);

        info!(
            "[{}] RX STATS (10s): audio={} pkts ({:.0} KB, ia_stddev={:.1}ms, gaps={}), \
             video={} decoded-rung pkts ({} key, {:.0} KB, ia_stddev={:.1}ms, gaps={} — all rungs), \
             heartbeat={}, errors={}",
            user_id,
            self.audio_packets,
            self.audio_bytes as f64 / 1024.0,
            audio_iastddev,
            self.audio_seq_gaps,
            self.video_packets,
            self.video_keyframes,
            self.video_bytes as f64 / 1024.0,
            video_iastddev,
            self.video_seq_gaps,
            self.heartbeat_packets,
            self.parse_errors,
        );
    }

    pub fn reset(&mut self) {
        // Preserve health counters across diagnostic resets — they are
        // drained independently by the health reporter on a 1s cadence.
        // Also preserve last_seen, max_*_seq, last_*_ts, and sender_names
        // since they track cross-window state. They are evicted by evict_stale().
        let health_counters = std::mem::take(&mut self.health_counters);
        let health_total = self.health_total_packets;
        let last_drain_snapshot = std::mem::take(&mut self.last_drain_snapshot);
        let last_seen = std::mem::take(&mut self.last_seen);
        let max_audio_seq = std::mem::take(&mut self.max_audio_seq);
        let max_video_seq = std::mem::take(&mut self.max_video_seq);
        let sender_names = std::mem::take(&mut self.sender_names);
        // Rolling 4s availability window — dropping it on the 10s diagnostic reset
        // would repeat the ramp and inflate the next health sample (#2206). Stale
        // entries are evicted HERE rather than on the packet path: `reset` runs every
        // 10s in both pin and observation modes, so this is the one sweep that bounds
        // the map against per-reconnect `session_id` churn.
        let mut rung_last_seen = std::mem::take(&mut self.rung_last_seen);
        let sweep_now = Instant::now();
        rung_last_seen.retain(|_, seen| sweep_now.duration_since(*seen) <= RUNG_WINDOW);
        let rtt_probe = self.rtt_probe.take();
        let keyframe_requester = self.keyframe_requester.take();
        let viewport_sender = self.viewport_sender.take();
        let layer_preference_sender = self.layer_preference_sender.take();
        #[cfg(feature = "metrics")]
        let metrics = self.metrics.take();
        *self = Self::default();
        self.health_counters = health_counters;
        self.health_total_packets = health_total;
        self.last_drain_snapshot = last_drain_snapshot;
        self.last_seen = last_seen;
        self.max_audio_seq = max_audio_seq;
        self.max_video_seq = max_video_seq;
        self.sender_names = sender_names;
        self.rung_last_seen = rung_last_seen;
        self.rtt_probe = rtt_probe;
        self.keyframe_requester = keyframe_requester;
        self.viewport_sender = viewport_sender;
        self.layer_preference_sender = layer_preference_sender;
        #[cfg(feature = "metrics")]
        {
            self.metrics = metrics;
        }

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

    /// Remove entries from ALL per-sender maps for senders not seen within `max_age`.
    /// Call this periodically (e.g. from the 10s reporting tick) to bound memory.
    pub fn evict_stale(&mut self, max_age: Duration) {
        let cutoff = Instant::now() - max_age;
        let stale_senders: Vec<String> = self
            .last_seen
            .iter()
            .filter(|(_, &ts)| ts < cutoff)
            .map(|(k, _)| k.clone())
            .collect();

        for sender in &stale_senders {
            self.last_seen.remove(sender);
            self.max_audio_seq.remove(sender);
            self.max_video_seq.remove(sender);
            self.health_counters.remove(sender);
            self.last_drain_snapshot.remove(sender);
        }

        // Also evict from the intern map — find Vec<u8> keys whose String value
        // matches a stale sender.
        if !stale_senders.is_empty() {
            self.sender_names.retain(|_, v| !stale_senders.contains(v));
        }
    }

    /// Drain per-sender health counters accumulated since the last drain.
    /// Returns `(per_sender_counters, total_packets)` and resets both to zero.
    ///
    /// Before clearing, the drained per-sender map is cloned into
    /// `last_drain_snapshot` so secondary consumers (e.g. the diagnostics
    /// reporter) can read the *same* one-second window the health reporter
    /// emitted — a single source of truth for per-sender rate counters.
    pub fn drain_health_counters(&mut self) -> (HashMap<String, SenderHealthCounters>, u64) {
        let counters = std::mem::take(&mut self.health_counters);
        let total = self.health_total_packets;
        self.health_total_packets = 0;
        self.last_drain_snapshot = counters.clone();
        (counters, total)
    }

    /// Non-destructive snapshot of the last drained health-counter window.
    ///
    /// The diagnostics reporter calls this each tick to emit
    /// `DiagnosticsPacket`s over the same ~1-second window the health reporter
    /// already observed. Returns an empty map before the first drain.
    pub fn snapshot_diagnostics_counters(&self) -> HashMap<String, SenderHealthCounters> {
        self.last_drain_snapshot.clone()
    }

    /// Convert raw user_id bytes to a String, reusing previous conversions
    /// to avoid per-packet allocation from `String::from_utf8_lossy`.
    fn intern_sender(&mut self, raw: &[u8]) -> &str {
        if !self.sender_names.contains_key(raw) {
            self.sender_names
                .insert(raw.to_vec(), String::from_utf8_lossy(raw).into_owned());
        }
        &self.sender_names[raw]
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

    const ALICE: u64 = 11;
    const BOB: u64 = 22;

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
        assert_eq!(stats.video_arrivals.len() as u64, total_packets);
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
            stats.rung_last_seen.insert((ALICE, rung), t0);
        }
        assert_eq!(stats.highest_arriving_rung(ALICE, t0), 2);

        // Rungs 1 and 2 shed; only rung 0 keeps arriving.
        let later = t0 + RUNG_WINDOW + Duration::from_millis(1);
        stats.rung_last_seen.insert((ALICE, 0), later);
        assert_eq!(
            stats.highest_arriving_rung(ALICE, later),
            0,
            "once the shed rungs age out, rung 0 IS the top arriving rung"
        );

        // Aged-out entries are IGNORED by the probe (above) and swept by `reset`,
        // which is the only eviction site now that freshness is folded into the
        // lookup — see `highest_arriving_rung`.
        assert!(
            stats.rung_last_seen.contains_key(&(ALICE, 2)),
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
                stats.rung_last_seen.insert((session, rung), stale);
            }
        }
        let fresh_session = 999u64;
        stats
            .rung_last_seen
            .insert((fresh_session, 1), Instant::now());
        assert_eq!(stats.rung_last_seen.len(), 151);

        stats.reset();

        assert_eq!(
            stats.rung_last_seen.len(),
            1,
            "every aged entry must be reclaimed, not merely ignored"
        );
        assert!(
            stats.rung_last_seen.contains_key(&(fresh_session, 1)),
            "a rung still inside the window must SURVIVE reset — dropping it would \
             repeat the ramp and inflate the next health sample (#2206)"
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

    #[test]
    fn the_drained_health_counter_is_rung_filtered_too() {
        // `fps_received` is built from the PER-SENDER health counters, not the
        // diagnostic total — so filtering only `self.video_packets` would leave the
        // actually-reported telemetry reading the ladder sum.
        let mut stats = InboundStats::default();
        for rung in 0..3u32 {
            stats.record_packet(
                "bot",
                &make_video_packet_on_rung("alice", ALICE, 0, 1000.0, rung),
            );
        }
        let (warm, _) = stats.drain_health_counters();
        let warm_count = warm.get("alice").map(|c| c.video_packets).unwrap_or(0);

        let frames = 6u64;
        for seq in 1..=frames {
            for rung in 0..3u32 {
                stats.record_packet(
                    "bot",
                    &make_video_packet_on_rung("alice", ALICE, seq, 1000.0 + seq as f64, rung),
                );
            }
        }
        let (drained, _) = stats.drain_health_counters();
        let c = drained.get("alice").expect("alice must be present");
        assert_eq!(
            c.video_packets, frames,
            "the REPORTED counter must count one rung, not the ladder sum"
        );
        // Bytes stay unfiltered here too, which is what liveness reads.
        assert_eq!(c.video_bytes, frames * 3 * 100);
        // The first frame's three rungs all count: each is the top rung SEEN SO FAR at
        // the moment it lands. That ramp is inherent to observing arrivals and is why
        // `reset` preserves the window rather than restarting it.
        assert_eq!(
            warm_count, 3,
            "the warm-up frame over-counts by the ladder depth"
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
            stats.rung_last_seen.is_empty(),
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
            boundary.rung_last_seen.is_empty(),
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
        assert_eq!(stats.max_video_seq.get("bob"), Some(&3));
    }

    #[test]
    fn test_interarrival_stddev() {
        // Perfectly uniform arrivals should have zero stddev
        let uniform = vec![0.0, 10.0, 20.0, 30.0, 40.0];
        let stddev = InboundStats::interarrival_stddev_ms(&uniform);
        assert!(
            stddev < 0.001,
            "uniform arrivals should have ~0 stddev, got {}",
            stddev
        );

        // Alternating 10ms and 20ms inter-arrival deltas
        // deltas = [10, 20, 10, 20], mean = 15, variance = 25, stddev = 5
        let jittery = vec![0.0, 10.0, 30.0, 40.0, 60.0];
        let stddev = InboundStats::interarrival_stddev_ms(&jittery);
        assert!(
            (stddev - 5.0).abs() < 0.01,
            "expected stddev ~5.0, got {}",
            stddev
        );

        // Too few arrivals should return 0
        assert_eq!(InboundStats::interarrival_stddev_ms(&[]), 0.0);
        assert_eq!(InboundStats::interarrival_stddev_ms(&[42.0]), 0.0);
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
        assert!(stats.max_audio_seq.contains_key("alice"));
        assert!(stats.max_video_seq.contains_key("bob"));

        // Backdate alice's last_seen to simulate being stale
        *stats.last_seen.get_mut("alice").unwrap() = Instant::now() - Duration::from_secs(120);

        // Evict with 60s threshold — alice should be removed, bob should remain
        stats.evict_stale(Duration::from_secs(60));

        assert!(
            !stats.last_seen.contains_key("alice"),
            "alice should be evicted"
        );
        assert!(!stats.max_audio_seq.contains_key("alice"));
        assert!(!stats.health_counters.contains_key("alice"));

        assert!(stats.last_seen.contains_key("bob"), "bob should remain");
        assert!(stats.max_video_seq.contains_key("bob"));
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
        let (counters, total) = stats.drain_health_counters();
        assert_eq!(total, 8);
        let alice = counters.get("alice").expect("alice should have counters");
        assert_eq!(alice.audio_packets, 5);
        assert_eq!(alice.video_packets, 3);

        // After drain, counters should be reset
        assert_eq!(stats.health_total_packets, 0);
        assert!(stats.health_counters.is_empty());

        // A second drain should return empty
        let (counters2, total2) = stats.drain_health_counters();
        assert_eq!(total2, 0);
        assert!(counters2.is_empty());
    }
}
