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

//! WebTransport Chat Session Actor
//!
//! This is a thin transport adapter that delegates all business logic
//! to `SessionLogic`. It handles WebTransport-specific I/O via channels.

use crate::actors::chat_server::ChatServer;
use crate::actors::packet_handler::DATAGRAM_MAX_SIZE;
use crate::actors::priority_drop::channel_full_drop_kind;
use crate::actors::priority_drop::{
    dimension_fill, evaluate as evaluate_priority_drop,
    evaluate_dual as evaluate_priority_drop_dual, outbound_priority, OutboundPriority,
    PriorityDropDecision, SharedQueueByteMeter,
};
use crate::actors::session_logic::{
    DownlinkDropSink, DownlinkReliefSignal, InboundAction, SessionLogic,
};
use crate::actors::shed_escalation::DownlinkShedEscalation;
use crate::constants::{
    wt_mailbox_capacity, wt_outbound_channel_capacity, AudioDownlinkLane, CLIENT_TIMEOUT,
    KICK_CLOSE_FLUSH_DELAY, OUTBOUND_SCREEN_BYTE_BUDGET, OUTBOUND_VIDEO_BYTE_BUDGET,
    WT_DATAGRAM_CHANNEL_CAPACITY,
};
use crate::messages::server::{ActivateConnection, Packet};
use crate::messages::session::{ForceClose, Message};
use crate::metrics::{
    OUTBOUND_CHANNEL_DROPS_TOTAL, RELAY_DOWNLINK_SHED_TOTAL, RELAY_OUTBOUND_QUEUE_DEPTH,
    RELAY_OUTBOUND_QUEUE_DEPTH_BY_SESSION, RELAY_PACKET_DROPS_TOTAL,
};
use crate::server_diagnostics::TrackerSender;
use crate::session_manager::SessionManager;
use actix::{
    fut, Actor, ActorContext, ActorFutureExt, Addr, AsyncContext, Context, ContextFutureSpawner,
    Handler, Message as ActixMessage, Running, WrapFuture,
};
use bytes::Bytes;
use protobuf::Enum as ProtobufEnum;
use protobuf::Message as ProtobufMessage;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::{MediaKind, PacketType};
use videocall_types::protos::packet_wrapper::PacketWrapper;

pub use crate::actors::session_logic::{RoomId, SessionId, UserId};

/// Heartbeat interval for WebTransport sessions.
///
/// `pub(crate)` so the #1637 relay-side QUIC path-stat sampler in
/// `webtransport::handle_webtransport_session` can sample at the SAME cadence as
/// this actor's heartbeat (the cadence the issue specifies), driven by the single
/// source of truth rather than a duplicated literal.
pub(crate) const WT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Keep-alive ping data (WebTransport-specific)
const KEEP_ALIVE_PING: &[u8] = b"ping";

/// Routing decision for an outbound WebTransport packet.
///
/// Produced by [`build_outbound`] and consumed by [`WtChatSession::send_auto`]
/// to pick the correct per-primitive channel. The bridge no longer sees this
/// enum — each variant maps 1:1 to a dedicated bridge writer task drained by
/// its own bounded channel of [`Bytes`].
///
/// The split-channel topology is the central architectural fix for the
/// WT-freeze symptom: when QUIC flow-control credits on the persistent uni
/// stream drain to zero, the unistream writer task blocks on `write_all`.
/// Because datagrams are drained by an independent task on an independent
/// channel, they continue to flow through `send_datagram` even while the
/// unistream writer is parked. See discussion #756 for the full analysis.
#[derive(Debug, Clone)]
pub enum WtOutbound {
    /// Send via a reliable, ordered, length-prefix framed unidirectional QUIC
    /// stream. Used for video, screen, audio (#2724) and oversized control.
    UniStream(Bytes),
    /// Send via QUIC datagram (unreliable, unordered, low latency). Used for
    /// non-media control under `DATAGRAM_MAX_SIZE`, and for audio only on the
    /// legacy arm of [`AudioDownlinkLane`].
    Datagram(Bytes),
}

/// The per-publisher media kinds that get a downlink stream of their own
/// (#2723). AUDIO is absent on purpose: it is receiver-scoped, one stream for
/// every speaker, not one per publisher (#2724; `2724-contract.md` A1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PublisherStreamKind {
    Video,
    Screen,
}

impl PublisherStreamKind {
    /// The wire byte for this kind in the #2723 stream header — the proto
    /// `MediaKind` value, so a renumbered proto cannot silently desync the
    /// header from the packets on the stream.
    pub fn media_kind_code(self) -> u8 {
        let kind = match self {
            PublisherStreamKind::Video => MediaKind::VIDEO,
            PublisherStreamKind::Screen => MediaKind::SCREEN,
        };
        kind.value() as u8
    }
}

/// The wire byte for AUDIO in a #2724 class-3 stream header, read off the proto
/// enum for the same reason [`PublisherStreamKind::media_kind_code`] is.
pub fn audio_media_kind_code() -> u8 {
    MediaKind::AUDIO.value() as u8
}

/// Which downlink QUIC stream one outbound unistream frame belongs on (#2723).
///
/// `Control` is the single receiver-scoped stream: Critical control (#2718), the
/// #2721 probe echoes, and sub-MTU media the relay cannot attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DownlinkStreamKey {
    Control,
    Publisher {
        session_id: u64,
        kind: PublisherStreamKind,
    },
    /// Every audio frame, both E2EE modes: one receiver-scoped stream, NOT one per
    /// publisher (#2724, contract A1).
    Audio,
    /// The SHARED overflow stream: bulk media the relay cannot attribute, and
    /// every caption.
    Shared,
}

impl DownlinkStreamKey {
    /// Derive the key from the ALREADY-parsed outer `PacketWrapper` fields the
    /// fan-out handler extracts — no second protobuf parse on the hot path.
    ///
    /// `MEDIA_KIND_UNSPECIFIED` is the fail-open bucket, and SIZE separates what
    /// lands in it: sub-MTU frames are control-shaped and keep the lifecycle
    /// lane, while anything larger is unattributable media that must NOT sit in
    /// front of Critical control, so it takes the shared overflow stream.
    ///
    /// Audio is recognised from EITHER signal, which fail in opposite directions
    /// (#2724, contract A4). `audio_lane` gates the ONLY arm returning
    /// [`Self::Audio`], so the revert restores the pre-#2724 key. Above
    /// `DATAGRAM_MAX_SIZE` it is not an Opus frame whatever it claims, and
    /// excluding it is what bounds the lane in bytes (A19).
    pub fn for_media(
        is_media: bool,
        is_audio: bool,
        publisher_session_id: u64,
        media_kind: MediaKind,
        len: usize,
        audio_lane: AudioDownlinkLane,
    ) -> Self {
        if !is_media {
            return Self::Control;
        }
        let is_audio_frame = is_audio || media_kind == MediaKind::AUDIO;
        if audio_lane == AudioDownlinkLane::Reliable && is_audio_frame && len <= DATAGRAM_MAX_SIZE {
            return Self::Audio;
        }
        match media_kind {
            MediaKind::VIDEO if publisher_session_id != 0 => Self::Publisher {
                session_id: publisher_session_id,
                kind: PublisherStreamKind::Video,
            },
            MediaKind::SCREEN if publisher_session_id != 0 => Self::Publisher {
                session_id: publisher_session_id,
                kind: PublisherStreamKind::Screen,
            },
            MediaKind::AUDIO => Self::Control,
            _ if len > DATAGRAM_MAX_SIZE => Self::Shared,
            _ => Self::Control,
        }
    }
}

/// One queued outbound WT packet. The priority rides WITH the payload so the
/// drain knows which [`SharedQueueByteMeter`] bucket to credit (#2717); the key
/// rides with it so the drain knows which downlink stream to write it on (#2723).
#[derive(Debug, Clone)]
pub struct WtOutboundFrame {
    pub priority: OutboundPriority,
    pub bytes: Bytes,
    pub key: DownlinkStreamKey,
}

impl WtOutboundFrame {
    /// A receiver-scoped frame: it rides the control stream. Media must use
    /// [`Self::keyed`] so it reaches its publisher's own stream.
    pub fn new(priority: OutboundPriority, bytes: Bytes) -> Self {
        Self::keyed(priority, bytes, DownlinkStreamKey::Control)
    }

    pub fn keyed(priority: OutboundPriority, bytes: Bytes, key: DownlinkStreamKey) -> Self {
        Self {
            priority,
            bytes,
            key,
        }
    }

    pub fn control(bytes: Bytes) -> Self {
        Self::new(OutboundPriority::Control, bytes)
    }

    /// A relay-generated RTT echo (#2721). See [`OutboundPriority::ProbeEcho`].
    pub fn probe_echo(bytes: Bytes) -> Self {
        Self::new(OutboundPriority::ProbeEcho, bytes)
    }
}

/// `kind` for every relay-side failure to echo an RTT probe, whichever surface
/// dropped it — the admission gate, a full channel, or the bridge's wedged-lane
/// shed. One series, because the client sees one outcome: a probe timeout.
/// Never `overflow_critical`: an echo is not a lifecycle packet (#2721).
pub(crate) const ECHO_DROP_KIND: &str = "rtt";

/// `0` disables the byte dimension: audio and control cost slots (#2261).
pub(crate) fn wt_unistream_byte_budget_for(priority: OutboundPriority) -> usize {
    match priority {
        OutboundPriority::Video => OUTBOUND_VIDEO_BYTE_BUDGET,
        OutboundPriority::Screen => OUTBOUND_SCREEN_BYTE_BUDGET,
        OutboundPriority::Audio
        | OutboundPriority::Critical
        | OutboundPriority::Control
        | OutboundPriority::ProbeEcho
        | OutboundPriority::Caption
        | OutboundPriority::CaptionStatus => 0,
    }
}

/// The fullest budgeted media dimension as a `(queued_bytes, budget)` pair, for
/// a priority judged on the media's byte pressure rather than its own (#2721).
fn fullest_media_byte_dimension(queued: &SharedQueueByteMeter) -> (usize, usize) {
    [OutboundPriority::Video, OutboundPriority::Screen]
        .into_iter()
        .map(|p| (queued.queued_for(p), wt_unistream_byte_budget_for(p)))
        .max_by(|a, b| dimension_fill(a.0, a.1).total_cmp(&dimension_fill(b.0, b.1)))
        .expect("the budgeted-media list is non-empty")
}

pub(crate) fn wt_unistream_decision(
    priority: OutboundPriority,
    free_capacity: usize,
    queued: &SharedQueueByteMeter,
) -> PriorityDropDecision {
    let (queued_bytes, budget) = if matches!(
        priority,
        OutboundPriority::ProbeEcho | OutboundPriority::Caption
    ) {
        fullest_media_byte_dimension(queued)
    } else {
        let budget = wt_unistream_byte_budget_for(priority);
        let queued_bytes = if budget == 0 {
            0
        } else {
            queued.queued_for(priority)
        };
        (queued_bytes, budget)
    };
    evaluate_priority_drop_dual(
        priority,
        free_capacity,
        wt_outbound_channel_capacity(),
        queued_bytes,
        budget,
    )
}

pub(crate) enum WtAdmission {
    Enqueued,
    PriorityDropped {
        reason: &'static str,
        free: usize,
        total: usize,
    },
    /// #2726 stage 1. Its own variant because `PriorityDropped` pages (E19).
    EscalationShed,
    /// Real overflow. The priority separates a Critical drop from a media one.
    Full {
        priority: OutboundPriority,
    },
    Closed,
}

/// THE credit site: a frame is charged iff `try_send` accepted it (#2717).
pub(crate) fn enqueue_unistream(
    tx: &mpsc::Sender<WtOutboundFrame>,
    queued: &SharedQueueByteMeter,
    frame: WtOutboundFrame,
) -> Result<(), mpsc::error::TrySendError<WtOutboundFrame>> {
    let priority = frame.priority;
    let len = frame.bytes.len();
    tx.try_send(frame)?;
    queued.on_enqueue(priority, len);
    Ok(())
}

/// Evaluate BOTH of the unistream lane's dimensions, then enqueue through
/// [`enqueue_unistream`]. Tests go through this too, so none can build a lane
/// state the policy would never produce (#2717).
pub(crate) fn wt_unistream_admit(
    tx: &mpsc::Sender<WtOutboundFrame>,
    queued: &SharedQueueByteMeter,
    priority: OutboundPriority,
    bytes: Bytes,
    key: DownlinkStreamKey,
) -> WtAdmission {
    let free = tx.capacity();
    if let PriorityDropDecision::Drop { reason } = wt_unistream_decision(priority, free, queued) {
        return WtAdmission::PriorityDropped {
            reason,
            free,
            total: wt_outbound_channel_capacity(),
        };
    }
    match enqueue_unistream(tx, queued, WtOutboundFrame::keyed(priority, bytes, key)) {
        Ok(()) => WtAdmission::Enqueued,
        Err(mpsc::error::TrySendError::Full(_)) => WtAdmission::Full { priority },
        Err(mpsc::error::TrySendError::Closed(_)) => WtAdmission::Closed,
    }
}

/// Classify a packet, route it to its lane, and offer it there — the whole of
/// [`WtChatSession::send_auto`] except the drop metrics. The pre-check runs
/// against the DESTINATION lane, which is why each lane has its own decision fn.
///
/// Free so a test can drive the real path; a `WtChatSession` needs NATS.
#[allow(clippy::too_many_arguments)]
pub(crate) fn wt_route_and_admit(
    unistream_tx: &mpsc::Sender<WtOutboundFrame>,
    datagram_tx: &mpsc::Sender<WtOutboundFrame>,
    unistream_bytes: &SharedQueueByteMeter,
    data: Vec<u8>,
    is_media: bool,
    is_audio: bool,
    parsed: Option<&PacketWrapper>,
    media_type: Option<MediaType>,
    publisher_session_id: u64,
    audio_lane: AudioDownlinkLane,
    escalation: &DownlinkShedEscalation,
) -> WtAdmission {
    let priority = outbound_priority(parsed, media_type);
    // Cleartext even under E2EE, so a sealed frame still keys to its
    // publisher's lane.
    let media_kind = parsed
        .and_then(|pw| pw.media_kind.enum_value().ok())
        .unwrap_or(MediaKind::MEDIA_KIND_UNSPECIFIED);
    // #2726 stage 1, decayed and never latched.
    if priority == OutboundPriority::Video && escalation.camera_video_is_shed() {
        return WtAdmission::EscalationShed;
    }
    let key = if matches!(
        priority,
        OutboundPriority::Caption | OutboundPriority::CaptionStatus
    ) {
        DownlinkStreamKey::Shared
    } else {
        DownlinkStreamKey::for_media(
            is_media,
            is_audio,
            publisher_session_id,
            media_kind,
            data.len(),
            audio_lane,
        )
    };
    match build_outbound(data, is_media, is_audio, priority, audio_lane) {
        WtOutbound::UniStream(bytes) => {
            wt_unistream_admit(unistream_tx, unistream_bytes, priority, bytes, key)
        }
        WtOutbound::Datagram(bytes) => wt_datagram_admit(datagram_tx, priority, bytes),
    }
}

/// No byte meter — see [`wt_datagram_decision`].
pub(crate) fn wt_datagram_admit(
    tx: &mpsc::Sender<WtOutboundFrame>,
    priority: OutboundPriority,
    bytes: Bytes,
) -> WtAdmission {
    let free = tx.capacity();
    if let PriorityDropDecision::Drop { reason } = wt_datagram_decision(priority, free) {
        return WtAdmission::PriorityDropped {
            reason,
            free,
            total: WT_DATAGRAM_CHANNEL_CAPACITY,
        };
    }
    match tx.try_send(WtOutboundFrame::new(priority, bytes)) {
        Ok(()) => WtAdmission::Enqueued,
        Err(mpsc::error::TrySendError::Full(_)) => WtAdmission::Full { priority },
        Err(mpsc::error::TrySendError::Closed(_)) => WtAdmission::Closed,
    }
}

