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
 */

//! Receiver-side packet quality diagnostics shared by WebSocket and WebTransport clients.
//!
//! Parses every inbound `PacketWrapper` → `MediaPacket`, tracks per-sender sequence
//! numbers, measures jitter (std dev of inter-arrival deltas), and computes A/V sync
//! drift. Reports a summary line at `INFO` level every 10 seconds.

use protobuf::Message;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::info;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

/// Tracks inbound packet statistics for quality-of-service diagnostics.
#[derive(Default)]
pub struct InboundStats {
    audio_packets: u64,
    video_packets: u64,
    video_keyframes: u64,
    heartbeat_packets: u64,
    other_packets: u64,
    audio_bytes: u64,
    video_bytes: u64,
    /// Last seen audio sequence per sender
    last_audio_seq: HashMap<String, u64>,
    /// Last seen video sequence per sender
    last_video_seq: HashMap<String, u64>,
    audio_seq_gaps: u64,
    video_seq_gaps: u64,
    /// Arrival times for jitter calculation
    video_arrivals: Vec<f64>,
    audio_arrivals: Vec<f64>,
    /// Last audio and video timestamps per sender for A/V sync measurement
    last_audio_ts: HashMap<String, f64>,
    last_video_ts: HashMap<String, f64>,
    av_sync_deltas: Vec<f64>,
    parse_errors: u64,
}

impl InboundStats {
    pub fn record_packet(&mut self, _my_user_id: &str, data: &[u8]) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as f64;

        let Ok(wrapper) = PacketWrapper::parse_from_bytes(data) else {
            self.parse_errors += 1;
            return;
        };

        if wrapper.packet_type.enum_value() != Ok(PacketType::MEDIA) {
            self.other_packets += 1;
            return;
        }

        let Ok(media) = MediaPacket::parse_from_bytes(&wrapper.data) else {
            self.parse_errors += 1;
            return;
        };

        // The relay populates wrapper.user_id but strips media.user_id,
        // so use the wrapper-level user_id for per-sender tracking.
        let sender = String::from_utf8_lossy(&wrapper.user_id).to_string();

        match media.media_type.enum_value() {
            Ok(MediaType::AUDIO) => {
                self.audio_packets += 1;
                self.audio_bytes += media.data.len() as u64;
                self.audio_arrivals.push(now_ms);
                self.last_audio_ts.insert(sender.clone(), media.timestamp);

                if let Some(meta) = media.audio_metadata.as_ref() {
                    let seq = meta.sequence;
                    if let Some(prev) = self.last_audio_seq.get(&sender) {
                        if seq > prev + 1 {
                            self.audio_seq_gaps += seq - prev - 1;
                        }
                    }
                    self.last_audio_seq.insert(sender.clone(), seq);
                }

                // A/V sync: compare against same sender's last video timestamp
                if let Some(vts) = self.last_video_ts.get(&sender) {
                    self.av_sync_deltas.push(media.timestamp - vts);
                }
            }
            Ok(MediaType::VIDEO) => {
                self.video_packets += 1;
                self.video_bytes += media.data.len() as u64;
                self.video_arrivals.push(now_ms);
                self.last_video_ts.insert(sender.clone(), media.timestamp);

                if media.frame_type == "key" {
                    self.video_keyframes += 1;
                }

                if let Some(meta) = media.video_metadata.as_ref() {
                    let seq = meta.sequence;
                    if let Some(prev) = self.last_video_seq.get(&sender) {
                        if seq > prev + 1 {
                            self.video_seq_gaps += seq - prev - 1;
                        }
                    }
                    self.last_video_seq.insert(sender, seq);
                }
            }
            Ok(MediaType::HEARTBEAT) => {
                self.heartbeat_packets += 1;
            }
            _ => {
                self.other_packets += 1;
            }
        }
    }

    fn jitter_ms(arrivals: &[f64]) -> f64 {
        if arrivals.len() < 2 {
            return 0.0;
        }
        let deltas: Vec<f64> = arrivals.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        let mean = deltas.iter().sum::<f64>() / deltas.len() as f64;
        let variance = deltas.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / deltas.len() as f64;
        variance.sqrt()
    }

    pub fn report(&self, user_id: &str) {
        let audio_jitter = Self::jitter_ms(&self.audio_arrivals);
        let video_jitter = Self::jitter_ms(&self.video_arrivals);

        let avg_av_sync = if self.av_sync_deltas.is_empty() {
            0.0
        } else {
            self.av_sync_deltas.iter().sum::<f64>() / self.av_sync_deltas.len() as f64
        };

        info!(
            "[{}] RX STATS (10s): audio={} pkts ({:.0} KB, jitter={:.1}ms, gaps={}), \
             video={} pkts ({} key, {:.0} KB, jitter={:.1}ms, gaps={}), \
             heartbeat={}, A/V sync={:.0}ms, errors={}",
            user_id,
            self.audio_packets,
            self.audio_bytes as f64 / 1024.0,
            audio_jitter,
            self.audio_seq_gaps,
            self.video_packets,
            self.video_keyframes,
            self.video_bytes as f64 / 1024.0,
            video_jitter,
            self.video_seq_gaps,
            self.heartbeat_packets,
            avg_av_sync,
            self.parse_errors,
        );
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}
