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

//! Dump a deterministic VP9 stream from the pure-Rust encoder to an IVF file.
//!
//! Produces the fixture consumed by the WebCodecs interop E2E test
//! (`e2e/tests/webcodecs-vp9-interop.spec.ts`), which feeds the stream to a real
//! Chromium `VideoDecoder` to prove our encoder's output is decodable by the
//! same VP9 decoder meeting participants use.
//!
//! ```text
//! cargo run -p videocall-codecs --example dump_vp9_ivf \
//!     --features test-utils -- e2e/fixtures/pure_rust_vp9.ivf
//! ```
//!
//! The sequence is 90 frames of 640x480 at 30 fps, 500 kbps, keyframe interval
//! 150 (so exactly one keyframe followed by 89 inter frames). Content is a white
//! box translating over a scrolling diagonal gradient: moving high-contrast edges
//! for inter prediction, broadband texture so decoded pixels are never uniform.

use std::fs::File;

use videocall_codecs::encoder::{Encodable, EncoderConfig};
use videocall_codecs::testing::i420::{gradient, moving_box};
use videocall_codecs::testing::ivf::IvfWriter;
use videocall_codecs::vp9::Vp9Encoder;

const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;
const FPS: u32 = 30;
const FRAMES: u32 = 90;
const BITRATE_KBPS: u32 = 500;
const KEYFRAME_INTERVAL: u32 = 150;

/// A white box (from [`moving_box`]) composited over a scrolling gradient
/// background (from [`gradient`]). Both sources are deterministic functions of
/// `t`, so the whole sequence is reproducible byte-for-byte.
fn frame(t: u32) -> Vec<u8> {
    let bg = gradient(WIDTH, HEIGHT, t);
    let fg = moving_box(WIDTH, HEIGHT, t);
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    let luma = w * h;

    let mut buf = bg;
    // Overlay the box's white pixels onto the gradient luma; leave chroma as the
    // gradient's so the box carries motion without a hard chroma edge.
    for i in 0..luma {
        if fg[i] == 255 {
            buf[i] = 255;
        }
    }
    buf
}

fn main() -> anyhow::Result<()> {
    let out_path = std::env::args()
        .nth(1)
        .expect("usage: dump_vp9_ivf <output.ivf>");

    let cfg = EncoderConfig {
        width: WIDTH,
        height: HEIGHT,
        framerate: FPS,
        bitrate_kbps: BITRATE_KBPS,
        keyframe_interval: KEYFRAME_INTERVAL,
        min_quantizer: 40,
        max_quantizer: 60,
        cpu_used: 7,
    };

    let mut enc = Vp9Encoder::new(cfg)?;
    let file = File::create(&out_path)?;
    let mut writer = IvfWriter::new(file, WIDTH as u16, HEIGHT as u16, FPS)?;

    let mut written = 0u32;
    let mut keyframes = 0u32;
    for t in 0..FRAMES {
        let i420 = frame(t);
        if let Some(ef) = enc.encode(t as i64, &i420)? {
            writer.write_frame(&ef.data, ef.pts as u64)?;
            written += 1;
            if ef.is_keyframe {
                keyframes += 1;
            }
        }
    }
    writer.into_inner()?;

    eprintln!(
        "wrote {written} frames ({keyframes} keyframe(s)) to {out_path} — {WIDTH}x{HEIGHT}@{FPS}, {BITRATE_KBPS} kbps"
    );
    assert_eq!(written, FRAMES, "encoder must emit one frame per input");
    assert_eq!(keyframes, 1, "expected exactly one keyframe");
    Ok(())
}
