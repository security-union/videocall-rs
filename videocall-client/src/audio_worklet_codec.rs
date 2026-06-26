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

#[allow(unused_imports)]
use gloo_utils::format::JsValueSerdeExt;
use js_sys::{Array, Function};
use serde::Serialize;
use std::{cell::RefCell, rc::Rc};
use wasm_bindgen_futures::JsFuture;
use web_sys::{AudioContext, AudioWorkletNode, AudioWorkletNodeOptions, MessagePort};

use wasm_bindgen::prelude::*;

#[derive(Serialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct EncoderInitOptions {
    // Sample rate of input
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_sample_rate: Option<u32>,

    // 2048 = Voice (Lower fidelity)
    // 2049 = Full Band Audio (Highest fidelity)
    // 2051 = Restricted Low Delay (Lowest latency)
    // Default: 2049
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_application: Option<u32>,

    // Specified in ms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_frame_size: Option<u32>,

    // Desired encoding sample rate. Audio will be resampled
    // Default: 48000
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_sample_rate: Option<u32>,

    // Tradeoff latency with overhead. Default: 40
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_frames_per_page: Option<u32>,

    // Default: 1
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number_of_channels: Option<u32>,

    // Value between 0 and 10 inclusive. 10 being highest
    // quality. Default: 10
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resample_quality: Option<u32>,

    // Default: 50000
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_bit_rate: Option<u32>,

    // Enable Opus in-band Forward Error Correction (FEC).
    // When enabled, the encoder embeds redundant data from the previous frame
    // into the current frame, allowing the decoder to partially recover from
    // single-packet losses without retransmission. Adds ~10-20% overhead.
    //
    // Serializes as `encoderFec`. The AudioWorklet (encoderWorker.min.js)
    // honors this AT ENCODER INIT ONLY: on init it calls OPUS_SET_INBAND_FEC
    // (ctl 4012) when this is true. When absent/false, the worklet makes no ctl
    // call and libopus keeps FEC OFF.
    //
    // RUNTIME CAVEAT — inband FEC does NOT engage on a mid-call AQ tier drop.
    // The mic encoder inits at the healthy top tier (AUDIO_QUALITY_TIERS[0],
    // enable_fec=false), and the worklet has no live reconfig path: a later AQ
    // tier change only writes shared atomics, it never re-applies the ctl to the
    // running encoder. So flipping a degraded tier's enable_fec to true does not
    // turn inband FEC on for an already-initialized encoder. Wiring this flag
    // through is the prerequisite; runtime FEC engagement (a live ctl-reconfig
    // message) is tracked as a follow-up (see #1567). See adaptive_quality_constants.rs for tier definitions that
    // set enable_fec per quality level.
    //
    // Default: false (no FEC)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_fec: Option<bool>,

    // Enable Opus Discontinuous Transmission (DTX).
    // When enabled, the encoder detects silence and sends comfort noise
    // parameters (~1-2 packets/sec) instead of full frames (~50 packets/sec),
    // reducing audio bandwidth by 80-90% during silence periods.
    //
    // Serializes as `encoderDtx`. The AudioWorklet (encoderWorker.min.js)
    // honors this at encoder init: it calls OPUS_SET_DTX (ctl 4016) when this is
    // true. When absent/false, the worklet makes no ctl call and libopus keeps
    // DTX OFF.
    //
    // Unlike FEC, DTX ENGAGES TODAY: every audio tier sets enable_dtx=true
    // (including the top tier the mic inits at), so DTX is applied at init and
    // is active for the whole call — no live reconfig is needed for it to work.
    // See adaptive_quality_constants.rs for tier definitions that set enable_dtx
    // per quality level.
    //
    // Default: false (no DTX)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_dtx: Option<bool>,

    // Expected packet-loss percentage (0-100) communicated to the Opus encoder.
    // libopus uses this hint to scale how much redundant FEC data it embeds:
    // a higher value makes FEC more aggressive (better concealment, more
    // overhead). This is only meaningful together with `encoder_fec`.
    //
    // Serializes as `encoderPacketLossPerc`. The AudioWorklet
    // (encoderWorker.min.js) honors this AT ENCODER INIT ONLY: on init it calls
    // OPUS_SET_PACKET_LOSS_PERC (ctl 4014) with this value when present and
    // non-zero. When absent/zero, the worklet makes no ctl call and libopus
    // keeps its default (0%).
    //
    // RUNTIME CAVEAT — same as `encoder_fec`: the mic inits at the top tier
    // (packet_loss_perc=0) and there is no live worklet reconfig, so a mid-call
    // AQ tier drop does not push a higher loss hint to the running encoder. This
    // value only takes effect at init; live re-application is part of the FEC
    // runtime-engagement follow-up.
    //
    // Default: None (libopus default of 0%)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_packet_loss_perc: Option<u32>,
}

