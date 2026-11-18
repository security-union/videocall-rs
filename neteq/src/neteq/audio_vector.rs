// Port target: WebRTC modules/audio_coding/neteq/audio_vector.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (tests: audio_vector_unittest.cc).
// Copyright (c) 2013 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// Conventions used for every frozen signature in this crate: a C pointer
// becomes a slice that starts at that pointer; explicit C length arguments
// are kept. Methods overloaded in C++ get a suffix (`_slice`, `_range`).

//! Circular buffer of `i16` samples with the exact C++ growth policy
//! (`Reserve` doubling, `begin_index_` / `end_index_`), since `cross_fade`,
//! `push_front` and `insert_at` semantics depend on it.
//!
//! One slot is always left unused so that `begin_index == end_index` means
//! empty, exactly as in C++. C++ allocates new storage uninitialized in
//! `Reserve`; here it is zeroed. No code path reads a slot outside
//! `[begin, end)` before writing it, so the observable contents are the same.

use std::ops::{Index, IndexMut};

/// C++ `AudioVector::kDefaultInitialSize`.
const DEFAULT_INITIAL_SIZE: usize = 10;

/// C++ `AudioVector`.
#[derive(Debug, Clone)]
pub struct AudioVector {
    /// Backing storage, `capacity` long.
    array: Vec<i16>,
    /// Allocated number of samples in the array.
    capacity: usize,
    /// Index of the first sample, except when `begin_index == end_index`,
    /// which means the buffer is empty.
    begin_index: usize,
    /// Index of the sample after the last sample.
    end_index: usize,
}

impl Default for AudioVector {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioVector {
    /// Empty vector with the default initial capacity.
    pub fn new() -> Self {
        let mut vector = Self::with_size(DEFAULT_INITIAL_SIZE);
        vector.clear();
        vector
    }

    /// Vector of `initial_size` zeros.
    pub fn with_size(initial_size: usize) -> Self {
        let capacity = initial_size + 1;
        Self {
            array: vec![0; capacity],
            capacity,
            begin_index: 0,
            end_index: capacity - 1,
        }
    }

    /// Deletes all values and makes the vector empty.
    pub fn clear(&mut self) {
        self.begin_index = 0;
        self.end_index = 0;
    }

    /// Copies all values into `copy_to`; its previous contents are lost.
    pub fn copy_to(&self, copy_to: &mut AudioVector) {
        let size = self.size();
        copy_to.reserve(size);
        self.copy_to_slice(size, 0, &mut copy_to.array);
        copy_to.begin_index = 0;
        copy_to.end_index = size;
    }

    /// C++ `CopyTo(this)`, i.e. `copy_to` with source and destination being
    /// the same vector (reachable through `AudioMultiVector::copy_channel(n, n)`).
    /// C++ then `memcpy`s the ring buffer onto itself in two chunks, and the
    /// second chunk reads data the first one already overwrote. Overlapping
    /// `memcpy` is UB; this ports it with `memmove` semantics, which is what
    /// the forward copy of the first chunk does on every libc. Finally C++
    /// sets `begin_index_ = 0` and then `end_index_ = Size()`, and that
    /// `Size()` is now computed with the new begin index, so `end_index_`
    /// keeps its old value (the size usually changes). Verified against the
    /// C++ build by `golden_audio_vector_ops`.
    pub(crate) fn copy_to_itself(&mut self) {
        let size = self.size();
        // `Reserve(Size())` is a no-op: capacity always exceeds the size.
        if size != 0 {
            let copy_index = self.begin_index % self.capacity;
            let first_chunk_length = size.min(self.capacity - copy_index);
            self.array
                .copy_within(copy_index..copy_index + first_chunk_length, 0);
            let remaining_length = size - first_chunk_length;
            if remaining_length > 0 {
                self.array
                    .copy_within(0..remaining_length, first_chunk_length);
            }
        }
        self.begin_index = 0;
        // `end_index_ = Size()` with `begin_index_` already 0.
        self.end_index = self.size();
    }

