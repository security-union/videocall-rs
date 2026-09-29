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

const _: () = assert!(
    videocall_transport::worker_proto::STALE_DELIVERY_CEILING_MS
        == videocall_codecs::jitter_buffer::MAX_PLAYOUT_AGE_MS,
    "videocall-transport's stale-frame ceiling drifted from videocall-codecs (#2728)"
);
const _: () = assert!(
    videocall_transport::worker_proto::SCREEN_STALE_DELIVERY_CEILING_MS
        == videocall_aq::constants::SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS,
    "videocall-transport's screen silence ceiling drifted from videocall-aq (#2728)"
);

// This submodule implements our WebMedia trait for WebTransportTask
//
// Sets up all the stream handling to support the callbacks on_connected, on_connection_lost, and
// on_inbound_media
//
use super::connection_lost_reason::ConnectionLostReason;
use super::url_log::strip_query_for_log;
use super::webmedia::{ConnectOptions, MediaStreamKey, WebMedia};
use log::debug;
use log::info;
use videocall_transport::inbound::{emit_packet, InboundFrame, InboundLane, MessageType};
use videocall_transport::webtransport::{
    FrameDropMeta, WebTransportCloseInfo, WebTransportService, WebTransportStatus, WebTransportTask,
};
use videocall_types::wt_close::WT_CLOSE_CODE_DOWNLINK_UNRECOVERABLE;
use videocall_types::Callback;

/// Map a server close code on an established session to a loss reason. Every
/// other code, `0` included, keeps today's generic session-dropped path.
pub(super) fn lost_reason_for_close_code(info: &WebTransportCloseInfo) -> ConnectionLostReason {
    let message = format!(
        "server closed the session: code {} reason {:?}",
        info.code, info.reason
    );
    if info.code == WT_CLOSE_CODE_DOWNLINK_UNRECOVERABLE {
        ConnectionLostReason::DownlinkUnrecoverable(message)
    } else {
        ConnectionLostReason::SessionDropped(message)
    }
}

impl WebMedia<WebTransportTask> for WebTransportTask {
    fn connect(options: ConnectOptions) -> anyhow::Result<WebTransportTask> {
        // Phase 3c: the netsim shim is installed once-per-tab from
        // `?netsim=<profile>` by `Connection::connect` via
        // `super::netsim_url::try_install_from_url`. We deliberately do
        // **not** install or clear the hook here — doing so would
        // overwrite the URL-driven slot with the unused, hardcoded
        // `ConnectOptions::netsim_hook` placeholder and silently
        // disable the simulator on every reconnect. See
        // `connection/netsim_hook.rs` for the full design (Option A —
        // thread-local hook + re-entrancy flag).

        let on_frame = {
            let callback = options.on_inbound_media.clone();
            Callback::from(move |frame: InboundFrame| {
                let message_type = match frame.lane {
                    InboundLane::Datagram => MessageType::Datagram,
                    InboundLane::Reliable => MessageType::UnidirectionalStream,
                };
                emit_packet(
                    frame.bytes,
                    message_type,
                    frame.received_at,
                    callback.clone(),
                )
            })
        };

        let notification = {
            let connected_callback = options.on_connected.clone();
            let connection_lost_callback = options.on_connection_lost.clone();
            Callback::from(move |status| match status {
                WebTransportStatus::Opened => connected_callback.emit(()),
                WebTransportStatus::ClosedBeforeReady(msg) => {
                    connection_lost_callback.emit(ConnectionLostReason::HandshakeFailed(msg));
                }
                WebTransportStatus::ClosedAfterReady(msg) => {
                    connection_lost_callback.emit(ConnectionLostReason::SessionDropped(msg));
                }
                WebTransportStatus::ClosedAfterReadyWithCode(info) => {
                    connection_lost_callback.emit(lost_reason_for_close_code(&info));
                }
                // Legacy variants — these should no longer fire with the updated
                // transport, but keep them as a defensive fallback.
                WebTransportStatus::Closed(e) => {
                    let msg = format!("{e:?}");
                    connection_lost_callback.emit(ConnectionLostReason::SessionDropped(msg));
                }
                WebTransportStatus::Error(e) => {
                    let msg = format!("{e:?}");
                    connection_lost_callback.emit(ConnectionLostReason::SessionDropped(msg));
                }
            })
        };
        info!(
            "WebTransport connecting to {}",
            strip_query_for_log(&options.webtransport_url)
        );
        let task = WebTransportService::connect(&options.webtransport_url, on_frame, notification)?;
        info!("WebTransport connection success");
        Ok(task)
    }

    /// Reliable media-packet send path.
    ///
    /// Phase 2 of the WebTransport freeze fix: every reliable packet rides on
    /// a **persistent** per-media-type QUIC unidirectional stream rather than
    /// opening a fresh stream per packet (~80 streams/sec/sender in the legacy
    /// pattern).  Stream identity is `stream_key.as_u8()`; the server-side
    /// reader at `actix-api/src/webtransport/bridge.rs` reads length-prefixed
    /// frames from each stream in a loop until EOF and routes by the MediaType
    /// inside the encrypted protobuf payload.
    fn send_bytes(&self, bytes: Vec<u8>, stream_key: MediaStreamKey) {
        self.send_bytes_with_drop_meta(bytes, stream_key, None);
    }