#[derive(Serialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct DecoderInitOptions {
    // Desired decoder sample rate.
    // Default: 48000
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoder_sample_rate: Option<u32>,

    // Desired output sample rate. Audio will be resampled
    // Default: 48000
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_buffer_sample_rate: Option<u32>,

    // Value between 0 and 10 inclusive. 10 being highest quality.
    // Default: 0
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resample_quality: Option<u32>,

    // Default: 1
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number_of_channels: Option<u32>,
}

#[derive(Serialize, Debug)]
#[serde(tag = "command", rename_all = "camelCase")]
pub enum CodecMessages<Options: Serialize> {
    Init {
        #[serde(flatten)]
        options: Option<Options>,
    },
    Start,
    Stop,
    Flush,
    Close,
    Done,
    Decode {
        pages: Vec<u8>,
    },
    /// Live Opus ctl re-application on the RUNNING encoder (issues #1567, #1578,
    /// #1398), with NO destroy/create — so a mid-call AQ audio-tier drop can
    /// actually engage inband FEC AND (in single-layer mode) lower the running
    /// Opus stream's bitrate without an audio gap or buffer re-alloc storm.
    ///
    /// Serializes (via `tag = "command"`, `rename_all = "camelCase"`) to
    /// `{"command":"reconfigOpus","fec":<bool>,"packetLossPerc":<u32>}` when
    /// `bit_rate` is `None`, and adds `"bitRate":<u32>` when it is `Some(bps)`.
    /// The AudioWorklet (`encoderWorker.min.js`) handles the `reconfigOpus`
    /// command by calling, on the live `OggOpusEncoder`,
    /// `setOpusControl(4012, fec?1:0)` (OPUS_SET_INBAND_FEC),
    /// `setOpusControl(4014, packetLossPerc|0)` (OPUS_SET_PACKET_LOSS_PERC), and
    /// — guarded by `if (data.bitRate)` —
    /// `setOpusControl(4002, bitRate)` (OPUS_SET_BITRATE). The ctl 4002 path is
    /// the SAME control libopus is initialized with (the worklet's init code
    /// calls `setOpusControl(4002, this.config.encoderBitRate)`), so re-applying
    /// it live just sets a new target bitrate on the running encoder.
    ///
    /// `bit_rate` (`#[serde(skip_serializing_if = "Option::is_none")]`):
    ///   * `None` OMITS the `bitRate` key entirely, so a FEC-only reconfig is
    ///     BYTE-IDENTICAL to the pre-#1398 wire shape and the worklet's
    ///     `if (data.bitRate)` guard sees `undefined` → falsy → makes NO ctl 4002
    ///     call (bitrate untouched). This is the multi-layer path: the
    ///     layer-ceiling lever handles audio congestion there, so we never
    ///     double-dip on bitrate.
    ///   * `Some(bps)` emits `bitRate: bps`, making the worklet call
    ///     `setOpusControl(4002, bps)`. This is the SINGLE-LAYER path (#1398):
    ///     a single-encoder publisher has no upper layer to shed, so the only way
    ///     to downshift audio under congestion is to lower the one running Opus
    ///     stream's bitrate live.
    ///
    /// This is the runtime-engagement family of #619/#1568/#621: init still
    /// applies the initial tier's FEC/DTX/loss/bitrate (via
    /// [`EncoderInitOptions`]); this re-applies FEC + loss-% (always) and bitrate
    /// (single-layer only) when the tier/congestion-floor later changes. DTX is
    /// intentionally NOT touched here — every tier inits with DTX on, so it is
    /// already live for the whole call and needs no runtime toggle. The FEC ctl
    /// here is the INBAND-FEC ctl, separate from the application-level RED path
    /// (`AUDIO_REDUNDANCY_ENABLED`).
    ///
    /// Safety: if this message is never sent, the worklet's behavior is
    /// byte-identical to today; the worklet's `reconfigOpus` case lives inside
    /// its `if (this.encoder)` guard, so a missing/destroyed encoder is a safe
    /// no-op (not a throw). A zero/absent `bitRate` is likewise a no-op for the
    /// ctl 4002 call (the `if (data.bitRate)` guard).
    ///
    /// The enum-level `rename_all` renames the VARIANT (→ `"reconfigOpus"` tag),
    /// not the inline fields, so the per-variant `rename_all` below is required
    /// to emit `packetLossPerc` and `bitRate` (the keys the worklet reads as
    /// `data.packetLossPerc` / `data.bitRate`). Without it the fields would
    /// serialize as `packet_loss_perc` / `bit_rate` and the worklet would read
    /// `undefined` → `|0` → 0, silently dropping the hint. The serde tests pin
    /// the exact `bitRate` key and the omit-when-`None` contract.
    #[serde(rename_all = "camelCase")]
    ReconfigOpus {
        fec: bool,
        packet_loss_perc: u32,
        /// Target Opus bitrate in BITS PER SECOND for the live ctl 4002
        /// (OPUS_SET_BITRATE). `None` (the multi-layer / FEC-only case) OMITS the
        /// `bitRate` wire key so the reconfig is byte-identical to pre-#1398;
        /// `Some(bps)` (the single-layer congestion downshift, #1398) re-applies
        /// the bitrate to the running encoder.
        #[serde(skip_serializing_if = "Option::is_none")]
        bit_rate: Option<u32>,
    },
}

