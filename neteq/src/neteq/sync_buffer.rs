// Port target: WebRTC modules/audio_coding/neteq/sync_buffer.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (tests: sync_buffer_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// C++ `SyncBuffer` inherits from `AudioMultiVector`; here it derefs to one.
// `push_back` and `push_back_interleaved` are inherent methods that shadow
// the `AudioMultiVector` ones, like the C++ override.

//! The output buffer: played-out history, then future samples from
//! `next_index`.

use std::ops::{Deref, DerefMut};

use crate::neteq::audio_multi_vector::AudioMultiVector;
use crate::neteq::audio_vector::AudioVector;

/// C++ `SyncBuffer`.
#[derive(Debug)]
pub struct SyncBuffer {
    inner: AudioMultiVector,
    next_index: usize,
    /// The timestamp of the last sample in the buffer.
    end_timestamp: u32,
    /// Index to the first non-DTMF sample in the buffer.
    dtmf_index: usize,
}

impl SyncBuffer {
    /// `channels` channels of `length` zeros; `next_index = length`.
    pub fn new(channels: usize, length: usize) -> Self {
        Self {
            inner: AudioMultiVector::with_size(channels, length),
            next_index: length,
            end_timestamp: 0,
            dtmf_index: 0,
        }
    }

    /// Number of samples yet to play (`size() - next_index()`).
    pub fn future_length(&self) -> usize {
        self.inner.size() - self.next_index
    }

    /// Appends `append_this` and pops the same amount from the front, so the
    /// size stays constant; moves `next_index` back accordingly.
    pub fn push_back(&mut self, append_this: &AudioMultiVector) {
        let samples_added = append_this.size();
        self.inner.push_back(append_this);
        self.inner.pop_front(samples_added);
        if samples_added <= self.next_index {
            self.next_index -= samples_added;
        } else {
            // Future data that was never played is pushed out. C++ keeps the
            // DCHECK disabled because 60 ms frames hit this path.
            self.next_index = 0;
        }
        self.dtmf_index -= self.dtmf_index.min(samples_added);
    }

    /// Interleaved variant of `push_back`.
    pub fn push_back_interleaved(&mut self, append_this: &[i16]) {
        let size_before_adding = self.inner.size();
        self.inner.push_back_interleaved(append_this);
        let samples_added_per_channel = self.inner.size() - size_before_adding;
        debug_assert_eq!(
            samples_added_per_channel * self.inner.channels(),
            append_this.len()
        );
        self.inner.pop_front(samples_added_per_channel);
        self.next_index -= self.next_index.min(samples_added_per_channel);
        self.dtmf_index -= self.dtmf_index.min(samples_added_per_channel);
    }

    /// Prepends `length` zeros and pops as many from the back.
    pub fn push_front_zeros(&mut self, length: usize) {
        self.insert_zeros_at_index(length, 0);
    }

    /// Inserts `length` zeros at `position`, keeping the size constant.
    pub fn insert_zeros_at_index(&mut self, length: usize, position: usize) {
        let position = position.min(self.inner.size());
        let length = length.min(self.inner.size() - position);
        self.inner.pop_back(length);
        for channel in self.inner.channels.iter_mut() {
            channel.insert_zeros_at(length, position);
        }
        if self.next_index >= position {
            // We are moving the `next_index` sample.
            self.set_next_index(self.next_index + length); // Overflow handled by subfunction.
        }
        if self.dtmf_index > 0 && self.dtmf_index >= position {
            // We are moving the `dtmf_index` sample.
            self.set_dtmf_index(self.dtmf_index + length); // Overflow handled by subfunction.
        }
    }

    /// Overwrites `length` samples per channel at `position` without growing.
    pub fn replace_at_index(
        &mut self,
        insert_this: &AudioMultiVector,
        length: usize,
        position: usize,
    ) {
        let position = position.min(self.inner.size()); // Cap `position` in the valid range.
        let length = length.min(self.inner.size() - position);
        self.inner.overwrite_at(insert_this, length, position);
    }