    /// Copies `length` values from `position` into `copy_to[..length]`.
    /// `length` is capped at `size() - position` (C++ `size_t` arithmetic).
    pub fn copy_to_slice(&self, length: usize, position: usize, copy_to: &mut [i16]) {
        if length == 0 {
            return;
        }
        let length = length.min(self.size().wrapping_sub(position));
        let copy_index = (self.begin_index + position) % self.capacity;
        let first_chunk_length = length.min(self.capacity - copy_index);
        copy_to[..first_chunk_length]
            .copy_from_slice(&self.array[copy_index..copy_index + first_chunk_length]);
        let remaining_length = length - first_chunk_length;
        if remaining_length > 0 {
            copy_to[first_chunk_length..first_chunk_length + remaining_length]
                .copy_from_slice(&self.array[..remaining_length]);
        }
    }

    /// C++ `CopyTo(position, MonoView dst)`: copies `dst.len()` values from
    /// `position`. Returns false if there are not enough samples.
    pub fn copy_to_view(&self, position: usize, dst: &mut [i16]) -> bool {
        if self.size().wrapping_sub(position) < dst.len() {
            return false;
        }
        let copy_index = (self.begin_index + position) % self.capacity;
        let first_chunk_length = dst.len().min(self.capacity - copy_index);
        dst[..first_chunk_length]
            .copy_from_slice(&self.array[copy_index..copy_index + first_chunk_length]);
        let remaining_length = dst.len() - first_chunk_length;
        if remaining_length > 0 {
            dst[first_chunk_length..].copy_from_slice(&self.array[..remaining_length]);
        }
        true
    }

    /// Prepends the contents of `prepend_this`.
    pub fn push_front(&mut self, prepend_this: &AudioVector) {
        let length = prepend_this.size();
        if length == 0 {
            return;
        }
        // One big Reserve first, like C++.
        self.reserve(self.size() + length);

        let first_chunk_length = length.min(prepend_this.capacity - prepend_this.begin_index);
        let remaining_length = length - first_chunk_length;
        if remaining_length > 0 {
            self.push_front_slice(&prepend_this.array[..remaining_length]);
        }
        let begin = prepend_this.begin_index;
        self.push_front_slice(&prepend_this.array[begin..begin + first_chunk_length]);
    }

    /// Prepends `prepend_this`.
    pub fn push_front_slice(&mut self, prepend_this: &[i16]) {
        let length = prepend_this.len();
        if length == 0 {
            return;
        }
        self.reserve(self.size() + length);
        let first_chunk_length = length.min(self.begin_index);
        self.array[self.begin_index - first_chunk_length..self.begin_index]
            .copy_from_slice(&prepend_this[length - first_chunk_length..]);
        let remaining_length = length - first_chunk_length;
        if remaining_length > 0 {
            let capacity = self.capacity;
            self.array[capacity - remaining_length..]
                .copy_from_slice(&prepend_this[..remaining_length]);
        }
        self.begin_index = (self.begin_index + self.capacity - length) % self.capacity;
    }

    /// Appends the contents of `append_this`.
    pub fn push_back(&mut self, append_this: &AudioVector) {
        self.push_back_range(append_this, append_this.size(), 0);
    }

    /// Appends `length` values of `append_this` starting at `position`.
    pub fn push_back_range(&mut self, append_this: &AudioVector, length: usize, position: usize) {
        debug_assert!(position <= append_this.size());
        debug_assert!(length <= append_this.size() - position);
        if length == 0 {
            return;
        }
        // One big Reserve first, like C++.
        self.reserve(self.size() + length);

        let start_index = (append_this.begin_index + position) % append_this.capacity;
        let first_chunk_length = length.min(append_this.capacity - start_index);
        self.push_back_slice(&append_this.array[start_index..start_index + first_chunk_length]);

        let remaining_length = length - first_chunk_length;
        if remaining_length > 0 {
            self.push_back_slice(&append_this.array[..remaining_length]);
        }
    }

