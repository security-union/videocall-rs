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

//! Runs the bot binary against a local WebSocket relay stand-in and counts the
//! media each participant puts on the wire. `BOT_WIRE_SECS=<n>` sets the run
//! length and prints the per-second counts.

use futures_util::{SinkExt, StreamExt};
use protobuf::Message;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use videocall_types::protos::layer_hint_packet::layer_hint_packet::{Entry, MediaKind as HintKind};
use videocall_types::protos::layer_hint_packet::LayerHintPacket;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Audio,
    Video,
}

#[derive(Clone, Debug)]
struct Wire {
    at: Instant,
    user: String,
    kind: Kind,
    layer: u32,
    key: bool,
    seq: u64,
    bytes: usize,
    payload_hash: u64,
}

type Frames = Arc<Mutex<Vec<Wire>>>;

/// Relay-side control packets sent to one publisher, timed from its first video packet.
#[derive(Clone)]
struct Control {
    target: &'static str,
    request_after: Duration,
    hint_after: Duration,
    sent: Arc<Mutex<Option<(Instant, Instant)>>>,
}

const STAND_IN_SESSION: u64 = 4242;

fn keyframe_request_for(target: &str) -> Vec<u8> {
    let media = MediaPacket {
        media_type: MediaType::KEYFRAME_REQUEST.into(),
        user_id: target.as_bytes().to_vec(),
        data: b"VIDEO".to_vec(),
        ..Default::default()
    };
    PacketWrapper {
        packet_type: PacketType::MEDIA.into(),
        user_id: b"probe".to_vec(),
        data: media.write_to_bytes().unwrap(),
        ..Default::default()
    }
    .write_to_bytes()
    .unwrap()
}

fn relay_packet(packet_type: PacketType, data: Vec<u8>) -> Vec<u8> {
    PacketWrapper {
        packet_type: packet_type.into(),
        session_id: STAND_IN_SESSION,
        data,
        ..Default::default()
    }
    .write_to_bytes()
    .unwrap()
}

fn base_layer_only_hint() -> Vec<u8> {
    let mut entry = Entry::new();
    entry.media_kind = HintKind::VIDEO.into();
    entry.max_requested_layer = 0;
    let hint = LayerHintPacket {
        entries: vec![entry],
        ..Default::default()
    };
    relay_packet(PacketType::LAYER_HINT, hint.write_to_bytes().unwrap())
}

async fn relay_stand_in() -> (u16, Frames) {
    relay_stand_in_with(None).await
}

async fn relay_stand_in_with(control: Option<Control>) -> (u16, Frames) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let frames: Frames = Arc::default();
    let sink = Arc::clone(&frames);
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let sink = Arc::clone(&sink);
            let control = control.clone();
            tokio::spawn(async move {
                let Ok(ws) = tokio_tungstenite::accept_async(tcp).await else {
                    return;
                };
                let (tx, mut rx) = ws.split();
                let mut tx = Some(tx);
                let mut scheduled = None;
                while let Some(Ok(msg)) = rx.next().await {
                    let Ok(wrapper) = PacketWrapper::parse_from_bytes(&msg.into_data()) else {
                        continue;
                    };
                    if wrapper.packet_type.enum_value() != Ok(PacketType::MEDIA) {
                        continue;
                    }
                    let Ok(media) = MediaPacket::parse_from_bytes(&wrapper.data) else {
                        continue;
                    };
                    let kind = match media.media_type.enum_value() {
                        Ok(MediaType::AUDIO) => Kind::Audio,
                        Ok(MediaType::VIDEO) => Kind::Video,
                        _ => continue,
                    };
                    let user = String::from_utf8_lossy(&wrapper.user_id).to_string();
                    if let Some(c) = &control {
                        if kind == Kind::Video && user == c.target && scheduled.is_none() {
                            scheduled = Some(Instant::now());
                        }
                        if let (Some(first), Some(_)) = (scheduled, tx.as_ref()) {
                            if first.elapsed() >= c.request_after {
                                let mut tx = tx.take().unwrap();
                                let c = c.clone();
                                tokio::spawn(async move {
                                    let assigned =
                                        relay_packet(PacketType::SESSION_ASSIGNED, Vec::new());
                                    tx.send(WsMessage::Binary(assigned)).await.unwrap();
                                    let request = keyframe_request_for(c.target);
                                    tx.send(WsMessage::Binary(request)).await.unwrap();
                                    let requested = Instant::now();
                                    tokio::time::sleep(c.hint_after - c.request_after).await;
                                    tx.send(WsMessage::Binary(base_layer_only_hint()))
                                        .await
                                        .unwrap();
                                    *c.sent.lock().unwrap() = Some((requested, Instant::now()));
                                    let _keep_open = tx;
                                    std::future::pending::<()>().await;
                                });
                            }
                        }
                    }
                    let mut hasher = DefaultHasher::new();
                    media.data.hash(&mut hasher);
                    sink.lock().unwrap().push(Wire {
                        at: Instant::now(),
                        user,
                        kind,
                        layer: wrapper.simulcast_layer_id,
                        key: media.frame_type == "key",
                        seq: media.video_metadata.as_ref().map_or(0, |m| m.sequence),
                        bytes: media.data.len(),
                        payload_hash: hasher.finish(),
                    });
                }
            });
        }
    });
    (port, frames)
}