/// Offer a relay-generated RTT echo on the primitive its probe arrived on.
///
/// UniStream echoes face the full unistream admission — slots AND the media byte
/// dimension — so the lane cannot shed video while still echoing the probe that
/// measures it (#2721). Datagram echoes keep the bare `try_send`: that lane is
/// metered on slots only and has no media shed for an echo to under-report.
pub(crate) fn wt_echo_admit(
    unistream_tx: &mpsc::Sender<WtOutboundFrame>,
    datagram_tx: &mpsc::Sender<WtOutboundFrame>,
    unistream_bytes: &SharedQueueByteMeter,
    source: WtInboundSource,
    bytes: Bytes,
) -> WtAdmission {
    match source {
        WtInboundSource::UniStream => wt_unistream_admit(
            unistream_tx,
            unistream_bytes,
            OutboundPriority::ProbeEcho,
            bytes,
            DownlinkStreamKey::Control,
        ),
        WtInboundSource::Datagram => match datagram_tx.try_send(WtOutboundFrame::probe_echo(bytes))
        {
            Ok(()) => WtAdmission::Enqueued,
            Err(mpsc::error::TrySendError::Full(_)) => WtAdmission::Full {
                priority: OutboundPriority::ProbeEcho,
            },
            Err(mpsc::error::TrySendError::Closed(_)) => WtAdmission::Closed,
        },
    }
}

/// Book one un-echoed RTT probe: the room-tagged reason for per-room triage,
/// and the protocol-wide [`ECHO_DROP_KIND`] for alerting.
pub(crate) fn record_echo_drop(room: &str, reason: &'static str) {
    RELAY_PACKET_DROPS_TOTAL
        .with_label_values(&[room, "webtransport", reason])
        .inc();
    OUTBOUND_CHANNEL_DROPS_TOTAL
        .with_label_values(&["webtransport", ECHO_DROP_KIND])
        .inc();
}

pub(crate) fn wt_echo_route_and_book(
    unistream_tx: &mpsc::Sender<WtOutboundFrame>,
    datagram_tx: &mpsc::Sender<WtOutboundFrame>,
    unistream_bytes: &SharedQueueByteMeter,
    room: &str,
    session_id: SessionId,
    source: WtInboundSource,
    bytes: Bytes,
) -> bool {
    match wt_echo_admit(unistream_tx, datagram_tx, unistream_bytes, source, bytes) {
        WtAdmission::Enqueued => false,
        WtAdmission::Closed => {
            warn!("Outbound channel closed while echoing RTT for session {session_id}");
            true
        }
        WtAdmission::PriorityDropped {
            reason,
            free,
            total,
        } => {
            record_echo_drop(room, reason);
            debug!("Shed RTT echo for session {session_id} ({reason}): free={free}/{total}");
            false
        }
        WtAdmission::Full { .. } => {
            record_echo_drop(room, "channel_full");
            debug!("Outbound channel full, dropping RTT echo for session {session_id}");
            false
        }
        // Unreachable, but exhaustive so a new producer is a compile error.
        WtAdmission::EscalationShed => {
            record_echo_drop(room, ECHO_DROP_KIND);
            false
        }
    }
}

/// The WT datagram lane's bound: SLOTS only. Only control, critical and — on
/// the legacy arm — audio route here, and each has a zero byte budget under
/// #2261 (#2717).
pub(crate) fn wt_datagram_decision(
    priority: OutboundPriority,
    free_capacity: usize,
) -> PriorityDropDecision {
    evaluate_priority_drop(priority, free_capacity, WT_DATAGRAM_CHANNEL_CAPACITY)
}

/// Result of attempting to send an outbound message to the WebTransport channel.
enum WtSendResult {
    /// Message sent successfully.
    Sent,
    /// Channel is full at the time of `try_send`; THIS (newest) message was
    /// dropped — a TAIL drop. A drop-oldest is not implementable on tokio mpsc
    /// from the sender side (see the `Full` arm in `send_auto`, #1638 PART 2);
    /// the congestion signal is instead delivered by the priority pre-drop and
    /// the bridge writer's backpressure-gated shed.
    /// The transport-agnostic drop counters and the legacy media-kind
    /// labels (`audio`/`video`/`screen`/`media`/`control`/`unknown`,
    /// or `overflow_critical` for Critical packets) are bumped at the
    /// call site.
    Dropped,
    /// Channel is closed; connection is dead.
    Dead,
    /// Packet was *preemptively* dropped before `try_send` by the
    /// priority-drop policy because the channel was approaching
    /// saturation. Distinct from `Dropped` so callers can keep both
    /// semantic paths in pattern matches even if today they take the
    /// same action (record the drop through `on_outbound_drop`). The
    /// drop metric is already incremented inside `send_auto` with the
    /// policy-specific label (`priority_drop_video` / `priority_drop_audio`).
    PriorityDropped,
}

/// Source of inbound data
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WtInboundSource {
    UniStream,
    Datagram,
}

/// Inbound message from WebTransport session
#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct WtInbound {
    pub data: Bytes,
    pub source: WtInboundSource,
}

/// Signal to stop the session (sent when I/O tasks end)
#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct StopSession;

/// WebTransport Chat Session Actor
///
/// A thin transport adapter that delegates business logic to `SessionLogic`.
/// Handles WebTransport-specific I/O via channels.
///
/// ### Why two outbound senders?
///
/// As of the Phase 2 WT-freeze fix (discussion #756), the actor holds
/// **two** independent `mpsc::Sender<Bytes>` handles — one feeding the
/// persistent uni-stream writer task and one feeding the datagram writer
/// task. The split mirrors the QUIC primitives:
///
/// * `unistream_tx` absorbs video, screen, audio, oversized control and a
///   unistream probe echo. QUIC flow control surfaces here, so the
///   priority-drop policy applies on BOTH dimensions ([`wt_unistream_decision`]).
/// * `datagram_tx` (capacity = [`WT_DATAGRAM_CHANNEL_CAPACITY`]) carries
///   non-media control under MTU, the echo of a datagram probe, and audio on
///   the legacy arm of [`AudioDownlinkLane`]. Datagrams
///   are independent of stream flow control, so the channel exists only to
///   absorb scheduling jitter.
///
/// Previously a single channel multiplexed both. When QUIC stalled the
/// uni-stream, the writer task parked on `write_all`, and audio datagrams
/// queued behind the stalled video write in the same channel. The split
/// removes that coupling: a stalled stream cannot starve datagrams. Since #2723
/// the reliable side is a lane per key, so audio's lane is likewise not behind
/// a stalled video write.
pub struct WtChatSession {
    /// Shared session logic (business logic)
    logic: SessionLogic,

    /// Heartbeat tracking (transport-specific timing)
    heartbeat: actix::clock::Instant,

    /// Channel to the reliable downlink writer, against whose fill ratio the
    /// priority-drop policy is evaluated.
    unistream_tx: mpsc::Sender<WtOutboundFrame>,

    /// Channel to the datagram writer task, independent of `unistream_tx` so a
    /// stalled uni stream cannot block datagram delivery.
    datagram_tx: mpsc::Sender<WtOutboundFrame>,

    /// Live byte occupancy of `unistream_tx`, shared with its drain (#2717).
    unistream_bytes: Arc<SharedQueueByteMeter>,

    /// Which primitive this receiver's audio takes (#2724), resolved once per
    /// session from the `ds` capability and `WT_AUDIO_DOWNLINK_LANE`.
    audio_lane: AudioDownlinkLane,

    escalation: DownlinkShedEscalation,

    /// Track if ActivateConnection has been sent
    activated: bool,

    /// Why the relay force-closed this session, so the connection closes
    /// with that reason's code.
    forced_close: Arc<OnceLock<ForceClose>>,
}

/// Pure outbound-routing decision used by [`WtChatSession::send_auto`].
///
/// Extracted as a free function so it can be unit-tested in isolation
/// (constructing a real `WtChatSession` requires a populated
/// `SessionLogic`, which requires NATS, addresses, etc.).
///
/// Routing rules (priority order):
/// 0. [`OutboundPriority::Critical`] → reliable unidirectional stream,
///    whatever the size (#2718). `classify_sealed_aware` never returns
///    `Critical` for a `MEDIA` packet, so this moves non-media control only.
///    [`OutboundPriority::Caption`] likewise never takes a datagram.
/// 1. Non-media, fits MTU → datagram (control / heartbeats / RTT).
/// 2. Media + audio + fits MTU → datagram, but ONLY while `audio_lane` is the
///    legacy [`AudioDownlinkLane::Datagram`] (#2724). Under the default the
///    frame takes the reliable stream and [`DownlinkStreamKey::Audio`]'s lane.
/// 3. Everything else (video, screen, oversized audio, oversized
///    control) → reliable unidirectional stream.
fn build_outbound(
    data: Vec<u8>,
    is_media: bool,
    is_audio: bool,
    priority: OutboundPriority,
    audio_lane: AudioDownlinkLane,
) -> WtOutbound {
    if matches!(
        priority,
        OutboundPriority::Critical | OutboundPriority::Caption | OutboundPriority::CaptionStatus
    ) {
        return WtOutbound::UniStream(data.into());
    }
    let fits_datagram = data.len() <= DATAGRAM_MAX_SIZE;
    if is_media {
        if is_audio && fits_datagram && audio_lane == AudioDownlinkLane::Datagram {
            WtOutbound::Datagram(data.into())
        } else {
            WtOutbound::UniStream(data.into())
        }
    } else if fits_datagram {
        WtOutbound::Datagram(data.into())
    } else {
        WtOutbound::UniStream(data.into())
    }
}

/// Book one #2726 stage-1 shed on the same non-alerting series #2718 uses, and
/// deliberately NOT the `PriorityDropped` arm's two, which would PAGE for a
/// bounded remedy working as designed (E19).
pub(crate) fn book_escalation_shed() {
    RELAY_DOWNLINK_SHED_TOTAL
        .with_label_values(&["webtransport"])
        .inc();
}

