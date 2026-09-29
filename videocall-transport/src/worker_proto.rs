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

//! Wire shape and pure policy for the #2728 session-Worker port.

use crate::downlink_stream::StreamKey;
use crate::inbound::InboundLane;
use crate::media_kind::kind;

/// Main -> Worker tags. The payload shape follows each name.
pub mod to_worker {
    pub const INIT: u8 = 0;
    pub const CREATE_SEND_STREAM: u8 = 1;
    pub const DRAINED: u8 = 2;
    pub const CLOSE: u8 = 3;
}

/// Worker -> main tags.
pub mod to_main {
    /// `[READY, time_origin_ms, datagramsWritable]` (writable transferred)
    pub const READY: u8 = 0;
    pub const STATUS: u8 = 1;
    /// `[FRAME, lane, worker_stamp_ms, ArrayBuffer]` (buffer transferred)
    pub const FRAME: u8 = 2;
    pub const SEND_STREAM_CREATED: u8 = 3;
    pub const SEND_STREAM_FAILED: u8 = 4;
    /// `[TELEMETRY, ...]`, see [`super::TelemetryPush`].
    pub const TELEMETRY: u8 = 5;
    pub const LOG: u8 = 6;
    pub const CLOSED: u8 = 7;
    pub const BOOTED: u8 = 8;
}

/// `WebTransportStatus` on the wire.
pub mod status_kind {
    pub const OPENED: u8 = 0;
    pub const CLOSED_BEFORE_READY: u8 = 1;
    pub const CLOSED_AFTER_READY: u8 = 2;
    pub const CLOSED_AFTER_READY_WITH_CODE: u8 = 3;
}

pub const LANE_RELIABLE: u8 = 0;
pub const LANE_DATAGRAM: u8 = 1;

pub const fn lane_code(lane: InboundLane) -> u8 {
    match lane {
        InboundLane::Reliable => LANE_RELIABLE,
        InboundLane::Datagram => LANE_DATAGRAM,
    }
}

pub const fn lane_from_code(code: u8) -> InboundLane {
    match code {
        LANE_DATAGRAM => InboundLane::Datagram,
        _ => InboundLane::Reliable,
    }
}

/// Ceiling on bytes posted to main and not yet acknowledged.
pub const MAX_MAIN_INBOX_BYTES: usize = 8 * 1024 * 1024;

/// How often the Worker pushes its drained counters to main.
pub const TELEMETRY_PUSH_MS: u32 = 500;

pub const WORKER_CLOSE_GRACE_MS: u32 = 250;

/// How often teardown re-checks for that ack.
pub const WORKER_CLOSE_POLL_MS: u32 = 25;

const _: () = assert!(WORKER_CLOSE_GRACE_MS % WORKER_CLOSE_POLL_MS == 0);

/// Deadline on ONE `CREATE_SEND_STREAM` round trip (#2733).
/// `SessionHost::wait_ready` is `Ok(())` on the Worker path, so the readiness
/// wait happens INSIDE this request and the deadline must outlast a real
/// handshake.
/// The `Err` on expiry is the point: this is awaited under the shared
/// persistent-stream mutex, so an unbounded wait wedges EVERY media kind.
pub const CREATE_SEND_STREAM_TIMEOUT_MS: u32 = 10_000;

const _: () = assert!(
    CREATE_SEND_STREAM_TIMEOUT_MS > WORKER_CLOSE_GRACE_MS,
    "a teardown must be able to resolve the request before this fires"
);

pub const ACK_EVERY_FRAMES: u32 = 32;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ShedTier {
    CameraVideo,
    Screen,
    Protected,
}

/// Classify one frame by its MEDIA KIND, whatever carrier it arrived on.
pub fn shed_tier(media_kind: Option<u8>) -> ShedTier {
    match media_kind {
        Some(kind::VIDEO) => ShedTier::CameraVideo,
        Some(kind::SCREEN) => ShedTier::Screen,
        _ => ShedTier::Protected,
    }
}

const STAGE1_NUM: usize = 3;
const STAGE1_DEN: usize = 4;

