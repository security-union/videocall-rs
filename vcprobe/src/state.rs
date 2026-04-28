use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use protobuf::Message;
use videocall_types::protos::health_packet::HealthPacket;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::user_id_bytes_to_string;

// Mark participant as stale (grayed out) after no packets
// Reduced from 6s to 2s since we track all packet types (AUDIO/VIDEO at ~70/sec combined)
const STALE_THRESHOLD: Duration = Duration::from_secs(2);
// Remove from list entirely after this long
const PRUNE_THRESHOLD: Duration = Duration::from_secs(10);

/// Quality data about a participant as observed by one of their peers.
/// Populated from HEALTH packets. May be absent if no health data received yet.
#[derive(Debug, Clone)]
pub struct QualitySnapshot {
    /// Audio jitter buffer current depth (ms) — how much audio is queued right now
    pub buf_depth_ms: f64,
    /// Delay manager target delay (ms) — the algorithm's estimate of network jitter.
    /// This is the real VoIP jitter metric: <30ms excellent, <75ms acceptable, >=75ms poor.
    pub target_delay_ms: f64,
    /// Audio concealment rate (expand_per_sec) — the key quality signal.
    /// 0 = perfect, >5 = degraded, >10 = poor
    pub conceal_per_sec: f64,
    /// Video FPS as observed by a peer
    pub fps: f64,
    /// Video bitrate as observed by a peer (kbps)
    pub bitrate_kbps: u64,
    /// Video frames dropped per second (windowed rate)
    pub decode_errors_per_sec: f64,
    /// Audio concealment percentage (0.0-100.0)
    pub audio_concealment_pct: f64,
    /// Audio packets received per second (from NetEQ). Near 0 = speaker is silent (DTX).
    pub audio_packets_per_sec: f64,
    /// Average decode latency in ms (optional)
    pub avg_decode_latency_ms: Option<f64>,
    /// Client-computed quality scores (0-100, absent when stream inactive)
    pub audio_quality_score: Option<f64>,
    pub video_quality_score: Option<f64>,
    pub call_quality_score: Option<f64>,
    pub updated_at: Instant,
}

#[derive(Debug)]
pub struct Participant {
    pub user_id: String,
    pub session_id: String,
    pub display_name: Option<String>,
    pub video_enabled: bool,
    pub audio_enabled: bool,
    pub last_heartbeat: Instant,
    /// RTT to server, self-reported in HEALTH packets
    pub rtt_ms: Option<f64>,
    /// Active transport type: "webtransport" or "websocket"
    pub active_transport: Option<String>,
    /// Latest quality snapshot from any peer observing this participant
    pub quality: Option<QualitySnapshot>,
    /// Tab visibility state
    pub is_tab_visible: bool,
    /// Memory used in bytes (Chrome only)
    pub memory_used_bytes: Option<u64>,
    /// Average encode latency in ms
    pub avg_encode_latency_ms: Option<f64>,
    /// Browser tab throttling state
    pub is_tab_throttled: bool,
    /// Bytes queued in send buffer
    pub send_queue_bytes: Option<u64>,
    /// Inbound packet rate (packets/sec)
    pub packets_received_per_sec: Option<f64>,
    /// Outbound packet rate (packets/sec)
    pub packets_sent_per_sec: Option<f64>,
}

impl Participant {
    fn new(user_id: String, session_id: String) -> Self {
        Self {
            user_id,
            session_id,
            display_name: None,
            video_enabled: false,
            audio_enabled: false,
            last_heartbeat: Instant::now(),
            rtt_ms: None,
            active_transport: None,
            quality: None,
            is_tab_visible: true,
            memory_used_bytes: None,
            avg_encode_latency_ms: None,
            is_tab_throttled: false,
            send_queue_bytes: None,
            packets_received_per_sec: None,
            packets_sent_per_sec: None,
        }
    }

    pub fn is_stale(&self) -> bool {
        self.last_heartbeat.elapsed() > STALE_THRESHOLD
    }