    /// Appends `append_this`.
    pub fn push_back_slice(&mut self, append_this: &[i16]) {
        let length = append_this.len();
        if length == 0 {
            return;
        }
        self.reserve(self.size() + length);
        let first_chunk_length = length.min(self.capacity - self.end_index);
        self.array[self.end_index..self.end_index + first_chunk_length]
            .copy_from_slice(&append_this[..first_chunk_length]);
        let remaining_length = length - first_chunk_length;
        if remaining_length > 0 {
            self.array[..remaining_length].copy_from_slice(&append_this[first_chunk_length..]);
        }
        self.end_index = (self.end_index + length) % self.capacity;
    }

    /// Removes `length` elements from the beginning.
    pub fn pop_front(&mut self, length: usize) {
        if length == 0 {
            return;
        }
        let length = length.min(self.size());
        self.begin_index = (self.begin_index + length) % self.capacity;
    }

    /// Removes `length` elements from the end.
    pub fn pop_back(&mut self, length: usize) {
        if length == 0 {
            return;
        }
        // Never remove more than what is in the array.
        let length = length.min(self.size());
        self.end_index = (self.end_index + self.capacity - length) % self.capacity;
    }

    /// Extends by `extra_length` zeros at the end.
    pub fn extend(&mut self, extra_length: usize) {
        if extra_length == 0 {
            return;
        }
        self.insert_zeros_by_push_back(extra_length, self.size());
    }

    /// Inserts `insert_this` before `position`. `position` past the end
    /// means append.
    pub fn insert_at(&mut self, insert_this: &[i16], position: usize) {
        if insert_this.is_empty() {
            return;
        }
        // Cap the insert position at the current array length.
        let position = self.size().min(position);
        // Insert from the end that moves less data, like C++.
        if position <= self.size() - position {
            self.insert_by_push_front(insert_this, position);
        } else {
            self.insert_by_push_back(insert_this, position);
        }
    }

    /// Inserts `length` zeros before `position`.
    pub fn insert_zeros_at(&mut self, length: usize, position: usize) {
        if length == 0 {
            return;
        }
        let position = self.size().min(position);
        if position <= self.size() - position {
            self.insert_zeros_by_push_front(length, position);
        } else {
            self.insert_zeros_by_push_back(length, position);
        }
    }

    /// Overwrites `length` values from `position` with the start of
    /// `insert_this`, extending the vector if needed.
    pub fn overwrite_at(&mut self, insert_this: &AudioVector, length: usize, position: usize) {
        debug_assert!(length <= insert_this.size());
        if length == 0 {
            return;
        }
        // Cap the insert position at the current array length.
        let position = self.size().min(position);

        // One big Reserve first, like C++.
        let new_size = self.size().max(position + length);
        self.reserve(new_size);

        let begin = insert_this.begin_index;
        let first_chunk_length = length.min(insert_this.capacity - begin);
        self.overwrite_at_slice(
            &insert_this.array[begin..begin + first_chunk_length],
            position,
        );
        let remaining_length = length - first_chunk_length;
        if remaining_length > 0 {
            self.overwrite_at_slice(
                &insert_this.array[..remaining_length],
                position + first_chunk_length,
            );
        }
    }

    /// Overwrites `insert_this.len()` values from `position`, extending the
    /// vector if needed.
    pub fn overwrite_at_slice(&mut self, insert_this: &[i16], position: usize) {
        let length = insert_this.len();
        if length == 0 {
            return;
        }
        let position = self.size().min(position);

        let new_size = self.size().max(position + length);
        self.reserve(new_size);

        let overwrite_index = (self.begin_index + position) % self.capacity;
        let first_chunk_length = length.min(self.capacity - overwrite_index);
        self.array[overwrite_index..overwrite_index + first_chunk_length]
            .copy_from_slice(&insert_this[..first_chunk_length]);
        let remaining_length = length - first_chunk_length;
        if remaining_length > 0 {
            self.array[..remaining_length].copy_from_slice(&insert_this[first_chunk_length..]);
        }

        self.end_index = (self.begin_index + new_size) % self.capacity;
    }