impl WtChatSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        addr: Addr<ChatServer>,
        room: String,
        user_id: String,
        display_name: String,
        is_guest: bool,
        unistream_tx: mpsc::Sender<WtOutboundFrame>,
        datagram_tx: mpsc::Sender<WtOutboundFrame>,
        unistream_bytes: Arc<SharedQueueByteMeter>,
        nats_client: async_nats::client::Client,
        tracker_sender: TrackerSender,
        session_manager: SessionManager,
        observer: bool,
        instance_id: Option<String>,
        is_host: bool,
        audio_lane: AudioDownlinkLane,
        escalation: DownlinkShedEscalation,
    ) -> Self {
        let logic = SessionLogic::new(
            addr,
            room,
            user_id,
            display_name,
            is_guest,
            nats_client,
            tracker_sender,
            session_manager,
            observer,
            instance_id,
            "webtransport",
            is_host,
        );

        WtChatSession {
            logic,
            heartbeat: actix::clock::Instant::now(),
            unistream_tx,
            datagram_tx,
            unistream_bytes,
            audio_lane,
            escalation,
            activated: false,
            forced_close: Arc::new(OnceLock::new()),
        }
    }

    /// Read BEFORE `start()`, like [`Self::session_id`].
    pub fn forced_close(&self) -> Arc<OnceLock<ForceClose>> {
        Arc::clone(&self.forced_close)
    }

    /// Carry the room token's `iat` claim into `JoinRoom` (#2934).
    pub fn with_token_iat(mut self, token_iat: Option<i64>) -> Self {
        self.logic.token_iat = token_iat;
        self
    }

    pub fn with_meeting_started_at_ms(mut self, meeting_started_at_ms: Option<u64>) -> Self {
        self.logic.meeting_started_at_ms = meeting_started_at_ms;
        self
    }

    /// Carry the client's `?conn_seq=` into `JoinRoom`.
    pub fn with_conn_seq(mut self, conn_seq: Option<u64>) -> Self {
        self.logic.conn_seq = conn_seq;
        self
    }

    #[cfg(test)]
    pub(crate) fn join_room_message_for_test(
        &self,
        closer: actix::Recipient<crate::messages::session::ForceClose>,
    ) -> crate::messages::server::JoinRoom {
        self.logic.create_join_room_message(closer)
    }

    /// The canonical per-session id for this connection (`SessionLogic::id`).
    ///
    /// The SAME `u64` the `session_id` label on `relay_session_drops_total`
    /// carries, so a series labelled with it JOINS that one. Read on the
    /// constructed actor value BEFORE `start()` consumes it.
    pub fn session_id(&self) -> SessionId {
        self.logic.id
    }

    /// Write handle on THIS receiver's #1219 relief epoch, for the #1638 shed in
    /// the bridge writer task (#2718). Read BEFORE `start()`, like
    /// [`Self::session_id`].
    pub fn downlink_relief_signal(&self) -> DownlinkReliefSignal {
        DownlinkReliefSignal::new(Arc::clone(&self.logic.downlink_congested_epoch))
    }

    /// The drop-booking half for the #2723 dispatcher (#2745). Read BEFORE
    /// `start()`, like [`Self::session_id`].
    pub fn downlink_drop_sink(&self) -> DownlinkDropSink {
        DownlinkDropSink::new(
            &self.logic.room,
            self.logic.id,
            "webtransport",
            self.downlink_relief_signal(),
            Arc::clone(&self.logic.congestion_tracker),
            self.logic.downlink_drop_booking.clone(),
        )
    }

    /// Send outbound message via the channel (reliable unidirectional stream).
    /// Returns false if the channel is closed (connection dead).
    ///
    /// `send()` is used for server-originated control packets that are
    /// part of the session lifecycle: `SESSION_ASSIGNED`,
    /// `MEETING_STARTED`, `MEETING_ENDED`. These are *Critical* under
    /// the priority-drop policy — they are never preemptively dropped
    /// and only fail when the channel is genuinely full. When that
    /// happens we record `kind="overflow_critical"` on the protocol-
    /// wide counter so saturation severe enough to drop lifecycle
    /// packets is alertable on its own (separate from the much higher-
    /// volume media drops).
    fn send(&self, data: Vec<u8>) -> bool {
        // Lifecycle control (SESSION_ASSIGNED, MEETING_STARTED, MEETING_ENDED)
        // routes via the reliable uni-stream channel by design — these packets
        // are not idempotent and must arrive in order. They never use the
        // datagram path even though they typically fit MTU.
        match enqueue_unistream(
            &self.unistream_tx,
            &self.unistream_bytes,
            WtOutboundFrame::control(data.into()),
        ) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!(
                    "UniStream outbound channel closed for session {}, connection dead",
                    self.logic.id
                );
                false
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                RELAY_PACKET_DROPS_TOTAL
                    .with_label_values(&[&self.logic.room, "webtransport", "channel_full"])
                    .inc();
                // Lifecycle control packet dropped on real overflow.
                // The priority-drop policy guarantees these are never
                // preempted by media saturation; a drop here means the
                // channel is so full even the highest-priority packets
                // cannot be admitted. Pages on this should be loud.
                OUTBOUND_CHANNEL_DROPS_TOTAL
                    .with_label_values(&["webtransport", "overflow_critical"])
                    .inc();
                error!(
                    "UniStream outbound channel full for session {} on Critical control packet, dropping (overflow_critical)",
                    self.logic.id
                );
                true // Channel still open, just full
            }
        }
    }

    /// Send outbound message, automatically choosing datagram or stream.
    ///
    /// Routing rules (in priority order):
    ///
    /// 1. **Non-media control packets** (heartbeats, RTT probes, diagnostics,
    ///    AES key exchange, …) that fit within `DATAGRAM_MAX_SIZE` use
    ///    unreliable datagrams. They are periodic and expendable, so lower
    ///    overhead matters more than guaranteed delivery.
    /// 2. **Audio media** takes the reliable stream and its own
    ///    [`DownlinkStreamKey::Audio`] lane (#2724), falling back to datagrams
    ///    only on the legacy arm `self.audio_lane` carries.
    /// 3. **Video / screen media** and any other media use the reliable stream.
    ///
    /// The `is_media` and `is_audio` hints are pre-computed by the caller
    /// from an already-parsed `PacketWrapper` / `MediaPacket`, avoiding a
    /// redundant protobuf parse on every outbound packet.
    ///
    /// `parsed` is the already-decoded outer wrapper (`None` when it failed to
    /// decode) and `media_type` the inner `MediaPacket.media_type` when it was
    /// readable. [`outbound_priority`] classifies the shed tier from both, and
    /// [`build_outbound`] reads that tier; `parsed.is_some()` and `media_type`
    /// also refine the drop-counter `kind` into `audio`/`video`/`screen`.
    fn send_auto(
        &self,
        data: Vec<u8>,
        is_media: bool,
        is_audio: bool,
        parsed: Option<&PacketWrapper>,
        media_type: Option<MediaType>,
        publisher_session_id: u64,
    ) -> WtSendResult {
        let admission = wt_route_and_admit(
            &self.unistream_tx,
            &self.datagram_tx,
            &self.unistream_bytes,
            data,
            is_media,
            is_audio,
            parsed,
            media_type,
            publisher_session_id,
            self.audio_lane,
            &self.escalation,
        );

        match admission {
            WtAdmission::PriorityDropped {
                reason,
                free,
                total,
            } => {
                RELAY_PACKET_DROPS_TOTAL
                    .with_label_values(&[&self.logic.room, "webtransport", reason])
                    .inc();
                OUTBOUND_CHANNEL_DROPS_TOTAL
                    .with_label_values(&["webtransport", reason])
                    .inc();
                // Per-session attribution (Tier B #1): name the slow receiver.
                self.logic.record_session_drop(reason);
                trace!(
                    "Priority-drop {reason} on WT session {}: free={free}/{total}",
                    self.logic.id,
                );
                WtSendResult::PriorityDropped
            }
            WtAdmission::EscalationShed => {
                book_escalation_shed();
                WtSendResult::PriorityDropped
            }
            WtAdmission::Enqueued => WtSendResult::Sent,
            WtAdmission::Closed => {
                warn!(
                    "Outbound channel closed for session {}, connection dead",
                    self.logic.id
                );
                WtSendResult::Dead
            }
            WtAdmission::Full { priority } => {
                // #1638 PART 2 — congestion signal, and why this is a TAIL drop
                // (newest dropped), NOT a drop-oldest:
                //
                // A true "drop the oldest queued frame to make room for the new
                // one" is NOT implementable with `tokio::sync::mpsc`. The
                // producer here holds only a `Sender`, and the tokio mpsc Sender
                // exposes no pop-front / evict-oldest operation — only the
                // single owning `Receiver` (the bridge writer task) can dequeue,
                // and it does so strictly in FIFO order. There is no
                // channel-type change that makes sender-side eviction safe
                // without also racing the writer's `recv()`. So on `Full` we
                // drop the NEWEST packet (tail drop) — and this comment must NOT
                // claim otherwise (per the adversarial-self-review rule, a
                // "drops oldest" comment over tail-drop code is a shippable
                // defect).
                //
                // Phase 8b TELEM-8: this drop site previously lacked any
                // counter increment, so a flood of media drops only surfaced
                // in the log line below. Increment both the room-tagged
                // RELAY_PACKET_DROPS_TOTAL (for per-room investigation) and
                // the protocol-wide OUTBOUND_CHANNEL_DROPS_TOTAL (for
                // alerting).
                RELAY_PACKET_DROPS_TOTAL
                    .with_label_values(&[&self.logic.room, "webtransport", "channel_full"])
                    .inc();
                let kind = channel_full_drop_kind(priority, parsed.is_some(), is_media, media_type);
                OUTBOUND_CHANNEL_DROPS_TOTAL
                    .with_label_values(&["webtransport", kind])
                    .inc();
                // Per-session attribution (Tier B #1): name the slow receiver.
                self.logic.record_session_drop(kind);
                error!(
                    "Outbound channel full for session {}, dropping message (kind={kind})",
                    self.logic.id
                );
                WtSendResult::Dropped
            }
        }
    }

    /// Check if either outbound channel is closed.
    ///
    /// The session is considered dead if **either** primitive's writer task
    /// has gone away — the actor cannot meaningfully continue if it can only
    /// deliver half of its outbound traffic. In practice both channels are
    /// dropped together when the bridge tears down on session end, so this
    /// is symmetric.
    fn is_connection_dead(&self) -> bool {
        self.unistream_tx.is_closed() || self.datagram_tx.is_closed()
    }

    /// Start heartbeat check (WebTransport-specific timing).
    ///
    /// Emits the `relay_outbound_queue_depth` gauge as the sum of the two
    /// per-primitive channels. The label scheme is preserved
    /// (`transport=webtransport`) so existing dashboards continue to work:
    /// the gauge now reflects the *total* outbound backlog across both
    /// primitives — the same operational signal it had before the split.
    /// Per-primitive depth can still be derived from the per-channel
    /// capacity constants and the `kind` label on `videocall_outbound_channel_drops_total`.
    fn start_heartbeat(&self, ctx: &mut Context<Self>) {
        ctx.run_interval(WT_HEARTBEAT_INTERVAL, |act, ctx| {
            // Per-primitive depths. Resolved capacity is memoised, so the
            // unistream call is a single pointer read after init; the
            // datagram capacity is a `const`.
            let uni_depth =
                wt_outbound_channel_capacity().saturating_sub(act.unistream_tx.capacity());
            let dgram_depth =
                WT_DATAGRAM_CHANNEL_CAPACITY.saturating_sub(act.datagram_tx.capacity());
            // Sum so the existing gauge label scheme is preserved end-to-end.
            let depth = uni_depth + dgram_depth;
            RELAY_OUTBOUND_QUEUE_DEPTH
                .with_label_values(&[&act.logic.room, "webtransport"])
                .set(depth as f64);
            let session_id = act.logic.id.to_string();
            RELAY_OUTBOUND_QUEUE_DEPTH_BY_SESSION
                .with_label_values(&[&act.logic.room, "webtransport", &session_id, "unistream"])
                .set(uni_depth as f64);
            RELAY_OUTBOUND_QUEUE_DEPTH_BY_SESSION
                .with_label_values(&[&act.logic.room, "webtransport", &session_id, "datagram"])
                .set(dgram_depth as f64);
            crate::metrics::record_outbound_queue_bytes(
                &act.logic.room,
                "webtransport",
                &session_id,
                &act.unistream_bytes.snapshot(),
            );

            // Check if connection is dead (channel closed)
            if act.is_connection_dead() {
                warn!(
                    "WebTransport connection dead (channel closed), stopping session {}",
                    act.logic.id
                );
                ctx.stop();
                return;
            }

            // Check heartbeat timeout
            if actix::clock::Instant::now().duration_since(act.heartbeat) > CLIENT_TIMEOUT {
                warn!(
                    "WebTransport client heartbeat failed, disconnecting session {}",
                    act.logic.id
                );
                ctx.stop();
            }
        });
    }
}

// =============================================================================
// Actor Implementation
// =============================================================================

impl Actor for WtChatSession {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // Relocate the overflow point off the tiny default actor mailbox
        // onto the policy-aware bounded outbound channel (issue #1057).
        //
        // Like the WS path, the actix `Context` mailbox defaults to
        // `DEFAULT_CAPACITY` (16) and sits *in front* of the outbound
        // channels in the relay fan-out path: ChatServer does
        // `recipient.try_send(Message)` (a mailbox enqueue), then
        // `Handler<Message>` routes the bytes into `unistream_tx` /
        // `datagram_tx`. Under a bursty fan-out storm the 16-slot mailbox
        // overflows long before the outbound channels do, and the mailbox
        // is a *dumb* queue: it drops indiscriminately and cannot feed the
        // drop tracker — the same room-wide-freeze failure mode described
        // for WS.
        //
        // The WT actor fronts TWO independent policy-aware outbound
        // channels — `unistream_tx` (cap `wt_outbound_channel_capacity()`,
        // env-tunable) and `datagram_tx` (cap
        // `WT_DATAGRAM_CHANNEL_CAPACITY`, fixed) — but a single shared
        // mailbox. A `Message` only splits into unistream vs datagram
        // *after* it leaves the mailbox (in `Handler<Message>` /
        // `handle_outbound`), so the mailbox holds a MIX of both. To keep
        // the mailbox from being the bottleneck in front of *either*
        // channel — for any traffic mix and any value of the env-tunable
        // unistream cap — size it to the SUM of both channel capacities.
        // (`max()` is insufficient: with the unistream cap tuned up, a
        // datagram-heavy burst could still fill and drop in the mailbox
        // before the datagram channel's priority-drop ever runs — PR #1060
        // review.) The value is derived from the same constants/resolver
        // the channels are built with, so it stays in lock-step with both.
        //
        // Sizing the mailbox at the SUM of both channels (issue #1057, PR
        // #1060) relocates a *steady-state* overflow off the dumb mailbox onto
        // the policy-aware channels, which shed camera VIDEO first (~80%), then
        // SCREEN (~90%, issue 1977), protect AUDIO to ~95%, never preempt
        // Critical lifecycle packets, and record drops via `on_outbound_drop`.
        //
        // A publisher-join fan-out BURST (issue #1144) can still overflow even
        // that sum, because the keyframe/join spike arrives in a tight window
        // before the actor is next scheduled to drain. So we add the same
        // modest `INBOUND_MAILBOX_HEADROOM_FACTOR` (2×) of burst-absorption
        // slack as the WS path: enough to hold a single join wave across one
        // scheduling gap and let it SPILL onto the policy-aware channels (the
        // shedding surface) instead of being dropped indiscriminately at the
        // mailbox. The hand-off in `Handler<Message>` is CPU-bound (it does NOT
        // block on `session.send_datagram` / the unistream write — those drain
        // separately), so the actor consumes this slack quickly. This does NOT
        // create a deep stale-video buffer: the per-channel staleness bound
        // lives on `unistream_tx`/`datagram_tx` (each unchanged at their
        // capacity), which still cap and fail-fast independently of the
        // mailbox. The headroom is applied to the SUM so the mailbox stays
        // sized for both channels under any traffic mix and any env tuning of
        // the unistream cap (the PR #1060 invariant).
        //
        // #1062: the argument is the shared `wt_mailbox_capacity()` binding —
        // the SINGLE source of truth the guard test also exercises (via the
        // pure `resolve_wt_mailbox_capacity` for env-override cases, and via
        // `wt_mailbox_capacity()` itself for the default-env value). Editing the
        // sizing here means editing that one binding, which the test tracks; the
        // prior in-test `wt_mailbox_capacity(env)` helper hand-duplicated this
        // expression and could drift from the call site silently.
        ctx.set_mailbox_capacity(wt_mailbox_capacity());

        // Track connection start
        self.logic.track_connection_start();

        // Start session via SessionManager
        let session_manager = self.logic.session_manager.clone();
        let room = self.logic.room.clone();
        let user_id = self.logic.user_id.clone();
        let session_id = self.logic.id;
        let meeting_started_at_ms = self.logic.meeting_started_at_ms;

        ctx.wait(
            async move {
                session_manager
                    .start_session(&room, &user_id, session_id, meeting_started_at_ms)
                    .await
            }
            .into_actor(self)
            .map(|result, act, ctx| match result {
                Ok(result) => {
                    act.send(act.logic.build_session_assigned());
                    let bytes = act.logic.build_meeting_started(
                        result.start_time_ms,
                        result.start_time_authoritative,
                        &result.creator_id,
                    );
                    act.send(bytes);
                }
                Err(e) => {
                    error!("Failed to start session: {}", e);
                    let bytes = act
                        .logic
                        .build_meeting_ended(&format!("Session rejected: {e}"));
                    act.send(bytes);
                    ctx.stop();
                }
            }),
        );

        // Register with ChatServer
        let addr = ctx.address();
        self.logic
            .addr
            .send(self.logic.create_connect_message(addr.recipient()))
            .into_actor(self)
            .then(|res, _act, ctx| {
                if let Err(err) = res {
                    error!("Failed to connect to ChatServer: {:?}", err);
                    ctx.stop();
                }
                fut::ready(())
            })
            .wait(ctx);

        // Join room
        self.join_room(ctx);

        // Start heartbeat AFTER all initialization is complete to avoid
        // premature timeout if Connect/JoinRoom are slow under load.
        self.start_heartbeat(ctx);
    }

    fn stopping(&mut self, _: &mut Self::Context) -> Running {
        self.logic.on_stopping();
        Running::Stop
    }
}

// =============================================================================
// Message Handlers
// =============================================================================

/// Handle outbound messages from ChatServer.
///
/// Uses `send_auto` to route packets across two QUIC primitives:
///
/// * Datagrams — non-media control packets (heartbeats, RTT, diagnostics,
///   AES key exchange), and sub-MTU audio only on the legacy arm of
///   [`AudioDownlinkLane`].
/// * Reliable unidirectional streams — video/screen media, audio (#2724), and
///   any oversized control that exceeds the datagram MTU.
///
/// The outbound `msg.msg` is a serialized `PacketWrapper`. We parse it
/// once to extract the sender's `session_id` (for congestion tracking),
/// the `packet_type`, and — when MEDIA — the inner `MediaType`, so
/// `send_auto` does not need to re-parse anything.
///
/// Encrypted media payloads cannot be inspected for `MediaType`; in
/// that case `is_audio` falls back to `false` and the packet uses the
/// reliable stream — preserving today's behaviour for end-to-end
/// encrypted streams.
///
/// Note: `msg.session` is the **receiver's** session ID (set by
/// `chat_server::handle_msg`), NOT the sender's. The sender's session
/// ID lives inside the serialized `PacketWrapper.session_id` field.
impl Handler<Message> for WtChatSession {
    type Result = ();