pub fn should_shed(tier: ShedTier, in_flight_bytes: usize, frame_len: usize, cap: usize) -> bool {
    let projected = in_flight_bytes.saturating_add(frame_len);
    match tier {
        ShedTier::Protected => false,
        ShedTier::Screen => projected > cap,
        ShedTier::CameraVideo => projected > lowest_watermark(cap),
    }
}

/// The first watermark any tier can cross.
const fn lowest_watermark(cap: usize) -> usize {
    cap / STAGE1_DEN * STAGE1_NUM
}

/// How long main may be silent before the Worker stops posting CAMERA VIDEO to
/// it. Mirrored from `videocall_codecs::jitter_buffer::MAX_PLAYOUT_AGE_MS`.
pub const STALE_DELIVERY_CEILING_MS: f64 = 1800.0;

/// The same, for SCREEN, which the ladder keeps alive longer.
///
/// Mirrored from `videocall_aq::constants::SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS`:
/// a screen frame dropped after this much silence costs at most one screen GOP,
/// and the publisher's periodic keyframe restores the picture inside that same
/// interval. Screen is also the low-rate contributor and its content is durable
/// — a static slide is still worth showing late — which is why the byte ladder
/// already sheds camera at three quarters of the cap and screen only at the cap.
pub const SCREEN_STALE_DELIVERY_CEILING_MS: f64 = 3000.0;

const _: () = assert!(
    STALE_DELIVERY_CEILING_MS < SCREEN_STALE_DELIVERY_CEILING_MS,
    "camera must shed before screen under silence, as it does under bytes"
);

const _: () = assert!(TELEMETRY_PUSH_MS as f64 * 2.0 < STALE_DELIVERY_CEILING_MS);

/// Which instant counts as main's last ack.
///
/// Main stamps the ack with its own `now`, already converted into the Worker's
/// domain. The Worker uses that rather than its arrival time, so the value means
/// "time since main last SENT" — attributable to main alone — instead of "time
/// since the Worker got round to reading it", which folds in the Worker's own
/// queue latency. It is what makes the two-telemetry-pushes-inside-the-ceiling
/// guarantee hold end to end rather than only on main's side.
///
/// An absent or non-finite stamp falls back to arrival, so a main that predates
/// the field, or a malformed message, degrades to the old behaviour instead of
/// poisoning the clock with a NaN.
pub fn ack_stamp_ms(main_stamp_ms: Option<f64>, arrived_at_ms: f64) -> f64 {
    match main_stamp_ms {
        Some(ms) if ms.is_finite() => ms,
        _ => arrived_at_ms,
    }
}

/// How long main may be silent before this tier is shed. `None` is never.
pub fn silence_ceiling_ms(tier: ShedTier) -> Option<f64> {
    match tier {
        ShedTier::CameraVideo => Some(STALE_DELIVERY_CEILING_MS),
        ShedTier::Screen => Some(SCREEN_STALE_DELIVERY_CEILING_MS),
        ShedTier::Protected => None,
    }
}

/// Whether the shed decision needs this frame's media kind at all.
pub fn needs_kind(in_flight_bytes: usize, frame_len: usize, cap: usize) -> bool {
    in_flight_bytes.saturating_add(frame_len) > lowest_watermark(cap)
}

/// The media kind the ladder keys on for one inbound frame. A v1 header's zero
/// byte means "this header names no kind", so it falls through to the payload.
pub fn frame_media_kind(key: Option<StreamKey>, bytes: &[u8]) -> Option<u8> {
    match key {
        Some(StreamKey::V1 { media_kind, .. }) if media_kind != kind::UNSPECIFIED => {
            Some(media_kind)
        }
        _ => crate::media_kind::peek_media_kind(bytes),
    }
}

/// The whole shed decision for one frame: bytes in flight OR main's silence,
/// then the ladder.
pub fn should_shed_frame(
    key: Option<StreamKey>,
    bytes: &[u8],
    in_flight_bytes: usize,
    cap: usize,
    main_silent_ms: f64,
) -> bool {
    let over_watermark = needs_kind(in_flight_bytes, bytes.len(), cap);
    let maybe_stale = main_silent_ms > STALE_DELIVERY_CEILING_MS;
    if !over_watermark && !maybe_stale {
        return false;
    }
    let tier = shed_tier(frame_media_kind(key, bytes));
    let Some(ceiling) = silence_ceiling_ms(tier) else {
        return false;
    };
    main_silent_ms > ceiling || should_shed(tier, in_flight_bytes, bytes.len(), cap)
}

