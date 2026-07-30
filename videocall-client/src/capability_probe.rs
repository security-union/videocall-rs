/*
 * Copyright 2026 Security Union LLC
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

//! Phase-0 WebCodecs capability probe for production observability.
//!
//! This module intentionally does not feed its result back into encoder,
//! decoder, transport, or codec-selection state. It emits one structured
//! `info!` line so field logs can compare the current UA sniff against what a
//! capability-driven ladder would have selected.

use crate::constants::{VIDEO_CODEC_VP8, VIDEO_CODEC_VP9};

#[cfg(target_arch = "wasm32")]
const CAPABILITY_PROBE_GLOBAL: &str = "__videocall_capability_probe_done";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeAcceleration {
    PreferHardware,
    PreferSoftware,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeCodec {
    Vp9,
    Vp8,
}

impl ProbeCodec {
    pub fn codec_string(self) -> &'static str {
        match self {
            Self::Vp9 => VIDEO_CODEC_VP9,
            Self::Vp8 => VIDEO_CODEC_VP8,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProbeRung {
    pub codec: ProbeCodec,
    pub acceleration: ProbeAcceleration,
}

pub const ENCODE_LADDER: &[ProbeRung] = &[
    ProbeRung {
        codec: ProbeCodec::Vp9,
        acceleration: ProbeAcceleration::PreferHardware,
    },
    ProbeRung {
        codec: ProbeCodec::Vp9,
        acceleration: ProbeAcceleration::PreferSoftware,
    },
    ProbeRung {
        codec: ProbeCodec::Vp8,
        acceleration: ProbeAcceleration::PreferSoftware,
    },
];

pub const DECODE_LADDER: &[ProbeRung] = &[
    ProbeRung {
        codec: ProbeCodec::Vp9,
        acceleration: ProbeAcceleration::PreferHardware,
    },
    ProbeRung {
        codec: ProbeCodec::Vp9,
        acceleration: ProbeAcceleration::PreferSoftware,
    },
    ProbeRung {
        codec: ProbeCodec::Vp8,
        acceleration: ProbeAcceleration::PreferSoftware,
    },
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupportedProbeRung {
    pub rung: ProbeRung,
    /// The `hardwareAcceleration` value echoed back by `isConfigSupported` for
    /// this rung, mapped to `true` when it reads `"prefer-hardware"`.
    ///
    /// IMPORTANT — this is a browser-REPORTED hint, NOT a verified hardware
    /// grant. Per the WebCodecs spec `hardwareAcceleration` is a hint the UA MAY
    /// ignore, and the returned config only echoes the recognized preference.
    /// Chrome returns `supported:false` for `prefer-hardware` when no HW encoder
    /// exists (so the HW rung simply won't be the first-supported rung — the
    /// ladder falls through to software), which makes the SIGNAL trustworthy on
    /// Chrome. Firefox IGNORES the hint and still reports supported+prefer-hardware
    /// for software-only VP9/AV1 (W3C webcodecs#896, Mozilla bug 1967793), so on
    /// Firefox this flag can falsely read `true` for a software-only client.
    /// Consumers MUST discount it when `is_firefox` (logged alongside). We do NOT
    /// call it `hw_granted` precisely to avoid implying a verified grant.
    pub hw_hint_reported: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityProbeInput {
    pub encode: Option<SupportedProbeRung>,
    pub decode: Option<SupportedProbeRung>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WouldChoose {
    Unavailable,
    Vp9Hw,
    Vp9Sw,
    Vp8,
}

impl WouldChoose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "Unavailable",
            Self::Vp9Hw => "Vp9Hw",
            Self::Vp9Sw => "Vp9Sw",
            Self::Vp8 => "Vp8",
        }
    }

    fn codec(self) -> Option<ProbeCodec> {
        match self {
            Self::Unavailable => None,
            Self::Vp9Hw | Self::Vp9Sw => Some(ProbeCodec::Vp9),
            Self::Vp8 => Some(ProbeCodec::Vp8),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityProbeDecision {
    pub ua_choice: ProbeCodec,
    pub would_encode: WouldChoose,
    pub encode_differs_from_ua: bool,
    pub would_decode: WouldChoose,
    pub decode_differs_from_ua: bool,
}

pub fn decide_capability_probe(
    input: CapabilityProbeInput,
    is_firefox: bool,
) -> CapabilityProbeDecision {
    let ua_choice = if is_firefox {
        ProbeCodec::Vp8
    } else {
        ProbeCodec::Vp9
    };
    let would_encode = classify_supported_rung(input.encode);
    let would_decode = classify_supported_rung(input.decode);

    CapabilityProbeDecision {
        ua_choice,
        would_encode,
        encode_differs_from_ua: would_encode.codec() != Some(ua_choice),
        would_decode,
        decode_differs_from_ua: would_decode.codec() != Some(ua_choice),
    }
}

fn classify_supported_rung(rung: Option<SupportedProbeRung>) -> WouldChoose {
    match rung {
        // The HW-preferred VP9 rung was the FIRST rung to report supported. On
        // Chrome this reliably means a HW VP9 encoder exists (Chrome returns
        // supported:false for prefer-hardware without HW). On Firefox the hint is
        // ignored so this can be software VP9 — see `hw_hint_reported` + the
        // `is_firefox` field logged alongside; analysis must discount Firefox.
        Some(SupportedProbeRung {
            rung:
                ProbeRung {
                    codec: ProbeCodec::Vp9,
                    acceleration: ProbeAcceleration::PreferHardware,
                },
            hw_hint_reported: true,
        }) => WouldChoose::Vp9Hw,
        Some(SupportedProbeRung {
            rung:
                ProbeRung {
                    codec: ProbeCodec::Vp9,
                    ..
                },
            ..
        }) => WouldChoose::Vp9Sw,
        Some(SupportedProbeRung {
            rung:
                ProbeRung {
                    codec: ProbeCodec::Vp8,
                    ..
                },
            ..
        }) => WouldChoose::Vp8,
        None => WouldChoose::Unavailable,
    }
}

#[cfg(target_arch = "wasm32")]
pub fn spawn_capability_probe() {
    use wasm_bindgen::JsValue;

    let Some(window) = web_sys::window() else {
        log_unavailable();
        return;
    };

    if js_sys::Reflect::get(&window, &JsValue::from_str(CAPABILITY_PROBE_GLOBAL))
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return;
    }

    let _ = js_sys::Reflect::set(
        &window,
        &JsValue::from_str(CAPABILITY_PROBE_GLOBAL),
        &JsValue::TRUE,
    );

    wasm_bindgen_futures::spawn_local(async {
        let start_ms = js_sys::Date::now();
        let result = probe_webcodecs().await;
        let probe_ms = (js_sys::Date::now() - start_ms).max(0.0).round() as u32;
        log_probe_result(result, probe_ms);
    });
}

#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_capability_probe() {}

#[cfg(target_arch = "wasm32")]
async fn probe_webcodecs() -> CapabilityProbeInput {
    CapabilityProbeInput {
        encode: probe_first_supported_encoder().await,
        decode: probe_first_supported_decoder().await,
    }
}

#[cfg(target_arch = "wasm32")]
async fn probe_first_supported_encoder() -> Option<SupportedProbeRung> {
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{LatencyMode, VideoEncoder, VideoEncoderConfig};

    if !has_webcodecs_support_probe("VideoEncoder") {
        return None;
    }

    for rung in ENCODE_LADDER {
        let config = VideoEncoderConfig::new(rung.codec.codec_string(), 720, 1280);
        config.set_latency_mode(LatencyMode::Realtime);
        set_encoder_acceleration(&config, rung.acceleration);

        let result = JsFuture::from(VideoEncoder::is_config_supported(&config))
            .await
            .ok()?;
        let supported = parse_support(&result).unwrap_or(false);
        if supported {
            return Some(SupportedProbeRung {
                rung: *rung,
                hw_hint_reported: resolved_hw_hint(&result),
            });
        }
    }

    None
}

#[cfg(target_arch = "wasm32")]
async fn probe_first_supported_decoder() -> Option<SupportedProbeRung> {
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{VideoDecoder, VideoDecoderConfig};

    if !has_webcodecs_support_probe("VideoDecoder") {
        return None;
    }

    for rung in DECODE_LADDER {
        let config = VideoDecoderConfig::new(rung.codec.codec_string());
        config.set_coded_height(720);
        config.set_coded_width(1280);
        set_decoder_acceleration(&config, rung.acceleration);

        let result = JsFuture::from(VideoDecoder::is_config_supported(&config))
            .await
            .ok()?;
        let supported = parse_support(&result).unwrap_or(false);
        if supported {
            return Some(SupportedProbeRung {
                rung: *rung,
                hw_hint_reported: resolved_hw_hint(&result),
            });
        }
    }

    None
}

#[cfg(target_arch = "wasm32")]
fn has_webcodecs_support_probe(name: &str) -> bool {
    use wasm_bindgen::JsValue;

    web_sys::window()
        .and_then(|window| js_sys::Reflect::get(&window, &JsValue::from_str(name)).ok())
        .filter(|constructor| constructor.is_function())
        .and_then(|constructor| {
            js_sys::Reflect::get(&constructor, &JsValue::from_str("isConfigSupported")).ok()
        })
        .map(|probe| probe.is_function())
        .unwrap_or(false)
}

#[cfg(target_arch = "wasm32")]
fn set_encoder_acceleration(config: &web_sys::VideoEncoderConfig, acceleration: ProbeAcceleration) {
    config.set_hardware_acceleration(to_web_sys_acceleration(acceleration));
}

#[cfg(target_arch = "wasm32")]
fn set_decoder_acceleration(config: &web_sys::VideoDecoderConfig, acceleration: ProbeAcceleration) {
    config.set_hardware_acceleration(to_web_sys_acceleration(acceleration));
}

#[cfg(target_arch = "wasm32")]
fn to_web_sys_acceleration(acceleration: ProbeAcceleration) -> web_sys::HardwareAcceleration {
    match acceleration {
        ProbeAcceleration::PreferHardware => web_sys::HardwareAcceleration::PreferHardware,
        ProbeAcceleration::PreferSoftware => web_sys::HardwareAcceleration::PreferSoftware,
    }
}

#[cfg(target_arch = "wasm32")]
// Public only so the separate Node wasm integration target can exercise the
// exact production parser; hidden from generated API documentation.
#[doc(hidden)]
pub fn parse_support(result: &wasm_bindgen::JsValue) -> Option<bool> {
    use wasm_bindgen::JsValue;

    js_sys::Reflect::get(result, &JsValue::from_str("supported"))
        .ok()
        .and_then(|value| value.as_bool())
}

#[cfg(target_arch = "wasm32")]
// See `parse_support` for why this internal parser is publicly reachable.
#[doc(hidden)]
pub fn resolved_hw_hint(result: &wasm_bindgen::JsValue) -> bool {
    use wasm_bindgen::JsValue;

    js_sys::Reflect::get(result, &JsValue::from_str("config"))
        .ok()
        .and_then(|config| {
            js_sys::Reflect::get(&config, &JsValue::from_str("hardwareAcceleration")).ok()
        })
        .and_then(|value| value.as_string())
        .map(|value| value == "prefer-hardware")
        .unwrap_or(false)
}

#[cfg(target_arch = "wasm32")]
fn log_probe_result(input: CapabilityProbeInput, probe_ms: u32) {
    let is_firefox = crate::utils::is_firefox();
    let decision = decide_capability_probe(input, is_firefox);
    let ua_choice = crate::constants::get_video_codec_string();

    // `is_firefox` is logged so analysis can discount the HW HINT (Firefox
    // ignores `prefer-hardware` and over-reports it for software VP9/AV1). The
    // `*_hw_hint` fields are the browser-REPORTED preference, NOT a verified
    // grant — reliable on Chromium, unreliable on Firefox. See `hw_hint_reported`.
    log::info!(
        "capability_probe: ua_choice={}; is_firefox={}; would_encode={}; encode_hw_hint={}; \
         encode_rung={}; encode_differs_from_ua={}; would_decode={}; \
         decode_hw_hint={}; decode_rung={}; decode_differs_from_ua={}; probe_ms={}",
        ua_choice,
        is_firefox,
        decision.would_encode.as_str(),
        input.encode.map(|r| r.hw_hint_reported).unwrap_or(false),
        input
            .encode
            .map(|r| r.rung.codec.codec_string())
            .unwrap_or("unavailable"),
        decision.encode_differs_from_ua,
        decision.would_decode.as_str(),
        input.decode.map(|r| r.hw_hint_reported).unwrap_or(false),
        input
            .decode
            .map(|r| r.rung.codec.codec_string())
            .unwrap_or("unavailable"),
        decision.decode_differs_from_ua,
        probe_ms
    );
}

#[cfg(target_arch = "wasm32")]
fn log_unavailable() {
    log_probe_result(
        CapabilityProbeInput {
            encode: None,
            decode: None,
        },
        0,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supported(
        codec: ProbeCodec,
        acceleration: ProbeAcceleration,
        hw_hint_reported: bool,
    ) -> SupportedProbeRung {
        SupportedProbeRung {
            rung: ProbeRung {
                codec,
                acceleration,
            },
            hw_hint_reported,
        }
    }

    #[test]
    fn chrome_vp9_hw_matches_ua_sniff_but_records_hardware() {
        let decision = decide_capability_probe(
            CapabilityProbeInput {
                encode: Some(supported(
                    ProbeCodec::Vp9,
                    ProbeAcceleration::PreferHardware,
                    true,
                )),
                decode: Some(supported(
                    ProbeCodec::Vp9,
                    ProbeAcceleration::PreferHardware,
                    true,
                )),
            },
            false,
        );

        assert_eq!(decision.ua_choice, ProbeCodec::Vp9);
        assert_eq!(decision.would_encode, WouldChoose::Vp9Hw);
        assert!(!decision.encode_differs_from_ua);
        assert_eq!(decision.would_decode, WouldChoose::Vp9Hw);
        assert!(!decision.decode_differs_from_ua);
    }

    #[test]
    fn chrome_vp9_without_hw_is_observed_as_software_vp9() {
        let decision = decide_capability_probe(
            CapabilityProbeInput {
                encode: Some(supported(
                    ProbeCodec::Vp9,
                    ProbeAcceleration::PreferHardware,
                    false,
                )),
                decode: Some(supported(
                    ProbeCodec::Vp9,
                    ProbeAcceleration::PreferSoftware,
                    false,
                )),
            },
            false,
        );

        assert_eq!(decision.would_encode, WouldChoose::Vp9Sw);
        assert!(!decision.encode_differs_from_ua);
        assert_eq!(decision.would_decode, WouldChoose::Vp9Sw);
        assert!(!decision.decode_differs_from_ua);
    }

    #[test]
    fn chrome_vp8_fallback_differs_from_ua_sniff() {
        let decision = decide_capability_probe(
            CapabilityProbeInput {
                encode: Some(supported(
                    ProbeCodec::Vp8,
                    ProbeAcceleration::PreferSoftware,
                    false,
                )),
                decode: Some(supported(
                    ProbeCodec::Vp8,
                    ProbeAcceleration::PreferSoftware,
                    false,
                )),
            },
            false,
        );

        assert_eq!(decision.ua_choice, ProbeCodec::Vp9);
        assert_eq!(decision.would_encode, WouldChoose::Vp8);
        assert!(decision.encode_differs_from_ua);
        assert_eq!(decision.would_decode, WouldChoose::Vp8);
        assert!(decision.decode_differs_from_ua);
    }

    #[test]
    fn firefox_vp9_capability_differs_from_ua_sniff() {
        let decision = decide_capability_probe(
            CapabilityProbeInput {
                encode: Some(supported(
                    ProbeCodec::Vp9,
                    ProbeAcceleration::PreferHardware,
                    true,
                )),
                decode: Some(supported(
                    ProbeCodec::Vp9,
                    ProbeAcceleration::PreferHardware,
                    true,
                )),
            },
            true,
        );

        assert_eq!(decision.ua_choice, ProbeCodec::Vp8);
        assert_eq!(decision.would_encode, WouldChoose::Vp9Hw);
        assert!(decision.encode_differs_from_ua);
        assert_eq!(decision.would_decode, WouldChoose::Vp9Hw);
        assert!(decision.decode_differs_from_ua);
    }

    #[test]
    fn unavailable_probe_records_no_codec_and_differs_from_ua() {
        let decision = decide_capability_probe(
            CapabilityProbeInput {
                encode: None,
                decode: None,
            },
            false,
        );

        assert_eq!(decision.would_encode, WouldChoose::Unavailable);
        assert!(decision.encode_differs_from_ua);
        assert_eq!(decision.would_decode, WouldChoose::Unavailable);
        assert!(decision.decode_differs_from_ua);
    }

    #[test]
    fn ladders_try_hardware_vp9_before_software_vp9_before_vp8() {
        assert_eq!(
            ENCODE_LADDER,
            &[
                ProbeRung {
                    codec: ProbeCodec::Vp9,
                    acceleration: ProbeAcceleration::PreferHardware,
                },
                ProbeRung {
                    codec: ProbeCodec::Vp9,
                    acceleration: ProbeAcceleration::PreferSoftware,
                },
                ProbeRung {
                    codec: ProbeCodec::Vp8,
                    acceleration: ProbeAcceleration::PreferSoftware,
                },
            ]
        );
        assert_eq!(DECODE_LADDER, ENCODE_LADDER);
    }
}
