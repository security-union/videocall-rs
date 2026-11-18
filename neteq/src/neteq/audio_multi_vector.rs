// Port target: WebRTC modules/audio_coding/neteq/audio_multi_vector.{h,cc} at
// rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd
// (tests: audio_multi_vector_unittest.cc).
// Copyright (c) 2013 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! One [`AudioVector`] per channel, with interleaved read/write helpers.

use std::ops::{Index, IndexMut};

use crate::neteq::audio_vector::AudioVector;

/// C++ `kMaxNumberOfAudioChannels` (api/audio/audio_frame.h).
const MAX_NUMBER_OF_AUDIO_CHANNELS: usize = 24;

/// C++ `AudioMultiVector`.
#[derive(Debug)]
pub struct AudioMultiVector {
    pub(crate) channels: Vec<AudioVector>,
}

/// C++ `InitializeChannelVector`: `channel_size == 0` means the default
/// (empty) `AudioVector`, otherwise `channel_size` zeros.
fn initialize_channel_vector(num_channels: usize, channel_size: usize) -> Vec<AudioVector> {
    debug_assert!(num_channels > 0);
    assert!(num_channels <= MAX_NUMBER_OF_AUDIO_CHANNELS);
    (0..num_channels)
        .map(|_| {
            if channel_size != 0 {
                AudioVector::with_size(channel_size)
            } else {
                AudioVector::new()
            }
        })
        .collect()
}

impl AudioMultiVector {
    /// `n` empty channels. `n` must be at least 1.
    pub fn new(n: usize) -> Self {
        Self {
            channels: initialize_channel_vector(n, 0),
        }
    }

    /// `n` channels of `initial_size` zeros each.
    pub fn with_size(n: usize, initial_size: usize) -> Self {
        Self {
            channels: initialize_channel_vector(n, initial_size),
        }
    }

    /// Deletes all values and makes the vector empty.
    pub fn clear(&mut self) {
        for c in &mut self.channels {
            c.clear();
        }
    }

    /// Clears and sets each channel to `length` zeros.
    pub fn zeros(&mut self, length: usize) {
        for c in &mut self.channels {
            c.clear();
            c.extend(length);
        }
    }

    /// Copies all values into `copy_to` (same channel count).
    pub fn copy_to(&self, copy_to: &mut AudioMultiVector) {
        for (i, c) in self.channels.iter().enumerate() {
            c.copy_to(&mut copy_to[i]);
        }
    }

    /// Appends interleaved samples; `append_this.len()` must be a multiple
    /// of `channels()`.
    pub fn push_back_interleaved(&mut self, append_this: &[i16]) {
        debug_assert_eq!(append_this.len() % self.channels(), 0);
        if append_this.is_empty() {
            return;
        }
        if self.channels() == 1 {
            // Special case to avoid extra allocation and data shuffling.
            self.channels[0].push_back_slice(append_this);
            return;
        }
        let num_channels = self.channels();
        let length_per_channel = append_this.len() / num_channels;
        let mut temp_array = vec![0i16; length_per_channel];
        for (channel, c) in self.channels.iter_mut().enumerate() {
            for (i, sample) in temp_array.iter_mut().enumerate() {
                *sample = append_this[channel + i * num_channels];
            }
            c.push_back_slice(&temp_array);
        }
    }

    /// Appends `append_this` (same channel count). `SyncBuffer` shadows this.
    pub fn push_back(&mut self, append_this: &AudioMultiVector) {
        debug_assert_eq!(self.channels(), append_this.channels());
        if self.channels() == append_this.channels() {
            for (c, other) in self.channels.iter_mut().zip(&append_this.channels) {
                c.push_back(other);
            }
        }
    }

    /// Appends `append_this` from `index` to its end.
    pub fn push_back_from_index(&mut self, append_this: &AudioMultiVector, index: usize) {
        debug_assert!(index < append_this.size());
        // C++ `Size() - 1` is `size_t`; an empty source wraps like C++ would.
        let index = index.min(append_this.size().wrapping_sub(1));
        let length = append_this.size().wrapping_sub(index);
        debug_assert_eq!(self.channels(), append_this.channels());
        if self.channels() == append_this.channels() {
            for (c, other) in self.channels.iter_mut().zip(&append_this.channels) {
                c.push_back_range(other, length, index);
            }
        }
    }