#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct TelemetryPush {
    pub read_loop_max_gap_ms: f64,
    /// Drained window max of the #2724 class-3 audio stream reader.
    pub audio_lane_max_gap_ms: f64,
    /// Absent until the queue is configured.
    pub incoming_queue_readback: Option<(f64, f64)>,
    /// Absolute totals.
    pub inbound_unistream_reset_count: u64,
    pub send_order_fallback_count: u64,
    pub inbox_shed_count: u64,
}

/// Page-lifetime total of a counter each Worker reports as its own absolute.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourcedTotals {
    /// `(source, absolute, last_seen)`, evicted least-recently-updated first.
    sources: Vec<(u64, u64, u64)>,
    /// `(source, absolute)` for evicted sources, so one that resumes is
    /// reconciled rather than counted twice.
    retired: Vec<(u64, u64)>,
    dropped: u64,
    tick: u64,
}

const MAX_TRACKED_SOURCES: usize = 32;
const MAX_RETIRED_SOURCES: usize = 32;

impl SourcedTotals {
    pub const fn new() -> Self {
        Self {
            sources: Vec::new(),
            retired: Vec::new(),
            dropped: 0,
            tick: 0,
        }
    }

    pub fn apply(&mut self, source: u64, absolute: u64) {
        self.tick = self.tick.saturating_add(1);
        let tick = self.tick;

        if let Some(entry) = self.sources.iter_mut().find(|(id, _, _)| *id == source) {
            entry.1 = entry.1.max(absolute);
            entry.2 = tick;
            return;
        }

        let mut seed = absolute;
        if let Some(at) = self.retired.iter().position(|(id, _)| *id == source) {
            let (_, was) = self.retired.remove(at);
            seed = seed.max(was);
        }

        if self.sources.len() >= MAX_TRACKED_SOURCES {
            self.evict_least_recently_updated();
        }
        self.sources.push((source, seed, tick));
    }

    fn evict_least_recently_updated(&mut self) {
        let Some(at) = self
            .sources
            .iter()
            .enumerate()
            .min_by_key(|(_, (_, _, last_seen))| *last_seen)
            .map(|(at, _)| at)
        else {
            return;
        };
        let (id, value, _) = self.sources.remove(at);
        if self.retired.len() >= MAX_RETIRED_SOURCES {
            let (_, oldest) = self.retired.remove(0);
            self.dropped = self.dropped.saturating_add(oldest);
        }
        self.retired.push((id, value));
    }

    /// Which sources are still tracked. Test-only.
    #[cfg(test)]
    pub(crate) fn tracked_sources(&self) -> Vec<u64> {
        self.sources.iter().map(|(id, _, _)| *id).collect()
    }

    pub fn total(&self) -> u64 {
        let retired = self
            .retired
            .iter()
            .fold(self.dropped, |acc, (_, value)| acc.saturating_add(*value));
        self.sources
            .iter()
            .fold(retired, |acc, (_, value, _)| acc.saturating_add(*value))
    }
}

/// Main-side accumulation of Worker pushes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TelemetryFold {
    pub read_loop_max_gap_ms: f64,
    pub audio_lane_max_gap_ms: f64,
    pub audio_lane_session_max_gap_ms: f64,
    pub incoming_queue_readback: Option<(f64, f64)>,
    pub inbound_unistream_reset_count: SourcedTotals,
    pub send_order_fallback_count: SourcedTotals,
    pub inbox_shed_count: SourcedTotals,
}

impl TelemetryFold {
    pub const fn new() -> Self {
        Self {
            read_loop_max_gap_ms: 0.0,
            audio_lane_max_gap_ms: 0.0,
            audio_lane_session_max_gap_ms: 0.0,
            incoming_queue_readback: None,
            inbound_unistream_reset_count: SourcedTotals::new(),
            send_order_fallback_count: SourcedTotals::new(),
            inbox_shed_count: SourcedTotals::new(),
        }
    }