    /// Appends `append_this` with a linear cross-fade of `fade_length`
    /// samples over the current tail.
    pub fn cross_fade(&mut self, append_this: &AudioVector, fade_length: usize) {
        // Fade length cannot be longer than the current vector or `append_this`.
        debug_assert!(fade_length <= self.size());
        debug_assert!(fade_length <= append_this.size());
        let fade_length = fade_length.min(self.size()).min(append_this.size());
        let position = self.size() - fade_length + self.begin_index;
        // `alpha` is the mixing factor in Q14. `fade_length` goes through
        // `static_cast<int>` in C++.
        let alpha_step: i32 = 16384 / (fade_length as i32 + 1);
        let mut alpha: i32 = 16384;
        for i in 0..fade_length {
            alpha -= alpha_step;
            let index = (position + i) % self.capacity;
            // Computed in `int`, stored with an implicit `int16_t` truncation.
            self.array[index] = ((alpha * self.array[index] as i32
                + (16384 - alpha) * append_this[i] as i32
                + 8192)
                >> 14) as i16;
        }
        debug_assert!(alpha >= 0); // Verify that the slope was correct.
                                   // Append what is left of `append_this`.
        let samples_to_push_back = append_this.size() - fade_length;
        if samples_to_push_back > 0 {
            self.push_back_range(append_this, samples_to_push_back, fade_length);
        }
    }

    /// Number of elements.
    pub fn size(&self) -> usize {
        (self.end_index + self.capacity - self.begin_index) % self.capacity
    }

    /// True if there are no elements.
    pub fn empty(&self) -> bool {
        self.begin_index == self.end_index
    }

    /// C++ `WrapIndex`: index within the capacity without a modulo.
    #[inline]
    fn wrap_index(&self, index: usize) -> usize {
        debug_assert!(index < self.capacity);
        debug_assert!(self.begin_index < self.capacity);
        let mut ix = self.begin_index + index;
        if ix >= self.capacity {
            ix -= self.capacity;
        }
        debug_assert!(ix < self.capacity);
        ix
    }

    /// C++ `Reserve`: grows the storage to `n + 1` slots (one spare to tell
    /// full from empty) and linearizes the contents. No-op if `capacity > n`.
    fn reserve(&mut self, n: usize) {
        if self.capacity > n {
            return;
        }
        let length = self.size();
        let mut temp_array = vec![0i16; n + 1];
        self.copy_to_slice(length, 0, &mut temp_array);
        self.array = temp_array;
        self.begin_index = 0;
        self.end_index = length;
        self.capacity = n + 1;
    }

    fn insert_by_push_back(&mut self, insert_this: &[i16], position: usize) {
        let move_chunk_length = self.size() - position;
        let mut temp_array = Vec::new();
        if move_chunk_length > 0 {
            temp_array = vec![0i16; move_chunk_length];
            self.copy_to_slice(move_chunk_length, position, &mut temp_array);
            self.pop_back(move_chunk_length);
        }

        self.reserve(self.size() + insert_this.len() + move_chunk_length);
        self.push_back_slice(insert_this);
        if move_chunk_length > 0 {
            self.push_back_slice(&temp_array);
        }
    }

    fn insert_by_push_front(&mut self, insert_this: &[i16], position: usize) {
        let mut temp_array = Vec::new();
        if position > 0 {
            temp_array = vec![0i16; position];
            self.copy_to_slice(position, 0, &mut temp_array);
            self.pop_front(position);
        }

        self.reserve(self.size() + insert_this.len() + position);
        self.push_front_slice(insert_this);
        if position > 0 {
            self.push_front_slice(&temp_array);
        }
    }

    fn insert_zeros_by_push_back(&mut self, length: usize, position: usize) {
        let move_chunk_length = self.size() - position;
        let mut temp_array = Vec::new();
        if move_chunk_length > 0 {
            temp_array = vec![0i16; move_chunk_length];
            self.copy_to_slice(move_chunk_length, position, &mut temp_array);
            self.pop_back(move_chunk_length);
        }

        self.reserve(self.size() + length + move_chunk_length);

        let first_zero_chunk_length = length.min(self.capacity - self.end_index);
        self.array[self.end_index..self.end_index + first_zero_chunk_length].fill(0);
        let remaining_zero_length = length - first_zero_chunk_length;
        if remaining_zero_length > 0 {
            self.array[..remaining_zero_length].fill(0);
        }
        self.end_index = (self.end_index + length) % self.capacity;

        if move_chunk_length > 0 {
            self.push_back_slice(&temp_array);
        }
    }