    /// Removes `length` elements from the beginning of each channel.
    pub fn pop_front(&mut self, length: usize) {
        for c in &mut self.channels {
            c.pop_front(length);
        }
    }

    /// Removes `length` elements from the end of each channel.
    pub fn pop_back(&mut self, length: usize) {
        for c in &mut self.channels {
            c.pop_back(length);
        }
    }

    /// Reads `length` samples per channel from the start, interleaved, into
    /// `destination`. Returns the number of samples written.
    pub fn read_interleaved(&self, length: usize, destination: &mut [i16]) -> usize {
        self.read_interleaved_from_index(0, length, destination)
    }

    /// Like `read_interleaved`, starting at `start_index`.
    pub fn read_interleaved_from_index(
        &self,
        start_index: usize,
        length: usize,
        destination: &mut [i16],
    ) -> usize {
        debug_assert!(start_index <= self.size());
        let start_index = start_index.min(self.size());
        let mut length = length;
        if length + start_index > self.size() {
            length = self.size() - start_index;
        }
        if self.channels() == 1 {
            // Special case to avoid the nested for loop below.
            self.channels[0].copy_to_slice(length, start_index, destination);
            return length;
        }
        let mut index = 0;
        for i in 0..length {
            for c in &self.channels {
                destination[index] = c[i + start_index];
                index += 1;
            }
        }
        index
    }

    /// C++ `ReadInterleavedFromIndex(start_index, InterleavedView dst)`:
    /// fills all of `dst` (`dst.len() / channels()` samples per channel).
    /// Returns false if there are not enough samples.
    pub fn read_interleaved_from_index_into(&self, start_index: usize, dst: &mut [i16]) -> bool {
        debug_assert_eq!(dst.len() % self.channels(), 0);
        let samples_per_channel = dst.len() / self.channels();
        if start_index + samples_per_channel > self.size() {
            return false;
        }
        if self.channels() == 1 {
            // Special case to avoid the nested for loop below.
            return self.channels[0].copy_to_view(start_index, dst);
        }
        let mut index = 0;
        for i in 0..samples_per_channel {
            for c in &self.channels {
                dst[index] = c[i + start_index];
                index += 1;
            }
        }
        true
    }

    /// Reads the last `length` samples per channel, interleaved.
    pub fn read_interleaved_from_end(&self, length: usize, destination: &mut [i16]) -> usize {
        let length = length.min(self.size()); // Cannot read more than size().
        self.read_interleaved_from_index(self.size() - length, length, destination)
    }

    /// Overwrites `length` samples per channel from `position` with
    /// `insert_this`, extending if needed.
    pub fn overwrite_at(&mut self, insert_this: &AudioMultiVector, length: usize, position: usize) {
        debug_assert_eq!(self.channels(), insert_this.channels());
        // Cap `length` at the length of `insert_this`.
        debug_assert!(length <= insert_this.size());
        let length = length.min(insert_this.size());
        if self.channels() == insert_this.channels() {
            for (c, other) in self.channels.iter_mut().zip(&insert_this.channels) {
                c.overwrite_at(other, length, position);
            }
        }
    }

    /// Appends `append_this` with a cross-fade of `fade_length` samples.
    pub fn cross_fade(&mut self, append_this: &AudioMultiVector, fade_length: usize) {
        debug_assert_eq!(self.channels(), append_this.channels());
        if self.channels() == append_this.channels() {
            for (c, other) in self.channels.iter_mut().zip(&append_this.channels) {
                c.cross_fade(other, fade_length);
            }
        }
    }

    /// Number of channels.
    pub fn channels(&self) -> usize {
        self.channels.len()
    }

    /// Samples per channel.
    pub fn size(&self) -> usize {
        self.channels[0].size()
    }

