// `LEGACY_BITEXACT` is off in WebRTC builds and is not ported.
//
// Port target: WebRTC modules/audio_coding/neteq/dtmf_buffer.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (tests: dtmf_buffer_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! RFC 4733 telephone-event buffer.

use std::collections::VecDeque;

/// C++ `DtmfEvent`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DtmfEvent {
    pub timestamp: u32,
    pub event_no: i32,
    pub volume: i32,
    pub duration: i32,
    pub end_bit: bool,
}

impl DtmfEvent {
    /// C++ `DtmfEvent(ts, ev, vol, dur, end)`.
    pub fn new(timestamp: u32, event_no: i32, volume: i32, duration: i32, end_bit: bool) -> Self {
        Self {
            timestamp,
            event_no,
            volume,
            duration,
            end_bit,
        }
    }
}

/// C++ `DtmfBuffer`.
#[derive(Debug, Clone)]
pub struct DtmfBuffer {
    max_extrapolation_samples: i32,
    frame_len_samples: i32,
    buffer: VecDeque<DtmfEvent>,
}

impl DtmfBuffer {
    /// C++ `kMaxDtmfBufferSize`.
    pub const K_MAX_DTMF_BUFFER_SIZE: usize = 100;
    /// C++ `kOK`.
    pub const K_OK: i32 = 0;
    /// C++ `kInvalidPointer`.
    pub const K_INVALID_POINTER: i32 = 1;
    /// C++ `kPayloadTooShort`.
    pub const K_PAYLOAD_TOO_SHORT: i32 = 2;
    /// C++ `kInvalidEventParameters`.
    pub const K_INVALID_EVENT_PARAMETERS: i32 = 3;
    /// C++ `kInvalidSampleRate`.
    pub const K_INVALID_SAMPLE_RATE: i32 = 4;

    /// Buffer at `fs_hz`.
    pub fn new(fs_hz: i32) -> Self {
        let mut buffer = Self {
            // C++ leaves these uninitialized for an invalid `fs_hz`.
            max_extrapolation_samples: 0,
            frame_len_samples: 0,
            buffer: VecDeque::new(),
        };
        buffer.set_sample_rate(fs_hz);
        buffer
    }

    /// Removes all events.
    pub fn flush(&mut self) {
        self.buffer.clear();
    }

    /// Parses an RFC 4733 payload into `event`. Returns a `K_*` code.
    pub fn parse_event(rtp_timestamp: u32, payload: &[u8], event: &mut DtmfEvent) -> i32 {
        if payload.len() < 4 {
            log::warn!("ParseEvent payload too short");
            return Self::K_PAYLOAD_TOO_SHORT;
        }

        event.event_no = payload[0] as i32;
        event.end_bit = (payload[1] & 0x80) != 0;
        event.volume = (payload[1] & 0x3F) as i32;
        event.duration = (payload[2] as i32) << 8 | payload[3] as i32;
        event.timestamp = rtp_timestamp;
        Self::K_OK
    }

    /// Inserts `event`, merging with an existing one if it is the same
    /// event. Returns a `K_*` code.
    pub fn insert_event(&mut self, event: &DtmfEvent) -> i32 {
        if event.event_no < 0
            || event.event_no > 15
            || event.volume < 0
            || event.volume > 63
            || event.duration <= 0
            || event.duration > 65535
        {
            log::warn!("InsertEvent invalid parameters");
            return Self::K_INVALID_EVENT_PARAMETERS;
        }
        for it in self.buffer.iter_mut() {
            if Self::merge_events(it, event) {
                // A matching event was found and the new event was merged.
                return Self::K_OK;
            }
        }
        if self.buffer.len() >= Self::K_MAX_DTMF_BUFFER_SIZE {
            log::warn!("DTMF buffer flushed.");
            self.flush();
        }
        self.buffer.push_back(*event);
        // Sort the buffer using CompareEvents to rank the events.
        Self::sort_events(&mut self.buffer);
        Self::K_OK
    }

