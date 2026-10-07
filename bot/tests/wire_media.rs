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

use futures_util::StreamExt;
use protobuf::Message;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Audio,
    Video,
}

type Frames = Arc<Mutex<Vec<(Instant, String, Kind, u32)>>>;

async fn relay_stand_in() -> (u16, Frames) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let frames: Frames = Arc::default();
    let sink = Arc::clone(&frames);
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let sink = Arc::clone(&sink);
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
                    return;
                };
                while let Some(Ok(msg)) = ws.next().await {
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
                    let layer = wrapper.simulcast_layer_id;
                    sink.lock()
                        .unwrap()
                        .push((Instant::now(), user, kind, layer));
                }
            });
        }
    });
    (port, frames)
}

fn write_assets(dir: &Path, port: u16) -> std::path::PathBuf {
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
            "meeting_id: wire-test\nws_url: ws://127.0.0.1:{port}\njwt_secret: test-secret-for-integration-tests\nconversation_dir: {}\nvideo_mode: ekg\nwarmup_secs: 0\nramp_up_delay_ms: 100\n",
            conv.display()
        ),
    )
    .unwrap();
    config
}

fn per_second(
    frames: &[(Instant, String, Kind, u32)],
    user: &str,
    kind: Kind,
    layer: Option<u32>,
) -> Vec<usize> {
    let Some(start) = frames.iter().find(|f| f.1 == user).map(|f| f.0) else {
        return Vec::new();
    };
    let mut buckets = Vec::new();
    for (at, u, k, l) in frames {
        if u == user && *k == kind && layer.is_none_or(|want| want == *l) {
            let s = at.duration_since(start).as_secs() as usize;
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

#[tokio::test(flavor = "multi_thread")]
async fn a_sigkill_after_media_start_leaves_every_join_on_disk() {
    let (port, _frames) = relay_stand_in().await;
    struct RemoveOnDrop(std::path::PathBuf);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
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