fn write_assets(dir: &Path, port: u16) -> std::path::PathBuf {
    write_assets_with(dir, port, "ekg")
}

fn write_assets_with(dir: &Path, port: u16, video_mode: &str) -> std::path::PathBuf {
    let conv = dir.join("conversation");
    std::fs::create_dir_all(conv.join("lines")).unwrap();
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = hound::WavWriter::create(conv.join("lines/alice.wav"), spec).unwrap();
    for i in 0..96_000 {
        let t = i as f32 / 48_000.0;
        wav.write_sample(((t * 440.0 * std::f32::consts::TAU).sin() * 8_000.0) as i16)
            .unwrap();
    }
    wav.finalize().unwrap();
    std::fs::write(
        conv.join("manifest.yaml"),
        "participants:\n  - name: alice\n    voice: v\npause_ms: 500\nlines:\n  - speaker: alice\n    audio_file: lines/alice.wav\n    duration_ms: 2000\n",
    )
    .unwrap();
    let config = dir.join("config.yaml");
    std::fs::write(
        &config,
        format!(
            "meeting_id: wire-test\nws_url: ws://127.0.0.1:{port}\njwt_secret: test-secret-for-integration-tests\nconversation_dir: {}\nvideo_mode: {video_mode}\nwarmup_secs: 0\nramp_up_delay_ms: 100\n",
            conv.display()
        ),
    )
    .unwrap();
    config
}

fn per_second(frames: &[Wire], user: &str, kind: Kind, layer: Option<u32>) -> Vec<usize> {
    let Some(start) = frames.iter().find(|f| f.user == user).map(|f| f.at) else {
        return Vec::new();
    };
    let mut buckets = Vec::new();
    for f in frames {
        if f.user == user && f.kind == kind && layer.is_none_or(|want| want == f.layer) {
            let s = f.at.duration_since(start).as_secs() as usize;
            if buckets.len() <= s {
                buckets.resize(s + 1, 0);
            }
            buckets[s] += 1;
        }
    }
    buckets
}

