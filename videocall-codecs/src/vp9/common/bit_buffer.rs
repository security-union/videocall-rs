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

//! Raw (non-arithmetic) MSB-first bit buffer for the VP9 uncompressed header.
//!
//! Port of libvpx `vpx_dsp/bitwriter_buffer.{h,c}` plus the matching reader
//! `vpx_dsp/bitreader_buffer.{h,c}`. Bits are packed most-significant-first
//! within each byte. The writer owns a growing `Vec<u8>`.

/// MSB-first raw-bit writer. Mirrors `struct vpx_write_bit_buffer`.
pub struct BitBufferWriter {
    buffer: Vec<u8>,
    bit_offset: usize,
}

impl BitBufferWriter {
    /// Create an empty writer.
    pub fn new() -> Self {
        BitBufferWriter {
            buffer: Vec::new(),
            bit_offset: 0,
        }
    }

    /// Number of whole bytes needed to hold the bits written so far
    /// (`vpx_wb_bytes_written`: rounds partial trailing bytes up).
    pub fn bytes_written(&self) -> usize {
        self.bit_offset.div_ceil(8)
    }

    /// Current bit position (for backpatch bookkeeping).
    pub fn bit_offset(&self) -> usize {
        self.bit_offset
    }

    /// Write a single bit, MSB-first within the current byte. `vpx_wb_write_bit`.
    pub fn write_bit(&mut self, bit: u8) {
        let off = self.bit_offset;
        let p = off / 8;
        let q = 7 - (off % 8);
        if q == 7 {
            // Starting a fresh byte.
            self.buffer.push((bit & 1) << q);
        } else {
            self.buffer[p] |= (bit & 1) << q;
        }
        self.bit_offset = off + 1;
    }

    /// Write the low `bits` of `data`, MSB-first. `vpx_wb_write_literal`.
    pub fn write_literal(&mut self, data: u32, bits: u32) {
        for bit in (0..bits).rev() {
            self.write_bit(((data >> bit) & 1) as u8);
        }
    }

    /// Write a sign-magnitude value: magnitude in `bits` bits then a sign bit.
    /// Mirrors `vpx_wb_write_inv_signed_literal`.
    pub fn write_signed(&mut self, data: i32, bits: u32) {
        self.write_literal(data.unsigned_abs(), bits);
        self.write_bit(u8::from(data < 0));
    }

    /// Overwrite `bits` bits (MSB-first) starting at `start_bit` with `value`.
    /// Used to backpatch a size field written earlier as zeros; the target bits
    /// must currently be zero (the bits are OR-ed in). Mirrors libvpx's
    /// `saved_wb` backpatch of the compressed-header size.
    pub fn backpatch_literal(&mut self, start_bit: usize, value: u32, bits: u32) {
        for i in 0..bits {
            let bit = ((value >> (bits - 1 - i)) & 1) as u8;
            let off = start_bit + i as usize;
            let p = off / 8;
            let q = 7 - (off % 8);
            self.buffer[p] |= bit << q;
        }
    }

    /// Consume the writer and return the packed bytes (trailing partial byte,
    /// if any, is already present with its low bits zero).
    pub fn finalize(self) -> Vec<u8> {
        self.buffer
    }

    /// Borrow the bytes written so far without consuming the writer.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buffer
    }
}

impl Default for BitBufferWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// MSB-first raw-bit reader. Mirrors `struct vpx_read_bit_buffer`. Reads past the
/// end return zero bits (matching libvpx's error-handler-less default).
pub struct BitBufferReader<'a> {
    buffer: &'a [u8],
    bit_offset: usize,
}

impl<'a> BitBufferReader<'a> {
    /// Create a reader over `buffer`.
    pub fn new(buffer: &'a [u8]) -> Self {
        BitBufferReader {
            buffer,
            bit_offset: 0,
        }
    }

    /// Bytes consumed so far, rounded up. `vpx_rb_bytes_read`.
    pub fn bytes_read(&self) -> usize {
        (self.bit_offset + 7) >> 3
    }

    /// Read a single bit. `vpx_rb_read_bit`.
    pub fn read_bit(&mut self) -> u8 {
        let off = self.bit_offset;
        let p = off >> 3;
        let q = 7 - (off & 0x7);
        if p < self.buffer.len() {
            self.bit_offset = off + 1;
            (self.buffer[p] >> q) & 1
        } else {
            0
        }
    }

    /// Read `bits` bits, MSB-first. `vpx_rb_read_literal`.
    pub fn read_literal(&mut self, bits: u32) -> u32 {
        let mut value = 0u32;
        for _ in 0..bits {
            value = (value << 1) | self.read_bit() as u32;
        }
        value
    }

    /// Read a sign-magnitude value written by [`BitBufferWriter::write_signed`].
    pub fn read_signed(&mut self, bits: u32) -> i32 {
        let value = self.read_literal(bits) as i32;
        if self.read_bit() != 0 {
            -value
        } else {
            value
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_round_trip() {
        let bits = [1u8, 0, 1, 1, 0, 0, 0, 1, 1, 1, 0, 1, 0];
        let mut w = BitBufferWriter::new();
        for &b in &bits {
            w.write_bit(b);
        }
        let bytes = w.finalize();
        let mut r = BitBufferReader::new(&bytes);
        for &b in &bits {
            assert_eq!(r.read_bit(), b);
        }
    }

    #[test]
    fn literal_msb_first_layout() {
        // 0xA (1010) in 4 bits then 0x3 (11) in 2 bits packed MSB-first from the
        // top of the first byte: 1010_11.. => 0xAC.
        let mut w = BitBufferWriter::new();
        w.write_literal(0xA, 4);
        w.write_literal(0x3, 2);
        let bytes = w.finalize();
        assert_eq!(bytes[0], 0b1010_1100);
    }

    #[test]
    fn literal_round_trip_all_widths() {
        for bits in 1..=16u32 {
            let max = (1u32 << bits) - 1;
            let values = [0u32, 1, max / 3, max - 1, max];
            let mut w = BitBufferWriter::new();
            for &v in &values {
                w.write_literal(v & max, bits);
            }
            let bytes = w.finalize();
            let mut r = BitBufferReader::new(&bytes);
            for &v in &values {
                assert_eq!(r.read_literal(bits), v & max, "width {bits}");
            }
        }
    }

    #[test]
    fn signed_round_trip() {
        let mut w = BitBufferWriter::new();
        let vals = [0i32, 1, -1, 7, -7, 15, -15];
        for &v in &vals {
            w.write_signed(v, 4);
        }
        let bytes = w.finalize();
        let mut r = BitBufferReader::new(&bytes);
        for &v in &vals {
            assert_eq!(r.read_signed(4), v);
        }
    }

    #[test]
    fn bytes_written_rounds_up() {
        let mut w = BitBufferWriter::new();
        w.write_literal(0, 3);
        assert_eq!(w.bytes_written(), 1);
        w.write_literal(0, 5);
        assert_eq!(w.bytes_written(), 1);
        w.write_bit(0);
        assert_eq!(w.bytes_written(), 2);
    }
}