    fn should_prune(&self) -> bool {
        self.last_heartbeat.elapsed() > PRUNE_THRESHOLD
    }

    pub fn quality_score(&self) -> Option<f64> {
        self.quality.as_ref().map(|q| {
            // 0.0 = perfect, 1.0 = terrible
            // Weighted: concealment is the most important signal
            let conceal_score = (q.conceal_per_sec / 10.0).min(1.0);
            // Only score jitter when audio is actually flowing; default to 0 (no penalty)
            let jitter_score = if q.audio_packets_per_sec >= 2.0 {
                (q.target_delay_ms / 150.0).min(1.0)
            } else {
                0.0
            };
            let rtt_score = self.rtt_ms.map(|r| (r / 300.0).min(1.0)).unwrap_or(0.0);
            (conceal_score * 0.5 + jitter_score * 0.3 + rtt_score * 0.2).min(1.0)
        })
    }
}

#[derive(Debug)]
pub struct Event {
    /// Wall-clock time captured at event creation — fixed, not recomputed.
    pub when: DateTime<Local>,
    pub msg: String,
}

pub struct MeetingState {
    pub meeting_id: String,
    pub started_at: Instant,
    pub participants: HashMap<String, Participant>,
    pub events: VecDeque<Event>,
    /// Stores display_name for sessions whose PARTICIPANT_JOINED arrived before MEDIA/HEALTH.
    pending_display_names: HashMap<String, String>,
}

impl MeetingState {
    pub fn new(meeting_id: String) -> Self {
        Self {
            meeting_id,
            started_at: Instant::now(),
            participants: HashMap::new(),
            events: VecDeque::new(),
            pending_display_names: HashMap::new(),
        }
    }

    pub fn process_packet(&mut self, raw: &[u8]) {
        let pkt = match PacketWrapper::parse_from_bytes(raw) {
            Ok(p) => p,
            Err(_) => return,
        };

        match pkt
            .packet_type
            .enum_value()
            .unwrap_or(PacketType::PACKET_TYPE_UNKNOWN)
        {
            PacketType::MEDIA => self.process_media(&pkt),
            PacketType::HEALTH => self.process_health(&pkt),
            PacketType::MEETING => self.process_meeting(&pkt),
            _ => {}
        }
    }

    fn process_media(&mut self, pkt: &PacketWrapper) {
        let media = match MediaPacket::parse_from_bytes(&pkt.data) {
            Ok(m) => m,
            Err(_) => return,
        };

        if media.user_id.is_empty() {
            log::debug!("process_media: MediaPacket has empty user_id, skipping");
            return;
        }
        let user_id = user_id_bytes_to_string(&media.user_id);

        // Use session_id as the unique key per browser tab
        if pkt.session_id == 0 {
            log::debug!("process_media: PacketWrapper has session_id=0, skipping");
            return;
        }
        let session_id = pkt.session_id.to_string();

        let is_new = !self.participants.contains_key(&session_id);
        let p = self
            .participants
            .entry(session_id.clone())
            .or_insert_with(|| Participant::new(user_id.clone(), session_id.clone()));
        p.last_heartbeat = Instant::now();
        if p.display_name.is_none() {
            if let Some(dn) = self.pending_display_names.get(&session_id) {
                p.display_name = Some(dn.clone());
            }
        }

        if media
            .media_type
            .enum_value()
            .unwrap_or(MediaType::MEDIA_TYPE_UNKNOWN)
            == MediaType::HEARTBEAT
        {
            if let Some(hb) = media.heartbeat_metadata.as_ref() {
                p.video_enabled = hb.video_enabled;
                p.audio_enabled = hb.audio_enabled;
            }
            if is_new {
                self.push_event(format!(
                    "{} joined (s:{})",
                    user_id,
                    &session_id[session_id.len().saturating_sub(4)..]
                ));
            }
        }
    }

