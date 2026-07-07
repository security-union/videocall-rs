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

//! Send-side state machine for the simulcast layer-preference control packet
//! (issue #989, Phase 2).
//!
//! Sibling of [`ViewportSender`](super::viewport_sender::ViewportSender). The
//! receiver-driven layer chooser ([`crate::decode::layer_chooser`]) produces, on
//! each ~1s monitor tick, the desired simulcast layer for every remote source
//! this client is decoding. This module relays that map to the relay as a
//! `LAYER_PREFERENCE` control packet so the relay drops the non-selected layers
//! it would otherwise forward — turning the receiver's local decision into real
//! downlink savings.
//!
//! ## Why a dedicated state machine
//!
//! Same real-time hazards the viewport sender guards against (CLAUDE.md Change
//! Impact Policy):
//!
//! 1. **Storm / rate-limit.** The chooser may nudge a layer every tick. The
//!    relay rate-limits accepted updates to `LAYER_PREFERENCE_MIN_UPDATE_INTERVAL`
//!    (200ms) and we mirror that client-side: we only emit on an *actual change*
//!    of the desired map AND no sooner than [`LAYER_PREFERENCE_MIN_UPDATE_MS`]
//!    after the last accepted send. Unchanged maps cost nothing.
//! 2. **Entry cap.** The relay accepts at most `LAYER_PREFERENCE_MAX_ENTRIES`
//!    (64) entries; we cap before sending so a large meeting never trips the
//!    relay's truncation.
//! 3. **Fail-open recovery on reconnect.** On reconnect/re-election the relay
//!    allocates a fresh, empty preference map (fail-open → every layer flows
//!    again). [`LayerPreferenceSender::reset_for_reconnect`] forces the next
//!    flush to re-send unconditionally so filtering resumes.
//!
//! ## Semantics (must match `layer_preference_packet.proto`)
//!
//! The map this sender carries is `source_session_id -> desired_layer`:
//!   * An entry `{session_id, desired_layer = N}` means "forward ONLY layer N
//!     from this source". `N = 0` means "base layer only" (drop upgraded
//!     layers), NOT "no preference".
//!   * OMITTING a source means "no preference → forward ALL its layers"
//!     (fail-open). The chooser always selects a concrete layer per peer, so in
//!     normal operation every decoded source has an entry; a source is only
//!     omitted when we have not yet chosen for it.
//!
//! ## Security (subject-authoritative; never trust payload identity)
//!
//! The relay records this preference keyed by the RECEIVER's own NATS subject
//! (`room.{id}.{receiver_session}`), so it can only ever subtract what THIS
//! receiver gets — a forged payload self-degrades only the forger's own view.
//! The `session_id` in each entry is the real relay session id of a peer this
//! client is receiving (the same id the viewport sender uses), never a forged
//! or inner field. The wasm glue that owns this sender sends it on the
//! receiver's own Control stream.
//!
//! The pure change-detection / canonicalization / rate-limit logic lives here
//! (and is unit-tested); the wasm-only transport plumbing lives in
//! `video_call_client.rs`, which drives this state machine.

use crate::decode::layer_chooser::PrefMediaKind;
use std::collections::BTreeMap;

/// Key for one desired-layer entry: a specific media kind of a specific source
/// session (issue #989, Phase 3). Ordered so the canonical map / on-wire entry
/// order is deterministic.
pub(crate) type PrefKey = (u64, PrefMediaKind);

/// Client-side minimum interval (ms) between accepted layer-preference sends.
///
/// Mirrors the relay's `LAYER_PREFERENCE_MIN_UPDATE_INTERVAL` (200ms): sending
/// faster than this is wasted work the relay would rate-limit anyway. Kept as a
/// local constant so this pure module has no dependency on the `actix-api`
/// crate; the value is asserted equal to the relay's in an integration check.
pub const LAYER_PREFERENCE_MIN_UPDATE_MS: u64 = 200;

/// Maximum entries per layer-preference packet, mirroring the relay's
/// `LAYER_PREFERENCE_MAX_ENTRIES` (64). We cap before sending so we never rely
/// on the relay's truncation behavior.
pub const LAYER_PREFERENCE_MAX_ENTRIES: usize = 64;

