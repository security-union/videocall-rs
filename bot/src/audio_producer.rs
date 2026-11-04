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

use crate::aq_controller::BotAq;
use crate::transport::{MediaTypeLabel, OutboundFrame, OutboundFrameSender};
use opus::{
    Application as OpusApp, Bitrate as OpusBitrate, Channels as OpusChannels,
    Encoder as OpusEncoder,
};
use protobuf::Message;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{error, info, warn};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{AudioMetadata, MediaPacket};
use videocall_types::protos::packet_wrapper::packet_wrapper::{MediaKind, PacketType};
use videocall_types::protos::packet_wrapper::PacketWrapper;

const RMS_SPEAKING_ENTER: f32 = 0.012;
const RMS_SPEAKING_EXIT: f32 = 0.008;

/// Frames below this RMS are skipped (not encoded or sent) unless the talker is continuous.
const SILENCE_RMS: f32 = 0.005;

/// Whether to skip a frame: near-silence is skipped, except for continuous talkers,
/// which send every 20 ms frame so receivers see ~50 audio packets/s.
pub fn skip_silent_frame(rms: f32, continuous: bool) -> bool {
    !continuous && rms < SILENCE_RMS
}

fn next_speaking_state(is_speaking: bool, rms: f32) -> bool {
    if is_speaking {
        rms > RMS_SPEAKING_EXIT
    } else {
        rms > RMS_SPEAKING_ENTER
    }
}

/// Stitch one looping 48 kHz timeline per broadcaster from the conversation lines.
///
/// `speakers[i]` spoke `line_audio[i]`. Each broadcaster gets its own lines in
/// place and silence elsewhere, with `pause_samples` of silence after every line.
/// Participants not in `broadcasters` get no entry. Returns the timelines and the
/// shared loop length in samples.
pub fn stitch_participant_audio(
    speakers: &[&str],
    line_audio: &[Vec<f32>],
    broadcasters: &[&str],
    pause_samples: usize,
) -> (std::collections::HashMap<String, Vec<f32>>, usize) {
    let lines_total: usize = line_audio.iter().map(|l| l.len() + pause_samples).sum();
    // With no lines a presenter still needs a silent timeline to send from.
    let total_samples = match lines_total {
        0 => crate::config::SILENT_LOOP.as_millis() as usize * 48,
        n => n,
    };
    let mut out = std::collections::HashMap::new();
    for name in broadcasters {
        let mut audio = Vec::with_capacity(total_samples);
        for (speaker, samples) in speakers.iter().zip(line_audio) {
            if speaker == name {
                audio.extend_from_slice(samples);
            } else {
                audio.resize(audio.len() + samples.len(), 0.0f32);
            }
            audio.resize(audio.len() + pause_samples, 0.0f32);
        }
        audio.resize(total_samples, 0.0f32);
        out.insert((*name).to_string(), audio);
    }
    (out, total_samples)
}

const SAMPLES_PER_PACKET: usize = 960;
const PACKET_INTERVAL_US: u64 = 20_000;
/// A wake-up sends at most this many owed packets, the newest; older ones are dropped.
const MAX_BACKLOG_PACKETS: u64 = 50;
/// A wake-up owing more than this many packets is logged.
const STALL_WARN_PACKETS: u64 = 10;

/// The 20 ms packet grid on the shared media clock. Tick `t` is sequence `t - first tick` and
/// carries the source window starting at `(t * 960) % wrap`, so each window starts where the
/// previous tick's ended.
#[derive(Debug)]
pub struct PacketGrid {
    next_tick: u64,
    wrap: usize,
}

impl PacketGrid {
    pub fn new(elapsed_us: u64, wrap: usize) -> Self {
        Self {
            next_tick: elapsed_us / PACKET_INTERVAL_US,
            wrap: wrap.max(1),
        }
    }

    /// The ticks to send back to back now: every owed tick, up to the newest `MAX_BACKLOG_PACKETS`.
    pub fn due(&mut self, elapsed_us: u64) -> std::ops::Range<u64> {
        let end = elapsed_us / PACKET_INTERVAL_US + 1;
        let start = self.next_tick.max(end.saturating_sub(MAX_BACKLOG_PACKETS));
        self.next_tick = end;
        start..end
    }

    pub fn window_start(&self, tick: u64) -> usize {
        (tick * SAMPLES_PER_PACKET as u64 % self.wrap as u64) as usize
    }