    fn handle(&mut self, msg: Message, ctx: &mut Self::Context) -> Self::Result {
        let bytes = self.logic.handle_outbound(&msg);

        // Parse the PacketWrapper once to extract the sender's session_id,
        // user_id, and packet_type. This avoids a redundant parse in send_auto
        // and ensures congestion tracking targets the correct (sender) session.
        let parsed = PacketWrapper::parse_from_bytes(&msg.msg).ok();
        let sender_session_id = parsed.as_ref().map(|pw| pw.session_id).unwrap_or(0);
        let sender_user_id = parsed
            .as_ref()
            .map(|pw| pw.user_id.clone())
            .unwrap_or_default();
        // Resolve the outer PacketType for the priority-drop classifier.
        // `enum_value().ok()` falls back to `PACKET_TYPE_UNKNOWN` when
        // the wire bytes carry a value not in our enum — Control class
        // under the priority policy, i.e. never preemptively dropped.
        let packet_type = parsed
            .as_ref()
            .and_then(|pw| pw.packet_type.enum_value().ok())
            .unwrap_or(PacketType::PACKET_TYPE_UNKNOWN);
        let is_media = packet_type == PacketType::MEDIA;

        // For MEDIA packets, peek at the inner MediaType. We use the
        // resolved `MediaType` enum twice:
        //   * `is_audio` selects the audio lane, and a datagram only on the
        //     legacy arm (#2724).
        //   * `media_type` (Some/None) refines the drop-counter `kind`
        //     label into `audio`/`video`/`screen` so a storm can be
        //     attributed to a specific media stream. The 2026-05-08
        //     production storm dropped 25,081 packets in 3 minutes and
        //     we had no metric-level way to tell audio from video.
        //   * priority-drop classifier consumes both `packet_type` and
        //     `media_type` to decide whether to preempt the enqueue.
        //
        // Encrypted payloads fail to parse and therefore (a) route via
        // the reliable stream — the safer default — and (b) fall through
        // to the `media` catch-all label, preserving the legacy series.
        let inner_media_type = if is_media {
            parsed
                .as_ref()
                .and_then(|pw| MediaPacket::parse_from_bytes(&pw.data).ok())
                .and_then(|mp| mp.media_type.enum_value().ok())
        } else {
            None
        };
        let is_audio = matches!(inner_media_type, Some(MediaType::AUDIO));
        match self.send_auto(
            bytes,
            is_media,
            is_audio,
            parsed.as_ref(),
            inner_media_type,
            sender_session_id,
        ) {
            WtSendResult::Sent => self.logic.observe_outbound_delivery(&msg),
            WtSendResult::Dead => {
                ctx.stop();
            }
            WtSendResult::Dropped | WtSendResult::PriorityDropped => {
                // Either real channel-full or priority preempt — both
                // are drops from the sender's perspective. Record the
                // drop for the actual sender so metrics and the #979
                // keyframe-relax path still see receiver-local overflow.
                if sender_session_id != 0 {
                    self.logic
                        .on_outbound_drop(sender_session_id, &sender_user_id);
                }
            }
        }
    }
}

/// Handle inbound data from WebTransport session
impl Handler<WtInbound> for WtChatSession {
    type Result = ();

    fn handle(&mut self, msg: WtInbound, ctx: &mut Self::Context) -> Self::Result {
        // Update heartbeat
        self.heartbeat = actix::clock::Instant::now();

        // Handle keep-alive ping (WebTransport-specific)
        if msg.source == WtInboundSource::Datagram && msg.data.as_ref() == KEEP_ALIVE_PING {
            trace!("Received keep-alive ping for session {}", self.logic.id);
            return;
        }

        let action = self.logic.handle_inbound(&msg.data);

        if !self.activated && SessionLogic::should_activate_on_action(&action) {
            self.logic.addr.do_send(ActivateConnection {
                session: self.logic.id,
            });
            self.activated = true;
            info!(
                "Session {} activated on first non-RTT packet",
                self.logic.id
            );
        }

        match action {
            InboundAction::Echo(data) => {
                if wt_echo_route_and_book(
                    &self.unistream_tx,
                    &self.datagram_tx,
                    &self.unistream_bytes,
                    &self.logic.room,
                    self.logic.id,
                    msg.source,
                    Bytes::from(data.as_ref().clone()),
                ) {
                    ctx.stop();
                }
            }
            InboundAction::Forward(data) => {
                ctx.notify(Packet {
                    data,
                    requires_host: false,
                });
            }
            // #2136: same mailbox hop as `Forward`; the flag tells
            // `Handler<Packet>` to build a host-gated `ClientMessage` that
            // `ChatServer` refuses to fan out unless this session is the room's
            // current host.
            InboundAction::ForwardHostOnly(data) => {
                ctx.notify(Packet {
                    data,
                    requires_host: true,
                });
            }
            InboundAction::Processed | InboundAction::KeepAlive => {}
        }
    }
}

/// Handle stop signal
impl Handler<StopSession> for WtChatSession {
    type Result = ();

    fn handle(&mut self, _msg: StopSession, ctx: &mut Self::Context) -> Self::Result {
        info!(
            "Received stop signal for WebTransport session {} in room {}",
            self.logic.id, self.logic.room
        );
        ctx.stop();
    }
}

impl Handler<ForceClose> for WtChatSession {
    type Result = ();

    fn handle(&mut self, msg: ForceClose, ctx: &mut Self::Context) -> Self::Result {
        info!(
            "Closing WebTransport session {} in room {}: {}",
            self.logic.id,
            self.logic.room,
            msg.description()
        );
        let _ = self.forced_close.set(msg);
        ctx.run_later(KICK_CLOSE_FLUSH_DELAY, |_act, ctx| ctx.stop());
    }
}

/// Handle outbound packets (forwarding to ChatServer)
impl Handler<Packet> for WtChatSession {
    type Result = ();

    fn handle(&mut self, msg: Packet, _ctx: &mut Self::Context) -> Self::Result {
        trace!(
            "Forwarding packet to ChatServer: session {} room {}",
            self.logic.id,
            self.logic.room
        );
        // #2136: `requires_host` rides the Packet so the funnel can refuse to fan
        // out a MEETING_TIMER from a non-host. Both transports delegate to the
        // SAME `client_message_for` rather than mirroring the branch, so the
        // gate cannot be disabled on one transport only.
        self.logic.addr.do_send(self.logic.client_message_for(msg));
    }
}

// =============================================================================
// Helper Methods
// =============================================================================

impl WtChatSession {
    fn join_room(&self, ctx: &mut Context<Self>) {
        let join_room = self.logic.addr.send(
            self.logic
                .create_join_room_message(ctx.address().recipient()),
        );
        let join_room = join_room.into_actor(self);
        join_room
            .then(|response, act, ctx| {
                if act.logic.handle_join_room_result(response) {
                    ctx.stop();
                }
                fut::ready(())
            })
            .wait(ctx);
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::priority_drop::{
        caption_bytes, caption_wrapper, media_wrapper, PRIORITY_DROP_RTT_ECHO_REASON,
    };
    use crate::constants::{
        resolve_wt_mailbox_capacity, INBOUND_MAILBOX_HEADROOM_FACTOR,
        WT_DOWNLINK_AUDIO_CHANNEL_CAPACITY, WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT,
    };
    use protobuf::Enum as ProtobufEnum;
    use videocall_types::protos::caption_packet::caption_packet::Kind as CaptionKind;

    // -----------------------------------------------------------------------
    // Issue #1057 (+ PR #1060 review) + #1144 headroom + #1062 shared binding:
    // the shared actor mailbox must hold a full backlog for BOTH outbound
    // channels, TIMES the burst-headroom factor.
    //
    // `WtChatSession::started` calls `ctx.set_mailbox_capacity(
    // wt_mailbox_capacity())`. The single actor mailbox fronts two independent
    // policy-aware channels — unistream (env-tunable) and datagram (fixed) —
    // and a `Message` only splits between them *after* leaving the mailbox, so
    // the mailbox holds a mix of both. `wt_mailbox_capacity()` sizes it to the
    // SUM of both channel caps (not `max()`, which would let a datagram burst
    // drop at the mailbox once the unistream cap is raised) times the headroom
    // factor (#1144). Constructing a live `WtChatSession` requires a populated
    // `SessionLogic` (NATS, addresses, …), so rather than read the capacity
    // back off a running context we guard the invariant at the value level.
    //
    // #1062: the test now exercises the SAME bindings `started()` calls — the
    // pure `resolve_wt_mailbox_capacity(env)` for env-override cases, and
    // `crate::constants::wt_mailbox_capacity()` for the default-env value — NOT
    // an in-test hand-copied helper that had to "mirror started() exactly" and
    // could silently drift from it. Editing the sizing in
    // `constants::{wt_mailbox_capacity,resolve_wt_mailbox_capacity}` is the only
    // way to change the value, and both the call site and this test read it.
    //
    // The pure resolver is used for the env cases so the test never races the
    // memoised `OnceLock` in `wt_outbound_channel_capacity()`; the default-env
    // case additionally asserts `resolve_wt_mailbox_capacity(None) ==
    // wt_mailbox_capacity()` so the pure path and the memoised call-site path
    // cannot diverge.
    // -----------------------------------------------------------------------

    /// actix mailbox default — see `actix::mailbox::DEFAULT_CAPACITY`.
    const ACTIX_DEFAULT_MAILBOX_CAPACITY: usize = 16;

    #[test]
    fn wt_mailbox_capacity_covers_both_outbound_channels() {
        // Sentinel: the documented burst-headroom factor (#1144). If this
        // changes, re-validate the join-burst absorption math.
        assert_eq!(
            INBOUND_MAILBOX_HEADROOM_FACTOR, 2,
            "inbound mailbox headroom factor changed; re-validate the #1144 \
             join-burst absorption math before changing this sentinel",
        );
        // #1062 lock-step: the pure resolver at the default env must equal the
        // memoised call-site binding `started()` actually feeds. (No
        // `WT_OUTBOUND_CHANNEL_CAPACITY` env is set under `cargo test`, so the
        // OnceLock-backed getter resolves to the default — this assert ties the
        // two code paths together so neither can drift.)
        assert_eq!(
            resolve_wt_mailbox_capacity(None),
            crate::constants::wt_mailbox_capacity(),
            "the pure resolver and the memoised call-site binding must agree at \
             the default env so the guard test tracks started()'s argument",
        );
        // Default env: mailbox = (unistream default + datagram cap) × headroom,
        // so it holds a full backlog for either channel PLUS burst slack
        // without dropping at the mailbox before the channel's priority-drop
        // runs.
        assert_eq!(
            resolve_wt_mailbox_capacity(None),
            (WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT + WT_DATAGRAM_CHANNEL_CAPACITY)
                * INBOUND_MAILBOX_HEADROOM_FACTOR,
        );
        assert_eq!(resolve_wt_mailbox_capacity(None), 3072);
        // The mailbox must be >= BOTH channels individually AND their sum —
        // the PR #1060 review invariant (a `max()`-sized mailbox would still
        // drop datagram traffic once the unistream cap is tuned up). With a
        // headroom factor >= 1 this still holds.
        assert!(resolve_wt_mailbox_capacity(None) >= WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT);
        assert!(resolve_wt_mailbox_capacity(None) >= WT_DATAGRAM_CHANNEL_CAPACITY);
        assert!(
            resolve_wt_mailbox_capacity(None)
                >= WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT + WT_DATAGRAM_CHANNEL_CAPACITY
        );
        // Compile-time guard: well clear of actix's dumb 16-slot default.
        const _: () =
            assert!(WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT > ACTIX_DEFAULT_MAILBOX_CAPACITY);
        // A deploy-time override of the unistream cap is honoured verbatim
        // and the datagram cap is still added on top before the headroom
        // factor, so the mailbox stays sized for both channels (× headroom)
        // even when operators tune the env var up.
        assert_eq!(
            resolve_wt_mailbox_capacity(Some("1024")),
            (1024 + WT_DATAGRAM_CHANNEL_CAPACITY) * INBOUND_MAILBOX_HEADROOM_FACTOR,
        );
    }

    /// Helper: construct an `is_audio` test packet of the requested size.
    /// Size is the *outbound bytes* length passed into `build_outbound`.
    fn audio_bytes(size: usize) -> Vec<u8> {
        vec![0xAA; size]
    }

    /// Helper: construct a `is_video / control` test packet of the requested size.
    fn other_bytes(size: usize) -> Vec<u8> {
        vec![0xBB; size]
    }

    fn is_datagram(o: &WtOutbound) -> bool {
        matches!(o, WtOutbound::Datagram(_))
    }

    fn is_unistream(o: &WtOutbound) -> bool {
        matches!(o, WtOutbound::UniStream(_))
    }

    // -----------------------------------------------------------------------
    // Audio routing: reliable by default (#2724), datagram on the legacy arm
    // -----------------------------------------------------------------------

    /// FAILS on the un-fixed code, which sent every sub-MTU cleartext audio
    /// frame to a datagram.
    #[test]
    fn audio_media_under_mtu_takes_the_reliable_stream_by_default() {
        let out = build_outbound(
            audio_bytes(100),
            /*is_media=*/ true,
            /*is_audio=*/ true,
            OutboundPriority::Audio,
            AudioDownlinkLane::Reliable,
        );
        assert!(
            is_unistream(&out),
            "a ds=1 receiver's audio must ride the reliable stream, got {:?}",
            out
        );
    }

    #[test]
    fn every_audio_size_takes_the_reliable_stream_by_default() {
        for len in [1usize, 100, DATAGRAM_MAX_SIZE - 1, DATAGRAM_MAX_SIZE, 1500] {
            let out = build_outbound(
                audio_bytes(len),
                /*is_media=*/ true,
                /*is_audio=*/ true,
                OutboundPriority::Audio,
                AudioDownlinkLane::Reliable,
            );
            assert!(
                is_unistream(&out),
                "{len}B audio must ride the reliable stream, got {:?}",
                out
            );
        }
    }

    #[test]
    fn audio_media_under_mtu_routes_via_datagram_on_the_legacy_arm() {
        let out = build_outbound(
            audio_bytes(100),
            /*is_media=*/ true,
            /*is_audio=*/ true,
            OutboundPriority::Audio,
            AudioDownlinkLane::Datagram,
        );
        assert!(
            is_datagram(&out),
            "small audio should route to datagram, got {:?}",
            out
        );
    }

    #[test]
    fn audio_media_at_mtu_routes_via_datagram_on_the_legacy_arm() {
        // Boundary case: payload exactly at the MTU still uses datagram.
        let out = build_outbound(
            audio_bytes(DATAGRAM_MAX_SIZE),
            /*is_media=*/ true,
            /*is_audio=*/ true,
            OutboundPriority::Audio,
            AudioDownlinkLane::Datagram,
        );
        assert!(
            is_datagram(&out),
            "audio at MTU boundary should still use datagram, got {:?}",
            out
        );
    }

    #[test]
    fn audio_media_over_mtu_falls_back_to_unistream_on_the_legacy_arm() {
        // Oversized audio (rare — e.g. concatenated frames) must use the
        // reliable stream because datagrams above MTU would be rejected
        // by the QUIC layer.
        let out = build_outbound(
            audio_bytes(1500),
            /*is_media=*/ true,
            /*is_audio=*/ true,
            OutboundPriority::Audio,
            AudioDownlinkLane::Datagram,
        );
        assert!(
            is_unistream(&out),
            "oversized audio must fall back to UniStream, got {:?}",
            out
        );
    }

    #[test]
    fn sealed_audio_is_reliable_on_both_arms() {
        for lane in [AudioDownlinkLane::Reliable, AudioDownlinkLane::Datagram] {
            let out = build_outbound(
                audio_bytes(203),
                /*is_media=*/ true,
                /*is_audio=*/ false,
                OutboundPriority::Audio,
                lane,
            );
            assert!(
                is_unistream(&out),
                "E2EE-sealed audio must stay on the reliable stream under {lane:?}, got {:?}",
                out
            );
        }
    }

    #[test]
    fn the_legacy_arm_is_byte_for_byte_the_pre_2724_route() {
        // Critical short-circuits before the audio branch, so it belongs in the
        // sweep (N2): the revert must not move it either.
        for (is_media, is_audio) in [(true, true), (true, false), (false, false)] {
            for len in [1usize, DATAGRAM_MAX_SIZE, DATAGRAM_MAX_SIZE + 1] {
                let priority = if is_audio {
                    OutboundPriority::Audio
                } else if is_media {
                    OutboundPriority::Video
                } else {
                    OutboundPriority::Control
                };
                let fits = len <= DATAGRAM_MAX_SIZE;
                let want_datagram = if is_media { is_audio && fits } else { fits };
                let out = build_outbound(
                    audio_bytes(len),
                    is_media,
                    is_audio,
                    priority,
                    AudioDownlinkLane::Datagram,
                );
                assert_eq!(
                    is_datagram(&out),
                    want_datagram,
                    "({is_media}, {is_audio}, {len}B) diverged from the pre-#2724 route",
                );
            }
        }
    }

    #[test]
    fn video_media_under_mtu_still_routes_via_unistream() {
        // Even tiny video media (e.g. KEYFRAME_REQUEST replays) keep the
        // reliable stream — we do not want any per-frame loss for video.
        let out = build_outbound(
            other_bytes(100),
            /*is_media=*/ true,
            /*is_audio=*/ false,
            OutboundPriority::Video,
            AudioDownlinkLane::Reliable,
        );
        assert!(
            is_unistream(&out),
            "video media under MTU must still use UniStream, got {:?}",
            out
        );
    }

    #[test]
    fn video_media_large_routes_via_unistream() {
        // A representative 50KB video packet (e.g. an I-frame fragment).
        let out = build_outbound(
            other_bytes(50_000),
            /*is_media=*/ true,
            /*is_audio=*/ false,
            OutboundPriority::Video,
            AudioDownlinkLane::Reliable,
        );
        assert!(
            is_unistream(&out),
            "video media must route via UniStream, got {:?}",
            out
        );
    }

    // -----------------------------------------------------------------------
    // Existing-behaviour preservation
    // -----------------------------------------------------------------------

    #[test]
    fn small_control_packet_still_routes_via_datagram() {
        // Control / non-media packets ≤ MTU keep their existing datagram path.
        let out = build_outbound(
            other_bytes(100),
            /*is_media=*/ false,
            /*is_audio=*/ false,
            OutboundPriority::Control,
            AudioDownlinkLane::Reliable,
        );
        assert!(
            is_datagram(&out),
            "small control packet must still use datagram, got {:?}",
            out
        );
    }

    #[test]
    fn oversized_control_packet_routes_via_unistream() {
        let out = build_outbound(
            other_bytes(DATAGRAM_MAX_SIZE + 1),
            /*is_media=*/ false,
            /*is_audio=*/ false,
            OutboundPriority::Control,
            AudioDownlinkLane::Reliable,
        );
        assert!(
            is_unistream(&out),
            "oversized control packet must use UniStream, got {:?}",
            out
        );
    }

    #[test]
    fn defensive_is_audio_without_is_media_still_routes_as_control() {
        // If a caller incorrectly sets is_audio=true while is_media=false,
        // we treat it as a regular non-media control packet (datagram if
        // small, stream otherwise). is_audio is meaningful only for media.
        let out = build_outbound(
            audio_bytes(100),
            /*is_media=*/ false,
            /*is_audio=*/ true,
            OutboundPriority::Control,
            AudioDownlinkLane::Reliable,
        );
        assert!(
            is_datagram(&out),
            "is_audio without is_media should fall through to control routing, got {:?}",
            out
        );
    }

    /// Enumerated from `PacketType::VALUES` and classified by production, so a
    /// later promotion to `Critical` is covered the moment it lands.
    #[test]
    fn every_critical_packet_type_routes_to_the_unistream_at_any_size() {
        let sizes = [
            1usize,
            200,
            DATAGRAM_MAX_SIZE - 1,
            DATAGRAM_MAX_SIZE,
            DATAGRAM_MAX_SIZE + 1,
        ];

        let mut critical_types = Vec::new();
        for packet_type in PacketType::VALUES.iter().copied() {
            let priority = OutboundPriority::classify_sealed_aware(
                true,
                packet_type,
                None,
                MediaKind::MEDIA_KIND_UNSPECIFIED,
            );
            if priority != OutboundPriority::Critical {
                continue;
            }
            critical_types.push(packet_type);
            for size in sizes {
                let out = build_outbound(
                    other_bytes(size),
                    /*is_media=*/ false,
                    /*is_audio=*/ false,
                    priority,
                    AudioDownlinkLane::Reliable,
                );
                assert!(
                    is_unistream(&out),
                    "{packet_type:?} at {size}B is Critical but routed to {out:?}; \
                     on a datagram it can be lost on the wire or evicted by quinn \
                     with no retransmit, which is how a DOWNLINK_CONGESTION \
                     step-down instruction disappears exactly when the link is \
                     congested",
                );
            }
        }

        assert_eq!(
            critical_types.len(),
            6,
            "the Critical set changed ({critical_types:?}); confirm each new \
             member should take the reliable lane, then update this count",
        );
    }

    /// BITES on an over-correction that routes all control to the unistream.
    #[test]
    fn small_non_critical_traffic_still_routes_to_the_datagram_lane() {
        let cases = [
            (
                PacketType::DIAGNOSTICS,
                None,
                MediaKind::MEDIA_KIND_UNSPECIFIED,
            ),
            (PacketType::HEALTH, None, MediaKind::MEDIA_KIND_UNSPECIFIED),
            (
                PacketType::PEER_EVENT,
                None,
                MediaKind::MEDIA_KIND_UNSPECIFIED,
            ),
            (
                PacketType::CONNECTION,
                None,
                MediaKind::MEDIA_KIND_UNSPECIFIED,
            ),
            (PacketType::MEDIA, Some(MediaType::AUDIO), MediaKind::AUDIO),
        ];

        for (packet_type, media_type, media_kind) in cases {
            let is_media = packet_type == PacketType::MEDIA;
            let is_audio = matches!(media_type, Some(MediaType::AUDIO));
            let priority =
                OutboundPriority::classify_sealed_aware(true, packet_type, media_type, media_kind);
            assert_ne!(
                priority,
                OutboundPriority::Critical,
                "test setup: {packet_type:?} is Critical, so it belongs in the \
                 sibling test instead",
            );
            // Driven on the LEGACY arm: the lane is inert for every non-audio
            // case, and it keeps the AUDIO row asserting what it always did.
            let out = build_outbound(
                other_bytes(200),
                is_media,
                is_audio,
                priority,
                AudioDownlinkLane::Datagram,
            );
            assert!(
                is_datagram(&out),
                "{packet_type:?} ({priority:?}) is not Critical and fits the MTU, \
                 so it must stay on the low-latency datagram lane; got {out:?}",
            );
        }
    }

    // -----------------------------------------------------------------------
    // Phase 2 split-channel invariant (discussion #756)
    //
    // The architectural fix is that the unistream and datagram channels are
    // independent: a saturated unistream channel — typical when QUIC flow
    // control stalls on a slow receiver — must not block the datagram
    // channel from accepting new audio frames. These tests lock in that
    // invariant at the `mpsc::channel` level. The bridge-level invariant
    // (a stalled `write_all` on the persistent stream does not park the
    // datagram writer task) is enforced by the structural split in
    // `bridge.rs::spawn_unistream_writer` / `spawn_datagram_writer` —
    // those run in independent `tokio::spawn` tasks and never share state.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn split_channels_datagrams_survive_unistream_saturation() {
        // The whole point of the channel split: if the unistream channel
        // were a shared queue with datagrams (the pre-Phase-2 topology),
        // saturating it would also drop audio. Under the split,
        // saturating the unistream channel has zero effect on the
        // datagram channel.
        const UNI_CAP: usize = 64;
        const DGRAM_CAP: usize = 16;

        let (uni_tx, _uni_rx) = mpsc::channel::<Bytes>(UNI_CAP);
        let (dgram_tx, mut dgram_rx) = mpsc::channel::<Bytes>(DGRAM_CAP);

        // Saturate the unistream channel completely — we never drain it,
        // simulating a writer task parked on `stream.write_all().await`.
        for i in 0..UNI_CAP {
            uni_tx
                .try_send(Bytes::from(vec![0xBB; 100]))
                .unwrap_or_else(|_| panic!("uni slot {i} must accept"));
        }
        assert_eq!(
            uni_tx.capacity(),
            0,
            "unistream channel must be exactly full before the test runs"
        );

        // The next unistream send must fail (channel full) — proves we
        // really did saturate it.
        match uni_tx.try_send(Bytes::from(vec![0xBB; 100])) {
            Err(mpsc::error::TrySendError::Full(_)) => {}
            other => panic!("expected Full, got {other:?}"),
        }

        // Now push DGRAM_CAP audio packets onto the datagram channel —
        // every one must succeed because the channels are independent.
        for i in 0..DGRAM_CAP {
            dgram_tx
                .try_send(Bytes::from(vec![0xAA; 80]))
                .unwrap_or_else(|_| {
                    panic!("datagram slot {i} must accept while unistream is full")
                });
        }

        // Drain the datagram receiver and confirm we received exactly
        // DGRAM_CAP packets — none were silently dropped or blocked.
        let mut received = 0usize;
        while dgram_rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(
            received, DGRAM_CAP,
            "datagram channel must deliver every packet pushed while unistream is saturated",
        );
    }