/// Tracks the most recently *sent* desired-layer map and the last send time so
/// we only emit a `LAYER_PREFERENCE` packet when the map genuinely changes and
/// the rate-limit allows it.
///
/// The map is a `BTreeMap<session_id, desired_layer>` so equality and the
/// on-wire entry order are deterministic regardless of the source `HashMap`
/// iteration order.
#[derive(Debug, Default)]
pub(crate) struct LayerPreferenceSender {
    /// Canonical form of the map last written to the wire. `None` means nothing
    /// sent yet on the current connection — the next change sends
    /// unconditionally. Reset to `None` on reconnect (fail-open recovery).
    last_sent: Option<BTreeMap<PrefKey, u32>>,
    /// Timestamp (ms) of the last accepted send, for the rate limiter. `None`
    /// until the first send.
    last_sent_ms: Option<u64>,
}

impl LayerPreferenceSender {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Canonicalize a desired-layer map: sort by (session_id, media_kind)
    /// (BTreeMap) and cap to [`LAYER_PREFERENCE_MAX_ENTRIES`]. When over the cap
    /// we drop the highest keys deterministically (stable, not
    /// bandwidth-meaningful — the relay would otherwise truncate arbitrarily).
    fn canonicalize(map: &std::collections::HashMap<PrefKey, u32>) -> BTreeMap<PrefKey, u32> {
        let mut sorted: BTreeMap<PrefKey, u32> = map.iter().map(|(&k, &v)| (k, v)).collect();
        while sorted.len() > LAYER_PREFERENCE_MAX_ENTRIES {
            if let Some((&k, _)) = sorted.iter().next_back() {
                sorted.remove(&k);
            } else {
                break;
            }
        }
        sorted
    }

    /// Decide whether to emit a `LAYER_PREFERENCE` packet for `desired`.
    ///
    /// Returns `Some(entries)` (canonical, capped `(session_id, media_kind,
    /// desired_layer)` tuples) to put on the wire, or `None` when there is
    /// nothing to send because either:
    ///   * the desired map is identical to what was last sent (change
    ///     detection — no spam when the choosers are stable), or
    ///   * fewer than [`LAYER_PREFERENCE_MIN_UPDATE_MS`] have elapsed since the
    ///     last accepted send (rate limit).
    ///
    /// On success the new map is promoted to `last_sent` and `now_ms` recorded.
    pub(crate) fn take_if_changed(
        &mut self,
        desired: &std::collections::HashMap<PrefKey, u32>,
        now_ms: u64,
    ) -> Option<Vec<(u64, PrefMediaKind, u32)>> {
        let canonical = Self::canonicalize(desired);

        // M1 (#1079): suppress an EMPTY preference unless we have a non-empty
        // preference at the relay to clear. With simulcast effectively off (or a
        // healthy receiver that constrains nothing), every chooser yields "no
        // preference" so `canonical` is empty; sending an empty LAYER_PREFERENCE
        // on every connect/reconnect is pure control-stream fan-out the relay's
        // fail-open already covers. We still allow exactly ONE empty send when
        // `last_sent` was a non-empty map — that empty packet is the signal to
        // the relay to STOP filtering (go back to forwarding all). `last_sent ==
        // Some(empty)` is treated as "nothing to clear" too.
        let prev_was_nonempty = matches!(self.last_sent.as_ref(), Some(m) if !m.is_empty());
        if canonical.is_empty() && !prev_was_nonempty {
            // Record that our effective state is "no preference" so a subsequent
            // identical empty tick stays suppressed and the rate-limit clock is
            // not advanced by a packet we never send.
            self.last_sent = Some(canonical);
            return None;
        }

        // Change detection: identical map → nothing to do (the common case).
        if self.last_sent.as_ref() == Some(&canonical) {
            return None;
        }

        // Rate limit: respect the relay's minimum interval. A pending change
        // that arrives too soon is simply deferred to a later tick (the choosers
        // re-present the desired map every tick, so no change is lost).
        if let Some(last) = self.last_sent_ms {
            if now_ms.saturating_sub(last) < LAYER_PREFERENCE_MIN_UPDATE_MS {
                return None;
            }
        }

        let entries: Vec<(u64, PrefMediaKind, u32)> = canonical
            .iter()
            .map(|(&(sid, kind), &layer)| (sid, kind, layer))
            .collect();
        self.last_sent = Some(canonical);
        self.last_sent_ms = Some(now_ms);
        Some(entries)
    }

