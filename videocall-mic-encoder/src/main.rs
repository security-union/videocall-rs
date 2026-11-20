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

//! The wasm the mic AudioWorklet instantiates (see `mic_encoder_worklet.js`).
//!
//! AudioWorkletGlobalScope has no fetch and no TextDecoder, so the worklet
//! skips the wasm-bindgen glue. Every export takes and returns plain numbers,
//! which wasm-bindgen leaves as raw wasm exports, and the worklet talks to
//! them through linear memory:
//!
//! 1. `mic_encoder_init(rate, bitrate)` once per Init message.
//! 2. Per render quantum: `mic_encoder_input(n)` gives a pointer to copy `n`
//!    samples into, then `mic_encoder_encode(n)` returns how many packets
//!    are ready.
//! 3. `mic_encoder_packet_ptr(i)` / `mic_encoder_packet_len(i)` locate each
//!    packet until the next `mic_encoder_encode`.

use std::cell::RefCell;
use videocall_mic_encoder::MicOpusEncoder;
use wasm_bindgen::prelude::wasm_bindgen;

struct Session {
    encoder: MicOpusEncoder,
    input: Vec<f32>,
    /// Packets from the last encode call, back to back.
    packets: Vec<u8>,
    /// Where each packet in `packets` ends.
    ends: Vec<usize>,
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

/// Starts a new encoder. Returns 0 on success, -1 on bad settings.
#[wasm_bindgen]
pub fn mic_encoder_init(input_rate: u32, bitrate_bps: u32) -> i32 {
    let session = MicOpusEncoder::new(input_rate, bitrate_bps)
        .map(|encoder| Session {
            encoder,
            input: Vec::new(),
            packets: Vec::new(),
            ends: Vec::new(),
        })
        .ok();
    let status = if session.is_some() { 0 } else { -1 };
    SESSION.with_borrow_mut(|slot| *slot = session);
    status
}

/// A buffer for `len` input samples, or null before a successful init.
#[wasm_bindgen]
pub fn mic_encoder_input(len: usize) -> *mut f32 {
    SESSION.with_borrow_mut(|slot| match slot {
        Some(session) => {
            session.input.resize(len, 0.0);
            session.input.as_mut_ptr()
        }
        None => std::ptr::null_mut(),
    })
}

/// Encodes the first `len` samples of the input buffer. Returns the number
/// of packets ready, or -1 on error.
#[wasm_bindgen]
pub fn mic_encoder_encode(len: usize) -> i32 {
    SESSION.with_borrow_mut(|slot| {
        let Some(session) = slot else { return -1 };
        let Session {
            encoder,
            input,
            packets,
            ends,
        } = session;
        packets.clear();
        ends.clear();
        let samples = &input[..len.min(input.len())];
        let result = encoder.push(samples, |packet| {
            packets.extend_from_slice(packet);
            ends.push(packets.len());
        });
        match result {
            Ok(()) => ends.len() as i32,
            Err(_) => -1,
        }
    })
}

/// Start of packet `index` from the last encode call.
#[wasm_bindgen]
pub fn mic_encoder_packet_ptr(index: usize) -> *const u8 {
    with_packet(index, |packets, start, _| packets[start..].as_ptr())
}

/// Length of packet `index` from the last encode call.
#[wasm_bindgen]
pub fn mic_encoder_packet_len(index: usize) -> usize {
    with_packet(index, |_, start, end| end - start)
}

fn with_packet<T: Default>(index: usize, read: impl FnOnce(&[u8], usize, usize) -> T) -> T {
    SESSION.with_borrow(|slot| {
        let Some(session) = slot else {
            return T::default();
        };
        let Some(&end) = session.ends.get(index) else {
            return T::default();
        };
        let start = index.checked_sub(1).map_or(0, |prev| session.ends[prev]);
        read(&session.packets, start, end)
    })
}

/// Required by the bin target; the worklet only calls the exports above.
fn main() {}
