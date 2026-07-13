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

//! Drop-in contract for the `video_encoder` adapter.
//!
//! This exercises the public `VideoEncoderBuilder` / `VideoEncoder` / `Frame` /
//! `Frames` API the same way `encoder_thread` does, using synthetic I420 frames
//! (no camera hardware). It must pass identically for both backends:
//!
//! - `cargo test -p videocall-cli`                (pure-Rust VP9, default)
//! - `cargo test -p videocall-cli --features libvpx` (legacy C libvpx backend)
//!
//! That parity is the real proof the swap is a drop-in replacement.

use videocall_cli::video_encoder::VideoEncoderBuilder;

const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;
const FPS: u32 = 30;
const CPU_USED: u8 = 7;

/// Build a deterministic I420 frame with a moving diagonal gradient so that
/// successive frames differ (giving inter frames real residual to encode).
fn synthetic_i420(frame_idx: u32) -> Vec<u8> {
    let w = WIDTH as usize;
    let h = HEIGHT as usize;
    let cw = w / 2;
    let ch = h / 2;
    let mut buf = vec![0u8; w * h + 2 * cw * ch];

    let shift = frame_idx as usize;
    // Y plane: diagonal gradient that scrolls with the frame index.
    for y in 0..h {
        for x in 0..w {
            buf[y * w + x] = ((x + y + shift) & 0xff) as u8;
        }
    }
    // U/V planes: slowly varying chroma so the frame is a valid full I420 image.
    let u_off = w * h;
    let v_off = u_off + cw * ch;
    for y in 0..ch {
        for x in 0..cw {
            buf[u_off + y * cw + x] = ((x + shift) & 0xff) as u8;
            buf[v_off + y * cw + x] = ((y + shift) & 0xff) as u8;
        }
    }
    buf
}

/// Feed a sequence of frames and collect (is_key, pts, payload_len) for every
/// emitted frame. Backends may buffer (0-frame results), so the number of
/// emitted frames can be fewer than the number of inputs.
fn encode_sequence(count: u32) -> Vec<(bool, i64, usize)> {
    let mut encoder = VideoEncoderBuilder::new(FPS, CPU_USED)
        .set_resolution(WIDTH, HEIGHT)
        .build()
        .expect("encoder should build at 640x480");

    let mut out = Vec::new();
    for i in 0..count {
        let frame = synthetic_i420(i);
        let frames = encoder
            .encode(i as i64, &frame)
            .expect("encode should succeed");
        for f in frames {
            out.push((f.key, f.pts, f.data.len()));
        }
    }
    out
}

#[test]
fn first_frame_is_keyframe_rest_are_delta() {
    let emitted = encode_sequence(30);
    assert!(
        !emitted.is_empty(),
        "expected at least one emitted frame from 30 inputs"
    );

    // The first emitted frame must be a keyframe; none of the rest should be
    // (keyframe interval is 150, far beyond this 30-frame sequence).
    assert!(emitted[0].0, "first emitted frame must be a keyframe");
    for (idx, (key, _, _)) in emitted.iter().enumerate().skip(1) {
        assert!(!key, "emitted frame {idx} must be a delta frame");
    }
}

#[test]
fn payloads_are_nonempty() {
    for (idx, (_, _, len)) in encode_sequence(30).iter().enumerate() {
        assert!(*len > 0, "emitted frame {idx} must have a nonzero payload");
    }
}

#[test]
fn pts_passes_through() {
    // pts values we feed in must come back out unchanged, in order.
    let emitted = encode_sequence(30);
    let ptss: Vec<i64> = emitted.iter().map(|(_, pts, _)| *pts).collect();
    let mut sorted = ptss.clone();
    sorted.sort_unstable();
    assert_eq!(
        ptss, sorted,
        "pts values must be emitted in nondecreasing order"
    );
    assert!(
        ptss.iter().all(|&p| (0..30).contains(&p)),
        "every emitted pts must be one we supplied"
    );
}

#[test]
fn update_bitrate_kbps_works_mid_stream() {
    let mut encoder = VideoEncoderBuilder::new(FPS, CPU_USED)
        .set_resolution(WIDTH, HEIGHT)
        .build()
        .expect("encoder should build");

    for i in 0..10 {
        encoder
            .encode(i, &synthetic_i420(i as u32))
            .expect("encode before bitrate change");
    }
    encoder
        .update_bitrate_kbps(1200)
        .expect("mid-stream bitrate update must succeed");
    for i in 10..20 {
        encoder
            .encode(i, &synthetic_i420(i as u32))
            .expect("encode after bitrate change");
    }
}

#[test]
fn build_rejects_odd_dimensions() {
    // `VideoEncoder` intentionally has no `Debug`, so assert on the error text
    // directly rather than via `expect_err`.
    fn build_err(w: u32, h: u32) -> String {
        match VideoEncoderBuilder::new(FPS, CPU_USED)
            .set_resolution(w, h)
            .build()
        {
            Ok(_) => panic!("build({w}x{h}) should have failed"),
            Err(e) => e.to_string(),
        }
    }

    assert!(build_err(641, 480).contains("Width must be divisible by 2"));
    assert!(build_err(640, 481).contains("Height must be divisible by 2"));
    assert!(build_err(0, 480).contains("Width must be divisible by 2"));
}