    fn process_health(&mut self, pkt: &PacketWrapper) {
        let health = match HealthPacket::parse_from_bytes(&pkt.data) {
            Ok(h) => h,
            Err(_) => return,
        };

        if health.reporting_user_id.is_empty() || health.session_id.is_empty() {
            return;
        }
        let reporting_user = user_id_bytes_to_string(&health.reporting_user_id);
        let session_id = health.session_id.clone();

        // Upsert reporter — create if not yet seen (listen-only, or HEALTH arrived before MEDIA)
        let is_new = !self.participants.contains_key(&session_id);
        let p = self
            .participants
            .entry(session_id.clone())
            .or_insert_with(|| Participant::new(reporting_user.clone(), session_id.clone()));
        if p.display_name.is_none() {
            // Prefer display_name carried in the health packet itself (field 19).
            // Fall back to pending_display_names populated from PARTICIPANT_JOINED.
            if let Some(dn) = health.display_name.as_deref().filter(|s| !s.is_empty()) {
                p.display_name = Some(dn.to_string());
            } else if let Some(dn) = self.pending_display_names.get(&session_id) {
                p.display_name = Some(dn.clone());
            }
        }
        p.rtt_ms = Some(health.active_server_rtt_ms);
        if !health.active_server_type.is_empty() {
            p.active_transport = Some(health.active_server_type.clone());
        }
        p.is_tab_visible = health.is_tab_visible;
        p.memory_used_bytes = health.memory_used_bytes;
        p.avg_encode_latency_ms = health.avg_encode_latency_ms;
        p.is_tab_throttled = health.is_tab_throttled;
        p.send_queue_bytes = health.send_queue_bytes;
        p.packets_received_per_sec = health.packets_received_per_sec;
        p.packets_sent_per_sec = health.packets_sent_per_sec;
        p.last_heartbeat = Instant::now();
        if is_new {
            self.push_event(format!(
                "{} joined (s:{})",
                reporting_user,
                &session_id[session_id.len().saturating_sub(4)..]
            ));
        }

        // peer_stats keys ARE session_id strings — direct lookup, no mapping needed
        for (peer_id, peer_stats) in &health.peer_stats {
            if let Some(p) = self.participants.get_mut(peer_id) {
                let buf_depth_ms = peer_stats
                    .neteq_stats
                    .as_ref()
                    .map(|n| n.current_buffer_size_ms)
                    .unwrap_or(0.0);

                let target_delay_ms = peer_stats
                    .neteq_stats
                    .as_ref()
                    .map(|n| n.target_delay_ms)
                    .unwrap_or(0.0);

                let conceal_per_sec = peer_stats
                    .neteq_stats
                    .as_ref()
                    .and_then(|n| n.network.as_ref())
                    .and_then(|net| net.operation_counters.as_ref())
                    .map(|op| op.expand_per_sec)
                    .unwrap_or(0.0);

                let fps = peer_stats
                    .video_stats
                    .as_ref()
                    .map(|v| v.fps_received)
                    .unwrap_or(0.0);

                let bitrate_kbps = peer_stats
                    .video_stats
                    .as_ref()
                    .map(|v| v.bitrate_kbps)
                    .unwrap_or(0);

                let audio_packets_per_sec = peer_stats
                    .neteq_stats
                    .as_ref()
                    .map(|n| n.packets_per_sec)
                    .unwrap_or(0.0);

                p.quality = Some(QualitySnapshot {
                    buf_depth_ms,
                    target_delay_ms,
                    conceal_per_sec,
                    fps,
                    bitrate_kbps,
                    decode_errors_per_sec: peer_stats.frames_dropped_per_sec,
                    audio_concealment_pct: peer_stats.audio_concealment_pct,
                    audio_packets_per_sec,
                    avg_decode_latency_ms: peer_stats.avg_decode_latency_ms,
                    audio_quality_score: peer_stats.audio_quality_score,
                    video_quality_score: peer_stats.video_quality_score,
                    call_quality_score: peer_stats.call_quality_score,
                    updated_at: Instant::now(),
                });
            }
        }
    }

