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

//! VP9 boolean arithmetic coder.
//!
//! [`BoolWriter`] is a faithful port of libvpx `vpx_dsp/bitwriter.{h,c}`
//! (`vpx_write`, `vpx_start_encode`, `vpx_stop_encode`). [`BoolReader`] decodes
//! the same bitstream (equivalent to `vpx_dsp/bitreader.{h,c}`, using the classic
//! byte-at-a-time formulation from RFC 6386 §7, which is bit-compatible with the
//! libvpx window reader). The reader exists for round-trip unit tests and the
//! debug stream parser; the shipped encoder only ever writes.
//!
//! The writer owns its output buffer (a growing `Vec<u8>`), so — unlike the
//! fixed-buffer libvpx original — it can never overflow and the `error` flag is
//! elided. Carry propagation across runs of `0xff` bytes already written is
//! preserved exactly.

/// The probability value denoting an even (1/2) split. Matches `vpx_prob_half`.
pub const PROB_HALF: u8 = 128;

/// libvpx `vpx_norm[256]`: the renormalisation shift for each possible `range`.
///
/// `vpx_norm[r]` is the number of leading zero bits of `r` interpreted as an
/// 8-bit value (i.e. how far `range` must be shifted left to re-normalise it
/// into `[128, 255]`). Copied verbatim from `vpx_dsp/prob.c`.
#[rustfmt::skip]
pub(crate) const VPX_NORM: [u8; 256] = [
    0, 7, 6, 6, 5, 5, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
    3, 3, 3, 3, 3, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// VP9 boolean arithmetic *encoder*.
///
/// Port of libvpx `vpx_writer`. Construct with [`BoolWriter::new`] (which
/// performs the `vpx_start_encode` leading marker-bit write), emit bits/literals,
/// then call [`BoolWriter::finalize`] (which performs `vpx_stop_encode`) to
/// obtain the encoded bytes.
pub struct BoolWriter {
    lowvalue: u32,
    range: u32,
    count: i32,
    buffer: Vec<u8>,
}

impl BoolWriter {
    /// Start a new bool-coded partition. Mirrors `vpx_start_encode`, including
    /// the leading `write_bit(0)` marker that the decoder consumes on init.
    pub fn new() -> Self {
        let mut w = BoolWriter {
            lowvalue: 0,
            range: 255,
            count: -24,
            buffer: Vec::new(),
        };
        w.write_bit(0);
        w
    }

    /// Number of bytes emitted so far (`vpx_writer::pos`).
    #[allow(dead_code)]
    pub fn pos(&self) -> usize {
        self.buffer.len()
    }

    /// Encode a single `bit` under 8-bit `probability` (of the bit being 0).
    /// Verbatim port of `vpx_write`.
    pub fn write(&mut self, bit: u8, probability: u8) {
        let mut count = self.count;
        let mut range = self.range;
        let mut lowvalue = self.lowvalue;

        let split = 1 + (((range - 1) * probability as u32) >> 8);
        range = split;
        if bit != 0 {
            lowvalue = lowvalue.wrapping_add(split);
            range = self.range - split;
        }

        let mut shift = VPX_NORM[range as usize] as i32;
        range <<= shift;
        count += shift;

        if count >= 0 {
            let offset = shift - count;
            // Carry propagation: if the bit shifted out is set, ripple +1 back
            // through any run of already-written 0xff bytes.
            if (lowvalue << (offset - 1)) & 0x8000_0000 != 0 {
                let mut x = self.buffer.len() as isize - 1;
                while x >= 0 && self.buffer[x as usize] == 0xff {
                    self.buffer[x as usize] = 0;
                    x -= 1;
                }
                // libvpx proves x >= 0 here (a carry never reaches before the
                // first byte); index directly to preserve that invariant.
                self.buffer[x as usize] = self.buffer[x as usize].wrapping_add(1);
            }
            self.buffer.push(((lowvalue >> (24 - offset)) & 0xff) as u8);
            lowvalue <<= offset;
            shift = count;
            lowvalue &= 0xffffff;
            count -= 8;
        }

        lowvalue <<= shift;
        self.count = count;
        self.lowvalue = lowvalue;
        self.range = range;
    }

    /// Encode `bit` at probability 128 (an equiprobable raw bit). `vpx_write_bit`.
    pub fn write_bit(&mut self, bit: u8) {
        self.write(bit, PROB_HALF);
    }

    /// Encode the low `bits` of `value`, most-significant bit first.
    /// `vpx_write_literal`.
    pub fn write_literal(&mut self, value: u32, bits: u32) {
        for bit in (0..bits).rev() {
            self.write_bit(((value >> bit) & 1) as u8);
        }
    }

    /// Flush and return the encoded bytes. Mirrors `vpx_stop_encode`: 32 trailing
    /// zero bits, plus the superframe-marker collision guard byte.
    pub fn finalize(mut self) -> Vec<u8> {
        for _ in 0..32 {
            self.write_bit(0);
        }
        // Avoid an ambiguous collision with a superframe index marker byte.
        if let Some(&last) = self.buffer.last() {
            if last & 0xe0 == 0xc0 {
                self.buffer.push(0);
            }
        }
        self.buffer
    }
}

impl Default for BoolWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// VP9 boolean arithmetic *decoder*.
///
/// Byte-at-a-time formulation (RFC 6386 §7.3), bit-compatible with libvpx
/// `vpx_reader`. Used only by tests and the debug stream parser. Reading past the
/// end of the buffer yields zero bits, matching libvpx's behaviour on exhausted
/// input.
pub struct BoolReader<'a> {
    buffer: &'a [u8],
    pos: usize,
    value: u32,
    range: u32,
    bit_count: i32,
}

