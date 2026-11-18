// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/signal_processing/copy_set_operations.c at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! `WebRtcSpl_MemSetW16`, `MemSetW32`, `CopyFromEndW16`, `ZerosArrayW16/W32`.

/// C `WebRtcSpl_MemSetW16`.
pub fn mem_set_w16(vector: &mut [i16], set_value: i16, vector_length: usize) {
    vector[..vector_length].fill(set_value);
}

/// C `WebRtcSpl_MemSetW32`.
pub fn mem_set_w32(vector: &mut [i32], set_value: i32, vector_length: usize) {
    vector[..vector_length].fill(set_value);
}

/// C `WebRtcSpl_ZerosArrayW16`.
pub fn zeros_array_w16(vector: &mut [i16], vector_length: usize) {
    mem_set_w16(vector, 0, vector_length);
}

/// C `WebRtcSpl_ZerosArrayW32`.
pub fn zeros_array_w32(vector: &mut [i32], vector_length: usize) {
    mem_set_w32(vector, 0, vector_length);
}

/// C `WebRtcSpl_CopyFromEndW16`: copies the last `samples` of
/// `vector_in[..length]` into `vector_out[..samples]`.
pub fn copy_from_end_w16(vector_in: &[i16], length: usize, samples: usize, vector_out: &mut [i16]) {
    vector_out[..samples].copy_from_slice(&vector_in[length - samples..length]);
}

#[cfg(test)]
mod tests {
    use super::*;

    // signal_processing_unittest.cc: SplTest.BasicArrayOperationsTest (the
    // copy/set part).
    #[test]
    fn basic_array_operations_test() {
        const N: usize = 4;
        let mut b16 = [0i16; N];
        let mut b32 = [0i32; N];
        mem_set_w16(&mut b16, 3, N);
        assert_eq!([3; N], b16);
        zeros_array_w16(&mut b16, N);
        assert_eq!([0; N], b16);
        mem_set_w32(&mut b32, 3, N);
        assert_eq!([3; N], b32);
        zeros_array_w32(&mut b32, N);
        assert_eq!([0; N], b32);

        let b16: [i16; N] = [0, 1, 2, 3];
        let mut tmp16 = [0i16; N];
        copy_from_end_w16(&b16, N, 2, &mut tmp16);
        assert_eq!([2, 3], tmp16[..2]);
    }
}