    fn process_meeting(&mut self, pkt: &PacketWrapper) {
        let meeting = match MeetingPacket::parse_from_bytes(&pkt.data) {
            Ok(m) => m,
            Err(_) => return,
        };

        if meeting.event_type.enum_value_or_default() != MeetingEventType::PARTICIPANT_JOINED {
            return;
        }

        if meeting.session_id == 0 || meeting.display_name.is_empty() {
            return;
        }

        let session_id = meeting.session_id.to_string();
        let display_name = user_id_bytes_to_string(&meeting.display_name);

        // Always store so MEDIA/HEALTH upserts can apply it even if they arrive later.
        self.pending_display_names
            .insert(session_id.clone(), display_name.clone());

        if let Some(p) = self.participants.get_mut(&session_id) {
            if p.display_name.is_none() {
                p.display_name = Some(display_name);
            }
        }
    }

    /// Called every second: prune participants that have been silent too long.
    pub fn tick(&mut self) {
        let stale: Vec<(String, String)> = self
            .participants
            .values()
            .filter(|p| p.should_prune())
            .map(|p| (p.session_id.clone(), p.user_id.clone()))
            .collect();

        for (sid, uid) in stale {
            self.participants.remove(&sid);
            self.push_event(format!(
                "{} left (s:{})",
                uid,
                &sid[sid.len().saturating_sub(4)..]
            ));
        }

        // Prune pending display names for sessions that no longer exist
        self.pending_display_names
            .retain(|sid, _| self.participants.contains_key(sid));
    }

    fn push_event(&mut self, msg: String) {
        self.events.push_back(Event {
            when: Local::now(),
            msg,
        });
        if self.events.len() > 500 {
            self.events.pop_front();
        }
    }

    /// Participants sorted alphabetically for stable table ordering.
    pub fn sorted_participants(&self, by_quality: bool) -> Vec<&Participant> {
        let mut v: Vec<&Participant> = self.participants.values().collect();

        if by_quality {
            // Sort worst-first by call_quality_score (0-100, higher = better → invert for sort).
            // Fall back to the locally-computed quality_score() (0.0-1.0, higher = worse) when
            // no HEALTH packets have arrived yet for a participant.
            v.sort_by(|a, b| {
                // Normalise both to 0.0-1.0 where 1.0 = worst, for a uniform comparison key.
                let score_a = a
                    .quality
                    .as_ref()
                    .and_then(|q| q.call_quality_score)
                    .map(|s| 1.0 - s / 100.0) // invert: 100→0.0, 0→1.0
                    .or_else(|| a.quality_score()) // fallback: already 0.0=good,1.0=bad
                    .unwrap_or(0.0); // no data → treat as perfect (float to bottom)
                let score_b = b
                    .quality
                    .as_ref()
                    .and_then(|q| q.call_quality_score)
                    .map(|s| 1.0 - s / 100.0)
                    .or_else(|| b.quality_score())
                    .unwrap_or(0.0);

                // Higher score = worse → sort descending (worst at top)
                match score_b.partial_cmp(&score_a) {
                    Some(std::cmp::Ordering::Equal) | None => a
                        .user_id
                        .cmp(&b.user_id)
                        .then(a.session_id.cmp(&b.session_id)),
                    Some(ord) => ord,
                }
            });
        } else {
            // Alphabetical by user_id, then session_id for same-user tabs
            v.sort_by(|a, b| {
                a.user_id
                    .cmp(&b.user_id)
                    .then(a.session_id.cmp(&b.session_id))
            });
        }

        v
    }

    pub fn elapsed_str(&self) -> String {
        let secs = self.started_at.elapsed().as_secs();
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        if h > 0 {
            format!("{}:{:02}:{:02}", h, m, s)
        } else {
            format!("{:02}:{:02}", m, s)
        }
    }
}