    fn send_bytes_with_drop_meta(
        &self,
        bytes: Vec<u8>,
        stream_key: MediaStreamKey,
        meta: Option<FrameDropMeta>,
    ) {
        // Phase 3b: consult the per-tab netsim shim. When the
        // `netsim` feature is off the entire block compiles out and
        // the send path is byte-for-byte identical to pre-3b.
        #[cfg(feature = "netsim")]
        {
            if super::netsim_hook::shape_uplink_reliable(&bytes, stream_key) {
                return;
            }
        }
        WebTransportTask::send_on_persistent_stream(
            self.host.clone(),
            self.persistent_streams.clone(),
            stream_key.as_u8(),
            stream_key.send_order(),
            bytes,
            meta,
        );
    }

    fn send_bytes_datagram(&self, bytes: Vec<u8>) {
        use crate::adaptive_quality_constants::DATAGRAM_MAX_SIZE;

        // Phase 3b: consult the per-tab netsim shim. See
        // `send_bytes` above for the no-feature compile-out.
        #[cfg(feature = "netsim")]
        {
            if super::netsim_hook::shape_uplink_datagram(&bytes) {
                return;
            }
        }

        if bytes.len() <= DATAGRAM_MAX_SIZE {
            // Packet fits within the datagram MTU -- send as unreliable datagram
            // for lower latency and no head-of-line blocking.  Datagrams are
            // on a separate primitive from persistent streams and are NOT
            // length-prefix framed.
            WebTransportTask::send_datagram(self.host.clone(), bytes);
        } else {
            // Packet exceeds datagram size limit (e.g., a keyframe).
            // Fall back to the Control persistent stream so the server's
            // framed reader can receive it as a complete frame without
            // application-layer fragmentation.
            debug!(
                "Packet size {} exceeds datagram MTU {}, falling back to Control persistent stream",
                bytes.len(),
                DATAGRAM_MAX_SIZE
            );
            WebTransportTask::send_on_persistent_stream(
                self.host.clone(),
                self.persistent_streams.clone(),
                MediaStreamKey::Control.as_u8(),
                MediaStreamKey::Control.send_order(),
                bytes,
                None,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_arch = "wasm32")]
    use js_sys::Reflect;

    fn close_info(code: u32, reason: &str) -> WebTransportCloseInfo {
        WebTransportCloseInfo {
            code,
            reason: reason.to_string(),
        }
    }

    #[test]
    fn the_downlink_unrecoverable_code_maps_to_its_own_reason() {
        let relay_reason =
            std::str::from_utf8(videocall_types::wt_close::WT_CLOSE_REASON_DOWNLINK_UNRECOVERABLE)
                .unwrap();
        let reason = lost_reason_for_close_code(&close_info(
            WT_CLOSE_CODE_DOWNLINK_UNRECOVERABLE,
            relay_reason,
        ));
        assert!(
            matches!(reason, ConnectionLostReason::DownlinkUnrecoverable(_)),
            "code {WT_CLOSE_CODE_DOWNLINK_UNRECOVERABLE} must not read as a generic drop"
        );
        assert_eq!(reason.label(), "downlink_unrecoverable");
        assert!(
            reason.message().contains("1001")
                && reason.message().contains("downlink-shed-escalation"),
            "the decision the relay made must survive into the log line: {}",
            reason.message()
        );
    }

    #[test]
    fn a_clean_or_unknown_close_code_stays_on_the_generic_path() {
        for code in [0, 1, 1000, 1002, 4242, u32::MAX] {
            let reason = lost_reason_for_close_code(&close_info(code, "bye"));
            assert!(
                matches!(reason, ConnectionLostReason::SessionDropped(_)),
                "close code {code} is not the downlink-unrecoverable code and must \
                 take the generic session-dropped path"
            );
            assert_eq!(reason.label(), "session_dropped");
        }
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test::wasm_bindgen_test]
    fn a_resolved_close_info_object_yields_its_code_and_reason() {
        let settled = js_sys::Object::new();
        Reflect::set(&settled, &"closeCode".into(), &1001_u32.into()).unwrap();
        Reflect::set(
            &settled,
            &"reason".into(),
            &"downlink-shed-escalation".into(),
        )
        .unwrap();

        assert_eq!(
            videocall_transport::webtransport::read_close_info(&settled),
            Some(close_info(1001, "downlink-shed-escalation")),
        );
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test::wasm_bindgen_test]
    fn a_rejection_yields_no_close_info() {
        let rejected = js_sys::Error::new("network error");
        assert_eq!(
            videocall_transport::webtransport::read_close_info(&rejected),
            None,
            "a WebTransportError has no closeCode and must not be read as a coded close"
        );

        assert_eq!(
            videocall_transport::webtransport::read_close_info(&wasm_bindgen::JsValue::from_str(
                "closed"
            )),
            None,
            "Reflect::get throws on a primitive; that must not panic or fabricate a code"
        );
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test::wasm_bindgen_test]
    fn a_clean_close_parses_and_defaults_its_missing_reason() {
        let settled = js_sys::Object::new();
        Reflect::set(&settled, &"closeCode".into(), &0_u32.into()).unwrap();
        assert_eq!(
            videocall_transport::webtransport::read_close_info(&settled),
            Some(close_info(0, "")),
        );
    }
}
