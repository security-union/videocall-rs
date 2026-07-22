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

//! Pure, host-testable helpers for the meeting-reactions feature (issue #1884).
//!
//! Everything here is deliberately free of `web-sys`/DOM/clock dependencies so
//! it runs under a plain native `#[test]` (no wasm harness): the self-throttle
//! takes `now_ms` as an argument, and the name resolver takes the resolved
//! cache entry + raw in-packet bytes. The UI (dioxus-ui) owns the throttle
//! state and calls [`ReactionSelfThrottle::try_acquire`] on each reaction click
//! before invoking `VideoCallClient::send_reaction`; the client's inbound
//! consume path calls [`resolve_reaction_display_name`].

use std::collections::VecDeque;

/// Minimum spacing between two reactions this client will SEND, in
/// milliseconds. Combined with [`REACTION_SELF_MAX_PER_WINDOW`] this keeps a
/// well-behaved client STRICTLY below the relay's per-sender ceiling
/// (`REACTION_MAX_PER_WINDOW` = 4 / `REACTION_WINDOW_MS` = 1000 in actix-api),
/// so a legitimate user never trips the relay limiter.
pub const REACTION_SELF_MIN_INTERVAL_MS: f64 = 350.0;

/// Maximum reactions this client will SEND within any rolling
/// [`REACTION_SELF_WINDOW_MS`]. `3` is strictly under the relay's cap of 4, so
/// even a user clicking as fast as the min-interval allows stays within budget.
pub const REACTION_SELF_MAX_PER_WINDOW: usize = 3;

/// Rolling window (milliseconds) over which [`REACTION_SELF_MAX_PER_WINDOW`] is
/// enforced. Matches the relay's `REACTION_WINDOW_MS`.
pub const REACTION_SELF_WINDOW_MS: f64 = 1000.0;

/// Maximum number of CHARACTERS of a reaction's cosmetic display name the
/// client will render (and the client will send). Mirrors the proto doc's
/// `<=64` cap. Applied on both the send side (cap the local name) and the
/// consume side (cap the attacker-controlled in-packet name).
pub const REACTION_DISPLAY_NAME_MAX_CHARS: usize = 64;

/// Attribution of last resort when a reaction's sender cannot be named from the
/// display-name cache and carries no usable in-packet fallback.
pub const REACTION_UNKNOWN_SENDER_NAME: &str = "Someone";

/// Client-side send self-throttle for reactions (issue #1884).
///
/// Enforces TWO limits together, both STRICTLY under the relay ceiling so a
/// well-behaved client never trips the relay limiter:
///   1. at least [`REACTION_SELF_MIN_INTERVAL_MS`] between accepted sends, and
///   2. at most [`REACTION_SELF_MAX_PER_WINDOW`] accepted sends in any rolling
///      [`REACTION_SELF_WINDOW_MS`].
///
/// The UI owns one instance and calls [`try_acquire`](Self::try_acquire) on
/// every reaction click; a `false` result is a SILENT no-op click (no packet,
/// no local echo) — the UI still shows the pressed feedback and auto-closes the
/// palette, it just does not send.
#[derive(Debug, Default)]
pub struct ReactionSelfThrottle {
    /// Timestamps (ms) of accepted sends within the current rolling window,
    /// oldest at the front. Bounded by [`REACTION_SELF_MAX_PER_WINDOW`].
    accepted: VecDeque<f64>,
}

impl ReactionSelfThrottle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide whether a reaction send at `now_ms` is allowed, recording it when
    /// so. Returns `true` (and records the send) when BOTH the min-interval and
    /// the rolling-window count allow it; `false` (recording nothing) when
    /// either limit would be exceeded.
    ///
    /// `now_ms` is a monotonic millisecond clock (e.g. `performance.now()` in
    /// the browser); passing it in keeps this deterministically host-testable.
    pub fn try_acquire(&mut self, now_ms: f64) -> bool {
        // Evict timestamps that have aged out of the rolling window.
        while let Some(&front) = self.accepted.front() {
            if now_ms - front >= REACTION_SELF_WINDOW_MS {
                self.accepted.pop_front();
            } else {
                break;
            }
        }

        // Min-interval since the most recent accepted send. The min-interval
        // (350ms) is under the window (1000ms), so the most recent send is
        // always still in `accepted` after the eviction above.
        if let Some(&last) = self.accepted.back() {
            if now_ms - last < REACTION_SELF_MIN_INTERVAL_MS {
                return false;
            }
        }

        // Rolling-window count cap: a SECONDARY hard bound guaranteeing at most
        // REACTION_SELF_MAX_PER_WINDOW accepted sends in any rolling window,
        // independent of the min-interval. Under the CURRENT constants it is
        // redundant — REACTION_SELF_MIN_INTERVAL_MS * REACTION_SELF_MAX_PER_WINDOW
        // (350 * 3 = 1050ms) exceeds REACTION_SELF_WINDOW_MS (1000ms), so the
        // oldest accepted send always ages out of the window before the count
        // could reach the cap while the min-interval check passes. It is kept as
        // defense-in-depth: if the min-interval is ever lowered, this cap alone
        // still holds the client STRICTLY under the relay's per-sender budget.
        if self.accepted.len() >= REACTION_SELF_MAX_PER_WINDOW {
            return false;
        }

        self.accepted.push_back(now_ms);
        true
    }
}