    /// `replace_at_index` with `length = insert_this.size()`.
    pub fn replace_at_index_all(&mut self, insert_this: &AudioMultiVector, position: usize) {
        self.replace_at_index(insert_this, insert_this.size(), position);
    }

    /// Reads `audio.len() / channels()` samples per channel from
    /// `next_index`, interleaved, and advances `next_index`. Returns false if
    /// not enough future samples exist.
    pub fn get_next_audio_interleaved(&mut self, audio: &mut [i16]) -> bool {
        debug_assert_eq!(audio.len() % self.inner.channels(), 0);
        let read = self
            .inner
            .read_interleaved_from_index_into(self.next_index, audio);
        if read {
            self.next_index += audio.len() / self.inner.channels();
        }
        read
    }

    /// Adds `increment` to `end_timestamp` (wrapping).
    pub fn increase_end_timestamp(&mut self, increment: u32) {
        self.end_timestamp = self.end_timestamp.wrapping_add(increment);
    }

    /// Zeros the buffer and sets `next_index` to the end.
    pub fn flush(&mut self) {
        let size = self.inner.size();
        self.inner.zeros(size);
        self.next_index = size;
        self.end_timestamp = 0;
        self.dtmf_index = 0;
    }

    /// Channel `n`.
    pub fn channel(&self, n: usize) -> &AudioVector {
        &self.inner[n]
    }

    /// Channel `n`, mutable.
    pub fn channel_mut(&mut self, n: usize) -> &mut AudioVector {
        &mut self.inner[n]
    }

    /// Index of the next sample to play.
    pub fn next_index(&self) -> usize {
        self.next_index
    }

    /// Sets `next_index`, capped at `size()`.
    pub fn set_next_index(&mut self, value: usize) {
        // Cannot set `next_index` larger than the size of the buffer.
        self.next_index = value.min(self.inner.size());
    }

    /// RTP timestamp of the last sample in the buffer.
    pub fn end_timestamp(&self) -> u32 {
        self.end_timestamp
    }

    /// Sets `end_timestamp`.
    pub fn set_end_timestamp(&mut self, value: u32) {
        self.end_timestamp = value;
    }

    /// Index of the first non-DTMF sample.
    pub fn dtmf_index(&self) -> usize {
        self.dtmf_index
    }

    /// Sets `dtmf_index`, capped at `size()`.
    pub fn set_dtmf_index(&mut self, value: usize) {
        // Cannot set `dtmf_index` larger than the size of the buffer.
        self.dtmf_index = value.min(self.inner.size());
    }
}

impl Deref for SyncBuffer {
    type Target = AudioMultiVector;
    fn deref(&self) -> &AudioMultiVector {
        &self.inner
    }
}