    /// Grows every channel to at least `required_size` (zero-filled).
    pub fn assert_size(&mut self, required_size: usize) {
        if self.size() < required_size {
            let extend_length = required_size - self.size();
            for c in &mut self.channels {
                c.extend(extend_length);
            }
        }
    }

    /// True if there are no samples.
    pub fn empty(&self) -> bool {
        self.channels[0].empty()
    }

    /// Copies channel `from_channel` over channel `to_channel`.
    pub fn copy_channel(&mut self, from_channel: usize, to_channel: usize) {
        debug_assert!(from_channel < self.channels());
        debug_assert!(to_channel < self.channels());
        if from_channel == to_channel {
            // C++ copies the channel onto itself through aliased pointers.
            self.channels[from_channel].copy_to_itself();
            return;
        }
        let (source, destination) = if from_channel < to_channel {
            let (low, high) = self.channels.split_at_mut(to_channel);
            (&low[from_channel], &mut high[0])
        } else {
            let (low, high) = self.channels.split_at_mut(from_channel);
            (&high[0], &mut low[to_channel])
        };
        source.copy_to(destination);
    }
}

impl Index<usize> for AudioMultiVector {
    type Output = AudioVector;
    fn index(&self, index: usize) -> &AudioVector {
        &self.channels[index]
    }
}

impl IndexMut<usize> for AudioMultiVector {
    fn index_mut(&mut self, index: usize) -> &mut AudioVector {
        &mut self.channels[index]
    }
}

/// Golden-test helpers mirroring `fill_multi` / `feed_multi` in
/// `spl/golden/golden_gen.cc`.
#[cfg(test)]
pub(crate) mod golden {
    use super::AudioMultiVector;
    use crate::neteq::audio_vector::golden::feed_vec;
    use crate::spl::test_util::{Hash, Rng};

    pub(crate) fn feed_multi(h: &mut Hash, v: &AudioMultiVector) {
        h.feed(v.channels() as i64);
        for c in 0..v.channels() {
            feed_vec(h, &v[c]);
        }
    }

    pub(crate) fn fill_multi(r: &mut Rng, v: &mut AudioMultiVector, max_len: i32) {
        v.clear();
        let len = r.range(0, max_len) as usize * v.channels();
        let d = r.vec16(len);
        v.push_back_interleaved(&d);
    }
}

#[cfg(test)]
mod tests {
    use super::golden::{feed_multi, fill_multi};
    use super::*;
    use crate::spl::test_util::{Hash, Rng};

    const ARRAY_LENGTH: usize = 10;

    /// The C++ fixture, parameterized on the channel count (1, 2 and 5).
    struct Fixture {
        num_channels: usize,
        array_interleaved: Vec<i16>,
    }

    impl Fixture {
        fn new(num_channels: usize) -> Self {
            let mut array_interleaved = Vec::with_capacity(num_channels * ARRAY_LENGTH);
            for i in 0..ARRAY_LENGTH {
                for j in 1..=num_channels {
                    array_interleaved.push((j * 100 + i) as i16);
                }
            }
            Self {
                num_channels,
                array_interleaved,
            }
        }
    }

    fn for_each_channel_count(test: impl Fn(&Fixture)) {
        for n in [1, 2, 5] {
            test(&Fixture::new(n));
        }
    }

    #[test]
    fn create_and_destroy() {
        for_each_channel_count(|f| {
            let vec1 = AudioMultiVector::new(f.num_channels);
            assert!(vec1.empty());
            assert_eq!(f.num_channels, vec1.channels());
            assert_eq!(0, vec1.size());

            let vec2 = AudioMultiVector::with_size(f.num_channels, 17);
            assert!(!vec2.empty());
            assert_eq!(f.num_channels, vec2.channels());
            assert_eq!(17, vec2.size());
        });
    }

    #[test]
    fn subscript_operator() {
        for_each_channel_count(|f| {
            let mut vec = AudioMultiVector::with_size(f.num_channels, ARRAY_LENGTH);
            for channel in 0..f.num_channels {
                for i in 0..ARRAY_LENGTH {
                    vec[channel][i] = i as i16;
                    let audio_vec = &vec[channel];
                    assert_eq!(i as i16, audio_vec[i]);
                }
            }
        });
    }