/// Sanitize an attacker-controlled in-packet reaction display name into a
/// renderable string, or `None` if nothing renderable remains.
///
/// The bytes come from a peer's `ReactionPacket.display_name`, which the proto
/// doc marks COSMETIC and attacker-controlled. We decode lossily, strip ALL
/// control characters (C0/C1, DEL, newlines, tabs — which could spoof layout or
/// smuggle terminal/log control sequences), trim surrounding whitespace, and
/// cap to [`REACTION_DISPLAY_NAME_MAX_CHARS`] characters. HTML-escaping is the
/// renderer's job (Dioxus escapes text by default); this is defense-in-depth on
/// top of that, and a hard length bound against bloat.
pub fn sanitize_reaction_display_name(raw: &[u8]) -> Option<String> {
    let decoded = String::from_utf8_lossy(raw);
    let cleaned: String = decoded.chars().filter(|c| !c.is_control()).collect();
    let capped: String = cleaned
        .trim()
        .chars()
        .take(REACTION_DISPLAY_NAME_MAX_CHARS)
        .collect();
    if capped.is_empty() {
        None
    } else {
        Some(capped)
    }
}

/// Resolve WHO reacted into a display string (issue #1884).
///
/// Attribution anchors on the relay-stamped envelope `session_id`, resolved via
/// the client's display-name cache (`cached`). The in-packet `display_name`
/// (`in_packet`) is a COSMETIC fallback only — never identity/authorization —
/// for the pre-join cache-race window where the cache has no entry yet. Order:
///   1. the cached name (trimmed, if non-empty) — authoritative;
///   2. else the sanitized in-packet name — cosmetic fallback;
///   3. else [`REACTION_UNKNOWN_SENDER_NAME`].
pub fn resolve_reaction_display_name(cached: Option<String>, in_packet: &[u8]) -> String {
    if let Some(name) = cached {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Some(name) = sanitize_reaction_display_name(in_packet) {
        return name;
    }
    REACTION_UNKNOWN_SENDER_NAME.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // =====================================================================
    // Self-throttle math (#1884)
    // =====================================================================

    #[test]
    fn throttle_denies_a_second_send_inside_the_min_interval() {
        // Two clicks 100ms apart: the second is under the 350ms min-interval and
        // must be a no-op. ADVERSARIAL: remove the min-interval check and the
        // second acquire would return true.
        let mut t = ReactionSelfThrottle::new();
        assert!(t.try_acquire(0.0), "first send is always allowed");
        assert!(
            !t.try_acquire(100.0),
            "a send 100ms after the last (< {REACTION_SELF_MIN_INTERVAL_MS}ms) must be denied"
        );
    }

    #[test]
    fn throttle_allows_sends_spaced_at_the_min_interval() {
        // Sends spaced exactly at the min-interval are allowed up to the
        // rolling-window cap.
        let mut t = ReactionSelfThrottle::new();
        assert!(t.try_acquire(0.0));
        assert!(
            t.try_acquire(REACTION_SELF_MIN_INTERVAL_MS),
            "a send exactly {REACTION_SELF_MIN_INTERVAL_MS}ms after the last must be allowed"
        );
    }

    #[test]
    fn throttle_count_cap_denies_a_full_window_even_when_min_interval_clears() {
        // The count cap (<= REACTION_SELF_MAX_PER_WINDOW per rolling window) is a
        // SECONDARY hard bound beneath the min-interval. Under the current
        // constants it is redundant (REACTION_SELF_MIN_INTERVAL_MS *
        // REACTION_SELF_MAX_PER_WINDOW = 350 * 3 = 1050ms > REACTION_SELF_WINDOW_MS
        // = 1000ms), so a window holding MAX sends whose newest STILL clears the
        // min-interval is not reachable through `try_acquire` — `try_acquire`
        // spaces accepted sends >= the min-interval, which ages the oldest out
        // before the count can reach the cap. We construct that state DIRECTLY to
        // prove the cap is live code (it would bind if the min-interval were ever
        // lowered), not a phantom layer that the min-interval always pre-empts.
        //
        // ADVERSARIAL (mutation): delete the `self.accepted.len() >=
        // REACTION_SELF_MAX_PER_WINDOW` branch and this send is admitted (the
        // min-interval already cleared) -> the assert fails.
        let mut t = ReactionSelfThrottle::new();
        let now = 10_000.0;
        // Fill the rolling window with MAX sends whose most-recent timestamp is
        // exactly REACTION_SELF_MIN_INTERVAL_MS before `now`: all are inside the
        // window (the min-interval is < the window), and the min-interval check
        // passes (now - (now - MIN) = MIN is not < MIN), so ONLY the count cap
        // can deny the next send.
        for _ in 0..REACTION_SELF_MAX_PER_WINDOW {
            t.accepted.push_back(now - REACTION_SELF_MIN_INTERVAL_MS);
        }
        assert!(
            !t.try_acquire(now),
            "with the rolling window already holding REACTION_SELF_MAX_PER_WINDOW sends \
             (min-interval cleared), the count cap must deny the next send"
        );
    }

    #[test]
    fn throttle_window_slides_to_admit_after_oldest_ages_out() {
        // After the window fills (t=0,350,700), a send at t=1050 is > 1000ms
        // after the oldest (t=0), which ages out, dropping the count to 2 — and
        // it is >350ms after the last (t=700) — so it is admitted. This is the
        // sustainable ~3/sec steady state, strictly under the relay's 4/sec.
        //
        // ADVERSARIAL: remove the window eviction and the count would stay at 3
        // forever, denying this send.
        let mut t = ReactionSelfThrottle::new();
        assert!(t.try_acquire(0.0));
        assert!(t.try_acquire(350.0));
        assert!(t.try_acquire(700.0));
        assert!(
            t.try_acquire(1050.0),
            "once the oldest send ages out of the rolling window, a new send is admitted"
        );
    }

    #[test]
    fn throttle_never_exceeds_relay_budget_under_hammering() {
        // Simulate a user hammering the button every 10ms for 3 seconds and
        // assert the accepted rate never exceeds the relay's per-sender budget
        // (REACTION_MAX_PER_WINDOW = 4) in ANY 1000ms window. This is the
        // safety property the self-throttle exists to guarantee.
        const RELAY_MAX_PER_WINDOW: usize = 4;
        let mut t = ReactionSelfThrottle::new();
        let mut accepted_times: Vec<f64> = Vec::new();
        let mut now = 0.0f64;
        while now <= 3000.0 {
            if t.try_acquire(now) {
                accepted_times.push(now);
            }
            now += 10.0;
        }
        // For every accepted send, at most RELAY_MAX_PER_WINDOW accepted sends
        // fall in the [t-1000, t] window.
        for &t0 in &accepted_times {
            let in_window = accepted_times
                .iter()
                .filter(|&&x| x > t0 - REACTION_SELF_WINDOW_MS && x <= t0)
                .count();
            assert!(
                in_window <= RELAY_MAX_PER_WINDOW,
                "accepted {in_window} sends in a 1000ms window ending at {t0}ms; must stay \
                 within the relay budget of {RELAY_MAX_PER_WINDOW}"
            );
        }
    }

    // =====================================================================
    // Name resolution (#1884)
    // =====================================================================

    #[test]
    fn resolver_prefers_the_cached_name() {
        // The authoritative path: a cache hit wins over the in-packet name, so a
        // forged in-packet name can never override the real (relay-attributed)
        // identity.
        let name = resolve_reaction_display_name(Some("Alice".to_string()), b"EVIL SPOOF");
        assert_eq!(
            name, "Alice",
            "a cached name must win over the in-packet name"
        );
    }

    #[test]
    fn resolver_falls_back_to_sanitized_in_packet_name() {
        // No cache entry (pre-join cache race): use the sanitized in-packet name.
        let name = resolve_reaction_display_name(None, b"Bob");
        assert_eq!(name, "Bob");
    }

    #[test]
    fn resolver_strips_control_chars_and_caps_length_of_in_packet_name() {
        // The in-packet name is attacker-controlled: control chars are stripped
        // and the length is capped even on the fallback path.
        let mut raw = b"ab\ncd\t".to_vec(); // control chars interspersed
        raw.extend(std::iter::repeat_n(b'x', 200)); // way over the cap
        let name = resolve_reaction_display_name(None, &raw);
        assert!(
            !name.contains('\n') && !name.contains('\t'),
            "control characters must be stripped from the in-packet name"
        );
        assert_eq!(
            name.chars().count(),
            REACTION_DISPLAY_NAME_MAX_CHARS,
            "the in-packet name must be capped at REACTION_DISPLAY_NAME_MAX_CHARS characters"
        );
    }

    #[test]
    fn resolver_uses_someone_when_nothing_usable() {
        // Empty cache AND empty/blank in-packet name → the last-resort label.
        assert_eq!(
            resolve_reaction_display_name(None, b""),
            REACTION_UNKNOWN_SENDER_NAME
        );
        // A cached-but-blank name must NOT be used; it falls through to the
        // in-packet name, then to "Someone".
        assert_eq!(
            resolve_reaction_display_name(Some("   ".to_string()), b"   \n\t"),
            REACTION_UNKNOWN_SENDER_NAME,
            "a whitespace-only cache entry and a control/whitespace-only in-packet name \
             must fall through to the last-resort label"
        );
    }

    #[test]
    fn sanitizer_returns_none_for_empty_after_cleaning() {
        assert_eq!(sanitize_reaction_display_name(b""), None);
        assert_eq!(sanitize_reaction_display_name(b"\n\t\r"), None);
        assert_eq!(
            sanitize_reaction_display_name(b"  Carol  "),
            Some("Carol".to_string())
        );
    }
}