    /// Looks up the event playing at `current_timestamp`. Returns true and
    /// fills `event` if one is found.
    pub fn get_event(&mut self, current_timestamp: u32, event: &mut DtmfEvent) -> bool {
        let mut it = 0usize;
        while it < self.buffer.len() {
            let current = self.buffer[it];
            // `event_end` is an estimate of where the current event ends. If
            // the end bit is set, we know that the event ends at `timestamp` +
            // `duration`.
            let mut event_end = current.timestamp.wrapping_add(current.duration as u32);
            if !current.end_bit {
                // If the end bit is not set, we allow extrapolation of the
                // event for some time.
                event_end = event_end.wrapping_add(self.max_extrapolation_samples as u32);
                if let Some(next) = self.buffer.get(it + 1) {
                    // If there is a next event in the buffer, we will not
                    // extrapolate over the start of that new event.
                    event_end = event_end.min(next.timestamp);
                }
            }
            if current_timestamp >= current.timestamp && current_timestamp <= event_end {
                // TODO(hlundin): Change to <.
                // Found a matching event.
                *event = current;
                return true;
            } else if current_timestamp > event_end {
                // TODO(hlundin): Change to >=.
                // Erase old event; `it` then points at the next element.
                self.buffer.remove(it);
            } else {
                it += 1;
            }
        }
        false
    }

    /// Number of events.
    pub fn length(&self) -> usize {
        self.buffer.len()
    }

    /// True if there are no events.
    pub fn empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Sets the sample rate. Returns a `K_*` code.
    pub fn set_sample_rate(&mut self, fs_hz: i32) -> i32 {
        if fs_hz != 8000 && fs_hz != 16000 && fs_hz != 32000 && fs_hz != 48000 {
            return Self::K_INVALID_SAMPLE_RATE;
        }
        self.max_extrapolation_samples = 7 * fs_hz / 100;
        self.frame_len_samples = fs_hz / 100;
        Self::K_OK
    }

    /// C++ `SameEvent`: same timestamp and event number. Long-lasting
    /// events split into segments are treated as separate events.
    fn same_event(a: &DtmfEvent, b: &DtmfEvent) -> bool {
        a.event_no == b.event_no && a.timestamp == b.timestamp
    }

    /// C++ `MergeEvents`: merges `event` into `it` if they are the same
    /// event. Returns true if merged.
    fn merge_events(it: &mut DtmfEvent, event: &DtmfEvent) -> bool {
        if Self::same_event(it, event) {
            if !it.end_bit {
                // Do not extend the duration of an event for which the end bit
                // was already received.
                it.duration = event.duration.max(it.duration);
            }
            if event.end_bit {
                it.end_bit = true;
            }
            true
        } else {
            false
        }
    }

    /// C++ `CompareEvents`: true if `a` goes before `b`, by start timestamp
    /// (with wraparound), then event number.
    fn compare_events(a: &DtmfEvent, b: &DtmfEvent) -> bool {
        if a.timestamp == b.timestamp {
            return a.event_no < b.event_no;
        }
        // Take wrap-around into account.
        b.timestamp.wrapping_sub(a.timestamp) < 0xFFFF_FFFF / 2
    }