    fn insert_zeros_by_push_front(&mut self, length: usize, position: usize) {
        let mut temp_array = Vec::new();
        if position > 0 {
            temp_array = vec![0i16; position];
            self.copy_to_slice(position, 0, &mut temp_array);
            self.pop_front(position);
        }

        self.reserve(self.size() + length + position);

        let first_zero_chunk_length = length.min(self.begin_index);
        self.array[self.begin_index - first_zero_chunk_length..self.begin_index].fill(0);
        let remaining_zero_length = length - first_zero_chunk_length;
        if remaining_zero_length > 0 {
            let capacity = self.capacity;
            self.array[capacity - remaining_zero_length..].fill(0);
        }
        self.begin_index = (self.begin_index + self.capacity - length) % self.capacity;

        if position > 0 {
            self.push_front_slice(&temp_array);
        }
    }
}

impl Index<usize> for AudioVector {
    type Output = i16;
    fn index(&self, index: usize) -> &i16 {
        &self.array[self.wrap_index(index)]
    }
}

impl IndexMut<usize> for AudioVector {
    fn index_mut(&mut self, index: usize) -> &mut i16 {
        let ix = self.wrap_index(index);
        &mut self.array[ix]
    }
}

/// Golden-test helpers mirroring `feed_vec` / `rand_vec` in
/// `spl/golden/golden_gen.cc`.
#[cfg(test)]
pub(crate) mod golden {
    use super::AudioVector;
    use crate::spl::test_util::{Hash, Rng};

    pub(crate) fn feed_vec(h: &mut Hash, v: &AudioVector) {
        h.feed(v.size() as i64);
        for i in 0..v.size() {
            h.feed(v[i] as i64);
        }
    }

    pub(crate) fn rand_vec(r: &mut Rng, max_len: i32) -> Vec<i16> {
        let len = r.range(0, max_len) as usize;
        r.vec16(len)
    }
}

#[cfg(test)]
mod tests {
    use super::golden::{feed_vec, rand_vec};
    use super::*;
    use crate::spl::test_util::{Hash, Rng};

    fn array() -> [i16; 10] {
        std::array::from_fn(|i| i as i16)
    }

    fn new_array() -> [i16; 5] {
        std::array::from_fn(|i| 100 + i as i16)
    }

    fn filled(values: &[i16]) -> AudioVector {
        let mut vec = AudioVector::new();
        vec.push_back_slice(values);
        vec
    }

    #[test]
    fn create_and_destroy() {
        let vec1 = AudioVector::new();
        assert!(vec1.empty());
        assert_eq!(0, vec1.size());

        let vec2 = AudioVector::with_size(17);
        assert!(!vec2.empty());
        assert_eq!(17, vec2.size());
    }

    #[test]
    fn subscript_operator() {
        let mut vec = AudioVector::with_size(10);
        for i in 0..10 {
            vec[i] = i as i16;
            let value = &vec[i];
            assert_eq!(i as i16, *value);
        }
    }

    #[test]
    fn push_back_and_copy() {
        let array = array();
        let mut vec = AudioVector::new();
        let mut vec_copy = AudioVector::new();
        vec.push_back_slice(&array);
        vec.copy_to(&mut vec_copy);
        assert_eq!(10, vec.size());
        assert_eq!(10, vec_copy.size());
        for i in 0..10 {
            assert_eq!(array[i], vec[i]);
            assert_eq!(array[i], vec_copy[i]);
        }

        vec.clear();
        assert!(vec.empty());

        vec.copy_to(&mut vec_copy);
        assert!(vec_copy.empty());
    }