    /// `source` identifies the Worker that sent it; see [`SourcedTotals`].
    pub fn apply(&mut self, source: u64, push: TelemetryPush) {
        self.read_loop_max_gap_ms = self.read_loop_max_gap_ms.max(push.read_loop_max_gap_ms);
        self.audio_lane_max_gap_ms = self.audio_lane_max_gap_ms.max(push.audio_lane_max_gap_ms);
        self.audio_lane_session_max_gap_ms = self
            .audio_lane_session_max_gap_ms
            .max(push.audio_lane_max_gap_ms);
        if push.incoming_queue_readback.is_some() {
            self.incoming_queue_readback = push.incoming_queue_readback;
        }
        self.inbound_unistream_reset_count
            .apply(source, push.inbound_unistream_reset_count);
        self.send_order_fallback_count
            .apply(source, push.send_order_fallback_count);
        self.inbox_shed_count.apply(source, push.inbox_shed_count);
    }

    /// Drain the reporting window, as the transport statics do.
    pub fn take_read_loop_max_gap_ms(&mut self) -> f64 {
        std::mem::take(&mut self.read_loop_max_gap_ms)
    }

    pub fn take_audio_lane_max_gap_ms(&mut self) -> f64 {
        std::mem::take(&mut self.audio_lane_max_gap_ms)
    }

    pub fn reset_session(&mut self) {
        self.read_loop_max_gap_ms = 0.0;
        self.audio_lane_max_gap_ms = 0.0;
        self.audio_lane_session_max_gap_ms = 0.0;
        self.incoming_queue_readback = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::Message;
    use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
    use videocall_types::protos::packet_wrapper::PacketWrapper;

    const SOURCE_A: u64 = 1;
    const SOURCE_B: u64 = 2;
    const CAP: usize = 1000;

    fn packet_bytes(media_kind: u8) -> Vec<u8> {
        let mut packet = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            user_id: "peer@example.invalid".into(),
            data: vec![9, 9, 9, 9],
            ..Default::default()
        };
        packet.media_kind = protobuf::EnumOrUnknown::from_i32(i32::from(media_kind));
        packet.write_to_bytes().expect("encode")
    }

    #[test]
    fn audio_and_an_unreadable_kind_are_never_shed() {
        assert_eq!(
            shed_tier(Some(kind::AUDIO)),
            ShedTier::Protected,
            "audio loss is the whole reason this epic exists"
        );
        assert_eq!(
            shed_tier(Some(kind::UNSPECIFIED)),
            ShedTier::Protected,
            "a publisher predating the cleartext discriminator fails OPEN"
        );
        assert_eq!(
            shed_tier(None),
            ShedTier::Protected,
            "a frame whose kind could not be read is not shed on a guess"
        );
        assert_eq!(
            shed_tier(Some(99)),
            ShedTier::Protected,
            "and a kind from a future relay is not shed either"
        );
    }

    #[test]
    fn camera_sheds_before_screen() {
        assert_eq!(shed_tier(Some(kind::VIDEO)), ShedTier::CameraVideo);
        assert_eq!(shed_tier(Some(kind::SCREEN)), ShedTier::Screen);
        let cap = 1000;
        assert!(should_shed(ShedTier::CameraVideo, 800, 0, cap));
        assert!(!should_shed(ShedTier::Screen, 800, 0, cap));
        assert!(should_shed(ShedTier::Screen, 1001, 0, cap));
        assert!(!should_shed(ShedTier::Protected, 10_000, 0, cap));
    }

    #[test]
    fn the_frame_about_to_be_posted_counts_against_the_cap() {
        let cap = 1000;
        assert!(
            !should_shed(ShedTier::Screen, 900, 50, cap),
            "900 + 50 is under the cap"
        );
        assert!(
            should_shed(ShedTier::Screen, 900, 200, cap),
            "900 + 200 is over it, and the frame is what pushes it over"
        );
    }