    /// C++ `std::list::sort(CompareEvents)`: a stable sort. Insertion sort
    /// on the "less than" predicate, so a comparator that is not a strict
    /// weak order (timestamps spanning half the wrap range) cannot panic.
    fn sort_events(buffer: &mut VecDeque<DtmfEvent>) {
        for i in 1..buffer.len() {
            let mut j = i;
            while j > 0 && Self::compare_events(&buffer[j], &buffer[j - 1]) {
                buffer.swap(j, j - 1);
                j -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Port of dtmf_buffer_unittest.cc (`LEGACY_BITEXACT` branches off).

    use super::*;

    const SAMPLE_RATE_HZ: i32 = 8000;

    /// RFC 4733 payload bytes, network byte order.
    fn make_dtmf_payload(event: i32, end: bool, volume: i32, duration: i32) -> [u8; 4] {
        let mut payload: u32 = 0;
        payload |= ((event & 0x00FF) as u32) << 24;
        payload |= if end { 0x0080_0000 } else { 0 };
        payload |= ((volume & 0x003F) as u32) << 16;
        payload |= (duration & 0xFFFF) as u32;
        payload.to_be_bytes()
    }

    fn equal_events(a: &DtmfEvent, b: &DtmfEvent) -> bool {
        a.duration == b.duration
            && a.end_bit == b.end_bit
            && a.event_no == b.event_no
            && a.timestamp == b.timestamp
            && a.volume == b.volume
    }

    #[test]
    fn create_and_destroy() {
        let _buffer = DtmfBuffer::new(SAMPLE_RATE_HZ);
    }

    /// Test the event parser.
    #[test]
    fn parse_event() {
        let event_no = 7;
        let end_bit = true;
        let volume = 17;
        let duration = 4711;
        let timestamp = 0x12345678;
        let payload = make_dtmf_payload(event_no, end_bit, volume, duration);
        let mut event = DtmfEvent::default();
        assert_eq!(
            DtmfBuffer::K_OK,
            DtmfBuffer::parse_event(timestamp, &payload, &mut event)
        );
        assert_eq!(duration, event.duration);
        assert_eq!(end_bit, event.end_bit);
        assert_eq!(event_no, event.event_no);
        assert_eq!(timestamp, event.timestamp);
        assert_eq!(volume, event.volume);

        assert_eq!(
            DtmfBuffer::K_PAYLOAD_TOO_SHORT,
            DtmfBuffer::parse_event(timestamp, &payload[..3], &mut event)
        );
    }

    #[test]
    fn simple_insert_and_get() {
        let event_no = 7;
        let end_bit = true;
        let volume = 17;
        let duration = 4711;
        let timestamp: u32 = 0x12345678;
        let event = DtmfEvent::new(timestamp, event_no, volume, duration, end_bit);
        let mut buffer = DtmfBuffer::new(SAMPLE_RATE_HZ);
        assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&event));
        assert_eq!(1, buffer.length());
        assert!(!buffer.empty());
        let mut out_event = DtmfEvent::default();
        // Too early to get event.
        assert!(!buffer.get_event(timestamp - 10, &mut out_event));
        assert_eq!(1, buffer.length());
        assert!(!buffer.empty());
        // Get the event at its starting timestamp.
        assert!(buffer.get_event(timestamp, &mut out_event));
        assert!(equal_events(&event, &out_event));
        assert_eq!(1, buffer.length());
        assert!(!buffer.empty());
        // Get the event some time into the event.
        assert!(buffer.get_event(timestamp + duration as u32 / 2, &mut out_event));
        assert!(equal_events(&event, &out_event));
        assert_eq!(1, buffer.length());
        assert!(!buffer.empty());
        // Give a "current" timestamp after the event has ended.
        assert!(!buffer.get_event(timestamp + duration as u32 + 10, &mut out_event));
        assert_eq!(0, buffer.length());
        assert!(buffer.empty());
    }

    #[test]
    fn merging_packets() {
        let event_no = 0;
        let end_bit = false;
        let volume = 17;
        let duration = 80;
        let timestamp: u32 = 0x12345678;
        let mut event = DtmfEvent::new(timestamp, event_no, volume, duration, end_bit);
        let mut buffer = DtmfBuffer::new(SAMPLE_RATE_HZ);
        assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&event));