impl DerefMut for SyncBuffer {
    fn deref_mut(&mut self) -> &mut AudioMultiVector {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::neteq::audio_multi_vector::golden::{feed_multi, fill_multi};
    use crate::spl::test_util::{Hash, Rng};

    #[test]
    fn create_and_destroy() {
        const LEN: usize = 10;
        const CHANNELS: usize = 2;
        let sync_buffer = SyncBuffer::new(CHANNELS, LEN);
        assert_eq!(CHANNELS, sync_buffer.channels());
        assert_eq!(LEN, sync_buffer.size());
        assert_eq!(LEN, sync_buffer.next_index());
        for channel in 0..CHANNELS {
            for i in 0..LEN {
                assert_eq!(0, sync_buffer[channel][i]);
            }
        }
    }

    #[test]
    fn set_next_index() {
        const LEN: usize = 100;
        let mut sync_buffer = SyncBuffer::new(2, LEN);
        sync_buffer.set_next_index(0);
        assert_eq!(0, sync_buffer.next_index());
        sync_buffer.set_next_index(LEN / 2);
        assert_eq!(LEN / 2, sync_buffer.next_index());
        sync_buffer.set_next_index(LEN);
        assert_eq!(LEN, sync_buffer.next_index());
        // Try to set larger than the buffer size; should cap at buffer size.
        sync_buffer.set_next_index(LEN + 1);
        assert_eq!(LEN, sync_buffer.next_index());
    }

    fn new_data(channels: usize, len: usize, offset: i16) -> AudioMultiVector {
        let mut data = AudioMultiVector::with_size(channels, len);
        for channel in 0..channels {
            for i in 0..len {
                data[channel][i] = offset + i as i16;
            }
        }
        data
    }

    #[test]
    fn push_back_and_flush() {
        const LEN: usize = 100;
        const CHANNELS: usize = 2;
        const NEW_LEN: usize = 10;
        let mut sync_buffer = SyncBuffer::new(CHANNELS, LEN);
        let new_data = new_data(CHANNELS, NEW_LEN, 0);
        sync_buffer.push_back(&new_data);
        assert_eq!(LEN, sync_buffer.size());
        assert_eq!(LEN - NEW_LEN, sync_buffer.next_index());
        for channel in 0..CHANNELS {
            for i in 0..NEW_LEN {
                assert_eq!(
                    new_data[channel][i],
                    sync_buffer[channel][sync_buffer.next_index() + i]
                );
            }
        }

        sync_buffer.flush();
        assert_eq!(LEN, sync_buffer.size());
        assert_eq!(LEN, sync_buffer.next_index());
        for channel in 0..CHANNELS {
            for i in 0..LEN {
                assert_eq!(0, sync_buffer[channel][i]);
            }
        }
    }

    #[test]
    fn push_front_zeros() {
        const LEN: usize = 100;
        const CHANNELS: usize = 2;
        const NEW_LEN: usize = 10;
        let mut sync_buffer = SyncBuffer::new(CHANNELS, LEN);
        let new_data = new_data(CHANNELS, NEW_LEN, 1000);
        sync_buffer.push_back(&new_data);
        assert_eq!(LEN, sync_buffer.size());

        sync_buffer.push_front_zeros(NEW_LEN - 1);
        assert_eq!(LEN, sync_buffer.size()); // Size should remain the same.
                                             // Expect `next_index` to be moved by NEW_LEN - 1 samples.
        assert_eq!(LEN - 1, sync_buffer.next_index());
        for channel in 0..CHANNELS {
            for i in 0..NEW_LEN - 1 {
                assert_eq!(0, sync_buffer[channel][i]);
            }
        }
        for channel in 0..CHANNELS {
            assert_eq!(1000, sync_buffer[channel][sync_buffer.next_index()]);
        }
    }

    #[test]
    fn get_next_audio_interleaved() {
        const LEN: usize = 100;
        const CHANNELS: usize = 2;
        const NEW_LEN: usize = 10;
        let mut sync_buffer = SyncBuffer::new(CHANNELS, LEN);
        let new_data = new_data(CHANNELS, NEW_LEN, 0);
        sync_buffer.push_back(&new_data);

        // Read to interleaved output. Read in two batches, one half at a time.
        const SAMPLES: usize = NEW_LEN / 2;
        let mut output1 = [0i16; SAMPLES * CHANNELS];
        assert!(sync_buffer.get_next_audio_interleaved(&mut output1));
        let mut output2 = [0i16; SAMPLES * CHANNELS];
        assert!(sync_buffer.get_next_audio_interleaved(&mut output2));

        let mut index = 0;
        for i in 0..SAMPLES {
            for channel in 0..CHANNELS {
                assert_eq!(new_data[channel][i], output1[index]);
                index += 1;
            }
        }
        index = 0;
        for i in NEW_LEN / 2..NEW_LEN {
            for channel in 0..CHANNELS {
                assert_eq!(new_data[channel][i], output2[index]);
                index += 1;
            }
        }
        // Not in WebRTC: no future samples left.
        assert_eq!(0, sync_buffer.future_length());
        assert!(!sync_buffer.get_next_audio_interleaved(&mut output1));
    }

    // Not in WebRTC: index bookkeeping of the remaining methods.
    #[test]
    fn index_bookkeeping() {
        let mut sync_buffer = SyncBuffer::new(1, 10);
        sync_buffer.set_next_index(6);
        sync_buffer.set_dtmf_index(4);
        sync_buffer.insert_zeros_at_index(3, 5);
        assert_eq!(
            (10, 9, 4),
            (
                sync_buffer.size(),
                sync_buffer.next_index(),
                sync_buffer.dtmf_index()
            )
        );
        sync_buffer.insert_zeros_at_index(3, 0);
        assert_eq!(
            (10, 7),
            (sync_buffer.next_index(), sync_buffer.dtmf_index())
        );

        // Overrunning `next_index` resets it to 0, like C++.
        sync_buffer.set_next_index(2);
        sync_buffer.push_back_interleaved(&[1, 2, 3]);
        assert_eq!((0, 4), (sync_buffer.next_index(), sync_buffer.dtmf_index()));
        sync_buffer.push_back(&new_data(1, 5, 7));
        assert_eq!((0, 0), (sync_buffer.next_index(), sync_buffer.dtmf_index()));

        let replacement = new_data(1, 4, 50);
        sync_buffer.replace_at_index_all(&replacement, 8);
        let contents: Vec<i16> = (0..sync_buffer.size()).map(|i| sync_buffer[0][i]).collect();
        assert_eq!(vec![0, 0, 1, 2, 3, 7, 8, 9, 50, 51], contents);

        sync_buffer.set_end_timestamp(u32::MAX);
        sync_buffer.increase_end_timestamp(2);
        assert_eq!(1, sync_buffer.end_timestamp());
    }

    // Golden hash against the real C++ (see spl/golden/golden_gen.cc).
    #[test]
    fn golden_sync_buffer_ops() {
        let mut r = Rng::new();
        let mut h = Hash::new();
        for _ in 0..300 {
            let ch = r.urange(1, 3);
            let len = r.urange(0, 40);
            let mut sb = SyncBuffer::new(ch, len);
            let mut other = AudioMultiVector::new(ch);
            for _ in 0..40 {
                let size = sb.size();
                match r.below(13) {
                    0 => {
                        fill_multi(&mut r, &mut other, 20);
                        sb.push_back(&other);
                    }
                    1 => {
                        let len = r.urange(0, 20) * ch;
                        let d = r.vec16(len);
                        sb.push_back_interleaved(&d);
                    }
                    2 => sb.push_front_zeros(r.below(size + 5)),
                    3 => {
                        let len = r.below(size + 5);
                        let pos = r.below(size + 5);
                        sb.insert_zeros_at_index(len, pos);
                    }
                    4 => {
                        fill_multi(&mut r, &mut other, 20);
                        let len = r.below(other.size() + 1);
                        let pos = r.below(size + 5);
                        sb.replace_at_index(&other, len, pos);
                    }
                    5 => {
                        fill_multi(&mut r, &mut other, 20);
                        let pos = r.below(size + 5);
                        sb.replace_at_index_all(&other, pos);
                    }
                    6 => {
                        let spc = r.below(15);
                        let mut dst = vec![0i16; spc * ch];
                        let ok = sb.get_next_audio_interleaved(&mut dst);
                        h.feed(ok as i64);
                        if ok {
                            h.feed16(&dst);
                        }
                    }
                    7 => sb.set_next_index(r.below(size + 5)),
                    8 => sb.set_dtmf_index(r.below(size + 5)),
                    9 => sb.increase_end_timestamp(r.next()),
                    10 => sb.flush(),
                    11 => sb.set_end_timestamp(r.next()),
                    12 => h.feed(sb.future_length() as i64),
                    _ => unreachable!(),
                }
                h.feed(sb.next_index() as i64);
                h.feed(sb.dtmf_index() as i64);
                h.feed(sb.end_timestamp() as i64);
                feed_multi(&mut h, &sb);
            }
        }
        assert_eq!(0xe48d616118753b55, h.value());
    }
}
