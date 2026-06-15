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

/// Error bucket for classifying `VideoEncoder.encode()` failures.
///
/// Used by both `CameraEncoder` and `ScreenEncoder` to increment the
/// appropriate observability counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeErrorBucket {
    /// Codec was closed or in an invalid state (e.g. "closed codec", "InvalidStateError").
    ClosedCodec,
    /// VPX memory allocation failure (e.g. "Memory allocation error",
    /// "Unable to find free frame buffer").
    VpxMemAlloc,
    /// Any other unrecognised error message.
    Generic,
}

/// Classify an `encode_with_options` error message into an observability bucket.
///
/// The classification uses substring matching against known browser error
/// strings.  Unknown messages fall through to [`EncodeErrorBucket::Generic`].
pub fn classify_encode_error(msg: &str) -> EncodeErrorBucket {
    if msg.contains("closed codec") || msg.contains("InvalidStateError") {
        EncodeErrorBucket::ClosedCodec
    } else if msg.contains("Memory allocation error")
        || msg.contains("Unable to find free frame buffer")
    {
        EncodeErrorBucket::VpxMemAlloc
    } else {
        EncodeErrorBucket::Generic
    }
}

/// Reason an encoder auto-restart cycle (`restart_count += 1`) was triggered
/// (issue #527). Mapped to the `reason` label of the
/// `videocall_encoder_restart_total{kind, reason}` Prometheus counter.
///
/// This reuses the SAME substring classification as [`classify_encode_error`]
/// (so a `ClosedCodec`/`VpxMemAlloc` encode failure that forces a restart is
/// labelled consistently with its error counter), and adds two restart-specific
/// reasons that have no encode-error analogue:
///   * [`RestartReason::Configure`] — a fatal `VideoEncoder.configure()` failure
///     (cold-start build, lazy rung build, or a per-frame reconfigure) OR an
///     encoder observed in `CodecState::Closed` at a reconfigure/guard point.
///     These are detected by the call site, not by message substring.
///   * [`RestartReason::Other`] — restarts NOT caused by a codec/memory/configure
///     fault: getUserMedia / getDisplayMedia / device-enumeration failures and
///     any unclassified error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartReason {
    /// Codec closed or in an invalid state.
    ClosedCodec,
    /// VPX / WebCodecs memory allocation failure.
    Memory,
    /// Fatal `configure()` failure or an encoder found already-closed at a
    /// reconfigure/guard point.
    Configure,
    /// Media-acquisition or otherwise unclassified restart trigger.
    Other,
}

impl RestartReason {
    /// Low-cardinality Prometheus `reason` label value. Stable wire contract —
    /// dashboards/alerts pivot on these exact strings; do not rename without a
    /// coordinated dashboard update.
    pub fn as_label(self) -> &'static str {
        match self {
            RestartReason::ClosedCodec => "closed_codec",
            RestartReason::Memory => "memory",
            RestartReason::Configure => "configure",
            RestartReason::Other => "other",
        }
    }
}

/// Map an error message to the restart reason for a restart triggered by an
/// ENCODE or BUILD error whose message is available. A closed-codec or memory
/// message maps to the matching reason; anything else (including media-access
/// failures) maps to [`RestartReason::Other`]. Call sites that restart because
/// of a `configure()` failure or an observed `CodecState::Closed` should use
/// [`RestartReason::Configure`] / [`RestartReason::ClosedCodec`] directly rather
/// than going through this — the trigger there is structural, not a message.
pub fn restart_reason_from_message(msg: &str) -> RestartReason {
    match classify_encode_error(msg) {
        EncodeErrorBucket::ClosedCodec => RestartReason::ClosedCodec,
        EncodeErrorBucket::VpxMemAlloc => RestartReason::Memory,
        EncodeErrorBucket::Generic => RestartReason::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_closed_codec_error() {
        assert_eq!(
            classify_encode_error("DOMException: closed codec"),
            EncodeErrorBucket::ClosedCodec
        );
        assert_eq!(
            classify_encode_error("InvalidStateError: The codec is not configured"),
            EncodeErrorBucket::ClosedCodec
        );
    }

    #[test]
    fn classify_vpx_mem_error() {
        assert_eq!(
            classify_encode_error("Memory allocation error during encoding"),
            EncodeErrorBucket::VpxMemAlloc
        );
        assert_eq!(
            classify_encode_error("Unable to find free frame buffer"),
            EncodeErrorBucket::VpxMemAlloc
        );
    }

    #[test]
    fn classify_generic_error() {
        assert_eq!(
            classify_encode_error("some unknown browser error XYZ"),
            EncodeErrorBucket::Generic
        );
        assert_eq!(classify_encode_error(""), EncodeErrorBucket::Generic);
    }

    #[test]
    fn restart_reason_labels_are_the_frozen_prometheus_values() {
        // Issue #527: these label strings are the `reason` label of
        // videocall_encoder_restart_total. Dashboards/alerts pivot on them, so
        // pin the exact wire values (this fails if any label is renamed).
        assert_eq!(RestartReason::ClosedCodec.as_label(), "closed_codec");
        assert_eq!(RestartReason::Memory.as_label(), "memory");
        assert_eq!(RestartReason::Configure.as_label(), "configure");
        assert_eq!(RestartReason::Other.as_label(), "other");
    }

    #[test]
    fn restart_reason_from_message_matches_the_error_classifier() {
        // Closed-codec messages → closed_codec (consistent with the error bucket).
        assert_eq!(
            restart_reason_from_message("DOMException: closed codec"),
            RestartReason::ClosedCodec
        );
        assert_eq!(
            restart_reason_from_message("InvalidStateError: not configured"),
            RestartReason::ClosedCodec
        );
        // Memory messages → memory.
        assert_eq!(
            restart_reason_from_message("Memory allocation error during encoding"),
            RestartReason::Memory
        );
        assert_eq!(
            restart_reason_from_message("Unable to find free frame buffer"),
            RestartReason::Memory
        );
        // Anything else (incl. media-access errors) → other, NOT configure:
        // configure restarts are reported structurally by the call site.
        assert_eq!(
            restart_reason_from_message("Camera access failed: NotAllowedError"),
            RestartReason::Other
        );
        assert_eq!(restart_reason_from_message(""), RestartReason::Other);
    }
}