pub type EncoderMessages = CodecMessages<EncoderInitOptions>;
pub type DecoderMessages = CodecMessages<DecoderInitOptions>;

/// Struct to describe a Audio Encoder or Decoder through the use of an AudioWorklet
#[derive(Clone, Default)]
pub struct AudioWorkletCodec {
    inner: Rc<RefCell<Option<AudioWorkletNode>>>,
}

impl AudioWorkletCodec {
    /// Instantiates a AudioWorkletNode under the provided context using the provided script name
    /// and registered processor name
    pub async fn create_node(
        &self,
        context: &AudioContext,
        script_path: &str,
        name: &str,
        channels: u32,
    ) -> Result<AudioWorkletNode, JsValue> {
        let _ = JsFuture::from(context.audio_worklet()?.add_module(script_path)?).await;

        let options = AudioWorkletNodeOptions::new();
        options.set_number_of_inputs(channels);
        options.set_number_of_outputs(channels);
        options.set_output_channel_count(
            &vec![channels]
                .into_iter()
                .map(JsValue::from)
                .collect::<Array>(),
        );

        let node = AudioWorkletNode::new_with_options(context, name, &options)?;
        let _ = self.inner.replace(Some(node));
        Ok(self.inner.borrow().to_owned().unwrap())
    }

    pub fn is_instantiated(&self) -> bool {
        self.inner.borrow().is_some()
    }

    pub fn start(&self) -> Result<(), JsValue> {
        self.send_message(EncoderMessages::Start)
    }

    pub fn stop(&self) -> Result<(), JsValue> {
        self.send_message(EncoderMessages::Stop)
    }

    pub fn destroy(&self) {
        let _ = self.send_message(EncoderMessages::Flush);
        let _ = self.send_message(EncoderMessages::Done);
        let _ = self.send_message(EncoderMessages::Close);
        let _ = self.inner.borrow_mut().take();
    }