    #[test]
    fn push_back_interleaved_and_copy() {
        for_each_channel_count(|f| {
            let mut vec = AudioMultiVector::new(f.num_channels);
            vec.push_back_interleaved(&f.array_interleaved);
            let mut vec_copy = AudioMultiVector::new(f.num_channels);
            vec.copy_to(&mut vec_copy);
            assert_eq!(f.num_channels, vec.channels());
            assert_eq!(ARRAY_LENGTH, vec.size());
            assert_eq!(f.num_channels, vec_copy.channels());
            assert_eq!(ARRAY_LENGTH, vec_copy.size());
            for channel in 0..vec.channels() {
                for i in 0..ARRAY_LENGTH {
                    assert_eq!(((channel + 1) * 100 + i) as i16, vec[channel][i]);
                    assert_eq!(vec[channel][i], vec_copy[channel][i]);
                }
            }

            vec.clear();
            assert!(vec.empty());

            vec.copy_to(&mut vec_copy);
            assert!(vec_copy.empty());
        });
    }

    // C++ CopyToNull passes a null pointer; a Rust reference cannot be null,
    // so there is nothing to port.

    #[test]
    fn push_back_vector() {
        for_each_channel_count(|f| {
            let mut vec1 = AudioMultiVector::with_size(f.num_channels, ARRAY_LENGTH);
            let mut vec2 = AudioMultiVector::with_size(f.num_channels, ARRAY_LENGTH);
            for channel in 0..f.num_channels {
                for i in 0..ARRAY_LENGTH {
                    vec1[channel][i] = (i + 100 * channel) as i16;
                    vec2[channel][i] = (i + 100 * channel + ARRAY_LENGTH) as i16;
                }
            }
            vec1.push_back(&vec2);
            assert_eq!(2 * ARRAY_LENGTH, vec1.size());
            for channel in 0..f.num_channels {
                for i in 0..2 * ARRAY_LENGTH {
                    assert_eq!((i + 100 * channel) as i16, vec1[channel][i]);
                }
            }
        });
    }

    #[test]
    fn push_back_from_index() {
        for_each_channel_count(|f| {
            let mut vec1 = AudioMultiVector::new(f.num_channels);
            vec1.push_back_interleaved(&f.array_interleaved);
            let mut vec2 = AudioMultiVector::new(f.num_channels);

            vec2.push_back_from_index(&vec1, ARRAY_LENGTH - 2);
            assert_eq!(2, vec2.size());
            for channel in 0..f.num_channels {
                for i in 0..2 {
                    assert_eq!(
                        f.array_interleaved[channel + f.num_channels * (ARRAY_LENGTH - 2 + i)],
                        vec2[channel][i]
                    );
                }
            }
        });
    }

    #[test]
    fn zeros() {
        for_each_channel_count(|f| {
            let mut vec = AudioMultiVector::new(f.num_channels);
            vec.push_back_interleaved(&f.array_interleaved);
            vec.zeros(2 * ARRAY_LENGTH);
            assert_eq!(f.num_channels, vec.channels());
            assert_eq!(2 * ARRAY_LENGTH, vec.size());
            for channel in 0..f.num_channels {
                for i in 0..2 * ARRAY_LENGTH {
                    assert_eq!(0, vec[channel][i]);
                }
            }
        });
    }

    #[test]
    fn read_interleaved() {
        for_each_channel_count(|f| {
            let mut vec = AudioMultiVector::new(f.num_channels);
            vec.push_back_interleaved(&f.array_interleaved);
            let mut output = vec![0i16; f.array_interleaved.len()];
            let read_samples = 5;
            assert_eq!(
                f.num_channels * read_samples,
                vec.read_interleaved(read_samples, &mut output)
            );
            assert_eq!(f.array_interleaved[..read_samples], output[..read_samples]);

            // Read too many samples. Expect to get all samples from the vector.
            assert_eq!(
                f.array_interleaved.len(),
                vec.read_interleaved(ARRAY_LENGTH + 1, &mut output)
            );
            assert_eq!(f.array_interleaved[..read_samples], output[..read_samples]);
        });
    }