impl<'a> BoolReader<'a> {
    /// Initialise a reader over `buffer` and consume the leading marker bit that
    /// [`BoolWriter::new`] wrote (mirrors `vpx_reader_init`).
    pub fn new(buffer: &'a [u8]) -> Self {
        let mut r = BoolReader {
            buffer,
            pos: 0,
            value: 0,
            range: 255,
            bit_count: 0,
        };
        // Prime the 16-bit value window with the first two bytes.
        let b0 = r.next_byte();
        let b1 = r.next_byte();
        r.value = ((b0 as u32) << 8) | b1 as u32;
        // Consume the marker bit start_encode wrote.
        let _marker = r.read_bit();
        r
    }

    #[inline]
    fn next_byte(&mut self) -> u8 {
        let b = self.buffer.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }

    /// Decode a single bit under 8-bit `probability` (of the bit being 0).
    pub fn read(&mut self, probability: u8) -> u8 {
        let split = 1 + (((self.range - 1) * probability as u32) >> 8);
        let big_split = split << 8;
        let bit;
        if self.value >= big_split {
            bit = 1;
            self.range -= split;
            self.value -= big_split;
        } else {
            bit = 0;
            self.range = split;
        }
        while self.range < 128 {
            self.value <<= 1;
            self.range <<= 1;
            self.bit_count += 1;
            if self.bit_count == 8 {
                self.bit_count = 0;
                self.value |= self.next_byte() as u32;
            }
        }
        bit
    }

    /// Decode a bit at probability 128. `vpx_read_bit`.
    pub fn read_bit(&mut self) -> u8 {
        self.read(PROB_HALF)
    }