    /// The canonical desired-layer map last written to the wire (issue #1695).
    ///
    /// This is what this client last PUT on the wire as its per-(source,kind)
    /// preference — i.e. the set of layers the relay will EXACT-MATCH forward once it
    /// has recorded them. `None` means nothing sent yet on the current connection
    /// (relay still fails open → forwards every layer). A `(sid,kind)` ABSENT from the
    /// map means "no preference recorded for that source" → the relay forwards ALL its
    /// layers (fail-open). Read by
    /// [`crate::decode::peer_decode_manager::PeerDecodeManager::reconcile_decode_guards_to_wire`]
    /// AFTER `take_if_changed` (an accepted send promotes `last_sent` to the
    /// just-sent map) so the decode guard is reconciled to the layer the relay
    /// will actually forward.
    ///
    /// BOUND (pre-existing, NOT introduced or worsened by #1695): the relay applies
    /// its OWN ~200ms min-interval to recorded preferences and silently DROPS (keeps
    /// its old map) a too-soon update, with no relay→client signal. If a client
    /// preference is dropped by the relay limiter, this `last_sent` (which the
    /// reconcile pins the guard to) can momentarily disagree with the relay's recorded
    /// map until the next changed-and-accepted publish. Reconciling the guard to
    /// `last_sent` makes guard == client-last-sent — the best the client can do
    /// without a relay ack, and what the guard already tracked before #1695.
    pub(crate) fn last_sent(&self) -> Option<&BTreeMap<PrefKey, u32>> {
        self.last_sent.as_ref()
    }