    #[test]
    fn read_interleaved_view() {
        for_each_channel_count(|f| {
            let mut vec = AudioMultiVector::new(f.num_channels);
            vec.push_back_interleaved(&f.array_interleaved);
            let samples_per_channel = 5;
            let mut buffer = vec![0i16; samples_per_channel * f.num_channels];
            assert!(vec.read_interleaved_from_index_into(0, &mut buffer));
            assert_eq!(f.array_interleaved[..buffer.len()], buffer[..]);
            assert!(!vec.read_interleaved_from_index_into(vec.size() - 4, &mut buffer));
        });
    }

    #[test]
    fn pop_front() {
        for_each_channel_count(|f| {
            let mut vec = AudioMultiVector::new(f.num_channels);
            vec.push_back_interleaved(&f.array_interleaved);
            vec.pop_front(1);
            assert_eq!(ARRAY_LENGTH - 1, vec.size());
            let mut ptr = f.num_channels;
            for i in 0..ARRAY_LENGTH - 1 {
                for channel in 0..f.num_channels {
                    assert_eq!(f.array_interleaved[ptr], vec[channel][i]);
                    ptr += 1;
                }
            }
            vec.pop_front(ARRAY_LENGTH);
            assert_eq!(0, vec.size());
        });
    }

    #[test]
    fn pop_back() {
        for_each_channel_count(|f| {
            let mut vec = AudioMultiVector::new(f.num_channels);
            vec.push_back_interleaved(&f.array_interleaved);
            vec.pop_back(1);
            assert_eq!(ARRAY_LENGTH - 1, vec.size());
            let mut ptr = 0;
            for i in 0..ARRAY_LENGTH - 1 {
                for channel in 0..f.num_channels {
                    assert_eq!(f.array_interleaved[ptr], vec[channel][i]);
                    ptr += 1;
                }
            }
            vec.pop_back(ARRAY_LENGTH);
            assert_eq!(0, vec.size());
        });
    }

    #[test]
    fn assert_size() {
        for_each_channel_count(|f| {
            let mut vec = AudioMultiVector::with_size(f.num_channels, ARRAY_LENGTH);
            assert_eq!(ARRAY_LENGTH, vec.size());
            vec.assert_size(0);
            vec.assert_size(ARRAY_LENGTH - 1);
            assert_eq!(ARRAY_LENGTH, vec.size());
            vec.assert_size(ARRAY_LENGTH + 1);
            assert_eq!(ARRAY_LENGTH + 1, vec.size());
            for channel in 0..vec.channels() {
                assert_eq!(ARRAY_LENGTH + 1, vec[channel].size());
            }
        });
    }

    #[test]
    fn overwrite_at() {
        for_each_channel_count(|f| {
            let mut vec1 = AudioMultiVector::new(f.num_channels);
            vec1.push_back_interleaved(&f.array_interleaved);
            let mut vec2 = AudioMultiVector::new(f.num_channels);
            vec2.zeros(3);
            vec1.overwrite_at(&vec2, 3, 5);
            assert_eq!(ARRAY_LENGTH, vec1.size());
            let mut ptr = 0;
            for i in 0..ARRAY_LENGTH - 1 {
                for channel in 0..f.num_channels {
                    if (5..=7).contains(&i) {
                        assert_eq!(0, vec1[channel][i]);
                    } else {
                        assert_eq!(f.array_interleaved[ptr], vec1[channel][i]);
                    }
                    ptr += 1;
                }
            }
        });
    }

