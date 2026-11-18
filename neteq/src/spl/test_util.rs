// Test-only helpers shared by the golden tests (`spl::golden_tests` and the
// NetEq vector/helper tests). They mirror `Rng` and `Hash` in
// `golden/golden_gen.cc` exactly, so both sides draw the same inputs.

//! xorshift32 input stream and FNV-1a output hash for golden tests.

/// Seed used by every golden test (C++ `kSeed`).
pub(crate) const SEED: u32 = 0x9e37_79b9;

/// C++ `Rng` in `golden_gen.cc`.
pub(crate) struct Rng {
    s: u32,
}

impl Rng {
    pub(crate) fn new() -> Self {
        Self { s: SEED }
    }

    pub(crate) fn next(&mut self) -> u32 {
        self.s ^= self.s << 13;
        self.s ^= self.s >> 17;
        self.s ^= self.s << 5;
        self.s
    }

    pub(crate) fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u32) as usize
    }

    /// Inclusive range, like C++ `range(lo, hi)`.
    pub(crate) fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next() % (hi - lo + 1) as u32) as i32
    }

    pub(crate) fn urange(&mut self, lo: usize, hi: usize) -> usize {
        self.range(lo as i32, hi as i32) as usize
    }

    pub(crate) fn i16(&mut self) -> i16 {
        match self.next() % 8 {
            0 => -32768,
            1 => 32767,
            2 | 3 => self.next() as i16,
            _ => ((self.next() % 2001) as i32 - 1000) as i16,
        }
    }

    pub(crate) fn i32(&mut self) -> i32 {
        match self.next() % 6 {
            0 => i32::MIN,
            1 => i32::MAX,
            2 | 3 => self.next() as i32,
            _ => (self.next() % 200001) as i32 - 100000,
        }
    }

    pub(crate) fn fill16(&mut self, v: &mut [i16]) {
        for x in v {
            *x = self.i16();
        }
    }

    pub(crate) fn fill32(&mut self, v: &mut [i32]) {
        for x in v {
            *x = self.i32();
        }
    }

    pub(crate) fn vec16(&mut self, len: usize) -> Vec<i16> {
        let mut v = vec![0; len];
        self.fill16(&mut v);
        v
    }
}

/// C++ `Hash` in `golden_gen.cc` (FNV-1a over sign-extended values).
pub(crate) struct Hash {
    h: u64,
}

impl Hash {
    pub(crate) fn new() -> Self {
        Self {
            h: 0xcbf2_9ce4_8422_2325,
        }
    }

    pub(crate) fn feed(&mut self, v: i64) {
        self.h ^= v as u64;
        self.h = self.h.wrapping_mul(0x0000_0100_0000_01b3);
    }

    pub(crate) fn feed16(&mut self, v: &[i16]) {
        for &x in v {
            self.feed(x as i64);
        }
    }

    pub(crate) fn feed32(&mut self, v: &[i32]) {
        for &x in v {
            self.feed(x as i64);
        }
    }

    pub(crate) fn value(&self) -> u64 {
        self.h
    }
}
