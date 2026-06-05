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

//! Per-receiver simulcast layer-preference emitter for the synthetic bot
//! (HCL follow-up #1083, section A2).
//!
//! Per-receiver simulcast lets a publisher emit multiple quality layers and has
//! the relay forward only the layer each *receiver* selected. A real browser
//! receiver expresses that choice with a `LAYER_PREFERENCE` control packet
//! carrying, per source `session_id` (+ media kind) it renders, the simulcast
//! layer it wants. The relay records that preference subject-authoritatively
//! (`actix-api/src/actors/chat_server.rs`) and drops the non-matching VIDEO
//! layers from that source toward that receiver.
//!
//! The load-test bot is a **publisher with no receiver chooser** — it has no
//! `videocall-client` dependency and no real per-tile layer-selection UI. But it
//! CAN validate the two halves of the feature it participates in:
//!   - the PUBLISH side (does the bot's own multi-layer ladder show up?), and
//!   - the RELAY FILTER (does a bot that asks for layer 0 receive only layer 0?).
//!
//! This module gives the bot the second half: a "pin to layer N" mode that makes
//! it emit a synthetic `LAYER_PREFERENCE` packet exactly like a browser
//! receiver. It is driven by inbound media: every time the bot observes a packet
//! from a source `session_id` (read from the cleartext `PacketWrapper.session_id`
//! the relay stamps on forwarded media), it adds that source to its known set and
//! re-emits a fresh preference covering every known source at the configured
//! `desired_layer`. When the known-source set is unchanged the packet is
//! suppressed, mirroring `viewport_sender.rs`.
//!
//! Default behaviour is OFF: with no configured `desired_layer` the sender never
//! emits, the relay sees no preference, and it forwards the full ladder
//! (fail-open) — existing bot behaviour is unchanged.
//!
//! Wire format matches the browser receiver: a `LayerPreferencePacket` with one
//! `Entry { session_id, desired_layer, media_kind }` per known source, carried
//! inside a `PacketWrapper { packet_type: LAYER_PREFERENCE, user_id: <self> }`.
//!
//! Layer semantics (from `layer_preference_packet.proto`):
//!   * NO ENTRY for a source = "no preference" → relay forwards every layer.
//!   * `desired_layer = 0` = "BASE LAYER ONLY" → relay drops every upgraded
//!     layer from that source (this is what `--pin-layer 0` requests).
//!   * `desired_layer = N` (N != 0) = "forward only layer N from this source".

use protobuf::Message;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;
use tracing::{info, warn};

use crate::transport::{MediaTypeLabel, OutboundFrame};
use videocall_types::protos::layer_preference_packet::layer_preference_packet::{
    Entry, EntryMediaKind,
};
use videocall_types::protos::layer_preference_packet::LayerPreferencePacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

/// Minimum spacing between two reconnect re-asserts. The bot has no
/// connection-state event (unlike the browser client), so the re-assert is
/// driven off a periodic hook; this bound keeps an unconditional re-send from
/// exceeding roughly one packet per interval even if that hook fires often.
/// Matches `viewport_sender::MIN_RESEND_INTERVAL`.
const MIN_RESEND_INTERVAL: Duration = Duration::from_secs(5);

/// Which media kind a "pin to layer" preference applies to. Maps directly to
/// the proto `EntryMediaKind`; kept as a small local enum so the bot config /
/// CLI never has to name a generated protobuf type. Defaults to VIDEO — the
/// only media kind the relay layer-filters today.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PinMediaKind {
    Video,
    Audio,
    Screen,
}

impl PinMediaKind {
    /// Parse a CLI/config token (`video` | `audio` | `screen`, case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "video" => Some(PinMediaKind::Video),
            "audio" => Some(PinMediaKind::Audio),
            "screen" => Some(PinMediaKind::Screen),
            _ => None,
        }
    }

    fn to_entry_kind(self) -> EntryMediaKind {
        match self {
            PinMediaKind::Video => EntryMediaKind::ENTRY_VIDEO,
            PinMediaKind::Audio => EntryMediaKind::ENTRY_AUDIO,
            PinMediaKind::Screen => EntryMediaKind::ENTRY_SCREEN,
        }
    }
}