    #[test]
    fn copy_to() {
        let mut vec = filled(&array());
        assert_eq!(10, vec.size());

        let mut buffer = [0i16; 10];
        assert!(!vec.copy_to_view(1, &mut buffer));
        assert!(vec.copy_to_view(0, &mut buffer));
        assert_eq!(5, buffer[5]);

        // Wrap the content around the end of the ring buffer.
        vec.pop_front(2);
        vec.push_back_slice(&[20, 21]);
        assert!(vec.copy_to_view(0, &mut buffer));
        assert_eq!(2, buffer[0]);
        assert_eq!(20, buffer[8]);
        assert_eq!(21, buffer[9]);
    }

    #[test]
    fn push_back_vector() {
        let mut vec1 = AudioVector::with_size(10);
        let mut vec2 = AudioVector::with_size(10);
        for i in 0..10 {
            vec1[i] = i as i16;
            vec2[i] = (i + 10) as i16;
        }
        vec1.push_back(&vec2);
        assert_eq!(20, vec1.size());
        for i in 0..20 {
            assert_eq!(i as i16, vec1[i]);
        }
    }

    #[test]
    fn push_front() {
        let array = array();
        let mut vec = AudioVector::new();
        vec.push_front_slice(&array);
        assert_eq!(10, vec.size());
        for i in 0..10 {
            assert_eq!(array[i], vec[i]);
        }
    }

    #[test]
    fn push_front_vector() {
        let mut vec1 = AudioVector::with_size(10);
        let mut vec2 = AudioVector::with_size(10);
        for i in 0..10 {
            vec1[i] = i as i16;
            vec2[i] = (i + 10) as i16;
        }
        vec2.push_front(&vec1);
        assert_eq!(20, vec2.size());
        for i in 0..20 {
            assert_eq!(i as i16, vec2[i]);
        }
    }

    #[test]
    fn pop_front() {
        let mut vec = filled(&array());
        vec.pop_front(1);
        assert_eq!(9, vec.size());
        for i in 0..9 {
            assert_eq!((i + 1) as i16, vec[i]);
        }
        vec.pop_front(10);
        assert_eq!(0, vec.size());
    }

    #[test]
    fn pop_back() {
        let mut vec = filled(&array());
        vec.pop_back(1);
        assert_eq!(9, vec.size());
        for i in 0..9 {
            assert_eq!(i as i16, vec[i]);
        }
        vec.pop_back(10);
        assert_eq!(0, vec.size());
    }

    #[test]
    fn extend() {
        let mut vec = filled(&array());
        vec.extend(5);
        assert_eq!(15, vec.size());
        for i in 10..15 {
            assert_eq!(0, vec[i]);
        }
    }

    #[test]
    fn insert_at() {
        let array = array();
        let new_array = new_array();
        let mut vec = filled(&array);
        let insert_position = 5;
        vec.insert_at(&new_array, insert_position);
        let mut pos = 0;
        for &value in &array[..insert_position] {
            assert_eq!(value, vec[pos]);
            pos += 1;
        }
        for &value in &new_array {
            assert_eq!(value, vec[pos]);
            pos += 1;
        }
        for &value in &array[insert_position..] {
            assert_eq!(value, vec[pos]);
            pos += 1;
        }
    }

    #[test]
    fn insert_zeros_at() {
        let mut vec = filled(&array());
        let mut vec_ref = filled(&array());
        vec.insert_zeros_at(5, 5);
        vec_ref.insert_at(&[0; 5], 5);
        assert_eq!(vec_ref.size(), vec.size());
        for i in 0..vec.size() {
            assert_eq!(vec_ref[i], vec[i]);
        }
    }

    #[test]
    fn insert_at_beginning() {
        let array = array();
        let new_array = new_array();
        let mut vec = filled(&array);
        vec.insert_at(&new_array, 0);
        let mut pos = 0;
        for &value in &new_array {
            assert_eq!(value, vec[pos]);
            pos += 1;
        }
        for &value in &array {
            assert_eq!(value, vec[pos]);
            pos += 1;
        }
    }

    #[test]
    fn insert_at_end() {
        let array = array();
        let new_array = new_array();
        let mut vec = filled(&array);
        vec.insert_at(&new_array, array.len());
        let mut pos = 0;
        for &value in &array {
            assert_eq!(value, vec[pos]);
            pos += 1;
        }
        for &value in &new_array {
            assert_eq!(value, vec[pos]);
            pos += 1;
        }
    }