    #[tokio::test]
    async fn split_channels_unistream_back_to_admit_after_drain() {
        // Sanity check that the unistream channel itself recovers normally
        // after being drained — proves the split doesn't introduce any
        // accidental persistence of the saturated state.
        const UNI_CAP: usize = 8;
        let (uni_tx, mut uni_rx) = mpsc::channel::<Bytes>(UNI_CAP);

        for _ in 0..UNI_CAP {
            uni_tx
                .try_send(Bytes::from(vec![0; 1]))
                .expect("saturating sends must succeed");
        }
        assert!(uni_tx.try_send(Bytes::from(vec![0; 1])).is_err());

        // Drain everything.
        while uni_rx.try_recv().is_ok() {}

        // After drain we can send up to UNI_CAP again.
        for _ in 0..UNI_CAP {
            uni_tx
                .try_send(Bytes::from(vec![0; 1]))
                .expect("post-drain sends must succeed");
        }
    }

    /// Integration-style verification of the split-writer flow.
    ///
    /// We do not build a real `WebTransportBridge` (that requires a real
    /// `quinn::Connection`), but we *do* mirror the bridge's per-primitive
    /// writer-task topology: spawn one task that drains the unistream
    /// receiver into a stub stream (which artificially blocks, mimicking
    /// QUIC flow-control stall), and another that drains the datagram
    /// receiver into a counter. The test then pushes N video packets onto
    /// the unistream channel and M audio packets onto the datagram
    /// channel concurrently, and asserts that all M datagrams are
    /// delivered even while the unistream writer is parked on the stub
    /// stream's blocking write.
    #[tokio::test]
    async fn split_writer_topology_datagrams_unblocked_by_stream_stall() {
        use std::sync::Arc;
        use tokio::sync::Notify;

        const UNI_CAP: usize = 64;
        const DGRAM_CAP: usize = 32;
        const M_AUDIO: usize = 30;

        let (uni_tx, mut uni_rx) = mpsc::channel::<Bytes>(UNI_CAP);
        let (dgram_tx, mut dgram_rx) = mpsc::channel::<Bytes>(DGRAM_CAP);

        // Stub "stream" — a writer task that consumes from `uni_rx` but
        // blocks forever on a single `Notify::notified()` after the first
        // message. This mimics a real `stream.write_all().await` that has
        // stalled on QUIC flow-control credit exhaustion. Critically, this
        // task never yields — once parked, it does NOT come back to drain
        // more packets. That is the production failure mode the split is
        // designed to survive.
        let stall = Arc::new(Notify::new());
        let stall_writer = stall.clone();
        let unistream_writer = tokio::spawn(async move {
            // Consume the first message to prove the writer was alive,
            // then park indefinitely.
            let _ = uni_rx.recv().await;
            stall_writer.notified().await;
            // After notify, drain remaining (used only by the test
            // teardown so the channel close is observed cleanly).
            while uni_rx.recv().await.is_some() {}
        });

        // Datagram writer — fully independent task that pulls from
        // `dgram_rx` and forwards each payload to a shared counter.
        // Mirrors `spawn_datagram_writer` (no blocking, no shared state
        // with the unistream writer).
        let delivered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let delivered_writer = delivered.clone();
        let datagram_writer = tokio::spawn(async move {
            while let Some(_packet) = dgram_rx.recv().await {
                delivered_writer.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });

        // Push N video packets onto the unistream channel (some will
        // saturate after the writer parks). The exact count doesn't
        // matter; only that the writer becomes blocked.
        for _ in 0..UNI_CAP {
            // Use try_send so the test never deadlocks on a real `send`
            // awaiting capacity. We expect some sends to fail once the
            // writer is parked and the channel fills up — that's the
            // failure mode we tolerate.
            let _ = uni_tx.try_send(Bytes::from(vec![0xBB; 1024]));
        }

        // Push M audio packets onto the *datagram* channel. The whole
        // point: every one of these must be delivered to the datagram
        // writer's counter, even while the unistream writer is parked.
        for i in 0..M_AUDIO {
            // `send` (not `try_send`) — datagram channel has capacity
            // DGRAM_CAP and the writer drains promptly, so this will not
            // block on a healthy split. If split independence is broken
            // we'll deadlock and fail the test's outer timeout.
            tokio::time::timeout(std::time::Duration::from_millis(500), async {
                dgram_tx.send(Bytes::from(vec![0xAA; 80])).await
            })
            .await
            .unwrap_or_else(|_| panic!("audio packet {i} blocked — split-channel invariant broken"))
            .expect("datagram channel must remain open");
        }

        // Give the datagram writer a moment to drain.
        for _ in 0..50 {
            if delivered.load(std::sync::atomic::Ordering::SeqCst) >= M_AUDIO {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(
            delivered.load(std::sync::atomic::Ordering::SeqCst),
            M_AUDIO,
            "every audio datagram must be delivered while the unistream writer is parked",
        );

        // Clean teardown: release the parked unistream writer and drop
        // the senders so both writer tasks exit.
        stall.notify_one();
        drop(uni_tx);
        drop(dgram_tx);
        let _ = unistream_writer.await;
        let _ = datagram_writer.await;
    }

    // -----------------------------------------------------------------------
    // #1638 PART 2: Full-arm congestion-signal behaviour.
    //
    // The ask was to make queue depth the congestion signal by dropping the
    // OLDEST queued frame on `Full`. A true sender-side drop-oldest is NOT
    // implementable on `tokio::sync::mpsc` (the producer holds only a `Sender`,
    // which has no pop-front / evict-oldest). So the honest implementation keeps
    // a TAIL drop on `Full` and delivers the congestion signal via (i) the
    // priority pre-drop and (ii) the bridge writer deadline (#1638 PART 1).
    //
    // These tests pin that honest contract against the REAL production paths —
    // they do NOT re-implement the decision logic inline:
    //   * the unistream pre-check is `wt_unistream_decision`, called by
    //     `wt_route_and_admit` BEFORE the enqueue;
    //   * the channel is a real `mpsc` of `WtOutboundFrame` whose `try_send` is
    //     what the `Full` arm runs. The pre-check test evaluates against the
    //     production capacity; the tail-drop test uses a 4-slot cap so it can
    //     fill the lane and reach `Full` in four pushes.
    // -----------------------------------------------------------------------

    /// The priority pre-check (the REAL shedding surface) sheds VIDEO before the
    /// channel ever reaches `Full`, so by the time `try_send` could return
    /// `Full` the policy has already done the congestion shedding. This is the
    /// "(i) priority pre-drop" half of the PART-2 honest design.
    #[test]
    fn part2_priority_precheck_is_the_real_video_shedding_surface() {
        let total = wt_outbound_channel_capacity();
        // 85% full: above the 80% video threshold, below the 95% audio one.
        let used = total * 85 / 100;
        let free = total - used;
        let no_bytes = SharedQueueByteMeter::default();

        assert!(
            matches!(
                wt_unistream_decision(OutboundPriority::Video, free, &no_bytes),
                PriorityDropDecision::Drop {
                    reason: "priority_drop_video"
                }
            ),
            "VIDEO must be shed by the unistream pre-check at 85% slot fill"
        );
        assert_eq!(
            wt_unistream_decision(OutboundPriority::Audio, free, &no_bytes),
            PriorityDropDecision::Admit,
            "AUDIO must be protected by the unistream pre-check at 85% slot fill"
        );
        let dgram_free = WT_DATAGRAM_CHANNEL_CAPACITY - WT_DATAGRAM_CHANNEL_CAPACITY * 85 / 100;
        assert_eq!(
            wt_datagram_decision(OutboundPriority::Audio, dgram_free),
            PriorityDropDecision::Admit,
        );
    }

    /// Honesty guard for the PART-2 decision: on a real full production-type
    /// channel, `try_send` is a TAIL drop — the NEWEST packet is rejected and
    /// every already-queued (older) packet is preserved in FIFO order. This
    /// proves drop-oldest is genuinely unavailable from the sender side, so the
    /// `Full`-arm comment must NOT claim "drops oldest" (a `try_send` on a full
    /// channel cannot evict the head). If a future edit fakes a drop-oldest by,
    /// say, popping from a side buffer, this test would have to change — keeping
    /// the code and the comment in lock-step.
    #[tokio::test]
    async fn part2_full_arm_is_tail_drop_not_drop_oldest() {
        const CAP: usize = 4;
        let (tx, mut rx) = mpsc::channel::<WtOutboundFrame>(CAP);
        let queued = SharedQueueByteMeter::default();

        for i in 0..CAP {
            enqueue_unistream(
                &tx,
                &queued,
                WtOutboundFrame::new(OutboundPriority::Video, Bytes::from(vec![i as u8])),
            )
            .unwrap_or_else(|_| panic!("pre-fill slot {i} must accept"));
        }
        assert_eq!(tx.capacity(), 0, "channel must be full before the Full arm");
        assert_eq!(
            queued.queued_for(OutboundPriority::Video),
            CAP,
            "each 1-byte frame must be charged exactly once"
        );

        let newest = WtOutboundFrame::new(OutboundPriority::Video, Bytes::from(vec![0xFFu8]));
        match enqueue_unistream(&tx, &queued, newest) {
            Err(mpsc::error::TrySendError::Full(rejected)) => {
                // The newest packet is what the channel handed back as rejected —
                // i.e. it is the tail drop. The sender did NOT evict the oldest.
                assert_eq!(
                    rejected.bytes.as_ref(),
                    &[0xFFu8],
                    "the NEWEST packet must be the one rejected (tail drop)"
                );
            }
            other => panic!("expected Full(newest), got {:?}", other.is_err()),
        }
        assert_eq!(
            queued.queued_for(OutboundPriority::Video),
            CAP,
            "a REFUSED frame must not be charged — the meter would then bound \
             the lane on bytes that were never queued"
        );

        for expected in 0..CAP {
            let got = rx.try_recv().expect("queued frame must still be present");
            assert_eq!(
                got.bytes.as_ref(),
                &[expected as u8],
                "queued frames must survive a Full try_send in FIFO order \
                 (oldest first); a drop-oldest would have evicted frame 0"
            );
        }
        assert!(
            rx.try_recv().is_err(),
            "exactly the CAP pre-filled frames should remain; the newest was \
             tail-dropped and never enqueued"
        );
    }

    fn fill_media_bytes_to(meter: &SharedQueueByteMeter, priority: OutboundPriority, pct: usize) {
        let budget = wt_unistream_byte_budget_for(priority);
        assert!(budget > 0, "{priority:?} has no byte budget to fill");
        meter.on_enqueue(priority, budget * pct / 100);
    }

    fn outbound_drops(kind: &str) -> f64 {
        crate::metrics::OUTBOUND_CHANNEL_DROPS_TOTAL
            .with_label_values(&["webtransport", kind])
            .get()
    }

    fn room_drops(room: &str, reason: &str) -> f64 {
        crate::metrics::RELAY_PACKET_DROPS_TOTAL
            .with_label_values(&[room, "webtransport", reason])
            .get()
    }

    /// #2721. BITES: admit the echo as `Control` (or `Critical`).
    #[tokio::test]
    async fn a_unistream_probe_is_echoed_on_the_unistream_with_the_probe_echo_class() {
        let (uni_tx, mut uni_rx) = mpsc::channel::<WtOutboundFrame>(wt_outbound_channel_capacity());
        let (dgram_tx, _dgram_rx) = mpsc::channel::<WtOutboundFrame>(WT_DATAGRAM_CHANNEL_CAPACITY);
        let meter = SharedQueueByteMeter::default();

        assert!(matches!(
            wt_echo_admit(
                &uni_tx,
                &dgram_tx,
                &meter,
                WtInboundSource::UniStream,
                Bytes::from_static(b"probe"),
            ),
            WtAdmission::Enqueued
        ));

        let echoed = uni_rx
            .try_recv()
            .expect("the echo must be on the unistream");
        assert_eq!(echoed.bytes.as_ref(), b"probe");
        assert_eq!(
            echoed.priority,
            OutboundPriority::ProbeEcho,
            "the echo must carry the probe-echo class: `Control` is exempt from \
             the byte dimension (S3) and `Critical` books overflow_critical on a \
             bridge shed (S2)",
        );
        assert_eq!(
            meter.queued_for(OutboundPriority::Video),
            0,
            "the echo witnesses the media budget, it must not be charged to it",
        );
        assert_eq!(
            meter.queued_for(OutboundPriority::ProbeEcho),
            b"probe".len(),
            "the echo's own bytes still ride the unbudgeted bucket",
        );
    }

    /// #2721 S3. BITES: give `ProbeEcho` the `Control` byte-dimension exemption;
    /// book the drop on any other kind.
    #[tokio::test]
    #[serial_test::serial]
    async fn byte_pressure_that_sheds_video_sheds_the_echo_onto_the_rtt_kind() {
        let (uni_tx, mut uni_rx) = mpsc::channel::<WtOutboundFrame>(wt_outbound_channel_capacity());
        let (dgram_tx, _dgram_rx) = mpsc::channel::<WtOutboundFrame>(WT_DATAGRAM_CHANNEL_CAPACITY);
        let meter = SharedQueueByteMeter::default();
        fill_media_bytes_to(&meter, OutboundPriority::Video, 81);
        let free = uni_tx.capacity();
        let slot_fill = (wt_outbound_channel_capacity() - free) as f32;
        assert_eq!(
            slot_fill, 0.0,
            "test setup: the SLOT dimension must be idle"
        );

        assert!(
            matches!(
                wt_unistream_decision(OutboundPriority::Video, free, &meter),
                PriorityDropDecision::Drop { .. }
            ),
            "test setup: this fill must be a fill that sheds camera video",
        );

        let room = "room-2721-echo-shed";
        let rtt_before = outbound_drops(ECHO_DROP_KIND);
        let critical_before = outbound_drops("overflow_critical");
        let reason_before = room_drops(room, PRIORITY_DROP_RTT_ECHO_REASON);

        assert_eq!(
            ECHO_DROP_KIND, "rtt",
            "the alert and the client's probe-timeout story both cite this \
             literal series name",
        );
        let dead = wt_echo_route_and_book(
            &uni_tx,
            &dgram_tx,
            &meter,
            room,
            7,
            WtInboundSource::UniStream,
            Bytes::from_static(b"probe"),
        );
        assert!(!dead, "a shed echo must not kill the session");
        assert!(
            uni_rx.try_recv().is_err(),
            "the echo must be shed alongside the video it measures, not queued",
        );

        assert_eq!(
            outbound_drops(ECHO_DROP_KIND) - rtt_before,
            1.0,
            "an unechoed probe belongs on the rtt kind",
        );
        assert_eq!(
            outbound_drops("overflow_critical") - critical_before,
            0.0,
            "overflow_critical PAGES and must stay lifecycle-only",
        );
        assert_eq!(
            room_drops(room, PRIORITY_DROP_RTT_ECHO_REASON) - reason_before,
            1.0,
            "the room-tagged counter carries the finer shed reason",
        );
    }

    /// BITES: subject the datagram arm of `wt_echo_admit` to a media shed.
    #[tokio::test]
    async fn a_datagram_probe_is_still_echoed_under_the_same_byte_pressure() {
        let (uni_tx, _uni_rx) = mpsc::channel::<WtOutboundFrame>(wt_outbound_channel_capacity());
        let (dgram_tx, mut dgram_rx) =
            mpsc::channel::<WtOutboundFrame>(WT_DATAGRAM_CHANNEL_CAPACITY);
        let meter = SharedQueueByteMeter::default();
        fill_media_bytes_to(&meter, OutboundPriority::Video, 81);

        assert!(
            matches!(
                wt_echo_admit(
                    &uni_tx,
                    &dgram_tx,
                    &meter,
                    WtInboundSource::UniStream,
                    Bytes::from_static(b"probe"),
                ),
                WtAdmission::PriorityDropped { .. }
            ),
            "test setup: this fill must shed the unistream echo",
        );

        assert!(
            matches!(
                wt_echo_admit(
                    &uni_tx,
                    &dgram_tx,
                    &meter,
                    WtInboundSource::Datagram,
                    Bytes::from_static(b"probe"),
                ),
                WtAdmission::Enqueued
            ),
            "the datagram lane is independent of stream flow control and has no \
             media byte budget — its echo must not inherit the unistream shed",
        );
        assert_eq!(
            dgram_rx
                .try_recv()
                .expect("the datagram echo must be on the datagram lane")
                .bytes
                .as_ref(),
            b"probe",
        );
    }

    /// BITES: move the arm to `PRIORITY_DROP_AUDIO_FILL_RATIO`; drop `Screen`
    /// from `fullest_media_byte_dimension`.
    #[test]
    fn the_echo_sheds_at_the_video_threshold_on_the_fullest_media_bucket() {
        let free = wt_outbound_channel_capacity();

        let just_below = SharedQueueByteMeter::default();
        fill_media_bytes_to(&just_below, OutboundPriority::Video, 79);
        assert_eq!(
            wt_unistream_decision(OutboundPriority::ProbeEcho, free, &just_below),
            PriorityDropDecision::Admit,
            "below the camera shed point the lane is delivering media, so the \
             probe must be delivered too",
        );

        let just_above = SharedQueueByteMeter::default();
        fill_media_bytes_to(&just_above, OutboundPriority::Video, 81);
        assert_eq!(
            wt_unistream_decision(OutboundPriority::ProbeEcho, free, &just_above),
            PriorityDropDecision::Drop {
                reason: PRIORITY_DROP_RTT_ECHO_REASON
            },
        );

        let screen_only = SharedQueueByteMeter::default();
        fill_media_bytes_to(&screen_only, OutboundPriority::Screen, 81);
        assert_eq!(
            wt_unistream_decision(OutboundPriority::ProbeEcho, free, &screen_only),
            PriorityDropDecision::Drop {
                reason: PRIORITY_DROP_RTT_ECHO_REASON
            },
            "a screen-only lane under pressure must shed the probe too",
        );
        assert_eq!(
            wt_unistream_decision(OutboundPriority::Audio, free, &screen_only),
            PriorityDropDecision::Admit,
            "test setup: nothing else sheds at this fill, so the probe arm is \
             what this asserts",
        );
    }

    /// BITES: return `ProbeEcho` from any `classify_sealed_aware` arm.
    #[test]
    fn no_fanned_out_packet_classifies_as_a_probe_echo() {
        for &packet_type in PacketType::VALUES {
            for &media_kind in MediaKind::VALUES {
                for media_type in
                    std::iter::once(None).chain(MediaType::VALUES.iter().copied().map(Some))
                {
                    for parsed in [false, true] {
                        assert_ne!(
                            OutboundPriority::classify_sealed_aware(
                                parsed,
                                packet_type,
                                media_type,
                                media_kind,
                            ),
                            OutboundPriority::ProbeEcho,
                            "({parsed}, {packet_type:?}, {media_type:?}, {media_kind:?}) \
                             classified as a probe echo",
                        );
                    }
                }
            }
        }
    }

    fn fill_unistream(
        meter: &SharedQueueByteMeter,
        priority: OutboundPriority,
        count: usize,
        frame_bytes: usize,
    ) -> usize {
        for _ in 0..count {
            meter.on_enqueue(priority, frame_bytes);
        }
        wt_outbound_channel_capacity() - count
    }

    /// Few LARGE frames use few slots and most of the budget, so bytes trip
    /// first. BITES on `evaluate`, which admits.
    #[test]
    fn few_large_video_frames_trip_the_byte_dimension_before_the_slot_one() {
        let frame_bytes = crate::constants::tier_frame_bytes(
            &videocall_aq::constants::VIDEO_QUALITY_TIERS
                [videocall_aq::constants::DEFAULT_VIDEO_TIER_INDEX],
        );
        let count = (OUTBOUND_VIDEO_BYTE_BUDGET * 80 / 100) / frame_bytes + 1;
        let meter = SharedQueueByteMeter::default();
        let free = fill_unistream(&meter, OutboundPriority::Video, count, frame_bytes);

        let slot_fill =
            (wt_outbound_channel_capacity() - free) as f32 / wt_outbound_channel_capacity() as f32;
        assert!(
            slot_fill < 0.20,
            "test setup: {count} frames must leave the SLOT dimension far below \
             its 0.80 shed point, or this proves nothing about bytes \
             (slot fill {slot_fill})",
        );

        assert_eq!(
            wt_unistream_decision(OutboundPriority::Video, free, &meter),
            PriorityDropDecision::Drop {
                reason: "priority_drop_video"
            },
            "{count} x {frame_bytes}B of queued camera video is past 80% of the \
             {OUTBOUND_VIDEO_BYTE_BUDGET}B budget and must shed, even though \
             only {count} of the lane's slots are used",
        );

        assert_eq!(
            wt_unistream_decision(OutboundPriority::Screen, free, &meter),
            PriorityDropDecision::Admit,
            "camera bytes must not be charged against the screen budget",
        );
        assert_eq!(
            wt_unistream_decision(OutboundPriority::Audio, free, &meter),
            PriorityDropDecision::Admit,
            "audio costs slots, not bytes (#2261)",
        );
    }

    #[test]
    fn screen_sheds_on_its_own_byte_budget_at_the_screen_threshold() {
        let frame_bytes =
            crate::constants::tier_frame_bytes(&videocall_aq::constants::SCREEN_QUALITY_TIERS[0]);
        let meter = SharedQueueByteMeter::default();

        let below = (OUTBOUND_SCREEN_BYTE_BUDGET * 85 / 100) / frame_bytes;
        let free = fill_unistream(&meter, OutboundPriority::Screen, below, frame_bytes);
        assert_eq!(
            wt_unistream_decision(OutboundPriority::Screen, free, &meter),
            PriorityDropDecision::Admit,
            "at 85% of its byte budget SCREEN is still held — it sheds at 90%, \
             a full 10 points after camera video",
        );

        let to_ninety = (OUTBOUND_SCREEN_BYTE_BUDGET * 90 / 100) / frame_bytes + 1 - below;
        let free = fill_unistream(&meter, OutboundPriority::Screen, to_ninety, frame_bytes);
        assert_eq!(
            wt_unistream_decision(OutboundPriority::Screen, free, &meter),
            PriorityDropDecision::Drop {
                reason: "priority_drop_video"
            },
            "past 90% of the screen byte budget SCREEN must shed",
        );
    }

    /// The unmetered lane is sound only while every priority reaching it has a
    /// zero byte budget.
    #[test]
    fn the_datagram_lane_carries_no_priority_with_a_byte_budget() {
        let cases = [
            (
                80usize,
                PacketType::MEDIA,
                Some(MediaType::AUDIO),
                MediaKind::AUDIO,
            ),
            (
                200,
                PacketType::MEDIA,
                Some(MediaType::VIDEO),
                MediaKind::VIDEO,
            ),
            (
                200,
                PacketType::MEDIA,
                Some(MediaType::SCREEN),
                MediaKind::SCREEN,
            ),
            (200, PacketType::MEDIA, None, MediaKind::VIDEO),
            (
                200,
                PacketType::MEDIA,
                Some(MediaType::HEARTBEAT),
                MediaKind::MEDIA_KIND_UNSPECIFIED,
            ),
            (
                200,
                PacketType::AES_KEY,
                None,
                MediaKind::MEDIA_KIND_UNSPECIFIED,
            ),
            (
                200,
                PacketType::DIAGNOSTICS,
                None,
                MediaKind::MEDIA_KIND_UNSPECIFIED,
            ),
            (
                200,
                PacketType::HEALTH,
                None,
                MediaKind::MEDIA_KIND_UNSPECIFIED,
            ),
            (
                200,
                PacketType::PEER_EVENT,
                None,
                MediaKind::MEDIA_KIND_UNSPECIFIED,
            ),
            (
                DATAGRAM_MAX_SIZE,
                PacketType::MEDIA,
                Some(MediaType::AUDIO),
                MediaKind::AUDIO,
            ),
            (
                DATAGRAM_MAX_SIZE + 1,
                PacketType::MEDIA,
                Some(MediaType::AUDIO),
                MediaKind::AUDIO,
            ),
        ];

        let mut datagram_routed = 0;
        for (len, packet_type, media_type, media_kind) in cases {
            let is_media = packet_type == PacketType::MEDIA;
            let is_audio = matches!(media_type, Some(MediaType::AUDIO));
            let priority =
                OutboundPriority::classify_sealed_aware(true, packet_type, media_type, media_kind);
            if matches!(
                build_outbound(
                    vec![0u8; len],
                    is_media,
                    is_audio,
                    priority,
                    AudioDownlinkLane::Datagram,
                ),
                WtOutbound::UniStream(_)
            ) {
                continue;
            }
            datagram_routed += 1;
            assert_eq!(
                wt_unistream_byte_budget_for(priority),
                0,
                "a {len}B {packet_type:?}/{media_type:?}/{media_kind:?} packet routes to the \
                 datagram lane as {priority:?}; giving that priority a byte budget means the \
                 lane needs a meter too (#2717)",
            );
        }
        assert!(
            datagram_routed >= 4,
            "only {datagram_routed} cases routed to the datagram lane; the \
             routing changed and this test no longer exercises it",
        );
        // Driven on the LEGACY arm on purpose: that is the widest datagram
        // surface, so the invariant is checked against every priority that can
        // still reach the lane (#2724).
    }

    /// BITES: drop the `on_enqueue` and the meter never rises, so neither the
    /// byte shed nor the #1638 gate ever fires.
    #[tokio::test]
    async fn the_credit_site_charges_accepted_frames_and_only_those() {
        let (tx, mut rx) = mpsc::channel::<WtOutboundFrame>(2);
        let queued = SharedQueueByteMeter::default();

        enqueue_unistream(
            &tx,
            &queued,
            WtOutboundFrame::new(OutboundPriority::Video, Bytes::from(vec![0u8; 900])),
        )
        .expect("first slot is free");
        enqueue_unistream(
            &tx,
            &queued,
            WtOutboundFrame::new(OutboundPriority::Screen, Bytes::from(vec![0u8; 700])),
        )
        .expect("second slot is free");

        assert_eq!(queued.queued_for(OutboundPriority::Video), 900);
        assert_eq!(queued.queued_for(OutboundPriority::Screen), 700);

        assert!(enqueue_unistream(
            &tx,
            &queued,
            WtOutboundFrame::new(OutboundPriority::Video, Bytes::from(vec![0u8; 5_000])),
        )
        .is_err());
        assert_eq!(
            queued.snapshot().queued_total(),
            1_600,
            "a frame the lane refused must not be charged",
        );

        rx.close();
        while rx.try_recv().is_ok() {}
        assert!(enqueue_unistream(
            &tx,
            &queued,
            WtOutboundFrame::new(OutboundPriority::Video, Bytes::from(vec![0u8; 11])),
        )
        .is_err());
        assert_eq!(queued.snapshot().queued_total(), 1_600);
    }

    /// The admission step must HONOUR its pre-check, not merely compute it.
    #[tokio::test]
    async fn the_admission_step_refuses_what_its_pre_check_sheds() {
        let (tx, _rx) = mpsc::channel::<WtOutboundFrame>(wt_outbound_channel_capacity());
        let queued = SharedQueueByteMeter::default();
        queued.on_enqueue(
            OutboundPriority::Video,
            OUTBOUND_VIDEO_BYTE_BUDGET * 80 / 100,
        );

        let admitted = wt_unistream_admit(
            &tx,
            &queued,
            OutboundPriority::Video,
            Bytes::from(vec![0u8; 3_000]),
            DownlinkStreamKey::Control,
        );
        assert!(
            matches!(admitted, WtAdmission::PriorityDropped { .. }),
            "a frame past the camera byte budget must be refused",
        );
        assert_eq!(
            tx.capacity(),
            wt_outbound_channel_capacity(),
            "a shed frame must never reach the channel",
        );
        assert_eq!(
            queued.queued_for(OutboundPriority::Video),
            OUTBOUND_VIDEO_BYTE_BUDGET * 80 / 100,
            "and must never be charged",
        );

        assert!(matches!(
            wt_unistream_admit(
                &tx,
                &queued,
                OutboundPriority::Critical,
                Bytes::from(vec![0u8; 3_000]),
                DownlinkStreamKey::Control,
            ),
            WtAdmission::Enqueued
        ));
    }

    fn armed_stage_one() -> DownlinkShedEscalation {
        let escalation = DownlinkShedEscalation::new();
        let base = crate::actors::session_logic::downlink_congested_epoch_now();
        let step = crate::constants::WT_SHED_ESCALATION_ROUND.as_millis() as u64;
        for round in 0..crate::constants::WT_SHED_ESCALATION_STAGE1_ROUNDS as u64 {
            escalation.record_shed_at(base + round * step, 40 + round);
        }
        assert!(
            escalation.camera_video_is_shed(),
            "test setup failed: stage 1 is not armed",
        );
        escalation
    }

    fn admit_one(
        escalation: &DownlinkShedEscalation,
        media_type: MediaType,
        media_kind: MediaKind,
    ) -> WtAdmission {
        let (uni_tx, _uni_rx) = mpsc::channel::<WtOutboundFrame>(wt_outbound_channel_capacity());
        let (dgram_tx, _dgram_rx) = mpsc::channel::<WtOutboundFrame>(WT_DATAGRAM_CHANNEL_CAPACITY);
        let queued = SharedQueueByteMeter::default();
        wt_route_and_admit(
            &uni_tx,
            &dgram_tx,
            &queued,
            vec![0u8; 4_096],
            true,
            media_type == MediaType::AUDIO,
            Some(&media_wrapper(media_kind)),
            Some(media_type),
            7,
            AudioDownlinkLane::Reliable,
            escalation,
        )
    }

    /// FAILS on the un-fixed code, which has no gate and enqueues the camera
    /// frame. BITES: widen the gate to SCREEN or AUDIO, or return
    /// `PriorityDropped` — that arm books the two series that page (E19).
    #[test]
    fn stage_one_drops_camera_video_at_admission_and_protects_screen_and_audio() {
        let escalation = armed_stage_one();

        assert!(
            matches!(
                admit_one(&escalation, MediaType::VIDEO, MediaKind::VIDEO),
                WtAdmission::EscalationShed
            ),
            "an escalated receiver must be sent no camera video at all",
        );
        assert!(
            matches!(
                admit_one(&escalation, MediaType::SCREEN, MediaKind::SCREEN),
                WtAdmission::Enqueued
            ),
            "SCREEN outranks cameras under #1977 and must survive stage 1",
        );
        assert!(
            matches!(
                admit_one(&escalation, MediaType::AUDIO, MediaKind::AUDIO),
                WtAdmission::Enqueued
            ),
            "AUDIO is never a shed candidate",
        );
    }

    /// BITES: point `book_escalation_shed` at `OUTBOUND_CHANNEL_DROPS_TOTAL` or
    /// `RELAY_PACKET_DROPS_TOTAL`.
    #[test]
    #[serial_test::serial]
    fn a_stage_one_shed_books_only_the_non_alerting_series() {
        let before = RELAY_DOWNLINK_SHED_TOTAL
            .with_label_values(&["webtransport"])
            .get();
        book_escalation_shed();
        assert_eq!(
            RELAY_DOWNLINK_SHED_TOTAL
                .with_label_values(&["webtransport"])
                .get()
                - before,
            1.0,
            "relay_downlink_shed_total is the series with no alert rule in any \
             of the three prometheus values files",
        );
    }

    #[test]
    fn an_unescalated_receiver_keeps_its_camera_video() {
        let escalation = DownlinkShedEscalation::new();
        assert!(
            !escalation.camera_video_is_shed(),
            "a receiver that has never shed is not escalated",
        );
        assert!(matches!(
            admit_one(&escalation, MediaType::VIDEO, MediaKind::VIDEO),
            WtAdmission::Enqueued
        ));
    }

    /// A CAPTION never takes the datagram lane or the control stream.
    #[tokio::test]
    async fn a_caption_rides_the_shared_stream_never_a_datagram() {
        let (uni_tx, mut uni_rx) = mpsc::channel::<WtOutboundFrame>(wt_outbound_channel_capacity());
        let (dgram_tx, mut dgram_rx) =
            mpsc::channel::<WtOutboundFrame>(WT_DATAGRAM_CHANNEL_CAPACITY);
        let queued = SharedQueueByteMeter::default();
        for len in [120, DATAGRAM_MAX_SIZE + 1] {
            let admitted = wt_route_and_admit(
                &uni_tx,
                &dgram_tx,
                &queued,
                vec![0u8; len],
                false,
                false,
                Some(&caption_wrapper(caption_bytes(CaptionKind::TEXT))),
                None,
                0,
                AudioDownlinkLane::Reliable,
                &DownlinkShedEscalation::new(),
            );
            assert!(matches!(admitted, WtAdmission::Enqueued), "len={len}");
            let frame = uni_rx.try_recv().expect("caption on the unistream lane");
            assert_eq!(frame.priority, OutboundPriority::Caption, "len={len}");
            assert_eq!(frame.key, DownlinkStreamKey::Shared, "len={len}");
        }
        assert!(dgram_rx.try_recv().is_err());
    }

    /// Admitted media reaches the caption threshold only as a screen backlog, or
    /// as one camera keyframe admitted below 0.80 that lands past 0.90; a
    /// camera-only backlog stops near 0.80 and captions pass. BITES: judge
    /// Caption on its own zero byte budget; shed it at the video or audio ratio.
    #[test]
    fn a_caption_sheds_only_once_admitted_media_passes_the_screen_threshold() {
        use crate::actors::priority_drop::{
            PRIORITY_DROP_AUDIO_FILL_RATIO, PRIORITY_DROP_CAPTION_REASON,
            PRIORITY_DROP_SCREEN_FILL_RATIO,
        };
        fn offer(
            tx: &mpsc::Sender<WtOutboundFrame>,
            meter: &SharedQueueByteMeter,
            priority: OutboundPriority,
            len: usize,
        ) -> Option<&'static str> {
            let bytes = Bytes::from(vec![0u8; len]);
            match wt_unistream_admit(tx, meter, priority, bytes, DownlinkStreamKey::Shared) {
                WtAdmission::Enqueued => None,
                WtAdmission::PriorityDropped { reason, .. } => Some(reason),
                _ => panic!("{priority:?} was neither admitted nor priority-dropped"),
            }
        }
        let video = wt_unistream_byte_budget_for(OutboundPriority::Video);
        let screen = wt_unistream_byte_budget_for(OutboundPriority::Screen);
        let fill =
            |meter: &SharedQueueByteMeter, p, budget| meter.queued_for(p) as f32 / budget as f32;

        let (tx, _rx) = mpsc::channel(wt_outbound_channel_capacity());
        let meter = SharedQueueByteMeter::default();
        while offer(&tx, &meter, OutboundPriority::Screen, screen / 25).is_none() {}
        assert!(fill(&meter, OutboundPriority::Screen, screen) < PRIORITY_DROP_AUDIO_FILL_RATIO);
        assert_eq!(
            offer(&tx, &meter, OutboundPriority::Caption, 64),
            Some(PRIORITY_DROP_CAPTION_REASON)
        );
        assert_eq!(offer(&tx, &meter, OutboundPriority::Audio, 64), None);

        let (tx, _rx) = mpsc::channel(wt_outbound_channel_capacity());
        let meter = SharedQueueByteMeter::default();
        while meter.queued_for(OutboundPriority::Video) + video / 32 < video * 80 / 100 {
            assert_eq!(
                offer(&tx, &meter, OutboundPriority::Video, video / 32),
                None
            );
        }
        assert_eq!(
            offer(&tx, &meter, OutboundPriority::Video, video * 15 / 100),
            None
        );
        let keyframe_fill = fill(&meter, OutboundPriority::Video, video);
        assert!(
            (PRIORITY_DROP_SCREEN_FILL_RATIO..PRIORITY_DROP_AUDIO_FILL_RATIO)
                .contains(&keyframe_fill),
            "{keyframe_fill}"
        );
        assert_eq!(
            offer(&tx, &meter, OutboundPriority::Caption, 64),
            Some(PRIORITY_DROP_CAPTION_REASON)
        );
        assert_eq!(offer(&tx, &meter, OutboundPriority::Audio, 64), None);

        let (tx, _rx) = mpsc::channel(wt_outbound_channel_capacity());
        let meter = SharedQueueByteMeter::default();
        while offer(&tx, &meter, OutboundPriority::Video, video / 32).is_none() {}
        assert!(fill(&meter, OutboundPriority::Video, video) < PRIORITY_DROP_SCREEN_FILL_RATIO);
        assert_eq!(offer(&tx, &meter, OutboundPriority::Caption, 64), None);
    }

    #[test]
    fn a_screen_backlog_sheds_caption_text_but_routes_caption_status_to_the_shared_stream() {
        use crate::actors::priority_drop::PRIORITY_DROP_CAPTION_REASON;
        let (uni_tx, mut uni_rx) = mpsc::channel(wt_outbound_channel_capacity());
        let (dgram_tx, _dgram_rx) = mpsc::channel(WT_DATAGRAM_CHANNEL_CAPACITY);
        let meter = SharedQueueByteMeter::default();
        let screen = wt_unistream_byte_budget_for(OutboundPriority::Screen);
        while matches!(
            wt_unistream_admit(
                &uni_tx,
                &meter,
                OutboundPriority::Screen,
                Bytes::from(vec![0u8; screen / 25]),
                DownlinkStreamKey::Shared,
            ),
            WtAdmission::Enqueued
        ) {}
        while uni_rx.try_recv().is_ok() {}
        let route = |kind| {
            let caption = caption_bytes(kind);
            wt_route_and_admit(
                &uni_tx,
                &dgram_tx,
                &meter,
                vec![0u8; caption.len() + 8],
                false,
                false,
                Some(&caption_wrapper(caption)),
                None,
                0,
                AudioDownlinkLane::Reliable,
                &DownlinkShedEscalation::new(),
            )
        };
        assert!(matches!(
            route(CaptionKind::TEXT),
            WtAdmission::PriorityDropped {
                reason: PRIORITY_DROP_CAPTION_REASON,
                ..
            }
        ));
        for kind in [CaptionKind::STATUS_ACTIVE, CaptionKind::STATUS_STOPPED] {
            assert!(matches!(route(kind), WtAdmission::Enqueued), "{kind:?}");
            let frame = uni_rx.try_recv().expect("status on the unistream lane");
            assert_eq!(frame.priority, OutboundPriority::CaptionStatus);
            assert_eq!(frame.key, DownlinkStreamKey::Shared);
        }
    }

    /// The real classify-route-enqueue path must charge the lane. Everything
    /// `send_auto` does except the drop metrics; it needs NATS to call directly.
    #[tokio::test]
    async fn a_routed_video_send_charges_the_unistream_meter() {
        // PRODUCTION capacity: a short test channel reads ~99% full.
        let (uni_tx, _uni_rx) = mpsc::channel::<WtOutboundFrame>(wt_outbound_channel_capacity());
        let (dgram_tx, _dgram_rx) = mpsc::channel::<WtOutboundFrame>(WT_DATAGRAM_CHANNEL_CAPACITY);
        let queued = SharedQueueByteMeter::default();

        let admitted = wt_route_and_admit(
            &uni_tx,
            &dgram_tx,
            &queued,
            vec![0u8; 4_096],
            true,
            false,
            Some(&media_wrapper(MediaKind::VIDEO)),
            Some(MediaType::VIDEO),
            7,
            AudioDownlinkLane::Reliable,
            &DownlinkShedEscalation::new(),
        );
        assert!(matches!(admitted, WtAdmission::Enqueued));
        assert_eq!(
            queued.queued_for(OutboundPriority::Video),
            4_096,
            "a routed camera frame must be charged to the camera bucket",
        );

        let admitted = wt_route_and_admit(
            &uni_tx,
            &dgram_tx,
            &queued,
            vec![0u8; 200],
            true,
            true,
            Some(&media_wrapper(MediaKind::AUDIO)),
            Some(MediaType::AUDIO),
            7,
            AudioDownlinkLane::Datagram,
            &DownlinkShedEscalation::new(),
        );
        assert!(matches!(admitted, WtAdmission::Enqueued));
        assert_eq!(
            queued.snapshot().queued_total(),
            4_096,
            "the datagram lane has no meter; charging it here would bound the \
             unistream lane on bytes that are not in it",
        );

        // #2724: the SAME packet on the default arm takes the reliable lane and
        // IS charged, with a budget of 0 so it can never be byte-shed.
        let admitted = wt_route_and_admit(
            &uni_tx,
            &dgram_tx,
            &queued,
            vec![0u8; 200],
            true,
            true,
            Some(&media_wrapper(MediaKind::AUDIO)),
            Some(MediaType::AUDIO),
            7,
            AudioDownlinkLane::Reliable,
            &DownlinkShedEscalation::new(),
        );
        assert!(matches!(admitted, WtAdmission::Enqueued));
        assert_eq!(
            queued.queued_for(OutboundPriority::Audio),
            200,
            "reliable audio is charged to the receiver's meter like every other \
             reliable frame (#2717)",
        );
        assert_eq!(
            wt_unistream_byte_budget_for(OutboundPriority::Audio),
            0,
            "audio must keep a zero byte budget: a budget is what makes a class \
             byte-sheddable, and audio must never be",
        );

        // E2EE on: the inner parse fails; the outer kind holds the bucket.
        let admitted = wt_route_and_admit(
            &uni_tx,
            &dgram_tx,
            &queued,
            vec![0u8; 1_000],
            true,
            false,
            Some(&media_wrapper(MediaKind::VIDEO)),
            None,
            7,
            AudioDownlinkLane::Reliable,
            &DownlinkShedEscalation::new(),
        );
        assert!(matches!(admitted, WtAdmission::Enqueued));
        assert_eq!(
            queued.queued_for(OutboundPriority::Video),
            5_096,
            "a sealed camera frame must be charged to the camera bucket, not \
             left uncounted as control",
        );
    }

    #[test]
    fn only_identified_video_and_screen_get_a_publisher_key() {
        const BULK: usize = DATAGRAM_MAX_SIZE + 1;
        const SMALL: usize = 64;

        assert_eq!(
            DownlinkStreamKey::for_media(
                true,
                false,
                42,
                MediaKind::VIDEO,
                BULK,
                AudioDownlinkLane::Reliable
            ),
            DownlinkStreamKey::Publisher {
                session_id: 42,
                kind: PublisherStreamKind::Video
            },
        );
        assert_eq!(
            DownlinkStreamKey::for_media(
                true,
                false,
                42,
                MediaKind::SCREEN,
                BULK,
                AudioDownlinkLane::Reliable
            ),
            DownlinkStreamKey::Publisher {
                session_id: 42,
                kind: PublisherStreamKind::Screen
            },
        );
        // Camera and screen from the SAME publisher are different streams — a
        // screen keyframe must not block that peer's camera.
        assert_ne!(
            DownlinkStreamKey::for_media(
                true,
                false,
                42,
                MediaKind::VIDEO,
                BULK,
                AudioDownlinkLane::Reliable
            ),
            DownlinkStreamKey::for_media(
                true,
                false,
                42,
                MediaKind::SCREEN,
                BULK,
                AudioDownlinkLane::Reliable
            ),
        );
        // FAILS on the un-fixed code, where every audio frame keyed `Control`.
        assert_eq!(
            DownlinkStreamKey::for_media(
                true,
                false,
                42,
                MediaKind::AUDIO,
                SMALL,
                AudioDownlinkLane::Reliable
            ),
            DownlinkStreamKey::Audio,
            "E2EE-sealed audio reaches the audio lane through the OUTER cleartext \
             media_kind (#2724)",
        );
        for len in [1usize, SMALL, DATAGRAM_MAX_SIZE] {
            assert_eq!(
                DownlinkStreamKey::for_media(
                    true,
                    true,
                    42,
                    MediaKind::AUDIO,
                    len,
                    AudioDownlinkLane::Reliable
                ),
                DownlinkStreamKey::Audio,
                "cleartext audio keys the audio lane at {len}B",
            );
            // The rollout case: a pre-`media_kind` publisher's cleartext audio
            // carries no outer kind, so the inner signal is the only one.
            assert_eq!(
                DownlinkStreamKey::for_media(
                    true,
                    true,
                    42,
                    MediaKind::MEDIA_KIND_UNSPECIFIED,
                    len,
                    AudioDownlinkLane::Reliable
                ),
                DownlinkStreamKey::Audio,
                "a pre-media_kind publisher's cleartext audio keys the audio lane \
                 at {len}B",
            );
        }
        assert_eq!(
            DownlinkStreamKey::for_media(
                false,
                true,
                42,
                MediaKind::AUDIO,
                SMALL,
                AudioDownlinkLane::Reliable
            ),
            DownlinkStreamKey::Control,
            "is_audio is meaningful only for media; a non-media frame keys control",
        );
        assert_eq!(
            DownlinkStreamKey::for_media(
                false,
                false,
                42,
                MediaKind::VIDEO,
                BULK,
                AudioDownlinkLane::Reliable
            ),
            DownlinkStreamKey::Control,
            "non-media never keys a publisher stream, whatever media_kind says",
        );

        // The fail-open bucket, split by size. Sub-MTU frames here are HEARTBEAT
        // and KEYFRAME_REQUEST, which are MEDIA packets that leave media_kind
        // unset: they are control-shaped and belong on the lifecycle lane.
        for (session_id, kind) in [
            (42u64, MediaKind::MEDIA_KIND_UNSPECIFIED),
            (0, MediaKind::MEDIA_KIND_UNSPECIFIED),
            (0, MediaKind::VIDEO),
            (0, MediaKind::SCREEN),
        ] {
            assert_eq!(
                DownlinkStreamKey::for_media(
                    true,
                    false,
                    session_id,
                    kind,
                    SMALL,
                    AudioDownlinkLane::Reliable
                ),
                DownlinkStreamKey::Control,
                "sub-MTU unattributable media is control-shaped: ({session_id}, {kind:?})",
            );
            assert_eq!(
                DownlinkStreamKey::for_media(
                    true,
                    false,
                    session_id,
                    kind,
                    BULK,
                    AudioDownlinkLane::Reliable
                ),
                DownlinkStreamKey::Shared,
                "BULK unattributable media must NOT sit in front of #2718 Critical \
                 control on the room's aggregation lane: ({session_id}, {kind:?})",
            );
        }
        assert_eq!(
            DownlinkStreamKey::for_media(
                true,
                false,
                42,
                MediaKind::MEDIA_KIND_UNSPECIFIED,
                DATAGRAM_MAX_SIZE,
                AudioDownlinkLane::Reliable
            ),
            DownlinkStreamKey::Control,
            "the split is strictly ABOVE the datagram MTU",
        );
    }

    /// FAILS on bdc8935e: `for_media` took no lane, so sealed audio kept the
    /// no-shed stream under the revert. Expectations are literals, not a second
    /// copy of the rule.
    #[test]
    fn the_datagram_revert_restores_the_pre_2724_key_for_every_audio_class() {
        const SMALL: usize = 64;
        const BULK: usize = DATAGRAM_MAX_SIZE + 1;

        // (is_audio, media_kind, len) -> the key the pre-#2724 `for_media` gave.
        let pre_2724 = [
            // E2EE-sealed audio: inner parse failed, outer kind is AUDIO.
            (false, MediaKind::AUDIO, SMALL, DownlinkStreamKey::Control),
            (false, MediaKind::AUDIO, BULK, DownlinkStreamKey::Control),
            // Cleartext audio, both sides of the MTU.
            (true, MediaKind::AUDIO, SMALL, DownlinkStreamKey::Control),
            (true, MediaKind::AUDIO, BULK, DownlinkStreamKey::Control),
            // A publisher older than `media_kind` sending cleartext audio.
            (
                true,
                MediaKind::MEDIA_KIND_UNSPECIFIED,
                SMALL,
                DownlinkStreamKey::Control,
            ),
            (
                true,
                MediaKind::MEDIA_KIND_UNSPECIFIED,
                BULK,
                DownlinkStreamKey::Shared,
            ),
        ];

        for (is_audio, media_kind, len, want) in pre_2724 {
            assert_eq!(
                DownlinkStreamKey::for_media(
                    true,
                    is_audio,
                    42,
                    media_kind,
                    len,
                    AudioDownlinkLane::Datagram
                ),
                want,
                "({is_audio}, {media_kind:?}, {len}B) must key exactly as it did \
                 before #2724 under the revert",
            );
        }

        // And the revert must not touch video or screen.
        assert_eq!(
            DownlinkStreamKey::for_media(
                true,
                false,
                42,
                MediaKind::VIDEO,
                BULK,
                AudioDownlinkLane::Datagram
            ),
            DownlinkStreamKey::Publisher {
                session_id: 42,
                kind: PublisherStreamKind::Video
            },
        );
    }

    /// The lane is bounded in SLOTS, so what may enter decides its byte worst
    /// case (A19).
    #[test]
    fn only_a_frame_that_fits_the_mtu_may_enter_the_audio_lane() {
        for (is_audio, media_kind) in [
            (true, MediaKind::AUDIO),
            (false, MediaKind::AUDIO),
            (true, MediaKind::MEDIA_KIND_UNSPECIFIED),
        ] {
            assert_eq!(
                DownlinkStreamKey::for_media(
                    true,
                    is_audio,
                    42,
                    media_kind,
                    DATAGRAM_MAX_SIZE,
                    AudioDownlinkLane::Reliable
                ),
                DownlinkStreamKey::Audio,
                "at the MTU it is still an Opus frame",
            );
            assert_ne!(
                DownlinkStreamKey::for_media(
                    true,
                    is_audio,
                    42,
                    media_kind,
                    DATAGRAM_MAX_SIZE + 1,
                    AudioDownlinkLane::Reliable
                ),
                DownlinkStreamKey::Audio,
                "one byte over the MTU it is not, whatever it claims — \
                 ({is_audio}, {media_kind:?})",
            );
        }

        // The worst case the lane can hold, which is what A7 must state.
        assert_eq!(
            WT_DOWNLINK_AUDIO_CHANNEL_CAPACITY * DATAGRAM_MAX_SIZE,
            614_400,
            "512 slots x the MTU is the audio lane's byte bound",
        );
    }

    #[test]
    fn the_publisher_kind_wire_bytes_match_the_proto() {
        assert_eq!(PublisherStreamKind::Video.media_kind_code(), 1);
        assert_eq!(PublisherStreamKind::Screen.media_kind_code(), 3);
        assert_eq!(audio_media_kind_code(), 2);
        assert_eq!(audio_media_kind_code() as i32, MediaKind::AUDIO.value());
        assert_eq!(
            PublisherStreamKind::Video.media_kind_code() as i32,
            MediaKind::VIDEO.value(),
        );
        assert_eq!(
            PublisherStreamKind::Screen.media_kind_code() as i32,
            MediaKind::SCREEN.value(),
        );
    }

    #[tokio::test]
    async fn the_routed_frame_carries_its_publisher_key() {
        let (uni_tx, mut uni_rx) = mpsc::channel::<WtOutboundFrame>(wt_outbound_channel_capacity());
        let (dgram_tx, _dgram_rx) = mpsc::channel::<WtOutboundFrame>(WT_DATAGRAM_CHANNEL_CAPACITY);
        let queued = SharedQueueByteMeter::default();

        wt_route_and_admit(
            &uni_tx,
            &dgram_tx,
            &queued,
            vec![0u8; 4_096],
            true,
            false,
            Some(&media_wrapper(MediaKind::VIDEO)),
            Some(MediaType::VIDEO),
            909,
            AudioDownlinkLane::Reliable,
            &DownlinkShedEscalation::new(),
        );
        let queued_frame = uni_rx.try_recv().expect("the frame must be queued");
        assert_eq!(
            queued_frame.key,
            DownlinkStreamKey::Publisher {
                session_id: 909,
                kind: PublisherStreamKind::Video
            },
            "the bridge routes on this key; a control key here would put every \
             publisher back on one stream",
        );
    }

    #[test]
    fn the_byte_meter_returns_to_empty_when_the_lane_drains() {
        let meter = SharedQueueByteMeter::default();
        meter.on_enqueue(OutboundPriority::Video, 4_000);
        meter.on_enqueue(OutboundPriority::Screen, 9_000);
        assert_eq!(meter.queued_for(OutboundPriority::Video), 4_000);
        assert_eq!(meter.snapshot().queued_total(), 13_000);

        meter.on_dequeue(OutboundPriority::Video, 4_000);
        meter.on_dequeue(OutboundPriority::Screen, 9_000);
        assert_eq!(meter.snapshot().queued_total(), 0);

        meter.on_dequeue(OutboundPriority::Video, 1);
        assert_eq!(meter.queued_for(OutboundPriority::Video), 0);
    }

    struct ForceCloseSink3285;
    impl Actor for ForceCloseSink3285 {
        type Context = actix::Context<Self>;
    }
    impl Handler<ForceClose> for ForceCloseSink3285 {
        type Result = ();
        fn handle(&mut self, _msg: ForceClose, _ctx: &mut Self::Context) {}
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn with_conn_seq_reaches_join_room_3285() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");
        let chat_server = ChatServer::new(nats_client.clone()).await.start();
        let (unistream_tx, _unistream_rx) = mpsc::channel::<WtOutboundFrame>(8);
        let (datagram_tx, _datagram_rx) = mpsc::channel::<WtOutboundFrame>(8);
        let (tracker_sender, _tracker_rx) = mpsc::unbounded_channel();
        let actor = WtChatSession::new(
            chat_server,
            "room-3285".to_string(),
            "wt@example.com".to_string(),
            "WT".to_string(),
            false,
            unistream_tx,
            datagram_tx,
            Arc::new(SharedQueueByteMeter::default()),
            nats_client,
            tracker_sender,
            SessionManager::new(),
            false,
            Some("iid-3285-wt".to_string()),
            false,
            AudioDownlinkLane::Reliable,
            DownlinkShedEscalation::new(),
        )
        .with_conn_seq(Some(9));

        let join = actor.join_room_message_for_test(ForceCloseSink3285.start().recipient());

        assert_eq!(join.conn_seq, Some(9));
    }
}