        event.duration += 80;
        assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&event));

        event.duration += 80;
        event.end_bit = true;
        assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&event));

        assert_eq!(1, buffer.length());

        let mut out_event = DtmfEvent::default();
        assert!(buffer.get_event(timestamp, &mut out_event));
        assert!(equal_events(&event, &out_event));
    }

    /// One shorter event completely overlapped by one longer event. Only the
    /// longer event is played.
    #[test]
    fn overlapping_events() {
        let mut event_no = 0;
        let mut end_bit = true;
        let volume = 1;
        let duration = 80;
        let mut timestamp: u32 = 0x12345678 + 80;
        let short_event = DtmfEvent::new(timestamp, event_no, volume, duration, end_bit);
        let mut buffer = DtmfBuffer::new(SAMPLE_RATE_HZ);
        assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&short_event));

        event_no = 10;
        end_bit = false;
        timestamp = 0x12345678;
        let mut long_event = DtmfEvent::new(timestamp, event_no, volume, duration, end_bit);
        assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&long_event));

        long_event.duration += 80;
        assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&long_event));

        long_event.duration += 80;
        long_event.end_bit = true;
        assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&long_event));

        assert_eq!(2, buffer.length());

        let mut out_event = DtmfEvent::default();
        // Expect to get the long event.
        assert!(buffer.get_event(timestamp, &mut out_event));
        assert!(equal_events(&long_event, &out_event));
        // Expect no more events.
        assert!(!buffer.get_event(timestamp + long_event.duration as u32 + 10, &mut out_event));
        assert!(buffer.empty());
    }

    #[test]
    fn extrapolation_time() {
        let mut event_no = 0;
        let end_bit = false;
        let volume = 1;
        let duration = 80;
        let mut timestamp: u32 = 0x12345678;
        let event1 = DtmfEvent::new(timestamp, event_no, volume, duration, end_bit);
        let mut buffer = DtmfBuffer::new(SAMPLE_RATE_HZ);
        assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&event1));
        assert_eq!(1, buffer.length());

        let mut out_event = DtmfEvent::default();
        // Get the event at the start.
        assert!(buffer.get_event(timestamp, &mut out_event));
        assert!(equal_events(&event1, &out_event));
        // Also get the event 100 samples after the end of the event (since
        // we're missing the end bit).
        let mut timestamp_now = timestamp + duration as u32 + 100;
        assert!(buffer.get_event(timestamp_now, &mut out_event));
        assert!(equal_events(&event1, &out_event));
        // Insert another event starting back-to-back with the previous event.
        timestamp += duration as u32;
        event_no = 1;
        let event2 = DtmfEvent::new(timestamp, event_no, volume, duration, end_bit);
        assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&event2));
        assert_eq!(2, buffer.length());
        // Now we expect to get the new event when supplying `timestamp_now`.
        assert!(buffer.get_event(timestamp_now, &mut out_event));
        assert!(equal_events(&event2, &out_event));
        // Expect the the first event to be erased now.
        assert_eq!(1, buffer.length());
        // Move `timestamp_now` to more than 560 samples after the end of the
        // second event. Expect that event to be erased.
        timestamp_now = timestamp + duration as u32 + 600;
        assert!(!buffer.get_event(timestamp_now, &mut out_event));
        assert!(buffer.empty());
    }

    #[test]
    fn timestamp_wraparound() {
        let event_no = 0;
        let end_bit = true;
        let volume = 1;
        let duration = 80;
        let timestamp1: u32 = 0xFFFF_FFFF - duration as u32;
        let event1 = DtmfEvent::new(timestamp1, event_no, volume, duration, end_bit);
        let timestamp2: u32 = 0;
        let event2 = DtmfEvent::new(timestamp2, event_no, volume, duration, end_bit);
        let mut buffer = DtmfBuffer::new(SAMPLE_RATE_HZ);
        assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&event1));
        assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&event2));
        assert_eq!(2, buffer.length());
        let mut out_event = DtmfEvent::default();
        assert!(buffer.get_event(timestamp1, &mut out_event));
        assert!(equal_events(&event1, &out_event));
        assert_eq!(2, buffer.length());

        buffer.flush();
        // Reverse the insert order. Expect same results.
        assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&event2));
        assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&event1));
        assert_eq!(2, buffer.length());
        assert!(buffer.get_event(timestamp1, &mut out_event));
        assert!(equal_events(&event1, &out_event));
        assert_eq!(2, buffer.length());
    }

    #[test]
    fn invalid_events() {
        let event_no = 0;
        let end_bit = true;
        let volume = 1;
        let duration = 80;
        let timestamp: u32 = 0x12345678;
        let mut event = DtmfEvent::new(timestamp, event_no, volume, duration, end_bit);
        let mut buffer = DtmfBuffer::new(SAMPLE_RATE_HZ);

        // Invalid event number.
        event.event_no = -1;
        assert_eq!(
            DtmfBuffer::K_INVALID_EVENT_PARAMETERS,
            buffer.insert_event(&event)
        );
        event.event_no = 16;
        assert_eq!(
            DtmfBuffer::K_INVALID_EVENT_PARAMETERS,
            buffer.insert_event(&event)
        );
        event.event_no = 0; // Valid value;

        // Invalid volume.
        event.volume = -1;
        assert_eq!(
            DtmfBuffer::K_INVALID_EVENT_PARAMETERS,
            buffer.insert_event(&event)
        );
        event.volume = 64;
        assert_eq!(
            DtmfBuffer::K_INVALID_EVENT_PARAMETERS,
            buffer.insert_event(&event)
        );
        event.volume = 0; // Valid value;

        // Invalid duration.
        event.duration = -1;
        assert_eq!(
            DtmfBuffer::K_INVALID_EVENT_PARAMETERS,
            buffer.insert_event(&event)
        );
        event.duration = 0;
        assert_eq!(
            DtmfBuffer::K_INVALID_EVENT_PARAMETERS,
            buffer.insert_event(&event)
        );
        event.duration = 0xFFFF + 1;
        assert_eq!(
            DtmfBuffer::K_INVALID_EVENT_PARAMETERS,
            buffer.insert_event(&event)
        );
        event.duration = 1; // Valid value;

        // Finish with a valid event, just to verify that all is ok.
        assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&event));
    }

    #[test]
    fn flushes_when_full_and_preserves_continuation_merge() {
        let mut buffer = DtmfBuffer::new(SAMPLE_RATE_HZ);
        for i in 0..DtmfBuffer::K_MAX_DTMF_BUFFER_SIZE {
            let end_bit = i + 1 < DtmfBuffer::K_MAX_DTMF_BUFFER_SIZE;
            let event = DtmfEvent::new((i * 160) as u32, 5, 10, 80, end_bit);
            assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&event));
        }
        assert_eq!(DtmfBuffer::K_MAX_DTMF_BUFFER_SIZE, buffer.length());

        // A continuation packet for an already-buffered tone merges without
        // flushing.
        let last_ts = ((DtmfBuffer::K_MAX_DTMF_BUFFER_SIZE - 1) * 160) as u32;
        let continuation = DtmfEvent::new(last_ts, 5, 10, 160, true);
        assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&continuation));
        assert_eq!(DtmfBuffer::K_MAX_DTMF_BUFFER_SIZE, buffer.length());

        // Inserting one more distinct event flushes the buffer and retains the
        // new event.
        let new_ts = (DtmfBuffer::K_MAX_DTMF_BUFFER_SIZE * 160) as u32;
        let overflow_event = DtmfEvent::new(new_ts, 9, 10, 80, true);
        assert_eq!(DtmfBuffer::K_OK, buffer.insert_event(&overflow_event));
        assert_eq!(1, buffer.length());

        let mut out_event = DtmfEvent::default();
        assert!(!buffer.get_event(0, &mut out_event));
        assert!(buffer.get_event(new_ts, &mut out_event));
        assert!(equal_events(&overflow_event, &out_event));
    }

    #[test]
    fn set_sample_rate_validates_and_sets_extrapolation() {
        let mut buffer = DtmfBuffer::new(SAMPLE_RATE_HZ);
        assert_eq!(
            DtmfBuffer::K_INVALID_SAMPLE_RATE,
            buffer.set_sample_rate(44100)
        );
        assert_eq!(DtmfBuffer::K_OK, buffer.set_sample_rate(48000));
        assert_eq!(7 * 48000 / 100, buffer.max_extrapolation_samples);
        assert_eq!(480, buffer.frame_len_samples);
    }

    #[test]
    fn events_are_sorted_by_timestamp_then_event_number() {
        let mut buffer = DtmfBuffer::new(SAMPLE_RATE_HZ);
        buffer.insert_event(&DtmfEvent::new(320, 3, 10, 80, true));
        buffer.insert_event(&DtmfEvent::new(160, 2, 10, 80, true));
        buffer.insert_event(&DtmfEvent::new(160, 1, 10, 80, true));
        let order: Vec<(u32, i32)> = buffer
            .buffer
            .iter()
            .map(|event| (event.timestamp, event.event_no))
            .collect();
        assert_eq!(vec![(160, 1), (160, 2), (320, 3)], order);
    }
}