    #[test]
    fn insert_beyond_end() {
        let array = array();
        let new_array = new_array();
        let mut vec = filled(&array);
        vec.insert_at(&new_array, array.len() + 10);
        let mut pos = 0;
        for &value in &array {
            assert_eq!(value, vec[pos]);
            pos += 1;
        }
        for &value in &new_array {
            assert_eq!(value, vec[pos]);
            pos += 1;
        }
    }

    #[test]
    fn overwrite_at() {
        let array = array();
        let new_array = new_array();
        let mut vec = filled(&array);
        let insert_position = 2;
        vec.overwrite_at_slice(&new_array, insert_position);
        let mut pos = 0;
        while pos < insert_position {
            assert_eq!(array[pos], vec[pos]);
            pos += 1;
        }
        for &value in &new_array {
            assert_eq!(value, vec[pos]);
            pos += 1;
        }
        while pos < array.len() {
            assert_eq!(array[pos], vec[pos]);
            pos += 1;
        }
    }

    #[test]
    fn overwrite_beyond_end() {
        let array = array();
        let new_array = new_array();
        let mut vec = filled(&array);
        let insert_position = array.len() - 2;
        vec.overwrite_at_slice(&new_array, insert_position);
        assert_eq!(array.len() - 2 + new_array.len(), vec.size());
        let mut pos = 0;
        while pos < insert_position {
            assert_eq!(array[pos], vec[pos]);
            pos += 1;
        }
        for &value in &new_array {
            assert_eq!(value, vec[pos]);
            pos += 1;
        }
        assert_eq!(vec.size(), pos);
    }

    #[test]
    fn cross_fade() {
        const LENGTH: usize = 100;
        const FADE_LENGTH: usize = 10;
        let mut vec1 = AudioVector::with_size(LENGTH);
        let mut vec2 = AudioVector::with_size(LENGTH);
        for i in 0..LENGTH {
            vec1[i] = 0;
            vec2[i] = 100;
        }
        vec1.cross_fade(&vec2, FADE_LENGTH);
        assert_eq!(2 * LENGTH - FADE_LENGTH, vec1.size());
        for i in 0..LENGTH - FADE_LENGTH {
            assert_eq!(0, vec1[i]);
        }
        for i in 0..FADE_LENGTH {
            let expected = ((i + 1) * 100 / (FADE_LENGTH + 1)) as i32;
            let actual = vec1[LENGTH - FADE_LENGTH + i] as i32;
            assert!(
                (expected - actual).abs() <= 1,
                "{i}: {expected} vs {actual}"
            );
        }
        for i in LENGTH..vec1.size() {
            assert_eq!(100, vec1[i]);
        }
    }

    // Not in WebRTC: pins the exact cross-fade samples (the C++ test only
    // checks them to within 1).
    #[test]
    fn cross_fade_exact_samples() {
        let mut vec1 = filled(&[0; 4]);
        let vec2 = filled(&[100, 100, 100, 100, 7]);
        vec1.cross_fade(&vec2, 3);
        // alpha_step = 16384 / 4 = 4096; alpha = 12288, 8192, 4096.
        let expected: Vec<i16> = vec![0, 25, 50, 75, 100, 7];
        let actual: Vec<i16> = (0..vec1.size()).map(|i| vec1[i]).collect();
        assert_eq!(expected, actual);
    }