    pub fn set_onmessage(&self, handler: &Function) {
        if let Some(port) = self.get_port() {
            port.set_onmessage(Some(handler));
        }
    }

    pub fn send_message<T: Serialize>(&self, message: T) -> Result<(), JsValue> {
        self.get_port()
            .ok_or(JsValue::from_str("AudioWorkletNode is not instantiated"))
            .and_then(|port| {
                #[allow(deprecated)]
                JsValue::from_serde(&message)
                    .map_err(|e| JsValue::from_str(&e.to_string()))
                    .and_then(|val| port.post_message(&val))
            })
    }

    fn get_port(&self) -> Option<MessagePort> {
        self.inner
            .borrow()
            .as_ref()
            .and_then(|node| node.port().ok())
    }
}

#[cfg(test)]
mod tests {
    //! Host-runnable serde tests (issue #619).
    //!
    //! These pin the JSON contract between the Rust `EncoderInitOptions` and the
    //! keys the AudioWorklet (`dioxus-ui/scripts/encoderWorker.min.js`) reads in
    //! its `if(this.config.encoderFec){this.setOpusControl(4012,1)}` /
    //! `encoderDtx` (4016) / `encoderPacketLossPerc` (4014) ctl gates. If a key
    //! name drifts, the worklet silently stops applying the corresponding Opus
    //! control and packet-loss recovery breaks on the wire — so these assert the
    //! exact camelCase keys, not just "it serializes".
    //!
    //! What this covers: the serialized message shape (keys + values + the
    //! "absent when default" guarantee the worklet's gating relies on).
    //! What it does NOT cover: that libopus actually conceals a dropped packet
    //! end-to-end — that requires a real Opus encode/decode harness in the
    //! browser worklet, which is not host-testable here (see the report / e2e
    //! note). The amplitude/correlation concealment check from the acceptance
    //! criteria lives in codec/browser territory, not this pure-serde layer.

    use super::*;

    fn init_json(options: EncoderInitOptions) -> serde_json::Value {
        serde_json::to_value(EncoderMessages::Init {
            options: Some(options),
        })
        .expect("EncoderInitOptions must serialize")
    }

    #[test]
    fn fec_dtx_loss_serialize_with_exact_worklet_keys() {
        let json = init_json(EncoderInitOptions {
            encoder_fec: Some(true),
            encoder_dtx: Some(true),
            encoder_packet_loss_perc: Some(10),
            ..Default::default()
        });

        // The keys MUST be the camelCase names the worklet ctl-gates read.
        assert_eq!(json["encoderFec"], serde_json::json!(true));
        assert_eq!(json["encoderDtx"], serde_json::json!(true));
        assert_eq!(json["encoderPacketLossPerc"], serde_json::json!(10));
    }

    #[test]
    fn defaults_omit_keys_so_worklet_behavior_is_unchanged() {
        // CRITICAL SAFETY (issue #619): when the encoder requests no FEC/DTX/loss
        // hint, the keys must be ABSENT (not `false`/`0`). The worklet gates on
        // `if(this.config.encoderFec){...}`; an absent key means no ctl call,
        // which is byte-identical to today's behavior and keeps libopus at its
        // default (FEC/DTX off, 0% loss).
        let json = init_json(EncoderInitOptions::default());

        assert!(
            json.get("encoderFec").is_none(),
            "encoderFec must be omitted when unset"
        );
        assert!(
            json.get("encoderDtx").is_none(),
            "encoderDtx must be omitted when unset"
        );
        assert!(
            json.get("encoderPacketLossPerc").is_none(),
            "encoderPacketLossPerc must be omitted when unset"
        );
    }