#[tokio::test(flavor = "multi_thread")]
async fn a_presenter_and_a_camera_keep_sending_media_for_the_whole_run() {
    let secs: u64 = std::env::var("BOT_WIRE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let (port, frames) = relay_stand_in().await;
    let dir = std::env::temp_dir().join(format!("bot-wire-{}-{port}", std::process::id()));
    let config = write_assets(&dir, port);
    let out = dir.join("participants.json");
    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_bot"))
        .args(["--config", config.to_str().unwrap()])
        .args(["--users", "2", "--broadcasters", "1", "--talkers", "1"])
        .args(["--cameras", "1", "--no-impair", "--participants-out"])
        .arg(&out)
        .arg("--duration")
        .arg(format!("{secs}s"))
        .env("RUST_LOG", "warn")
        .kill_on_drop(true)
        .status();
    let status = tokio::time::timeout(Duration::from_secs(secs + 60), status)
        .await
        .expect("the bot must exit after --duration")
        .unwrap();
    let list: Option<serde_json::Value> = std::fs::read_to_string(&out)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(status.success(), "bot exited with {}", status);
    let list = list.expect("the participant list is written");
    assert!(list["ended_at"].is_number(), "{}", list);
    let people = list["participants"].as_array().unwrap();
    assert_eq!(people.len(), 2);
    assert!(
        people.iter().all(|p| p["video_source"] == "ekg-live"),
        "{}",
        list
    );
    assert!(people.iter().all(|p| p["leave_ts"].is_number()), "{}", list);

    let frames = frames.lock().unwrap().clone();
    let audio = per_second(&frames, "alice", Kind::Audio, None);
    let camera_video = per_second(&frames, "bot-002", Kind::Video, None);
    let camera_audio = per_second(&frames, "bot-002", Kind::Audio, None);
    println!("presenter audio packets/s: {audio:?}");
    for layer in 0..3 {
        println!(
            "presenter video layer {layer} frames/s: {:?}",
            per_second(&frames, "alice", Kind::Video, Some(layer))
        );
    }
    println!("camera video frames/s: {camera_video:?}");

    let held = 1..(secs as usize - 2);
    assert!(held.len() >= 2, "run too short to measure the hold");
    for s in held {
        let a = audio.get(s).copied().unwrap_or(0);
        let v = camera_video.get(s).copied().unwrap_or(0);
        assert!(
            a >= 40,
            "second {}: presenter sent {} audio packets, want ~50",
            s,
            a
        );
        assert!(v >= 5, "second {}: camera sent {} video frames", s, v);
    }
    assert!(
        camera_audio.iter().all(|&n| n == 0),
        "a camera's mic is muted"
    );
}

/// A relay that reads each lobby request and never answers it.
async fn stalled_lobby() -> (u16, Arc<Mutex<Vec<String>>>) {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = Arc::clone(&requests);
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = tcp.read(&mut buf).await.unwrap_or(0);
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).to_string());
                std::future::pending::<()>().await;
                drop(tcp);
            });
        }
    });
    (port, requests)
}

