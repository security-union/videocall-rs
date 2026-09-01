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

//! Allocation budget for `InboundStats::record_packet` (#2250). The counting
//! `GlobalAlloc` is per test BINARY, so a second test here would race on it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use protobuf::Message;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{AudioMetadata, MediaPacket, VideoMetadata};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

use bot::inbound_stats::InboundStats;

static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static A: Counting = Counting;

fn media_packet(sender: &str, session_id: u64, kind: MediaType, seq: u64, rung: u32) -> Vec<u8> {
    let mut media = MediaPacket::new();
    media.media_type = kind.into();
    media.data = vec![0u8; 100];
    media.timestamp = seq as f64;
    match kind {
        MediaType::AUDIO => {
            media.audio_metadata = Some(AudioMetadata {
                sequence: seq,
                ..Default::default()
            })
            .into();
        }
        MediaType::VIDEO => {
            media.video_metadata = Some(VideoMetadata {
                sequence: seq,
                ..Default::default()
            })
            .into();
        }
        _ => {}
    }
    let wrapper = PacketWrapper {
        user_id: sender.as_bytes().to_vec(),
        packet_type: PacketType::MEDIA.into(),
        session_id,
        simulcast_layer_id: rung,
        data: media.write_to_bytes().unwrap(),
        ..Default::default()
    };
    wrapper.write_to_bytes().unwrap()
}

/// Per steady-state media packet. The residual floor is the two protobuf parses.
const MAX_ALLOCS_PER_PACKET: f64 = 4.0;

#[test]
fn steady_state_media_packets_stay_within_the_allocation_budget() {
    let packets: Vec<Vec<u8>> = (0..6)
        .map(|i| {
            let (kind, rung) = if i < 3 {
                (MediaType::AUDIO, i)
            } else {
                (MediaType::VIDEO, i - 3)
            };
            media_packet("alice", 11, kind, 0, rung)
        })
        .collect();

    let mut stats = InboundStats::default();

    for _ in 0..64 {
        for p in &packets {
            stats.record_packet("bot", p);
        }
    }

    const ITERS: u64 = 20_000;

    let calls_before = ALLOC_CALLS.load(Ordering::Relaxed);
    let bytes_before = ALLOC_BYTES.load(Ordering::Relaxed);
    let t0 = Instant::now();
    for _ in 0..ITERS {
        for p in &packets {
            stats.record_packet("bot", p);
        }
    }
    let elapsed = t0.elapsed();
    let calls = ALLOC_CALLS.load(Ordering::Relaxed) - calls_before;
    let bytes = ALLOC_BYTES.load(Ordering::Relaxed) - bytes_before;

    let total = ITERS * packets.len() as u64;
    let per_packet = calls as f64 / total as f64;
    let bytes_per_packet = bytes as f64 / total as f64;
    let ns_per_packet = elapsed.as_nanos() as f64 / total as f64;

    println!(
        "record_packet: {total} packets, {per_packet:.3} allocs/pkt, \
         {bytes_per_packet:.1} bytes/pkt, {ns_per_packet:.1} ns/pkt"
    );

    assert!(
        per_packet <= MAX_ALLOCS_PER_PACKET,
        "{:.3} allocator calls per steady-state media packet exceeds the {} budget",
        per_packet,
        MAX_ALLOCS_PER_PACKET
    );
}
