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

//! Wire-contract limits that BOTH the relay and its clients must agree on.
//!
//! These live here — in the crate both sides already depend on — rather than in
//! `actix-api`, because they are not private server policy: a client is required
//! to keep its own send rate strictly under the relay's ceiling, so the ceiling
//! is part of the contract the client is written against. Duplicating the value
//! on the client side (even in a test) produces a pin that cannot fail: a
//! mutation of the relay's constant never reaches a copy, so the "we are under
//! budget" property silently stops being checked the moment the budget moves.

/// Maximum number of RAISE_HAND packets the relay forwards from a single sending
/// session within [`RAISE_HAND_WINDOW_MS`] (issue #2135).
///
/// Like REACTION, a RAISE_HAND is a client-authored packet the relay
/// RE-BROADCASTS room-wide, so it needs an abuse ceiling. The budget is set
/// SEPARATELY from REACTION's because the legitimate traffic shape is different:
/// a hand toggle is human-paced and rare, but a client whose hand is raised ALSO
/// RE-ANNOUNCES its state whenever a new peer joins (the mechanism that makes a
/// hand raised before you joined visible to you). A join wave therefore produces
/// a short burst that a reaction never does.
///
/// `6` per [`RAISE_HAND_WINDOW_MS`] (2s) accommodates that burst with room to
/// spare while still bounding a flood at ~3/sec. The client MUST coalesce its
/// re-announces across a join wave (one send per debounce window, not one per
/// PARTICIPANT_JOINED) and self-throttle STRICTLY below this ceiling — a
/// property `videocall_client::client::raise_hand` pins against THIS constant.
///
/// WHY THE MARGIN MATTERS MORE HERE THAN FOR REACTIONS: dropping a REACTION
/// loses one ephemeral float that nobody misses. Dropping a RAISE_HAND loses a
/// STATE TRANSITION, and because the relay holds no hand registry there is no
/// server-side repair — the room's view of that participant stays wrong until
/// the next announce. That is why the ceiling is generous rather than tight, and
/// why the wire contract asks the client to re-announce on the next peer-join
/// (the packet is idempotent state, so a repeat is always safe).
pub const RAISE_HAND_MAX_PER_WINDOW: u32 = 6;

/// Time window (in milliseconds) for RAISE_HAND rate limiting (issue #2135).
///
/// Paired with [`RAISE_HAND_MAX_PER_WINDOW`] as the per-sender tumbling window.
/// Deliberately 2000ms rather than REACTION's 1000ms: the burst being absorbed
/// is a join wave (whose arrivals are spread over seconds), not a click storm,
/// so a wider window with a proportionally larger budget tolerates the real
/// traffic shape without raising the sustained rate much above REACTION's.
pub const RAISE_HAND_WINDOW_MS: u64 = 2000;

/// Maximum number of MEETING_TIMER packets the relay forwards from a single
/// sending session within [`MEETING_TIMER_WINDOW_MS`] (issue #2136).
///
/// Like REACTION, a MEETING_TIMER is a client-authored packet the relay
/// RE-BROADCASTS room-wide, so it needs an abuse ceiling. The budget is its own
/// rather than a reuse of REACTION's because the legitimate traffic shape is
/// completely different — it is not click-driven at all. A well-behaved host
/// sends:
///
///   * a ~5s HEARTBEAT while a timer runs (0.2/s), which is what makes a late
///     joiner and a reconnected client converge without any join-event
///     plumbing; and
///   * a 3-packet REPEAT BURST on each transition (start / extend / cancel),
///     spread ~1s apart, which is what stops a single lost QUIC datagram from
///     stranding a transition on the WebTransport path.
///
/// Worst realistic case is therefore a transition burst overlapping a
/// heartbeat: ~4 packets in a 2s window. `8` doubles that, so a well-behaved
/// host never comes near the ceiling while a flood is bounded to a SUSTAINED
/// 4/s. (The window is tumbling, so a burst straddling a boundary can pass up
/// to 16 within one 2s span — that is the instantaneous ceiling, and it is
/// still bounded work on a 256-byte packet class.)
///
/// ONE SHAPE THIS DOES NOT COVER, and the client half handles it: a host
/// clicking "+1 min" three times in two seconds is 3 transitions x 3 repeats +
/// a heartbeat = 10, over budget. That is fixed by DEBOUNCING transitions
/// client-side — coalescing a rapid run of adjustments into one repeat burst for
/// the SETTLED state — rather than by raising this ceiling. See
/// `videocall_client::client::meeting_timer::MeetingTimerScheduler`, whose
/// worst-case send rate is pinned against THIS constant.
///
/// WHY THE MARGIN MATTERS: dropping a MEETING_TIMER loses a STATE TRANSITION,
/// and the relay holds no timer registry to repair from. Dropping a CANCEL is
/// the worst case in the whole feature — the room keeps counting down to an
/// audible expiry the host already called off — so the ceiling is deliberately
/// generous rather than tight. The heartbeat repairs a dropped START within one
/// interval; only the transition burst repairs a dropped CANCEL, which is why
/// the budget must comfortably admit the whole burst.
///
/// NOTE this is a PER-SENDER budget and only ONE session in a room (the host)
/// can send this packet type at all — every other sender is rejected by the
/// host gate — so the aggregate room load is bounded by this alone.
pub const MEETING_TIMER_MAX_PER_WINDOW: u32 = 8;