    #[test]
    fn a_window_value_folds_with_max_and_drains_to_zero() {
        let mut fold = TelemetryFold::default();
        fold.apply(
            SOURCE_A,
            TelemetryPush {
                read_loop_max_gap_ms: 40.0,
                audio_lane_max_gap_ms: 22.0,
                ..Default::default()
            },
        );
        fold.apply(
            SOURCE_A,
            TelemetryPush {
                read_loop_max_gap_ms: 12.0,
                audio_lane_max_gap_ms: 3000.0,
                ..Default::default()
            },
        );
        assert_eq!(
            fold.read_loop_max_gap_ms, 40.0,
            "a smaller later push must not shrink the window"
        );
        assert_eq!(fold.take_read_loop_max_gap_ms(), 40.0);
        assert_eq!(
            fold.take_read_loop_max_gap_ms(),
            0.0,
            "a silent Worker must read 0 so the server gauge recovers"
        );
        assert_eq!(fold.take_audio_lane_max_gap_ms(), 3000.0);
        assert_eq!(
            fold.audio_lane_session_max_gap_ms, 3000.0,
            "the session high-water the diagnostics seam reads is NOT drained"
        );
    }

    #[test]
    fn an_absent_queue_readback_never_clears_the_one_already_pushed() {
        let mut fold = TelemetryFold::default();
        fold.apply(
            SOURCE_A,
            TelemetryPush {
                incoming_queue_readback: Some((2048.0, 3000.0)),
                ..Default::default()
            },
        );
        fold.apply(SOURCE_A, TelemetryPush::default());
        assert_eq!(
            fold.incoming_queue_readback,
            Some((2048.0, 3000.0)),
            "the read-back is a one-shot per-browser constant, not a window"
        );
    }

    #[test]
    fn absolute_counters_track_the_workers_total_without_summing_pushes() {
        let mut fold = TelemetryFold::default();
        fold.apply(
            SOURCE_A,
            TelemetryPush {
                inbound_unistream_reset_count: 4,
                ..Default::default()
            },
        );
        fold.apply(
            SOURCE_A,
            TelemetryPush {
                inbound_unistream_reset_count: 6,
                ..Default::default()
            },
        );
        assert_eq!(
            fold.inbound_unistream_reset_count.total(),
            6,
            "each push carries the Worker's absolute total; summing the pushes \
             themselves would report 10"
        );
    }

    #[test]
    fn a_restarted_worker_does_not_walk_the_page_total_backwards() {
        let mut fold = TelemetryFold::default();
        fold.apply(
            SOURCE_A,
            TelemetryPush {
                inbound_unistream_reset_count: 9,
                ..Default::default()
            },
        );
        fold.reset_session();
        fold.apply(
            SOURCE_B,
            TelemetryPush {
                inbound_unistream_reset_count: 2,
                ..Default::default()
            },
        );
        assert_eq!(
            fold.inbound_unistream_reset_count.total(),
            11,
            "the in-page path counted resets for the life of the page; a \
             Worker restart must not make this counter fall"
        );
        assert_eq!(
            fold.audio_lane_session_max_gap_ms, 0.0,
            "the session high-water, unlike the total, IS per session"
        );
    }

    #[test]
    fn two_racing_candidates_are_summed_not_double_counted() {
        let mut fold = TelemetryFold::default();
        let pushes = [
            (SOURCE_A, 10u64),
            (SOURCE_B, 3),
            (SOURCE_A, 12),
            (SOURCE_B, 5),
            (SOURCE_A, 12),
        ];
        for (source, absolute) in pushes {
            fold.apply(
                source,
                TelemetryPush {
                    inbound_unistream_reset_count: absolute,
                    ..Default::default()
                },
            );
        }
        assert_eq!(
            fold.inbound_unistream_reset_count.total(),
            17,
            "12 from one candidate plus 5 from the other; accumulating deltas \
             against a single watermark would read 27"
        );
    }

    #[test]
    fn a_push_that_arrives_out_of_order_never_walks_its_own_source_back() {
        let mut totals = SourcedTotals::new();
        totals.apply(SOURCE_A, 7);
        totals.apply(SOURCE_A, 4);
        assert_eq!(totals.total(), 7);
    }