    #[test]
    fn copy_channel() {
        for_each_channel_count(|f| {
            if f.num_channels < 2 {
                return;
            }
            let mut vec = AudioMultiVector::new(f.num_channels);
            vec.push_back_interleaved(&f.array_interleaved);
            let mut reference = AudioMultiVector::new(f.num_channels);
            reference.push_back(&vec);
            vec.copy_channel(0, f.num_channels - 1);
            for i in 0..ARRAY_LENGTH {
                for channel in 0..f.num_channels - 1 {
                    assert_eq!(reference[channel][i], vec[channel][i]);
                }
                assert_eq!(vec[0][i], vec[f.num_channels - 1][i]);
            }
            // Not in WebRTC: copying downwards.
            // With two channels, channel 1 already holds channel 0's data.
            let source = if f.num_channels == 2 { 0 } else { 1 };
            vec.copy_channel(1, 0);
            for i in 0..ARRAY_LENGTH {
                assert_eq!(reference[source][i], vec[0][i]);
                assert_eq!(reference[source][i], vec[1][i]);
            }
        });
    }

    #[test]
    fn push_back_empty_array() {
        for_each_channel_count(|f| {
            let mut vec = AudioMultiVector::new(f.num_channels);
            vec.push_back_interleaved(&[]);
            assert!(vec.empty());
        });
    }

    // Golden hash against the real C++ (see spl/golden/golden_gen.cc).
    #[test]
    fn golden_audio_multi_vector_ops() {
        let mut r = Rng::new();
        let mut h = Hash::new();
        for _ in 0..300 {
            let ch = r.urange(1, 3);
            let vec_default = AudioMultiVector::new(ch);
            let vec_sized = AudioMultiVector::with_size(ch, r.urange(0, 20));
            let mut vec = if r.below(2) != 0 {
                vec_default
            } else {
                vec_sized
            };
            let mut other = AudioMultiVector::new(ch);
            fill_multi(&mut r, &mut other, 30);
            for _ in 0..40 {
                let size = vec.size();
                match r.below(16) {
                    0 => {
                        let len = r.urange(0, 30) * ch;
                        let d = r.vec16(len);
                        vec.push_back_interleaved(&d);
                    }
                    1 => vec.push_back(&other),
                    2 => {
                        if other.size() > 0 {
                            let index = r.below(other.size());
                            vec.push_back_from_index(&other, index);
                        }
                    }
                    3 => vec.pop_front(r.below(size + 5)),
                    4 => vec.pop_back(r.below(size + 5)),
                    5 => vec.zeros(r.below(30)),
                    6 => vec.assert_size(r.below(size + 20)),
                    7 => {
                        let len = r.below(other.size() + 1);
                        let pos = r.below(size + 5);
                        vec.overwrite_at(&other, len, pos);
                    }
                    8 => {
                        let fade = r.below(size.min(other.size()) + 1);
                        vec.cross_fade(&other, fade);
                    }
                    9 => {
                        // Distinct channels only, see golden_gen.cc.
                        if ch > 1 {
                            let from = r.below(ch);
                            let to = (from + 1 + r.below(ch - 1)) % ch;
                            vec.copy_channel(from, to);
                        }
                    }
                    10 => {
                        let len = r.below(size + 3);
                        let mut dst = vec![0i16; (size + 3) * ch];
                        h.feed(vec.read_interleaved(len, &mut dst) as i64);
                        h.feed16(&dst);
                    }
                    11 => {
                        let start = r.below(size + 1);
                        let len = r.below(size + 3);
                        let mut dst = vec![0i16; (size + 3) * ch];
                        h.feed(vec.read_interleaved_from_index(start, len, &mut dst) as i64);
                        h.feed16(&dst);
                    }
                    12 => {
                        let start = r.below(size + 2);
                        let spc = r.below(10);
                        let mut dst = vec![0i16; spc * ch];
                        let ok = vec.read_interleaved_from_index_into(start, &mut dst);
                        h.feed(ok as i64);
                        if ok {
                            h.feed16(&dst);
                        }
                    }
                    13 => {
                        let len = r.below(size + 3);
                        let mut dst = vec![0i16; (size + 3) * ch];
                        h.feed(vec.read_interleaved_from_end(len, &mut dst) as i64);
                        h.feed16(&dst);
                    }
                    14 => vec.copy_to(&mut other),
                    15 => fill_multi(&mut r, &mut other, 30),
                    _ => unreachable!(),
                }
                feed_multi(&mut h, &vec);
            }
        }
        assert_eq!(0x744b19c960660be0, h.value());
    }
}