    #[test]
    fn reconfig_opus_serializes_with_exact_worklet_command_and_keys() {
        // Live ctl-reconfig (issue #1567). The worklet's switch matches on
        // `data.command === "reconfigOpus"` and reads `data.fec` /
        // `data.packetLossPerc`. If the tag or either key name drifts, the
        // worklet silently ignores the message (hits `default:`) and inband FEC
        // never engages at runtime — so pin the EXACT wire shape, not just "it
        // serializes".
        let json = serde_json::to_value(EncoderMessages::ReconfigOpus {
            fec: true,
            packet_loss_perc: 15,
            bit_rate: None,
        })
        .expect("ReconfigOpus must serialize");

        assert_eq!(json["command"], serde_json::json!("reconfigOpus"));
        assert_eq!(json["fec"], serde_json::json!(true));
        assert_eq!(json["packetLossPerc"], serde_json::json!(15));
        // FEC-only reconfig (bit_rate=None) MUST omit the bitRate key so it is
        // byte-identical to the pre-#1398 wire shape and the worklet's
        // `if(data.bitRate)` guard makes no ctl 4002 call.
        assert!(
            json.get("bitRate").is_none(),
            "bitRate must be omitted when bit_rate is None"
        );
    }

    #[test]
    fn reconfig_opus_off_serializes_fec_false() {
        // The recover path turns FEC back OFF; the worklet's `data.fec?1:0`
        // must receive a literal `false` (not an omitted key) so it applies
        // OPUS_SET_INBAND_FEC(0). `fec` is a plain bool field (no skip), so it
        // always serializes.
        let json = serde_json::to_value(EncoderMessages::ReconfigOpus {
            fec: false,
            packet_loss_perc: 0,
            bit_rate: None,
        })
        .expect("ReconfigOpus must serialize");

        assert_eq!(json["fec"], serde_json::json!(false));
        assert_eq!(json["packetLossPerc"], serde_json::json!(0));
        assert!(
            json.get("bitRate").is_none(),
            "bitRate must be omitted when bit_rate is None"
        );
    }

    #[test]
    fn reconfig_opus_bitrate_omitted_when_none_emitted_when_some() {
        // Single-layer congestion downshift (issue #1398). The worklet reads
        // `data.bitRate` (guarded by `if(data.bitRate)`) and calls
        // `setOpusControl(4002, data.bitRate)` (OPUS_SET_BITRATE). Pin the EXACT
        // `bitRate` key + value AND the omit-when-None contract: if the key drifts
        // (e.g. to `bit_rate`) the worklet reads `undefined` → falsy → never
        // lowers the bitrate, so a single-layer publisher can't downshift audio
        // at all. This test FAILS if the field's `rename_all`/key drifts or if
        // `skip_serializing_if` is dropped.
        let omitted = serde_json::to_value(EncoderMessages::ReconfigOpus {
            fec: true,
            packet_loss_perc: 10,
            bit_rate: None,
        })
        .expect("ReconfigOpus must serialize");
        assert!(
            omitted.get("bitRate").is_none(),
            "bit_rate=None must OMIT the bitRate key (byte-identical to pre-#1398)"
        );

        let present = serde_json::to_value(EncoderMessages::ReconfigOpus {
            fec: true,
            packet_loss_perc: 10,
            bit_rate: Some(32000),
        })
        .expect("ReconfigOpus must serialize");
        assert_eq!(
            present["bitRate"],
            serde_json::json!(32000),
            "bit_rate=Some(bps) must emit the EXACT camelCase `bitRate` key with the bps value"
        );
        // The FEC/loss part is unchanged regardless of the bitrate field.
        assert_eq!(present["fec"], serde_json::json!(true));
        assert_eq!(present["packetLossPerc"], serde_json::json!(10));
    }

    #[test]
    fn fec_false_serializes_as_false_not_omitted() {
        // `Some(false)` for FEC still omits the key only if the field were
        // skipped on false — it is NOT (we skip on None). Verify the explicit
        // `false` path serializes to `false`, so the worklet's truthiness gate
        // (`if(this.config.encoderFec)`) correctly treats it as "no ctl call".
        let json = init_json(EncoderInitOptions {
            encoder_fec: Some(false),
            ..Default::default()
        });
        assert_eq!(json["encoderFec"], serde_json::json!(false));
    }
}