/// Tracks the bot's known peers and emits a `LAYER_PREFERENCE` packet whenever
/// the set of known sources changes, pinning every source to a fixed layer.
///
/// Structurally a sibling of [`crate::viewport_sender::ViewportSender`]: same
/// inbound-driven discovery, same change-suppression, same try_send + pending +
/// reconnect re-assert idiom. The semantic difference is the payload — a
/// `LayerPreferencePacket` (per-source desired layer) instead of a
/// `ViewportPacket` (which sources to render at all).
pub struct LayerPreferenceSender {
    /// Our own user id (the receiver), stamped into the LAYER_PREFERENCE wrapper.
    self_user_id: String,
    /// The layer to pin every known source to. `None` disables emission
    /// entirely (default — relay fails open and forwards every layer).
    desired_layer: Option<u32>,
    /// Which media kind the preference constrains. Only meaningful when
    /// `desired_layer` is `Some`.
    media_kind: PinMediaKind,
    /// Every distinct source session_id seen so far, kept sorted ascending so
    /// the emitted entry order is deterministic across runs.
    known_sources: BTreeSet<u64>,
    /// The source set most recently sent to the relay. Used to suppress
    /// duplicate LAYER_PREFERENCE packets when the known set is unchanged.
    last_sent: Vec<u64>,
    /// Source set that should be sent once the outbound channel accepts it.
    /// Set before an emit attempt and cleared only after the packet is queued,
    /// so a transient full channel cannot permanently lose the first preference.
    pending: Option<Vec<u64>>,
    /// Whether we have emitted at least one LAYER_PREFERENCE this connection.
    has_sent: bool,
    /// Channel to send outbound packets.
    packet_tx: Sender<OutboundFrame>,
    /// Counter for total LAYER_PREFERENCE packets sent.
    pub preferences_sent: Arc<AtomicU64>,
    /// When the last reconnect re-assert (via [`Self::resend_on_reconnect`])
    /// went out. Rate-limits the re-assert; `None` until the first re-assert.
    last_resend: Option<Instant>,
}

impl LayerPreferenceSender {
    /// Create a new layer-preference sender. `desired_layer == None` disables
    /// emission (default — preserves legacy fail-open relay behaviour).
    pub fn new(
        self_user_id: String,
        desired_layer: Option<u32>,
        media_kind: PinMediaKind,
        packet_tx: Sender<OutboundFrame>,
    ) -> Self {
        Self {
            self_user_id,
            desired_layer,
            media_kind,
            known_sources: BTreeSet::new(),
            last_sent: Vec::new(),
            pending: None,
            has_sent: false,
            packet_tx,
            preferences_sent: Arc::new(AtomicU64::new(0)),
            last_resend: None,
        }
    }

