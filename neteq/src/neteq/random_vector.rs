// Port target: WebRTC modules/audio_coding/neteq/random_vector.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (tests: random_vector_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! Table-driven pseudo-random noise used by Expand and Merge.

/// C++ `RandomVector`.
#[derive(Debug, Clone)]
pub struct RandomVector {
    seed: u32,
    seed_increment: i16,
}

impl Default for RandomVector {
    fn default() -> Self {
        Self::new()
    }
}

impl RandomVector {
    /// Size of the noise table.
    pub const K_RANDOM_TABLE_SIZE: usize = 256;

    /// C++ `kRandomTable`.
    #[rustfmt::skip]
    pub const K_RANDOM_TABLE: [i16; Self::K_RANDOM_TABLE_SIZE] = [
    2680, 5532, 441, 5520, 16170, -5146, -1024, -8733, 3115, 9598,
    -10380, -4959, -1280, -21716, 7133, -1522, 13458, -3902, 2789, -675,
    3441, 5016, -13599, -4003, -2739, 3922, -7209, 13352, -11617, -7241,
    12905, -2314, 5426, 10121, -9702, 11207, -13542, 1373, 816, -5934,
    -12504, 4798, 1811, 4112, -613, 201, -10367, -2960, -2419, 3442,
    4299, -6116, -6092, 1552, -1650, -480, -1237, 18720, -11858, -8303,
    -8212, 865, -2890, -16968, 12052, -5845, -5912, 9777, -5665, -6294,
    5426, -4737, -6335, 1652, 761, 3832, 641, -8552, -9084, -5753,
    8146, 12156, -4915, 15086, -1231, -1869, 11749, -9319, -6403, 11407,
    6232, -1683, 24340, -11166, 4017, -10448, 3153, -2936, 6212, 2891,
    -866, -404, -4807, -2324, -1917, -2388, -6470, -3895, -10300, 5323,
    -5403, 2205, 4640, 7022, -21186, -6244, -882, -10031, -3395, -12885,
    7155, -5339, 5079, -2645, -9515, 6622, 14651, 15852, 359, 122,
    8246, -3502, -6696, -3679, -13535, -1409, -704, -7403, -4007, 1798,
    279, -420, -12796, -14219, 1141, 3359, 11434, 7049, -6684, -7473,
    14283, -4115, -9123, -8969, 4152, 4117, 13792, 5742, 16168, 8661,
    -1609, -6095, 1881, 14380, -5588, 6758, -6425, -22969, -7269, 7031,
    1119, -1611, -5850, -11281, 3559, -8952, -10146, -4667, -16251, -1538,
    2062, -1012, -13073, 227, -3142, -5265, 20, 5770, -7559, 4740,
    -4819, 992, -8208, -7130, -4652, 6725, 7369, -1036, 13144, -1588,
    -5304, -2344, -449, -5705, -8894, 5205, -17904, -11188, -1022, 4852,
    10101, -5255, -4200, -752, 7941, -1543, 5959, 14719, 13346, 17045,
    -15605, -1678, -1600, -9230, 68, 23348, 1172, 7750, 11212, -18227,
    9956, 4161, 883, 3947, 4341, 1014, -4889, -2603, 1246, -5630,
    -3596, -870, -1298, 2784, -3317, -6612, -20541, 4166, 4181, -8625,
    3562, 12890, 4761, 3205, -12259, -8579,
    ];

    /// Seed 777, increment 1.
    pub fn new() -> Self {
        Self {
            seed: 777,
            seed_increment: 1,
        }
    }

    /// Back to seed 777, increment 1.
    pub fn reset(&mut self) {
        self.seed = 777;
        self.seed_increment = 1;
    }

    /// Writes `length` table samples into `output[..length]`.
    ///
    /// C++ `seed_ += seed_increment_` adds a sign-extended `int16_t` to a
    /// `uint32_t`, which wraps modulo 2^32.
    pub fn generate(&mut self, length: usize, output: &mut [i16]) {
        for out in output[..length].iter_mut() {
            self.seed = self.seed.wrapping_add(self.seed_increment as i32 as u32);
            let position = (self.seed as usize) & (Self::K_RANDOM_TABLE_SIZE - 1);
            *out = Self::K_RANDOM_TABLE[position];
        }
    }

    /// Adds `increase_by` to the seed increment (masked to the table size).
    ///
    /// The sum is an `int` stored back into `int16_t` (truncating; this
    /// conversion is implementation-defined before C++20 and modular on every
    /// supported compiler), then masked with 255.
    pub fn increase_seed_increment(&mut self, increase_by: i16) {
        self.seed_increment = (self.seed_increment as i32 + increase_by as i32) as i16;
        self.seed_increment &= (Self::K_RANDOM_TABLE_SIZE - 1) as i16;
    }

    /// Current seed increment.
    pub fn seed_increment(&self) -> i16 {
        self.seed_increment
    }

    /// Sets the seed increment.
    pub fn set_seed_increment(&mut self, value: i16) {
        self.seed_increment = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spl::test_util::{Hash, Rng};

    // random_vector_unittest.cc: RandomVector.CreateAndDestroy
    #[test]
    fn create_and_destroy() {
        let _random_vector = RandomVector::new();
    }

    // Not in WebRTC: pins the sequence.
    #[test]
    fn generate_sequence() {
        let mut random_vector = RandomVector::new();
        let mut output = [0i16; 3];
        random_vector.generate(3, &mut output);
        // Seeds 778, 779, 780 -> table positions 10, 11, 12.
        assert_eq!([-10380, -4959, -1280], output);

        random_vector.increase_seed_increment(254);
        assert_eq!(255, random_vector.seed_increment());
        random_vector.increase_seed_increment(2);
        assert_eq!(1, random_vector.seed_increment());

        // A negative increment walks the seed backwards (wrapping).
        random_vector.set_seed_increment(-781);
        random_vector.generate(1, &mut output);
        assert_eq!(RandomVector::K_RANDOM_TABLE[255], output[0]);

        random_vector.reset();
        random_vector.generate(1, &mut output);
        assert_eq!(-10380, output[0]);
    }

    // Golden hash against the real C++ (see spl/golden/golden_gen.cc).
    #[test]
    fn golden_random_vector_ops() {
        let mut r = Rng::new();
        let mut h = Hash::new();
        let mut rv = RandomVector::new();
        let mut out = [0i16; 50];
        for _ in 0..4000 {
            match r.below(5) {
                0 | 1 => {
                    let len = r.below(50);
                    rv.generate(len, &mut out);
                    h.feed16(&out[..len]);
                }
                2 => rv.increase_seed_increment(r.range(-300, 300) as i16),
                3 => rv.set_seed_increment(r.i16()),
                4 => {
                    if r.below(10) == 0 {
                        rv.reset();
                    }
                }
                _ => unreachable!(),
            }
            h.feed(rv.seed_increment() as i64);
        }
        assert_eq!(0xd1783307aaf47cb1, h.value());
    }
}