    /// Forget what was last sent so the next change re-sends unconditionally.
    /// Call on (re)connect: the relay allocated a fresh empty preference map for
    /// the new session_id, so we must repopulate it. Also clears the rate-limit
    /// clock so the recovery send is not delayed.
    pub(crate) fn reset_for_reconnect(&mut self) {
        self.last_sent = None;
        self.last_sent_ms = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const V: PrefMediaKind = PrefMediaKind::Video;
    const S: PrefMediaKind = PrefMediaKind::Screen;
    const A: PrefMediaKind = PrefMediaKind::Audio;

    /// Build a desired map of VIDEO-kind entries (the common case; per-kind
    /// behavior has its own test).
    fn map(pairs: &[(u64, u32)]) -> HashMap<PrefKey, u32> {
        pairs.iter().map(|&(s, l)| ((s, V), l)).collect()
    }

    /// Build a desired map of explicit (session, kind, layer) entries.
    fn kmap(pairs: &[(u64, PrefMediaKind, u32)]) -> HashMap<PrefKey, u32> {
        pairs.iter().map(|&(s, k, l)| ((s, k), l)).collect()
    }

    #[test]
    fn first_send_emits_entries() {
        let mut s = LayerPreferenceSender::new();
        let out = s.take_if_changed(&map(&[(10, 0), (20, 2)]), 1000);
        assert_eq!(
            out,
            Some(vec![(10, V, 0), (20, V, 2)]),
            "sorted by session_id"
        );
    }

    #[test]
    fn unchanged_map_does_not_resend() {
        let mut s = LayerPreferenceSender::new();
        assert!(s.take_if_changed(&map(&[(10, 1)]), 1000).is_some());
        // Same map later (past the rate-limit) → no packet.
        assert_eq!(s.take_if_changed(&map(&[(10, 1)]), 5000), None);
    }

    #[test]
    fn change_detection_is_order_independent() {
        let mut s = LayerPreferenceSender::new();
        assert!(s.take_if_changed(&map(&[(1, 0), (2, 1)]), 1000).is_some());
        // Same members inserted in different order → not a change.
        assert_eq!(s.take_if_changed(&map(&[(2, 1), (1, 0)]), 5000), None);
    }

    #[test]
    fn layer_value_change_resends() {
        let mut s = LayerPreferenceSender::new();
        assert!(s.take_if_changed(&map(&[(7, 0)]), 1000).is_some());
        // Same source, different desired layer → a real change.
        assert_eq!(
            s.take_if_changed(&map(&[(7, 2)]), 5000),
            Some(vec![(7, V, 2)])
        );
    }

    #[test]
    fn per_media_kind_entries_are_addressed_independently() {
        // Phase 3: camera VIDEO and SCREEN of the SAME source are distinct
        // entries; changing one is a change, and both ride the same packet.
        let mut s = LayerPreferenceSender::new();
        let out = s
            .take_if_changed(&kmap(&[(9, V, 2), (9, S, 0), (9, A, 1)]), 1000)
            .expect("first send");
        // Sorted by (session, kind): Video(1) < Audio(2) < Screen(3).
        assert_eq!(out, vec![(9, V, 2), (9, A, 1), (9, S, 0)]);
        // Changing ONLY the screen layer is a change; video/audio unchanged.
        assert_eq!(
            s.take_if_changed(&kmap(&[(9, V, 2), (9, S, 1), (9, A, 1)]), 5000),
            Some(vec![(9, V, 2), (9, A, 1), (9, S, 1)]),
            "a screen-only change must re-send with video/audio intact"
        );
    }

    #[test]
    fn rate_limit_blocks_sends_within_min_interval() {
        let mut s = LayerPreferenceSender::new();
        assert!(s.take_if_changed(&map(&[(1, 0)]), 1000).is_some());
        // A genuine change but only 100ms later (< 200ms) → blocked.
        assert_eq!(
            s.take_if_changed(&map(&[(1, 1)]), 1100),
            None,
            "must respect the relay's min update interval"
        );
        // Same change at +200ms → now allowed.
        assert_eq!(
            s.take_if_changed(&map(&[(1, 1)]), 1200),
            Some(vec![(1, V, 1)])
        );
    }

    #[test]
    fn entry_cap_truncates_to_max() {
        let mut s = LayerPreferenceSender::new();
        let mut big: HashMap<PrefKey, u32> = HashMap::new();
        for i in 0..(LAYER_PREFERENCE_MAX_ENTRIES as u64 + 10) {
            big.insert((i, V), 1);
        }
        let out = s.take_if_changed(&big, 1000).expect("first send");
        assert_eq!(
            out.len(),
            LAYER_PREFERENCE_MAX_ENTRIES,
            "capped to relay max"
        );
        // Deterministic: keeps the lowest session_ids.
        assert_eq!(out.first().map(|e| e.0), Some(0));
        assert_eq!(
            out.last().map(|e| e.0),
            Some(LAYER_PREFERENCE_MAX_ENTRIES as u64 - 1)
        );
    }

    #[test]
    fn base_layer_zero_is_an_explicit_entry_not_omission() {
        // desired_layer = 0 must be SENT as an entry (base-only), distinct from
        // omitting the source (which would mean "forward all" at the relay).
        let mut s = LayerPreferenceSender::new();
        let out = s.take_if_changed(&map(&[(42, 0)]), 1000);
        assert_eq!(
            out,
            Some(vec![(42, V, 0)]),
            "layer 0 is base-only, an explicit entry"
        );
    }

    #[test]
    fn empty_map_with_no_prior_preference_is_suppressed() {
        // M1 (#1079): nothing to constrain (no chooser wants a preference) and
        // nothing previously filtered at the relay → emit NOTHING. The relay's
        // fail-open already forwards all layers, so an empty packet would be pure
        // control-stream fan-out (the bug: one such packet per connect/reconnect).
        let mut s = LayerPreferenceSender::new();
        assert_eq!(
            s.take_if_changed(&HashMap::new(), 1000),
            None,
            "empty desired with no prior preference must not emit a packet"
        );
        // A second empty tick stays suppressed (no spam, rate-limit clock unused).
        assert_eq!(s.take_if_changed(&HashMap::new(), 1100), None);
        // Then a real constraining map IS a change and sends.
        assert_eq!(
            s.take_if_changed(&map(&[(1, 1)]), 5000),
            Some(vec![(1, V, 1)])
        );
    }

    #[test]
    fn empty_map_after_nonempty_sends_one_clear_then_suppresses() {
        // M1 (#1079): an empty map IS sent exactly once when it clears a prior
        // non-empty preference — that empty packet tells the relay to stop
        // filtering (forward all again). Subsequent empty ticks are suppressed.
        let mut s = LayerPreferenceSender::new();
        assert!(s.take_if_changed(&map(&[(7, 0)]), 1000).is_some());
        // Now the constraint lifts (chooser climbed back to top) → one clear send.
        assert_eq!(
            s.take_if_changed(&HashMap::new(), 5000),
            Some(vec![]),
            "empty after a non-empty preference must send one clearing packet"
        );
        // Further empty ticks are no-ops.
        assert_eq!(s.take_if_changed(&HashMap::new(), 9000), None);
    }

    #[test]
    fn reconnect_forces_resend_of_current_map() {
        let mut s = LayerPreferenceSender::new();
        assert!(s.take_if_changed(&map(&[(5, 2)]), 1000).is_some());
        // Same map again → normally no-op...
        assert_eq!(s.take_if_changed(&map(&[(5, 2)]), 5000), None);
        // ...but after reconnect the relay forgot us, so re-send unconditionally
        // and without rate-limit delay.
        s.reset_for_reconnect();
        assert_eq!(
            s.take_if_changed(&map(&[(5, 2)]), 5001),
            Some(vec![(5, V, 2)]),
            "reconnect must re-send the current map to resume filtering"
        );
    }
}
