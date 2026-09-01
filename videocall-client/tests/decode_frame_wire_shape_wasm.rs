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
 */

#![cfg(target_arch = "wasm32")]

// Browser-configured on purpose: CI's `wasm-pack test --headless --chrome` step runs this.

use js_sys::{Array, Reflect, Uint8Array};
use videocall_codecs::frame::{FrameBuffer, FrameCodec, FrameType, VideoFrame};
use videocall_codecs::messages::WorkerMessage;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_test::wasm_bindgen_test;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const SMALL_FRAME_BYTES: usize = 600;
const LARGE_FRAME_BYTES: usize = 3000;

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn decode_frame(len: usize) -> WorkerMessage {
    WorkerMessage::DecodeFrame(FrameBuffer::new(
        VideoFrame {
            sequence_number: 42,
            frame_type: FrameType::KeyFrame,
            codec: FrameCodec::Vp9Profile0Level10Bit8,
            data: payload(len),
            timestamp: 1234.5,
        },
        99,
    ))
}

/// `{ DecodeFrame: { frame: { data: ... }, arrival_time_ms } }` — serde's externally-tagged enum.
fn frame_data_js(msg: &WorkerMessage) -> JsValue {
    let js = serde_wasm_bindgen::to_value(msg).expect("DecodeFrame must serialize");
    let variant = Reflect::get(&js, &JsValue::from_str("DecodeFrame"))
        .expect("the externally-tagged DecodeFrame variant must be present");
    let frame =
        Reflect::get(&variant, &JsValue::from_str("frame")).expect("FrameBuffer.frame must exist");
    Reflect::get(&frame, &JsValue::from_str("data")).expect("VideoFrame.data must exist")
}

/// The regression lock: a byte-equality round-trip passes either way, so only SHAPE pins it.
#[wasm_bindgen_test]
fn decode_frame_data_crosses_the_worker_boundary_as_a_uint8array() {
    for len in [SMALL_FRAME_BYTES, LARGE_FRAME_BYTES] {
        let data = frame_data_js(&decode_frame(len));
        assert!(
            !data.is_instance_of::<Array>(),
            "VideoFrame.data serialized as a JS Array of {len} numbers: one wasm->JS crossing per \
             byte (issue 2571)"
        );
        let view = data
            .dyn_ref::<Uint8Array>()
            .expect("VideoFrame.data must serialize as a Uint8Array");
        assert_eq!(view.length() as usize, len);
    }
}

#[wasm_bindgen_test]
fn a_decode_frame_round_trips_byte_identically() {
    for len in [0, 1, SMALL_FRAME_BYTES, LARGE_FRAME_BYTES] {
        let original = decode_frame(len);
        let js = serde_wasm_bindgen::to_value(&original).expect("serialize");
        let back: WorkerMessage = serde_wasm_bindgen::from_value(js).expect("deserialize");

        let (WorkerMessage::DecodeFrame(sent), WorkerMessage::DecodeFrame(received)) =
            (&original, &back)
        else {
            panic!("the round trip must preserve the DecodeFrame variant");
        };
        assert_eq!(received.frame.data, sent.frame.data, "len={len}");
        assert_eq!(received.frame.sequence_number, sent.frame.sequence_number);
        assert_eq!(received.frame.frame_type, sent.frame.frame_type);
        assert_eq!(received.frame.codec, sent.frame.codec);
        assert_eq!(received.frame.timestamp, sent.frame.timestamp);
        assert_eq!(received.arrival_time_ms, sent.arrival_time_ms);
    }
}

/// `serde_bytes`' `Vec<u8>` deserializer keeps a `visit_seq` arm, so `Array`-shaped `data` decodes.
#[wasm_bindgen_test]
fn an_array_shaped_payload_still_deserializes() {
    let js = serde_wasm_bindgen::to_value(&decode_frame(SMALL_FRAME_BYTES)).expect("serialize");
    let variant = Reflect::get(&js, &JsValue::from_str("DecodeFrame")).expect("variant");
    let frame = Reflect::get(&variant, &JsValue::from_str("frame")).expect("frame");
    let bytes: Uint8Array = Reflect::get(&frame, &JsValue::from_str("data"))
        .expect("data")
        .unchecked_into();
    assert!(
        Reflect::set(
            &frame,
            &JsValue::from_str("data"),
            &Array::from(bytes.as_ref()),
        )
        .expect("rewrite data as a plain Array"),
        "a refused write would leave the Uint8Array in place and never reach the seq arm"
    );

    let back: WorkerMessage = serde_wasm_bindgen::from_value(js).expect("array-shaped deserialize");
    let WorkerMessage::DecodeFrame(received) = back else {
        panic!("variant");
    };
    assert_eq!(received.frame.data, payload(SMALL_FRAME_BYTES));
}

#[wasm_bindgen_test]
fn decode_frame_serde_cost_is_reported() {
    const ITERATIONS: u32 = 20_000;
    let clock = web_sys::window()
        .expect("a browser window")
        .performance()
        .expect("performance.now()");

    for len in [SMALL_FRAME_BYTES, LARGE_FRAME_BYTES] {
        let msg = decode_frame(len);
        // Without this the first size measured absorbs the JIT cost of both.
        for _ in 0..2_000 {
            let js = serde_wasm_bindgen::to_value(&msg).expect("serialize");
            let _: WorkerMessage = serde_wasm_bindgen::from_value(js).expect("deserialize");
        }

        let start = clock.now();
        let mut sunk = 0u32;
        for _ in 0..ITERATIONS {
            let js = serde_wasm_bindgen::to_value(&msg).expect("serialize");
            sunk = sunk.wrapping_add(!js.is_undefined() as u32);
        }
        let to_value_us = (clock.now() - start) * 1000.0 / f64::from(ITERATIONS);

        let js = serde_wasm_bindgen::to_value(&msg).expect("serialize");
        let start = clock.now();
        for _ in 0..ITERATIONS {
            let back: WorkerMessage =
                serde_wasm_bindgen::from_value(js.clone()).expect("deserialize");
            sunk = sunk.wrapping_add(matches!(back, WorkerMessage::DecodeFrame(_)) as u32);
        }
        let from_value_us = (clock.now() - start) * 1000.0 / f64::from(ITERATIONS);

        // Visible only under `-- --nocapture`: the harness swallows console output on a pass.
        web_sys::console::log_1(&JsValue::from_str(&format!(
            "[2571 BENCH] DecodeFrame {len} B: to_value {to_value_us:.2} us, \
             from_value {from_value_us:.2} us ({ITERATIONS} iterations)"
        )));
        assert_eq!(
            sunk,
            2 * ITERATIONS,
            "both measured loops must have run {ITERATIONS} times"
        );
    }
}