    /// Returns the shared atomic counter for LAYER_PREFERENCE packets sent.
    pub fn preferences_sent_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.preferences_sent)
    }

    /// Whether this sender will ever emit a LAYER_PREFERENCE (i.e. it was
    /// configured with a `desired_layer`). Used to gate logging at construction.
    pub fn is_enabled(&self) -> bool {
        self.desired_layer.is_some()
    }

    /// Record a source `session_id` observed on an inbound media packet. When a
    /// newly observed source is added, a fresh LAYER_PREFERENCE covering all
    /// known sources is emitted. A `session_id` of 0 is the proto-3 default /
    /// unstamped sentinel and is ignored — only relay-stamped sources
    /// participate in the preference.
    pub fn on_source_seen(&mut self, session_id: u64) {
        // Disabled mode: never emit. Skip the unstamped sentinel.
        if self.desired_layer.is_none() || session_id == 0 {
            return;
        }

        // First time we see this source — adding it changes the entry set.
        if !self.known_sources.insert(session_id) {
            return;
        }

        let sources: Vec<u64> = self.known_sources.iter().copied().collect();

        // Never emit an empty preference (nothing to constrain), and suppress a
        // re-emit when the known set is unchanged.
        if sources.is_empty() || (self.has_sent && sources == self.last_sent) {
            return;
        }

        self.pending = Some(sources);
        self.flush_pending("change");
    }

    /// Re-assert the CURRENT layer preference after a reconnect / re-election.
    ///
    /// The relay drops a receiver's recorded preference on disconnect; a
    /// reconnect leaves it empty → fail-open, so the bot would silently start
    /// receiving the full ladder again. The browser client re-sends its
    /// preference on the `Connected` state edge for the same reason; the bot
    /// mirrors that intent here, driven off the periodic reset hook (it has no
    /// connection-state event of its own).
    ///
    /// No-op when disabled (`desired_layer == None`), when nothing has been
    /// sent and no failed send is pending (first-connect never double-sends), or
    /// when there are no known sources. Rate-limited by [`MIN_RESEND_INTERVAL`].
    pub fn resend_on_reconnect(&mut self) {
        // Disabled mode → nothing to restore.
        if self.desired_layer.is_none() {
            return;
        }

        // Rate-limit: skip if a re-assert went out within the last interval.
        if let Some(last) = self.last_resend {
            if last.elapsed() < MIN_RESEND_INTERVAL {
                return;
            }
        }

        // Retry a previously-failed first send even if nothing has reached the
        // wire yet (the source is already known).
        if self.pending.is_some() {
            if self.flush_pending("retry") {
                self.last_resend = Some(Instant::now());
            }
            return;
        }

        // No preference ever established and no failed pending send → nothing to
        // restore, so a first-connect caller never double-sends.
        if !self.has_sent {
            return;
        }

        let sources: Vec<u64> = self.known_sources.iter().copied().collect();
        if sources.is_empty() {
            return;
        }

        self.pending = Some(sources);
        if self.flush_pending("reconnect") {
            self.last_resend = Some(Instant::now());
        }
    }

    /// Try to queue the current pending preference. The pending value is cleared
    /// only after a successful send, so transient channel backpressure is
    /// retried by the periodic reset/reconnect hook.
    fn flush_pending(&mut self, reason: &str) -> bool {
        let Some(sources) = self.pending.clone() else {
            return false;
        };
        if self.emit_preference(sources, reason) {
            self.pending = None;
            true
        } else {
            false
        }
    }

    /// Build and send a LAYER_PREFERENCE for `sources`, updating `last_sent` /
    /// counters on success. `reason` only labels the log line. Returns `true`
    /// if the packet was queued, `false` on build/channel error.
    fn emit_preference(&mut self, sources: Vec<u64>, reason: &str) -> bool {
        let Some(layer) = self.desired_layer else {
            return false;
        };
        match build_layer_preference(&self.self_user_id, &sources, layer, self.media_kind) {
            Ok(bytes) => {
                let frame = OutboundFrame::new(MediaTypeLabel::Other, bytes);
                if self.packet_tx.try_send(frame).is_err() {
                    warn!(
                        "[{}] Failed to send LAYER_PREFERENCE ({} sources, channel full, will retry on reset/source change)",
                        self.self_user_id,
                        sources.len()
                    );
                    false
                } else {
                    self.last_sent = sources.clone();
                    self.has_sent = true;
                    self.preferences_sent.fetch_add(1, Ordering::Relaxed);
                    info!(
                        "[{}] Sent LAYER_PREFERENCE ({}) pinning {} source(s) to layer {} ({:?}): {:?}",
                        self.self_user_id,
                        reason,
                        sources.len(),
                        layer,
                        self.media_kind,
                        sources
                    );
                    true
                }
            }
            Err(e) => {
                warn!(
                    "[{}] Failed to build LAYER_PREFERENCE packet: {}",
                    self.self_user_id, e
                );
                false
            }
        }
    }
}