    // Not in WebRTC: the growth policy and wrap-around bookkeeping.
    #[test]
    fn growth_policy_and_wraparound() {
        let mut vec = AudioVector::new();
        assert_eq!(11, vec.capacity);
        vec.push_back_slice(&[1; 10]);
        assert_eq!(11, vec.capacity); // 10 samples fit in 11 slots.
        vec.push_back_slice(&[2]);
        assert_eq!(12, vec.capacity); // Reserve(11) allocates 12.
        vec.pop_front(5);
        vec.push_back_slice(&[3, 4, 5, 6]);
        // Wrapped: begin 5, end (11 + 4) % 12 = 3.
        assert_eq!((5, 3), (vec.begin_index, vec.end_index));
        let contents: Vec<i16> = (0..vec.size()).map(|i| vec[i]).collect();
        assert_eq!(vec![1, 1, 1, 1, 1, 2, 3, 4, 5, 6], contents);
        vec.push_front_slice(&[9, 8]);
        vec.insert_zeros_at(2, 6);
        vec.insert_at(&[-1, -2], 10);
        let contents: Vec<i16> = (0..vec.size()).map(|i| vec[i]).collect();
        assert_eq!(
            vec![9, 8, 1, 1, 1, 1, 0, 0, 1, 2, -1, -2, 3, 4, 5, 6],
            contents
        );
    }

    // Golden hash against the real C++ (see spl/golden/golden_gen.cc): random
    // sequences of every operation, hashing the contents after each one.
    #[test]
    fn golden_audio_vector_ops() {
        let mut r = Rng::new();
        let mut h = Hash::new();
        for _ in 0..300 {
            let vec_default = AudioVector::new();
            let vec_sized = AudioVector::with_size(r.urange(0, 20));
            let mut vec = if r.below(2) != 0 {
                vec_default
            } else {
                vec_sized
            };
            let mut other = AudioVector::new();
            let d = rand_vec(&mut r, 30);
            other.push_back_slice(&d);
            for _ in 0..40 {
                let size = vec.size();
                match r.below(20) {
                    0 => {
                        let d = rand_vec(&mut r, 30);
                        vec.push_back_slice(&d);
                    }
                    1 => {
                        let d = rand_vec(&mut r, 30);
                        vec.push_front_slice(&d);
                    }
                    2 => vec.push_back(&other),
                    3 => vec.push_front(&other),
                    4 => {
                        let pos = r.below(other.size() + 1);
                        let len = r.below(other.size() - pos + 1);
                        vec.push_back_range(&other, len, pos);
                    }
                    5 => vec.pop_front(r.below(size + 5)),
                    6 => vec.pop_back(r.below(size + 5)),
                    7 => vec.extend(r.below(20)),
                    8 => {
                        let d = rand_vec(&mut r, 20);
                        let pos = r.below(size + 5);
                        vec.insert_at(&d, pos);
                    }
                    9 => {
                        let len = r.below(20);
                        let pos = r.below(size + 5);
                        vec.insert_zeros_at(len, pos);
                    }
                    10 => {
                        let len = r.below(other.size() + 1);
                        let pos = r.below(size + 5);
                        vec.overwrite_at(&other, len, pos);
                    }
                    11 => {
                        let d = rand_vec(&mut r, 20);
                        let pos = r.below(size + 5);
                        vec.overwrite_at_slice(&d, pos);
                    }
                    12 => {
                        let fade = r.below(size.min(other.size()) + 1);
                        vec.cross_fade(&other, fade);
                    }
                    13 => vec.copy_to(&mut other),
                    14 => {
                        if size > 0 {
                            let i = r.below(size);
                            vec[i] = r.i16();
                        }
                    }
                    15 => vec.clear(),
                    16 => {
                        let pos = r.below(size + 2);
                        let mut dst = vec![0i16; r.below(10)];
                        let ok = pos <= size && vec.copy_to_view(pos, &mut dst);
                        h.feed(ok as i64);
                        if ok {
                            h.feed16(&dst);
                        }
                    }
                    17 => {
                        let len = r.below(size + 3);
                        let pos = r.below(size + 1);
                        let mut dst = vec![0i16; len];
                        vec.copy_to_slice(len, pos, &mut dst);
                        h.feed16(&dst);
                    }
                    18 => {
                        other.clear();
                        let d = rand_vec(&mut r, 30);
                        other.push_back_slice(&d);
                    }
                    // C++ `vec.CopyTo(&vec)`: the aliased self-copy.
                    19 => vec.copy_to_itself(),
                    _ => unreachable!(),
                }
                feed_vec(&mut h, &vec);
            }
        }
        assert_eq!(0xf897dec4f167b712, h.value());
    }
}