    pub fn next_deadline_us(&self) -> u64 {
        self.next_tick * PACKET_INTERVAL_US
    }
}

/// The audio thread's view of the shared media clock.
trait MediaClock {
    fn elapsed_us(&self) -> u64;
    fn sleep_until_us(&mut self, target_us: u64);
}

struct WallClock(Instant);

impl MediaClock for WallClock {
    fn elapsed_us(&self) -> u64 {
        self.0.elapsed().as_micros() as u64
    }

    fn sleep_until_us(&mut self, target_us: u64) {
        let deadline = self.0 + Duration::from_micros(target_us);
        let now = Instant::now();
        if now < deadline {
            thread::sleep(deadline - now);
        }
    }
}

/// Copy one packet of `source`, starting at `start` and wrapping at its end.
pub fn fill_window(source: &[f32], start: usize, out: &mut [f32]) {
    for (i, sample) in out.iter_mut().enumerate() {
        *sample = source[(start + i) % source.len()];
    }
}

pub struct AudioProducer {
    #[allow(dead_code)]
    user_id: String,
    quit: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AudioProducer {
    /// Read WAV file duration without loading all samples.
    #[allow(dead_code)]
    pub fn wav_duration(wav_path: &str) -> anyhow::Result<Duration> {
        let reader = hound::WavReader::open(wav_path)?;
        let spec = reader.spec();
        let num_samples = reader.len() as u64; // total sample frames
        let duration_ms = num_samples * 1000 / spec.sample_rate as u64;
        Ok(Duration::from_millis(duration_ms))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        user_id: String,
        audio_data: Vec<f32>,
        packet_sender: OutboundFrameSender,
        media_start: Instant,
        loop_duration: Duration,
        is_speaking: Arc<AtomicBool>,
        aq: Arc<BotAq>,
        transport_drops_counter: Arc<AtomicU64>,
        continuous: bool,
    ) -> anyhow::Result<Self> {
        let quit = Arc::new(AtomicBool::new(false));
        let quit_clone = quit.clone();
        let user_id_clone = user_id.clone();

        let handle = thread::spawn(move || {
            if let Err(e) = Self::audio_loop(
                user_id_clone,
                audio_data,
                packet_sender,
                quit_clone,
                WallClock(media_start),
                loop_duration,
                is_speaking,
                aq,
                transport_drops_counter,
                continuous,
            ) {
                error!("Audio producer error: {}", e);
            }
        });

        Ok(AudioProducer {
            user_id,
            quit,
            handle: Some(handle),
        })
    }

    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    pub fn from_wav_file(
        user_id: String,
        wav_path: &str,
        packet_sender: OutboundFrameSender,
        media_start: Instant,
        loop_duration: Duration,
        is_speaking: Arc<AtomicBool>,
        aq: Arc<BotAq>,
        transport_drops_counter: Arc<AtomicU64>,
    ) -> anyhow::Result<Self> {
        info!("Loading WAV file for {}: {}", user_id, wav_path);

        let mut reader = hound::WavReader::open(wav_path)?;
        let spec = reader.spec();
        let sample_rate = spec.sample_rate;
        let channels = spec.channels as u8;

        info!(
            "WAV spec -> sample_rate: {} Hz, channels: {}",
            sample_rate, channels
        );

        // Validate Opus requirements
        let opus_sample_rates = [8000, 12000, 16000, 24000, 48000];
        if !opus_sample_rates.contains(&sample_rate) {
            warn!(
                "Sample rate {} Hz not supported by Opus, audio may not work correctly",
                sample_rate
            );
        }

        if channels > 2 {
            warn!(
                "Opus only supports mono/stereo, but WAV has {} channels",
                channels
            );
        }

        // Read samples and convert to f32
        let wav_samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Int => reader
                .samples::<i16>()
                .map(|s| s.unwrap() as f32 / 32768.0)
                .collect(),
            hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
        };

        info!(
            "Loaded {} samples ({:.2} seconds)",
            wav_samples.len(),
            wav_samples.len() as f32 / sample_rate as f32 / channels as f32
        );

        Self::new(
            user_id,
            wav_samples,
            packet_sender,
            media_start,
            loop_duration,
            is_speaking,
            aq,
            transport_drops_counter,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn audio_loop(
        user_id: String,
        audio_data: Vec<f32>,
        packet_sender: OutboundFrameSender,
        quit: Arc<AtomicBool>,
        mut clock: impl MediaClock,
        loop_duration: Duration,
        is_speaking: Arc<AtomicBool>,
        aq: Arc<BotAq>,
        transport_drops_counter: Arc<AtomicU64>,
        continuous: bool,
    ) -> anyhow::Result<()> {
        if audio_data.is_empty() {
            warn!("Audio producer for {} has no audio data, exiting", user_id);
            return Ok(());
        }

        let sample_rate = 48000u32;
        let channels = 1u8;
        let loop_samples =
            (loop_duration.as_micros() as u64 * sample_rate as u64 / 1_000_000) as usize;
        if loop_samples == 0 {
            warn!(
                "Audio producer for {} has zero loop duration, exiting",
                user_id
            );
            return Ok(());
        }
        let wrap = loop_samples.min(audio_data.len());

        // Create Opus encoder seeded from the AQ controller's current audio tier.
        let mut a = aq.snapshot_audio();
        let mut last_epoch: u64 = aq.tier_epoch();

        let mut opus_encoder = OpusEncoder::new(sample_rate, OpusChannels::Mono, OpusApp::Voip)?;
        // Apply initial AQ-derived settings.
        if let Err(e) = opus_encoder.set_bitrate(OpusBitrate::Bits((a.bitrate_kbps * 1000) as i32))
        {
            warn!(
                "[{}] audio AQ: failed to set initial bitrate {}kbps: {}",
                user_id, a.bitrate_kbps, e
            );
        }
        if let Err(e) = opus_encoder.set_inband_fec(a.fec) {
            warn!(
                "[{}] audio AQ: failed to set initial FEC={}: {}",
                user_id, a.fec, e
            );
        }
        // NOTE: the `opus` 0.3 crate does not expose `set_dtx`. Opus defaults
        // to DTX=off at the codec level, and the software VAD below (RMS <
        // 0.005 skip) already produces DTX-like behavior at the frame layer.
        // When/if we upgrade to an opus crate that exposes OPUS_SET_DTX, the
        // `a.dtx` field is already plumbed through and ready to use.
        // TODO(bot-aq): plumb set_dtx once the opus bindings expose it.
        info!(
            "Audio producer started for {} ({}Hz, {}ch, {}ms packets, AQ tier={}, {}kbps, fec={}, dtx={} [software-only])",
            user_id,
            sample_rate,
            channels,
            20,
            aq.audio_tier_index(),
            a.bitrate_kbps,
            a.fec,
            a.dtx,
        );

        let user_id_bytes = user_id.clone().into_bytes();
        let mut grid = PacketGrid::new(clock.elapsed_us(), wrap);
        let first_tick = grid.next_tick;
        let mut packet_samples = vec![0.0f32; SAMPLES_PER_PACKET];
        let mut stalls: u64 = 0;

        loop {
            if quit.load(Ordering::Relaxed) {
                info!("Audio producer stopping for {}", user_id);
                break;
            }

            // Cheap lock-free poll: only re-snapshot on a tier change.
            let current_epoch = aq.tier_epoch();
            if current_epoch != last_epoch {
                let new_a = aq.snapshot_audio();
                last_epoch = current_epoch;

                if new_a.bitrate_kbps != a.bitrate_kbps {
                    if let Err(e) = opus_encoder
                        .set_bitrate(OpusBitrate::Bits((new_a.bitrate_kbps * 1000) as i32))
                    {
                        warn!(
                            "[{}] audio AQ: failed to update bitrate to {}kbps: {}",
                            user_id, new_a.bitrate_kbps, e
                        );
                    }
                }
                if new_a.fec != a.fec {
                    if let Err(e) = opus_encoder.set_inband_fec(new_a.fec) {
                        warn!(
                            "[{}] audio AQ: failed to update FEC to {}: {}",
                            user_id, new_a.fec, e
                        );
                    }
                }
                // DTX flag tracked but not applied (see note above).
                info!(
                    "[{}] audio AQ: tier change bitrate {}->{}kbps, fec {}->{} (dtx {}->{})",
                    user_id, a.bitrate_kbps, new_a.bitrate_kbps, a.fec, new_a.fec, a.dtx, new_a.dtx,
                );
                a = new_a;
            }

            let owed_from = grid.next_tick;
            let due = grid.due(clock.elapsed_us());
            let owed = due.end.saturating_sub(owed_from);
            if owed > STALL_WARN_PACKETS {
                stalls += 1;
                if stalls % 100 == 1 {
                    warn!(
                        "[{}] audio: woke {} ticks late; sending {} ticks back to back, dropping {} (stalls: {})",
                        user_id,
                        owed - 1,
                        due.end - due.start,
                        due.start - owed_from,
                        stalls
                    );
                }
            }
            for tick in due {
                fill_window(
                    &audio_data[..wrap],
                    grid.window_start(tick),
                    &mut packet_samples,
                );
                let rms = (packet_samples.iter().map(|s| s * s).sum::<f32>()
                    / packet_samples.len() as f32)
                    .sqrt();
                let speaking_now = next_speaking_state(is_speaking.load(Ordering::Relaxed), rms);
                is_speaking.store(speaking_now, Ordering::Relaxed);
                if skip_silent_frame(rms, continuous) {
                    continue;
                }

                let mut encoded = vec![0u8; 4000];
                let bytes_written = match opus_encoder.encode_float(&packet_samples, &mut encoded) {
                    Ok(n) => n,
                    Err(e) => {
                        error!("Opus encoding failed for {}: {}", user_id, e);
                        continue;
                    }
                };
                encoded.truncate(bytes_written);
                let media_packet = MediaPacket {
                    user_id: user_id_bytes.clone(),
                    media_type: MediaType::AUDIO.into(),
                    data: encoded,
                    frame_type: "key".to_string(),
                    timestamp: get_timestamp_ms(),
                    audio_metadata: Some(AudioMetadata {
                        sequence: tick - first_tick,
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                };
                let packet_wrapper = PacketWrapper {
                    data: media_packet.write_to_bytes()?,
                    user_id: user_id_bytes.clone(),
                    packet_type: PacketType::MEDIA.into(),
                    // Cleartext discriminator so the relay always forwards
                    // AUDIO regardless of viewport (HCL issue #988). Matches
                    // the real client (microphone_encoder.rs).
                    media_kind: MediaKind::AUDIO.into(),
                    ..Default::default()
                };
                let packet_data = packet_wrapper.write_to_bytes()?;
                let frame = OutboundFrame::new(MediaTypeLabel::Audio, packet_data);
                if let Err(_e) = packet_sender.try_send(frame) {
                    static AUDIO_DROP_COUNT: AtomicU64 = AtomicU64::new(0);
                    let count = AUDIO_DROP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                    transport_drops_counter.fetch_add(1, Ordering::Relaxed);
                    if count % 100 == 1 {
                        warn!(
                            "Dropped audio packets due to full send channel (total: {})",
                            count,
                        );
                    }
                }
            }

            clock.sleep_until_us(grid.next_deadline_us());
        }

        Ok(())
    }

    pub fn stop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AudioProducer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn get_timestamp_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64
}

#[cfg(test)]
mod tests {
    use super::{
        fill_window, next_speaking_state, skip_silent_frame, stitch_participant_audio, MediaClock,
        PacketGrid, WallClock, MAX_BACKLOG_PACKETS, PACKET_INTERVAL_US, RMS_SPEAKING_ENTER,
        RMS_SPEAKING_EXIT, SAMPLES_PER_PACKET,
    };
    use protobuf::Message;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use videocall_types::protos::media_packet::MediaPacket;
    use videocall_types::protos::packet_wrapper::PacketWrapper;

    const WRAP: usize = 48 * 3010;

    fn ramp() -> Vec<f32> {
        (0..WRAP).map(|i| i as f32).collect()
    }

    #[test]
    fn every_packet_window_starts_where_the_previous_one_ended_under_late_wakeups() {
        let source = ramp();
        let late_us = [
            0, 7_000, 0, 19_900, 45_000, 0, 3_000, 199_000, 1, 60_000, 0, 9_700,
        ];
        let mut grid = PacketGrid::new(1_234_567, WRAP);
        let mut sent: Vec<(u64, Vec<f32>)> = Vec::new();
        for wake in 0..600 {
            let elapsed = grid.next_deadline_us() + late_us[wake % late_us.len()];
            for tick in grid.due(elapsed) {
                let mut window = vec![0.0f32; SAMPLES_PER_PACKET];
                fill_window(&source, grid.window_start(tick), &mut window);
                sent.push((tick, window));
            }
        }
        assert!(
            sent.len() > 3 * WRAP / SAMPLES_PER_PACKET,
            "crosses the loop wrap"
        );
        for pair in sent.windows(2) {
            let ((t0, w0), (t1, w1)) = (&pair[0], &pair[1]);
            assert_eq!(*t1, t0 + 1, "sequence numbers are contiguous");
            let expected_next = (w0[SAMPLES_PER_PACKET - 1] as usize + 1) % WRAP;
            assert_eq!(
                w1[0] as usize, expected_next,
                "packet {} starts where packet {} ended",
                t1, t0
            );
            for (a, b) in w1.iter().zip(&w1[1..]) {
                assert_eq!(
                    (*a as usize + 1) % WRAP,
                    *b as usize,
                    "window is contiguous"
                );
            }
        }
    }

    const START_US: u64 = 1_234_567;
    const TONE_HZ: f64 = 250.0;

    struct ScriptedClock {
        now_us: u64,
        late_us: Vec<u64>,
        wakes: usize,
        quit: Arc<AtomicBool>,
    }

    impl MediaClock for ScriptedClock {
        fn elapsed_us(&self) -> u64 {
            self.now_us
        }

        fn sleep_until_us(&mut self, target_us: u64) {
            self.now_us =
                self.now_us.max(target_us) + self.late_us[self.wakes % self.late_us.len()];
            self.wakes += 1;
            if self.wakes == 300 {
                self.quit.store(true, Ordering::Relaxed);
            }
        }
    }

    fn tone(len: usize) -> Vec<f32> {
        let w = 2.0 * std::f64::consts::PI * TONE_HZ / 48_000.0;
        (0..len)
            .map(|n| (0.3 * (w * n as f64).sin()) as f32)
            .collect()
    }

    /// Runs the real `audio_loop` for 300 wake-ups on a scripted clock and returns each
    /// sent packet's sequence number and Opus payload.
    fn run_on_scripted_clock(
        source: Vec<f32>,
        loop_duration: Duration,
        late_us: Vec<u64>,
        continuous: bool,
    ) -> Vec<(u64, Vec<u8>)> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
        let quit = Arc::new(AtomicBool::new(false));
        let clock = ScriptedClock {
            now_us: START_US,
            late_us,
            wakes: 0,
            quit: quit.clone(),
        };
        let producer_quit = quit.clone();
        let producer = std::thread::spawn(move || {
            super::AudioProducer::audio_loop(
                "tone".into(),
                source,
                tx.into(),
                producer_quit,
                clock,
                loop_duration,
                Arc::new(AtomicBool::new(false)),
                crate::aq_controller::BotAq::with_default_clock(),
                Arc::new(AtomicU64::new(0)),
                continuous,
            )
        });
        let watchdog = Instant::now() + Duration::from_secs(20);
        while !producer.is_finished() && Instant::now() < watchdog {
            std::thread::sleep(Duration::from_millis(10));
        }
        let finished = producer.is_finished();
        quit.store(true, Ordering::Relaxed);
        producer.join().unwrap().unwrap();
        assert!(finished, "the producer runs on the injected clock");

        let mut packets = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            let wrapper = PacketWrapper::parse_from_bytes(&frame.bytes).unwrap();
            let media = MediaPacket::parse_from_bytes(&wrapper.data).unwrap();
            packets.push((media.audio_metadata.sequence, media.data));
        }
        packets
    }

    /// Decodes each packet and returns the tone phase fitted over it.
    fn decoded_tone_phases(packets: &[(u64, Vec<u8>)]) -> Vec<f64> {
        let w = 2.0 * std::f64::consts::PI * TONE_HZ / 48_000.0;
        let mut decoder = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap();
        let mut pcm = vec![0.0f32; SAMPLES_PER_PACKET];
        packets
            .iter()
            .map(|(_, data)| {
                assert_eq!(decoder.decode_float(data, &mut pcm, false).unwrap(), 960);
                let (i, q) = pcm.iter().enumerate().fold((0.0, 0.0), |(i, q), (n, x)| {
                    let t = w * n as f64;
                    (i + *x as f64 * t.sin(), q + *x as f64 * t.cos())
                });
                q.atan2(i)
            })
            .collect()
    }

    fn degrees_apart(a: f64, b: f64) -> f64 {
        use std::f64::consts::PI;
        ((a - b + 3.0 * PI).rem_euclid(2.0 * PI) - PI)
            .abs()
            .to_degrees()
    }

    #[test]
    fn the_producer_sends_contiguous_source_through_late_wakeups() {
        let packets = run_on_scripted_clock(
            tone(4 * 48_000),
            Duration::from_secs(4),
            vec![0, 7_000, 0, 3_100, 45_000, 0, 1_500, 13_000, 0, 9_700, 0, 0],
            false,
        );
        let sequences: Vec<u64> = packets.iter().map(|(seq, _)| *seq).collect();
        assert!(packets.len() > 300, "late wake-ups send the owed packets");
        assert_eq!(sequences, (0..packets.len() as u64).collect::<Vec<_>>());

        let phases = decoded_tone_phases(&packets);
        for k in 3..phases.len() {
            let step = degrees_apart(phases[k], phases[k - 1]);
            assert!(
                step < 20.0,
                "packet {} starts {:.0} degrees of a 192-sample tone away from where packet {} ended",
                sequences[k],
                step,
                sequences[k - 1]
            );
        }
    }

    #[test]
    fn a_speaker_spends_one_sequence_number_per_tick_through_silence_and_stalls() {
        const LOOP_PACKETS: u64 = 100;
        const TONE_PACKETS: u64 = 50;
        let mut source = tone(TONE_PACKETS as usize * SAMPLES_PER_PACKET);
        source.resize(LOOP_PACKETS as usize * SAMPLES_PER_PACKET, 0.0);
        let mut late_us = vec![0; 300];
        late_us[50] = 500_000;
        let packets = run_on_scripted_clock(source, Duration::from_secs(2), late_us, false);

        assert!(
            packets.len() as u64 > LOOP_PACKETS,
            "crosses several silent stretches"
        );
        let sequences: Vec<u64> = packets.iter().map(|(seq, _)| *seq).collect();
        let first_tick = START_US / PACKET_INTERVAL_US;
        let last = *sequences.last().unwrap();
        let audible: Vec<u64> = (0..=last)
            .filter(|seq| (first_tick + seq) % LOOP_PACKETS < TONE_PACKETS)
            .collect();
        assert!(
            (51..=76).all(|seq| audible.contains(&seq)),
            "the 500 ms stall lands in a tone stretch"
        );
        assert_eq!(
            sequences, audible,
            "every tick's packet carries its own sequence number; skipped silence leaves a gap, a stall does not"
        );
    }

    #[test]
    fn the_source_wraps_at_the_video_loop_not_at_the_end_of_the_audio() {
        // Production audio is up to 47 samples longer than the loop, which is whole milliseconds.
        // 4004 ms is whole tone periods but not whole packets, so a window straddles the wrap.
        let packets = run_on_scripted_clock(
            tone(4004 * 48 + 47),
            Duration::from_millis(4004),
            vec![0],
            true,
        );
        let first_tick = START_US / PACKET_INTERVAL_US;
        let loop_tick = 4004 * 48 / SAMPLES_PER_PACKET as u64;
        assert!(
            packets.first().unwrap().0 + first_tick < loop_tick
                && packets.last().unwrap().0 + first_tick > loop_tick + 50,
            "crosses the loop wrap"
        );

        let phases = decoded_tone_phases(&packets);
        for (k, phase) in phases.iter().enumerate().skip(4) {
            assert!(
                degrees_apart(*phase, phases[3]) < 20.0,
                "packet {} is {:.0} degrees off the loop's tone phase",
                packets[k].0,
                degrees_apart(*phase, phases[3])
            );
        }
    }

    #[test]
    fn the_wall_clock_sleeps_to_the_deadline_so_slow_steps_do_not_add_up() {
        let start = Instant::now();
        let mut clock = WallClock(start);
        for tick in 1..=10 {
            std::thread::sleep(Duration::from_millis(15));
            clock.sleep_until_us(tick * PACKET_INTERVAL_US);
            assert!(clock.elapsed_us() >= tick * PACKET_INTERVAL_US);
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(275),
            "10 ticks took {:?}; 15 ms of work per 20 ms tick should still finish near 200 ms",
            elapsed
        );
    }

    #[test]
    fn a_stall_within_the_backlog_bound_sends_every_owed_packet() {
        let mut grid = PacketGrid::new(0, WRAP);
        assert_eq!(grid.due(5), 0..1);
        let stalled = grid.due(PACKET_INTERVAL_US * 26 + 5);
        assert_eq!(
            stalled,
            1..27,
            "a 500 ms stall owes 26 packets and sends all of them"
        );
        assert_eq!(grid.due(grid.next_deadline_us()), 27..28);
    }

    #[test]
    fn a_stall_past_the_backlog_sends_only_the_newest_packets() {
        let mut grid = PacketGrid::new(0, WRAP);
        assert_eq!(grid.due(5), 0..1);
        let frozen = grid.due(PACKET_INTERVAL_US * (MAX_BACKLOG_PACKETS + 10) + 5);
        assert_eq!(
            frozen,
            11..MAX_BACKLOG_PACKETS + 11,
            "60 owed: the oldest 10 are dropped, the newest 50 sent"
        );
        assert_eq!(
            grid.due(grid.next_deadline_us()),
            MAX_BACKLOG_PACKETS + 11..MAX_BACKLOG_PACKETS + 12
        );
    }

    #[test]
    fn producers_started_at_different_times_send_the_same_window_on_the_same_tick() {
        let early = PacketGrid::new(0, WRAP);
        let mut late = PacketGrid::new(5_013_000, WRAP);
        let tick = late.due(5_013_000).start;
        assert_eq!(tick, 250);
        assert_eq!(late.window_start(tick), early.window_start(tick));
    }

    #[test]
    fn continuous_talkers_never_skip_silence() {
        assert!(skip_silent_frame(0.0, false), "speech mode skips silence");
        assert!(!skip_silent_frame(0.1, false));
        assert!(
            !skip_silent_frame(0.0, true),
            "continuous mode sends every frame"
        );
    }

    #[test]
    fn stitching_gives_timelines_only_to_broadcasters() {
        let lines = vec![vec![1.0f32; 3], vec![2.0f32; 2]];
        let (audio, total) = stitch_participant_audio(&["alice", "bob"], &lines, &["alice"], 1);
        assert_eq!(total, 3 + 1 + 2 + 1);
        assert_eq!(audio.len(), 1, "receive-only participants hold no timeline");
        assert_eq!(audio["alice"], vec![1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn a_presenter_on_a_host_without_lines_still_sends_audio() {
        use crate::transport::MediaTypeLabel;
        use std::sync::atomic::{AtomicBool, AtomicU64};
        use std::sync::Arc;
        use std::time::{Duration, Instant};
        let (audio, total) = stitch_participant_audio(&[], &[], &["presenter"], 960);
        assert!(total > 0);
        let loop_duration = crate::config::media_loop_duration(total);
        assert_eq!(loop_duration, crate::config::SILENT_LOOP);
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let producer = super::AudioProducer::new(
            "presenter".into(),
            audio["presenter"].clone(),
            tx.into(),
            Instant::now(),
            loop_duration,
            Arc::new(AtomicBool::new(false)),
            crate::aq_controller::BotAq::with_default_clock(),
            Arc::new(AtomicU64::new(0)),
            true,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut sequences = Vec::new();
        while sequences.len() < 5 && Instant::now() < deadline {
            match rx.try_recv() {
                Ok(frame) if frame.kind == MediaTypeLabel::Audio => {
                    use protobuf::Message;
                    let wrapper =
                        videocall_types::protos::packet_wrapper::PacketWrapper::parse_from_bytes(
                            &frame.bytes,
                        )
                        .unwrap();
                    let media =
                        videocall_types::protos::media_packet::MediaPacket::parse_from_bytes(
                            &wrapper.data,
                        )
                        .unwrap();
                    sequences.push(media.audio_metadata.sequence);
                }
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        drop(producer);
        assert_eq!(
            sequences,
            vec![0, 1, 2, 3, 4],
            "a continuous presenter sends audio with contiguous sequences"
        );
    }

    #[test]
    fn a_broadcaster_without_lines_gets_full_length_silence() {
        let lines = vec![vec![1.0f32; 4]];
        let (audio, total) = stitch_participant_audio(&["alice"], &lines, &["alice", "bot-003"], 2);
        assert_eq!(audio["bot-003"], vec![0.0; total]);
        assert_eq!(audio["alice"].len(), total);
    }

    #[test]
    fn entering_speaking_requires_upper_threshold() {
        assert!(!next_speaking_state(false, RMS_SPEAKING_EXIT + 0.0005));
        assert!(next_speaking_state(false, RMS_SPEAKING_ENTER + 0.0005));
    }

    #[test]
    fn staying_speaking_uses_lower_threshold() {
        assert!(next_speaking_state(true, RMS_SPEAKING_ENTER - 0.0005));
        assert!(!next_speaking_state(true, RMS_SPEAKING_EXIT - 0.0005));
    }
}