/// Build a serialized `LAYER_PREFERENCE` `PacketWrapper` pinning each source in
/// `sources` to `desired_layer` for `media_kind`.
fn build_layer_preference(
    self_user_id: &str,
    sources: &[u64],
    desired_layer: u32,
    media_kind: PinMediaKind,
) -> anyhow::Result<Vec<u8>> {
    let entries = sources
        .iter()
        .map(|&session_id| Entry {
            session_id,
            desired_layer,
            media_kind: media_kind.to_entry_kind().into(),
            ..Default::default()
        })
        .collect();
    let pref = LayerPreferencePacket {
        entries,
        ..Default::default()
    };
    let wrapper = PacketWrapper {
        packet_type: PacketType::LAYER_PREFERENCE.into(),
        user_id: self_user_id.as_bytes().to_vec(),
        data: pref.write_to_bytes()?,
        ..Default::default()
    };
    Ok(wrapper.write_to_bytes()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    /// Parse a queued frame back into (renderer user_id, entries).
    fn parse_sent(bytes: &[u8]) -> (Vec<u8>, Vec<Entry>) {
        let wrapper = PacketWrapper::parse_from_bytes(bytes).expect("parse wrapper");
        assert_eq!(
            wrapper.packet_type.enum_value(),
            Ok(PacketType::LAYER_PREFERENCE)
        );
        let pref = LayerPreferencePacket::parse_from_bytes(&wrapper.data).expect("parse pref");
        (wrapper.user_id, pref.entries)
    }

    #[test]
    fn media_kind_parse_is_case_insensitive() {
        assert_eq!(PinMediaKind::parse("VIDEO"), Some(PinMediaKind::Video));
        assert_eq!(PinMediaKind::parse(" audio "), Some(PinMediaKind::Audio));
        assert_eq!(PinMediaKind::parse("Screen"), Some(PinMediaKind::Screen));
        assert_eq!(PinMediaKind::parse("garbage"), None);
    }

    #[tokio::test]
    async fn disabled_mode_never_sends() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut lps =
            LayerPreferenceSender::new("bot-1".to_string(), None, PinMediaKind::Video, tx);
        assert!(!lps.is_enabled());
        for sid in 1..=5 {
            lps.on_source_seen(sid);
        }
        assert!(
            rx.try_recv().is_err(),
            "disabled mode must not emit LAYER_PREFERENCE"
        );
        assert_eq!(lps.preferences_sent.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn zero_session_id_ignored() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut lps =
            LayerPreferenceSender::new("bot-1".to_string(), Some(0), PinMediaKind::Video, tx);
        lps.on_source_seen(0);
        assert!(rx.try_recv().is_err(), "unstamped sentinel must be ignored");
        assert!(lps.known_sources.is_empty());
    }

    #[tokio::test]
    async fn pin_layer_zero_emits_base_only_entry_per_source() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut lps =
            LayerPreferenceSender::new("bot-1".to_string(), Some(0), PinMediaKind::Video, tx);

        // Observe out of order: 30, 10. Entries should be sorted ascending and
        // every entry should be desired_layer=0 (BASE LAYER ONLY) for VIDEO.
        lps.on_source_seen(30);
        let (uid, entries) = parse_sent(&rx.try_recv().expect("first emit").bytes);
        assert_eq!(uid, b"bot-1");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, 30);
        assert_eq!(entries[0].desired_layer, 0);
        assert_eq!(
            entries[0].media_kind.enum_value(),
            Ok(EntryMediaKind::ENTRY_VIDEO)
        );

        lps.on_source_seen(10);
        let (_, entries) = parse_sent(&rx.try_recv().expect("second emit").bytes);
        let ids: Vec<u64> = entries.iter().map(|e| e.session_id).collect();
        assert_eq!(ids, vec![10, 30], "entries must be sorted ascending");
        assert!(entries.iter().all(|e| e.desired_layer == 0));

        assert_eq!(lps.preferences_sent.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn pin_nonzero_layer_and_audio_kind_round_trip() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut lps =
            LayerPreferenceSender::new("bot-1".to_string(), Some(2), PinMediaKind::Audio, tx);
        lps.on_source_seen(7);
        let (_, entries) = parse_sent(&rx.try_recv().expect("emit").bytes);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].desired_layer, 2);
        assert_eq!(
            entries[0].media_kind.enum_value(),
            Ok(EntryMediaKind::ENTRY_AUDIO)
        );
    }

    #[tokio::test]
    async fn duplicate_source_does_not_re_emit() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut lps =
            LayerPreferenceSender::new("bot-1".to_string(), Some(0), PinMediaKind::Video, tx);
        lps.on_source_seen(5);
        assert!(rx.try_recv().is_ok());
        lps.on_source_seen(5);
        assert!(rx.try_recv().is_err(), "duplicate source must not re-emit");
        assert_eq!(lps.preferences_sent.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn reconnect_resends_current_preference() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut lps =
            LayerPreferenceSender::new("bot-1".to_string(), Some(0), PinMediaKind::Video, tx);

        lps.on_source_seen(10);
        lps.on_source_seen(20);
        let _ = rx.try_recv().expect("emit 1");
        let (_, entries) = parse_sent(&rx.try_recv().expect("emit 2").bytes);
        let ids: Vec<u64> = entries.iter().map(|e| e.session_id).collect();
        assert_eq!(ids, vec![10, 20]);
        assert_eq!(lps.preferences_sent.load(Ordering::Relaxed), 2);

        // Reconnect: known_sources preserved → re-assert the current set.
        lps.resend_on_reconnect();
        let (uid, entries) = parse_sent(&rx.try_recv().expect("reconnect re-send").bytes);
        assert_eq!(uid, b"bot-1");
        let ids: Vec<u64> = entries.iter().map(|e| e.session_id).collect();
        assert_eq!(ids, vec![10, 20]);
        assert_eq!(lps.preferences_sent.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn reconnect_no_resend_when_never_sent() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut lps =
            LayerPreferenceSender::new("bot-1".to_string(), Some(0), PinMediaKind::Video, tx);
        lps.resend_on_reconnect();
        assert!(
            rx.try_recv().is_err(),
            "no preference established yet → reconnect must be a no-op"
        );
        assert_eq!(lps.preferences_sent.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn reconnect_no_resend_in_disabled_mode() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut lps =
            LayerPreferenceSender::new("bot-1".to_string(), None, PinMediaKind::Video, tx);
        for sid in 1..=3 {
            lps.on_source_seen(sid);
        }
        lps.resend_on_reconnect();
        assert!(
            rx.try_recv().is_err(),
            "disabled mode must not emit on reconnect"
        );
        assert_eq!(lps.preferences_sent.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn failed_initial_send_is_retried_on_reconnect_hook() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(1);
        tx.try_send(OutboundFrame::new(MediaTypeLabel::Other, vec![0]))
            .expect("prefill channel");
        let mut lps =
            LayerPreferenceSender::new("bot-1".to_string(), Some(0), PinMediaKind::Video, tx);

        lps.on_source_seen(7);
        assert_eq!(lps.preferences_sent.load(Ordering::Relaxed), 0);
        assert!(!lps.has_sent);
        assert_eq!(lps.pending, Some(vec![7]));

        let _ = rx.try_recv().expect("remove prefilled frame");
        lps.resend_on_reconnect();

        let (uid, entries) = parse_sent(&rx.try_recv().expect("retried preference").bytes);
        assert_eq!(uid, b"bot-1");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, 7);
        assert_eq!(lps.preferences_sent.load(Ordering::Relaxed), 1);
        assert!(lps.pending.is_none());
    }

    #[tokio::test]
    async fn reconnect_resend_is_rate_limited() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(16);
        let mut lps =
            LayerPreferenceSender::new("bot-1".to_string(), Some(0), PinMediaKind::Video, tx);

        lps.on_source_seen(7);
        assert!(rx.try_recv().is_ok());
        assert_eq!(lps.preferences_sent.load(Ordering::Relaxed), 1);

        lps.resend_on_reconnect();
        assert!(rx.try_recv().is_ok(), "first re-assert");
        assert_eq!(lps.preferences_sent.load(Ordering::Relaxed), 2);

        // Immediately again: inside the rate-limit window → suppressed.
        lps.resend_on_reconnect();
        assert!(
            rx.try_recv().is_err(),
            "re-assert within MIN_RESEND_INTERVAL must be suppressed"
        );
        assert_eq!(lps.preferences_sent.load(Ordering::Relaxed), 2);
    }
}