/// Time window (in milliseconds) for MEETING_TIMER rate limiting (issue #2136).
///
/// Paired with [`MEETING_TIMER_MAX_PER_WINDOW`] as the per-sender tumbling
/// window. 2000ms rather than REACTION's 1000ms because the burst being absorbed
/// is a ~1s-spaced transition repeat rather than a click storm: a window
/// narrower than the burst would make a legitimate 3-packet cancel repeat
/// straddle two windows for no benefit.
pub const MEETING_TIMER_WINDOW_MS: u64 = 2000;

/// Maximum `MeetingTimerPacket.duration_ms` the relay accepts at ingress
/// (issue #2136). 24 hours.
///
/// THIS IS ARITHMETIC HYGIENE, NOT AUTHORIZATION. The sender is the
/// relay-verified meeting host and is entitled to any duration inside this
/// bound; the cap exists so an unbounded `u64` cannot overflow or garble
/// client-side arithmetic (`duration_ms` is display-only — a client derives
/// `started_at_ms = ends_at_ms - duration_ms` from it to render a progress
/// proportion). 24h is orders of magnitude above any real presentation slot, so
/// nothing legitimate is refused.
///
/// Over-cap DROPS the whole packet rather than clamping. Clamping would emit a
/// self-inconsistent state (`duration_ms` no longer matching
/// `ends_at_ms - started_at_ms`) that every renderer would then have to
/// second-guess; and a sender that produced an out-of-range duration is broken
/// or forged, which makes its `ends_at_ms` equally untrustworthy. A well-behaved
/// client cannot reach this, so nothing legitimate is lost.
///
/// A RECEIVING client MUST clamp against this INDEPENDENTLY rather than trusting
/// that the relay already did, exactly as it does for REACTION's `custom_emoji`:
/// a peer may be running an older or newer relay than the one this client is
/// connected to reasons about. See
/// `videocall_client::client::meeting_timer::clamp_duration_ms`.
///
/// NOTE what is deliberately NOT bounded: `ends_at_ms`. Bounding an ABSOLUTE
/// instant requires reading the relay's own clock, and a relay whose clock
/// stepped backwards would then start rejecting legitimate timers — a
/// fail-closed wedge of exactly the #2122 shape, in exchange for no security
/// benefit (the host may set any end time it likes). `duration_ms` is bounded by
/// its own magnitude alone, with no clock involved, so it carries no such risk.
pub const MEETING_TIMER_MAX_DURATION_MS: u64 = 24 * 60 * 60 * 1000;

/// Highest `MediaStreamKey` wire value: 1=audio 2=video 3=screen 4=control.
pub const MAX_MEDIA_STREAM_KEY: u8 = 4;

/// Per-stream counter-array length; index 0 is unused so a key indexes its slot.
pub const MEDIA_STREAM_COUNTER_SLOTS: usize = MAX_MEDIA_STREAM_KEY as usize + 1;