    #[test]
    fn an_overflow_lane_video_frame_is_shed_like_any_other_camera_video() {
        let overflow = Some(StreamKey::V1 {
            class: 2,
            publisher_session_id: 0,
            media_kind: kind::UNSPECIFIED,
        });
        assert_eq!(
            frame_media_kind(overflow, &packet_bytes(kind::VIDEO)),
            Some(kind::VIDEO),
            "a header that names no kind must fall through to the frame's own \
             cleartext discriminator"
        );
        assert!(
            should_shed_frame(overflow, &packet_bytes(kind::VIDEO), CAP, CAP, 0.0),
            "and overflow-lane camera video must then shed like any other"
        );
    }

    #[test]
    fn a_header_that_names_a_kind_is_taken_at_its_word() {
        let publisher = Some(StreamKey::V1 {
            class: 1,
            publisher_session_id: 7,
            media_kind: kind::SCREEN,
        });
        assert_eq!(
            frame_media_kind(publisher, &packet_bytes(kind::VIDEO)),
            Some(kind::SCREEN),
            "the v1 header is authoritative when it carries a kind, even where \
             the payload disagrees"
        );
    }

    #[test]
    fn a_control_frame_still_fails_open_after_the_fallthrough() {
        let control = Some(StreamKey::V1 {
            class: 0,
            publisher_session_id: 0,
            media_kind: kind::UNSPECIFIED,
        });
        let heartbeat = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            user_id: "a@b.c".into(),
            data: vec![1, 2, 3],
            ..Default::default()
        }
        .write_to_bytes()
        .expect("encode");
        assert_eq!(frame_media_kind(control, &heartbeat), None);
        assert!(
            !should_shed_frame(control, &heartbeat, CAP, CAP, 0.0),
            "control must never be shed, whatever the inbox depth"
        );
    }

    #[test]
    fn a_legacy_key_still_reaches_the_cleartext_discriminator() {
        assert_eq!(
            frame_media_kind(Some(StreamKey::Legacy), &packet_bytes(kind::SCREEN)),
            Some(kind::SCREEN)
        );
        assert_eq!(
            frame_media_kind(None, &packet_bytes(kind::VIDEO)),
            Some(kind::VIDEO)
        );
    }

    #[test]
    fn a_session_below_every_watermark_sheds_nothing() {
        for k in [kind::VIDEO, kind::SCREEN, kind::AUDIO] {
            assert!(
                !should_shed_frame(Some(StreamKey::Legacy), &packet_bytes(k), 0, CAP, 0.0),
                "kind {k} must not shed on an empty inbox"
            );
        }
    }

    #[test]
    fn the_kind_is_resolved_only_once_a_watermark_could_bind() {
        let camera_watermark = CAP / 4 * 3;
        assert!(
            !needs_kind(0, 1, CAP),
            "an empty inbox cannot shed, so it must not resolve a kind"
        );
        assert!(
            !needs_kind(camera_watermark - 1, 1, CAP),
            "exactly at the camera watermark is still not over it"
        );
        assert!(
            needs_kind(camera_watermark, 1, CAP),
            "one byte past the camera watermark is where the kind starts to matter"
        );
        assert!(
            needs_kind(CAP, 1, CAP),
            "and above the screen watermark it certainly does"
        );
    }

    #[test]
    fn video_is_dropped_once_main_has_been_silent_past_the_ceiling() {
        for (k, tier) in [
            (kind::VIDEO, ShedTier::CameraVideo),
            (kind::SCREEN, ShedTier::Screen),
        ] {
            let stale = silence_ceiling_ms(tier).expect("a sheddable tier") + 1.0;
            assert!(
                should_shed_frame(Some(StreamKey::Legacy), &packet_bytes(k), 0, CAP, stale),
                "kind {k} at {stale} ms is already condemned by the receiver's \
                 jitter buffer; posting it only costs main a parse"
            );
        }
    }

    #[test]
    fn audio_is_never_dropped_however_long_main_is_silent() {
        let ancient = STALE_DELIVERY_CEILING_MS * 10.0;
        assert!(!should_shed_frame(
            Some(StreamKey::Legacy),
            &packet_bytes(kind::AUDIO),
            0,
            CAP,
            ancient
        ));
        let control = Some(StreamKey::V1 {
            class: 0,
            publisher_session_id: 0,
            media_kind: kind::UNSPECIFIED,
        });
        assert!(
            !should_shed_frame(control, &[0xff, 0xff], 0, CAP, ancient),
            "an unreadable kind fails OPEN on the age path too"
        );
    }

    #[test]
    fn the_byte_cap_still_binds_while_main_is_answering_promptly() {
        let bytes = packet_bytes(kind::VIDEO);
        assert!(
            should_shed_frame(Some(StreamKey::Legacy), &bytes, CAP, CAP, 0.0),
            "a full inbox must shed camera video even with main answering every \
             ack instantly, or nothing bounds memory on a slow link"
        );
        assert!(
            should_shed_frame(
                Some(StreamKey::Legacy),
                &packet_bytes(kind::SCREEN),
                CAP,
                CAP,
                0.0
            ),
            "and screen at the higher watermark, likewise"
        );
        assert!(
            !should_shed_frame(Some(StreamKey::Legacy), &bytes, 0, CAP, 0.0),
            "while an empty inbox with a responsive main sheds nothing"
        );
    }

    #[test]
    fn the_protected_tier_is_decided_before_the_staleness_test() {
        let forever = STALE_DELIVERY_CEILING_MS * 1000.0;
        for k in [kind::AUDIO, kind::UNSPECIFIED] {
            assert!(
                !should_shed_frame(Some(StreamKey::Legacy), &packet_bytes(k), CAP, CAP, forever),
                "kind {k} must survive a full inbox AND an indefinitely silent \
                 main; both gates have to fall through to Protected"
            );
        }
        assert!(
            !should_shed_frame(Some(StreamKey::Legacy), &[0xff, 0xff], CAP, CAP, forever),
            "and a frame whose kind cannot be read fails OPEN on both gates"
        );
    }

    #[test]
    fn screen_outlives_camera_under_the_silence_gate() {
        let between = (STALE_DELIVERY_CEILING_MS + SCREEN_STALE_DELIVERY_CEILING_MS) / 2.0;
        assert!(
            should_shed_frame(
                Some(StreamKey::Legacy),
                &packet_bytes(kind::VIDEO),
                0,
                CAP,
                between
            ),
            "camera video sheds at the lower ceiling"
        );
        assert!(
            !should_shed_frame(
                Some(StreamKey::Legacy),
                &packet_bytes(kind::SCREEN),
                0,
                CAP,
                between
            ),
            "while screen is still carried at {between} ms of silence"
        );
        assert!(
            should_shed_frame(
                Some(StreamKey::Legacy),
                &packet_bytes(kind::SCREEN),
                0,
                CAP,
                SCREEN_STALE_DELIVERY_CEILING_MS + 1.0
            ),
            "and sheds once past its own, higher ceiling"
        );
    }

    #[test]
    fn the_ack_is_dated_when_main_sent_it_not_when_the_worker_read_it() {
        let main_sent_at = 1_000.0;
        let worker_read_it_at = 1_450.0;
        assert_eq!(
            ack_stamp_ms(Some(main_sent_at), worker_read_it_at),
            main_sent_at,
            "450 ms of Worker queue latency must not read as main having been \
             alive more recently than it was"
        );
    }

    #[test]
    fn a_missing_or_broken_ack_stamp_falls_back_to_arrival() {
        let arrived = 7.5;
        assert_eq!(ack_stamp_ms(None, arrived), arrived);
        assert_eq!(ack_stamp_ms(Some(f64::NAN), arrived), arrived);
        assert_eq!(ack_stamp_ms(Some(f64::INFINITY), arrived), arrived);
    }

    #[test]
    fn every_sheddable_tier_has_a_ceiling_and_protected_has_none() {
        assert_eq!(
            silence_ceiling_ms(ShedTier::CameraVideo),
            Some(STALE_DELIVERY_CEILING_MS)
        );
        assert_eq!(
            silence_ceiling_ms(ShedTier::Screen),
            Some(SCREEN_STALE_DELIVERY_CEILING_MS)
        );
        assert_eq!(
            silence_ceiling_ms(ShedTier::Protected),
            None,
            "a ceiling on Protected is how audio would start shedding"
        );
    }

    #[test]
    fn the_early_out_uses_the_lowest_ceiling_of_any_tier() {
        assert!(
            STALE_DELIVERY_CEILING_MS
                <= silence_ceiling_ms(ShedTier::Screen).expect("screen has a ceiling"),
            "if screen ever became the lower of the two, the early-out would \
             return false for screen frames past their own ceiling"
        );
    }

    #[test]
    fn the_telemetry_cadence_keeps_a_silent_room_from_looking_stalled() {
        assert!(
            f64::from(TELEMETRY_PUSH_MS) * 2.0 < STALE_DELIVERY_CEILING_MS,
            "two telemetry pushes must fit inside the lowest silence ceiling, \
             or a room with nobody speaking would shed its own video"
        );
    }

    #[test]
    fn the_silence_ceiling_binds_at_its_stated_value() {
        let bytes = packet_bytes(kind::VIDEO);
        assert!(
            !should_shed_frame(
                Some(StreamKey::Legacy),
                &bytes,
                0,
                CAP,
                STALE_DELIVERY_CEILING_MS
            ),
            "exactly at the ceiling is not past it"
        );
        assert!(should_shed_frame(
            Some(StreamKey::Legacy),
            &bytes,
            0,
            CAP,
            STALE_DELIVERY_CEILING_MS + 0.001
        ));
    }

    #[test]
    fn retiring_an_old_source_keeps_its_final_count() {
        let mut totals = SourcedTotals::new();
        for source in 0..(MAX_TRACKED_SOURCES as u64 + 4) {
            totals.apply(source, 2);
        }
        assert_eq!(
            totals.total(),
            (MAX_TRACKED_SOURCES as u64 + 4) * 2,
            "a long call's reconnects must not silently drop their counts when \
             the tracked-source list rolls over"
        );
    }

    #[test]
    fn a_source_that_resumes_after_eviction_is_not_counted_twice() {
        let incumbent = 1_u64;
        let mut totals = SourcedTotals::new();
        totals.apply(incumbent, 100);
        for candidate in 2..=(MAX_TRACKED_SOURCES as u64 + 1) {
            totals.apply(candidate, 0);
        }
        totals.apply(incumbent, 150);
        assert_eq!(
            totals.total(),
            150,
            "a source that pushes again after eviction must carry its retired \
             value forward, not add a second copy of it to the page total"
        );
    }

    #[test]
    fn eviction_never_retires_a_source_that_is_still_pushing() {
        let incumbent = 1_u64;
        let candidates = 39_u64;
        let mut totals = SourcedTotals::new();
        totals.apply(incumbent, 100);
        for candidate in 2..=(candidates + 1) {
            totals.apply(candidate, 5);
            assert!(
                totals.tracked_sources().contains(&incumbent),
                "candidate {candidate} evicted an incumbent that is still \
                 reporting; eviction must take the least-recently-updated source"
            );
            totals.apply(incumbent, 100);
        }
        assert_eq!(
            totals.total(),
            100 + candidates * 5,
            "and the page total stays exact"
        );
    }

    #[test]
    fn a_worker_that_loses_a_push_is_caught_up_by_the_next_one() {
        let mut fold = TelemetryFold::default();
        fold.apply(
            SOURCE_A,
            TelemetryPush {
                inbox_shed_count: 3,
                ..Default::default()
            },
        );
        fold.apply(
            SOURCE_A,
            TelemetryPush {
                inbox_shed_count: 30,
                ..Default::default()
            },
        );
        assert_eq!(
            fold.inbox_shed_count.total(),
            30,
            "absolutes are self-correcting: a dropped push costs no counts"
        );
    }

    #[test]
    fn an_unknown_lane_code_decodes_as_reliable() {
        assert_eq!(lane_from_code(LANE_DATAGRAM), InboundLane::Datagram);
        assert_eq!(lane_from_code(LANE_RELIABLE), InboundLane::Reliable);
        assert_eq!(lane_from_code(200), InboundLane::Reliable);
        assert_eq!(lane_code(InboundLane::Datagram), LANE_DATAGRAM);
        assert_eq!(lane_code(InboundLane::Reliable), LANE_RELIABLE);
    }
}