    /// Decode `bits` raw bits, most-significant first. `vpx_read_literal`.
    pub fn read_literal(&mut self, bits: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..bits {
            v = (v << 1) | self.read_bit() as u32;
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift32 mirrored bit-for-bit in `fixture_gen.c`.
    struct XorShift32(u32);
    impl XorShift32 {
        fn next(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }
    }

    /// Byte-exact fixture: these bytes were produced by compiling a verbatim copy
    /// of libvpx `vpx_dsp/bitwriter.c` (`vpx_write`/`start`/`stop` + `vpx_norm`)
    /// and driving it with the exact `FIXTURE_OPS` sequence below (see
    /// `scratchpad/fixture_gen.c`). Asserting our writer reproduces them proves
    /// byte-for-byte libvpx compatibility, including the 0xff carry ripple (note
    /// the long 0xff run) and the stop-encode flush.
    #[rustfmt::skip]
    const FIXTURE: [u8; 160] = [
        0x00,0xed,0x44,0x32,0x3d,0x7c,0x86,0x4e,0x65,0xd9,0x4e,0xb4,0xde,0x9e,0xbf,0x6d,
        0x92,0x9a,0x29,0x3a,0x4f,0x13,0x07,0x93,0x13,0x1f,0xdc,0x7d,0x0c,0xee,0x40,0x22,
        0x80,0x69,0x35,0x35,0xcc,0x51,0xbb,0xa0,0xdc,0xa6,0x1b,0x5c,0xbf,0x85,0xff,0xff,
        0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,
        0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,
        0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xff,0xd8,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
        0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
        0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
        0x00,0x00,0x00,0xab,0xcd,0x00,0xe0,0xac,0x1a,0xc6,0x35,0xd3,0x48,0x4a,0xb1,0xde,
        0x38,0x08,0x02,0x55,0x80,0x02,0xc8,0xe0,0xdf,0xc2,0x2d,0xfa,0x80,0x01,0x0c,0x80,
    ];

    /// Enum of the operations the fixture performs, so the writer test and the
    /// reader round-trip test drive the identical sequence.
    #[derive(Clone, Copy)]
    enum Op {
        Write(u8, u8),
        Literal(u32, u32),
    }

    fn fixture_ops() -> Vec<Op> {
        let mut rng = XorShift32(0x1234_5678);
        let mut ops = Vec::new();
        for _ in 0..256 {
            let p = (rng.next() % 254) as u8 + 1;
            let b = ((rng.next() >> 3) & 1) as u8;
            ops.push(Op::Write(b, p));
        }
        for _ in 0..64 {
            ops.push(Op::Write(1, 250));
        }
        for _ in 0..64 {
            ops.push(Op::Write(0, 6));
        }
        ops.push(Op::Literal(0xABCD, 16));
        ops.push(Op::Literal(0x00, 8));
        for _ in 0..128 {
            let p = (rng.next() % 254) as u8 + 1;
            let b = ((rng.next() >> 7) & 1) as u8;
            ops.push(Op::Write(b, p));
        }
        ops
    }

    #[test]
    fn byte_exact_fixture_matches_libvpx() {
        let mut w = BoolWriter::new();
        for op in fixture_ops() {
            match op {
                Op::Write(b, p) => w.write(b, p),
                Op::Literal(v, n) => w.write_literal(v, n),
            }
        }
        let out = w.finalize();
        assert_eq!(
            out.len(),
            FIXTURE.len(),
            "length mismatch vs libvpx fixture"
        );
        assert_eq!(
            out.as_slice(),
            &FIXTURE[..],
            "bytes differ from libvpx fixture"
        );
    }

    #[test]
    fn fixture_round_trip() {
        // Write the documented sequence, then decode it back and confirm every
        // bit/literal matches. Note: literals are decoded MSB-first so the value
        // comes back identical.
        let mut w = BoolWriter::new();
        let ops = fixture_ops();
        for op in &ops {
            match *op {
                Op::Write(b, p) => w.write(b, p),
                Op::Literal(v, n) => w.write_literal(v, n),
            }
        }
        let bytes = w.finalize();
        let mut r = BoolReader::new(&bytes);
        for op in &ops {
            match *op {
                Op::Write(b, p) => assert_eq!(r.read(p), b),
                Op::Literal(v, n) => assert_eq!(r.read_literal(n), v),
            }
        }
    }

    #[test]
    fn exhaustive_random_round_trip() {
        // Thousands of random (bit, prob) sequences, including prob extremes and
        // long single-bit runs that force 0xff carry ripples.
        let mut rng = XorShift32(0xDEAD_BEEF);
        for trial in 0..2000u32 {
            let len = 1 + (rng.next() % 400);
            let mut bits = Vec::with_capacity(len as usize);
            let mut w = BoolWriter::new();
            for i in 0..len {
                // Mix in extremes 1/128/254 and long runs periodically.
                let p = match (trial + i) % 7 {
                    0 => 1u8,
                    1 => 128,
                    2 => 254,
                    _ => (rng.next() % 254) as u8 + 1,
                };
                let b = if trial % 5 == 0 {
                    // Long run of a fixed bit to stress carry propagation.
                    (trial & 1) as u8
                } else {
                    (rng.next() & 1) as u8
                };
                bits.push((b, p));
                w.write(b, p);
            }
            let bytes = w.finalize();
            let mut r = BoolReader::new(&bytes);
            for (i, &(b, p)) in bits.iter().enumerate() {
                assert_eq!(r.read(p), b, "trial {trial} bit {i} mismatch");
            }
        }
    }

    #[test]
    fn literal_round_trip_all_widths() {
        for bits in 1..=16u32 {
            let max = if bits == 32 {
                u32::MAX
            } else {
                (1u32 << bits) - 1
            };
            let values = [0u32, 1, max / 2, max - 1, max];
            let mut w = BoolWriter::new();
            for &v in &values {
                w.write_literal(v & max, bits);
            }
            let bytes = w.finalize();
            let mut r = BoolReader::new(&bytes);
            for &v in &values {
                assert_eq!(r.read_literal(bits), v & max, "width {bits} value {v}");
            }
        }
    }

    #[test]
    fn prob_extremes_do_not_panic_and_round_trip() {
        for &p in &[1u8, 2, 128, 200, 254, 255] {
            let mut w = BoolWriter::new();
            let seq: Vec<u8> = (0..300).map(|i| (i & 1) as u8).collect();
            for &b in &seq {
                w.write(b, p);
            }
            let bytes = w.finalize();
            let mut r = BoolReader::new(&bytes);
            for &b in &seq {
                assert_eq!(r.read(p), b);
            }
        }
    }
}