struct RemoveOnDrop(std::path::PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn epoch_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_binary_ends_its_run_and_its_room_tokens_by_the_deadline() {
    let (port, requests) = stalled_lobby().await;
    let dir = std::env::temp_dir().join(format!("bot-deadline-{}-{port}", std::process::id()));
    let _cleanup = RemoveOnDrop(dir.clone());
    let config = write_assets(&dir, port);
    let run = |deadline: u64| {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_bot"))
            .args(["--config", config.to_str().unwrap(), "--users", "1"])
            .args(["--no-impair", "--duration", "10m", "--deadline"])
            .arg(deadline.to_string())
            .env("RUST_LOG", "warn")
            .kill_on_drop(true)
            .output()
    };

    let deadline = epoch_secs() as u64 + 4;
    let out = tokio::time::timeout(Duration::from_secs(20), run(deadline))
        .await
        .expect("the bot must stop at its deadline, not after --duration or the stop grace")
        .unwrap();
    let ended = epoch_secs();
    assert!(
        ended <= deadline as f64 + 2.0,
        "stopped {:.1} s after the deadline",
        ended - deadline as f64
    );
    let requests = requests.lock().unwrap().clone();
    assert!(!requests.is_empty(), "no lobby request: {:?}", out);
    let mut validation = jsonwebtoken::Validation::default();
    validation.validate_exp = false;
    for request in requests {
        let token = request
            .split(['?', '&', ' '])
            .find_map(|part| part.strip_prefix("token="))
            .expect("the lobby request carries a token");
        let claims = jsonwebtoken::decode::<serde_json::Value>(
            token,
            &jsonwebtoken::DecodingKey::from_secret(b"test-secret-for-integration-tests"),
            &validation,
        )
        .unwrap()
        .claims;
        assert!(claims["exp"].as_u64().unwrap() <= deadline, "{}", claims);
    }

    let past = tokio::time::timeout(Duration::from_secs(20), run(epoch_secs() as u64 - 1))
        .await
        .unwrap()
        .unwrap();
    assert!(!past.status.success());
    assert!(String::from_utf8_lossy(&past.stderr).contains("has passed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sigkill_after_media_start_leaves_every_join_on_disk() {
    let (port, _frames) = relay_stand_in().await;
    let dir = std::env::temp_dir().join(format!("bot-kill-{}-{port}", std::process::id()));
    let _cleanup = RemoveOnDrop(dir.clone());
    let config = write_assets(&dir, port);
    let out = dir.join("participants.json");
    let mut bot = tokio::process::Command::new(env!("CARGO_BIN_EXE_bot"))
        .args(["--config", config.to_str().unwrap()])
        .args(["--users", "3", "--no-impair", "--participants-out"])
        .arg(&out)
        .env("RUST_LOG", "warn")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let read = || -> Option<serde_json::Value> {
        serde_json::from_str(&std::fs::read_to_string(&out).ok()?).ok()
    };
    let ready = |v: &serde_json::Value| {
        v["media_started_at"].is_number()
            && v["participants"]
                .as_array()
                .is_some_and(|ps| ps.iter().all(|p| p["join_ts"].is_number()))
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while !read().is_some_and(|v| ready(&v)) {
        assert!(
            Instant::now() < deadline,
            "joins never reached the file: {:?}",
            read()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bot.start_kill().unwrap();
    let status = bot.wait().await.unwrap();
    let v = read().expect("the file survives the kill");
    assert!(!status.success(), "SIGKILL, not a clean exit");
    assert!(ready(&v));
    assert_eq!(v["participants"].as_array().unwrap().len(), 3);
    assert_eq!(v["planned"], 3);
    assert!(v["ended_at"].is_null());
}

fn video(frames: &[Wire], user: &str, layer: u32) -> Vec<Wire> {
    frames
        .iter()
        .filter(|f| f.user == user && f.kind == Kind::Video && f.layer == layer)
        .cloned()
        .collect()
}

struct ReplayRun {
    frames: Vec<Wire>,
    people: Vec<serde_json::Value>,
    requested: Instant,
    hinted: Instant,
}

const REPLAY_SEED: &str = "11";

/// Circular mean of `times` modulo `period_s`, in seconds.
fn circular_mean(times: impl Iterator<Item = f64>, period_s: f64) -> f64 {
    let (mut c, mut s) = (0.0, 0.0);
    for t in times {
        let a = std::f64::consts::TAU * (t % period_s) / period_s;
        c += a.cos();
        s += a.sin();
    }
    s.atan2(c).rem_euclid(std::f64::consts::TAU) / std::f64::consts::TAU * period_s
}

fn circular_spread(values: &[f64], period_s: f64) -> f64 {
    values
        .iter()
        .flat_map(|a| {
            values
                .iter()
                .map(move |b| (a - b).abs().min(period_s - (a - b).abs()))
        })
        .fold(0.0, f64::max)
}

/// One replay bot process (talker alice + cameras bot-002..004) against its own
/// stand-in, which sends bot-002 a KEYFRAME_REQUEST, then a base-layer-only
/// LAYER_HINT, and is stopped with SIGTERM once the hint has acted. `answer:
/// false` is set through the environment (`BOT_VIDEO_MODE`,
/// `BOT_REPLAY_ANSWER_KEYFRAME_REQUESTS`) to cover that path.
async fn run_replay_bot(answer: bool) -> ReplayRun {
    let control = Control {
        target: "bot-002",
        request_after: Duration::from_secs(23),
        hint_after: Duration::from_secs(25),
        sent: Arc::default(),
    };
    let sent = Arc::clone(&control.sent);
    let (port, frames) = relay_stand_in_with(Some(control)).await;
    let dir = std::env::temp_dir().join(format!("bot-replay-{}-{port}", std::process::id()));
    let config = write_assets_with(&dir, port, if answer { "replay" } else { "ekg" });
    let out = dir.join("participants.json");
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_bot"));
    if answer {
        command.args(["--replay-answer-keyframe-requests", "true"]);
    } else {
        command
            .env("BOT_VIDEO_MODE", "replay")
            .env("BOT_REPLAY_ANSWER_KEYFRAME_REQUESTS", "false");
    }
    let mut bot = command
        .args(["--config", config.to_str().unwrap()])
        .args([
            "--users",
            "4",
            "--broadcasters",
            "1",
            "--talkers",
            "1",
            "--cameras",
            "3",
        ])
        .args([
            "--replay-loop-secs",
            "5",
            "--no-impair",
            "--participants-out",
        ])
        .arg(&out)
        .args(["--replay-seed", REPLAY_SEED, "--duration", "600s"])
        .env("RUST_LOG", "warn")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(480);
    let (requested, hinted) = loop {
        let done = *sent.lock().unwrap();
        if let Some((r, h)) = done {
            if h.elapsed() >= Duration::from_secs(5) {
                break (r, h);
            }
        }
        assert!(
            Instant::now() < deadline,
            "the stand-in never sent its control packets"
        );
        assert!(bot.try_wait().unwrap().is_none(), "the bot exited early");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let pid = bot.id().unwrap().to_string();
    std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(60), bot.wait())
        .await
        .expect("the bot must stop on SIGTERM")
        .unwrap();
    let list: Option<serde_json::Value> = std::fs::read_to_string(&out)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(status.success(), "bot exited with {}", status);
    let list = list.expect("the participant list is written");
    let people = list["participants"].as_array().unwrap().clone();
    let frames = frames.lock().unwrap().clone();
    ReplayRun {
        frames,
        people,
        requested,
        hinted,
    }
}

fn key_times(frames: &[Wire], user: &str, layer: u32, from: Instant) -> Vec<f64> {
    video(frames, user, layer)
        .iter()
        .filter(|f| f.key)
        .map(|f| f.at.saturating_duration_since(from).as_secs_f64())
        .collect()
}

fn assert_periodic(keys: &[f64], what: &str) {
    assert!(keys.len() >= 3, "{}: {:?}", what, keys);
    for w in keys[1..].windows(2) {
        assert!(
            (w[1] - w[0] - 5.0).abs() < 0.3,
            "{}: keyframes at {:?}",
            what,
            keys
        );
    }
}

fn check_replay_run(run: &ReplayRun, answer: bool) -> Vec<f64> {
    let frames = &run.frames;
    let people = &run.people;
    assert_eq!(people.len(), 4);
    for p in people {
        assert_eq!(p["video_source"], "ekg-replay", "{}", p);
        assert_eq!(p["replay_answer_keyframe_requests"], answer, "{}", p);
        let forced: Vec<u64> = p["replay_forced_keyframes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap())
            .collect();
        let tier: Vec<u64> = p["replay_tier_change_keyframes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap())
            .collect();
        if p["user_id"] == "bot-002" && answer {
            assert!(forced.iter().all(|&n| n >= 1), "{}", p);
            assert!(
                tier[0] >= 1,
                "the LAYER_HINT shed keys video layer 0: {}",
                p
            );
            assert_eq!(
                &tier[1..],
                &[0, 0],
                "only video layer 0 is left after the shed: {}",
                p
            );
        } else {
            assert_eq!(forced, vec![0, 0, 0], "{}", p);
            assert_eq!(tier, vec![0, 0, 0], "{}", p);
        }
    }

    let publishers = ["alice", "bot-002", "bot-003", "bot-004"];
    let first = |user: &str| {
        frames
            .iter()
            .find(|f| f.user == user && f.kind == Kind::Video)
            .map(|f| f.at)
            .unwrap_or_else(|| panic!("{} sent no video", user))
    };
    let media_start = publishers.iter().map(|u| first(u)).min().unwrap();
    let hold = 20u64;
    let mut phases = Vec::new();
    for user in publishers {
        let t0 = first(user) + Duration::from_secs(1);
        let t1 = t0 + Duration::from_secs(hold);
        for (layer, fps) in [(0u32, 7u64), (1, 15), (2, 30)] {
            let l = video(frames, user, layer);
            let seqs: Vec<u64> = l.iter().map(|f| f.seq).collect();
            assert_eq!(
                seqs,
                (0..seqs.len() as u64).collect::<Vec<_>>(),
                "{} layer {}",
                user,
                layer
            );
            assert!(l[0].key, "{} layer {} starts on a keyframe", user, layer);
            let n = l.iter().filter(|f| f.at >= t0 && f.at < t1).count() as u64;
            let want = fps * hold;
            assert!(
                n * 10 >= want * 9 && n * 10 <= want * 11,
                "{} layer {}: {} frames in {} s",
                user,
                layer,
                n,
                hold
            );
        }
        let before_request: Vec<f64> = key_times(frames, user, 2, media_start)
            .into_iter()
            .filter(|&k| k < run.requested.duration_since(media_start).as_secs_f64())
            .collect();
        assert_periodic(&before_request, user);
        phases.push(before_request[1] % 5.0);
    }
    let spread = circular_spread(&phases, 5.0);
    let tick_s = 1.0 / 30.0;
    let offsets: Vec<f64> = publishers
        .iter()
        .map(|user| {
            circular_mean(
                video(frames, user, 2)
                    .iter()
                    .map(|f| f.at.duration_since(media_start).as_secs_f64()),
                tick_s,
            )
        })
        .collect();
    assert!(
        circular_spread(&offsets, tick_s) > 0.003,
        "publishers send on the same instant of the tick: {:?}",
        offsets
    );
    assert!(
        spread > 0.2,
        "publishers keyframe in step: phases {:?}",
        phases
    );

    let requested = run.requested.duration_since(media_start).as_secs_f64();
    for layer in 0..3 {
        let keys = key_times(frames, "bot-002", layer, media_start);
        let answered = keys.iter().any(|&k| k >= requested && k <= requested + 0.4);
        let near_periodic = keys
            .iter()
            .any(|&k| k < requested && (requested + 0.4 - k) % 5.0 < 0.75);
        if answer {
            assert!(
                answered,
                "layer {}: request at {} s, keyframes {:?}",
                layer, requested, keys
            );
        } else if !near_periodic {
            assert!(
                !answered,
                "layer {}: request at {} s answered: {:?}",
                layer, requested, keys
            );
        }
    }
    if !answer {
        assert_periodic(
            &key_times(frames, "bot-002", 0, media_start),
            "bot-002 layer 0",
        );
    }

    let shed_from = run.hinted + Duration::from_secs(2);
    let after = |user: &str, layer: u32| {
        video(frames, user, layer)
            .iter()
            .filter(|f| f.at >= shed_from)
            .count()
    };
    assert_eq!(
        after("bot-002", 1) + after("bot-002", 2),
        0,
        "LAYER_HINT 0 switches video layers 1-2 off"
    );
    assert!(after("bot-002", 0) >= 14, "video layer 0 keeps flowing");
    assert!(after("bot-003", 2) >= 60, "other publishers are not capped");

    let hashes = |user: &str| -> HashSet<u64> {
        video(frames, user, 2)
            .iter()
            .map(|f| f.payload_hash)
            .collect()
    };
    assert!(
        !hashes("bot-003").is_disjoint(&hashes("bot-004")),
        "cameras in one process replay the same library"
    );
    let bytes = |user: &str| -> usize { video(frames, user, 2).iter().map(|f| f.bytes).sum() };
    assert!(
        bytes("alice") >= 3 * bytes("bot-003"),
        "the talker sends the speaking variant: alice {} B, camera {} B",
        bytes("alice"),
        bytes("bot-003")
    );
    phases
}

#[tokio::test(flavor = "multi_thread")]
async fn replay_publishers_keep_the_live_rates_and_answer_requests_only_when_told_to() {
    let (answering, ignoring) = tokio::join!(run_replay_bot(true), run_replay_bot(false));
    let a = check_replay_run(&answering, true);
    let b = check_replay_run(&ignoring, false);
    for (x, y) in a.iter().zip(&b) {
        let d = (x - y).abs().min(5.0 - (x - y).abs());
        assert!(
            d < 0.1,
            "the same --replay-seed draws the same phases: {:?} vs {:?}",
            a,
            b
        );
    }
}

#[test]
fn replay_settings_from_the_environment_are_validated() {
    let dir = std::env::temp_dir().join(format!("bot-replay-env-{}", std::process::id()));
    let config = write_assets(&dir, 9);
    let run = |envs: &[(&str, &str)], args: &[&str]| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_bot"));
        command
            .args(["--config", config.to_str().unwrap()])
            .args(args);
        for (k, v) in envs {
            command.env(k, v);
        }
        let out = command.env("RUST_LOG", "error").output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string()
            + &String::from_utf8_lossy(&out.stderr);
        (out.status.success(), text)
    };
    let (ok, text) = run(
        &[("BOT_VIDEO_MODE", "replay")],
        &["--simulcast-layers", "1"],
    );
    assert!(
        !ok && text.contains("--simulcast-layers 2 or 3"),
        "{}",
        text
    );
    let (ok, text) = run(&[("BOT_REPLAY_ANSWER_KEYFRAME_REQUESTS", "maybe")], &[]);
    assert!(
        !ok && text.contains("BOT_REPLAY_ANSWER_KEYFRAME_REQUESTS"),
        "{}",
        text
    );
    let _ = std::fs::remove_dir_all(&dir);
}
