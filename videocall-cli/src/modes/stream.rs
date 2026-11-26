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

use anyhow::bail;
use tokio::sync::mpsc::channel;
use videocall_cli::auth::store::Store;
use videocall_cli::auth::{ApiOrigin, Credentials};
use videocall_cli::cli_args::Stream;
use videocall_cli::consumers::camera_synk::CameraSynk;
use videocall_cli::consumers::dead_synk::DeadSynk;
use videocall_cli::consumers::webtransport::{check_insecure_skip_verify, WebTransportClient};
use videocall_cli::consumers::CameraSynks;
use videocall_cli::producers::{
    camera::{CameraConfig, CameraDaemon},
    microphone::MicrophoneDaemon,
    producer::Producer,
    test_pattern_sender::TestPatternSender,
};
use videocall_cli::room::RoomSession;

/// User id stamped on packets in `--debug-offline-streaming-test`, which
/// never talks to a server.
const OFFLINE_TEST_USER_ID: &str = "offline-streaming-test";

/// Resolves on Ctrl-C, or on SIGTERM on unix (systemd/docker stop).
///
/// A requested stop exits 0 wherever it happens (joining, connecting or
/// streaming). Once connected, the relay session is closed explicitly
/// (code 0) so the relay drops us right away. The CLI does not call
/// meeting-api `leave`: its account may also be in the meeting from a
/// browser, and leaving would mark that participant as gone.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!("can't listen for Ctrl-C: {e}");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    () = ctrl_c => {}
                    _ = term.recv() => {}
                }
            }
            Err(e) => {
                tracing::warn!("can't listen for SIGTERM: {e}");
                ctrl_c.await;
            }
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}

/// Join the meeting through meeting-api, then stream to the relay.
pub async fn stream(opt: Stream) -> anyhow::Result<()> {
    // One listener for the whole run, so a signal between selects isn't lost.
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    // Parse resolution
    let resolution: Vec<&str> = opt.resolution.split('x').collect();
    if resolution.len() != 2 {
        panic!("invalid resolution: {}", opt.resolution);
    }
    let width = resolution[0].parse::<u32>().expect("invalid width");
    let height = resolution[1].parse::<u32>().expect("invalid height");
    let format = opt.frame_format;

    println!(
        "User requested resolution {}x{} and format {}",
        width, height, format
    );
    let framerate = opt.fps;
    // validate framerate
    let valid_framerates = [10u32, 15u32, 30u32, 60u32];
    if !valid_framerates.contains(&framerate) {
        panic!("invalid framerate: {framerate}, we currently support only {valid_framerates:?}",);
    }
    let video_device_index = opt.video_device_index.clone().unwrap();
    let send_test_pattern = opt.send_test_pattern;
    let audio_device = opt.audio_device.clone();
    let local_streaming = opt.local_streaming_test;
    let bitrate_kbps = opt.bitrate_kbps;
    let cpu_used = opt.vp9_cpu_used;
    let frame_format = opt.frame_format;
    let (user_id, mut client) = if local_streaming {
        (
            OFFLINE_TEST_USER_ID.to_string(),
            CameraSynks::DeadSynk(DeadSynk::new(opt)),
        )
    } else {
        // Fail fast on a bad flag before logging in or joining anything.
        check_insecure_skip_verify(&opt.url, opt.insecure_skip_verify)?;
        let api = ApiOrigin::from_url(&opt.api.api_url)?;
        let creds = Credentials::load(Store::open_default()?, api).await?;
        let meeting_id = opt.meeting_id.clone();
        println!("Joining meeting '{meeting_id}'...");
        let room = tokio::select! {
            room = RoomSession::join(creds, &meeting_id, opt.display_name.clone()) => room?,
            () = &mut shutdown => {
                println!("Stopped before joining meeting '{meeting_id}'.");
                return Ok(());
            }
        };
        println!("Admitted to meeting '{meeting_id}' as {}.", room.user_id());
        (
            room.user_id().to_string(),
            CameraSynks::CameraSynk(Box::new(WebTransportClient::new(opt, room))),
        )
    };
    tokio::select! {
        result = client.connect() => result?,
        () = &mut shutdown => {
            println!("Stopped.");
            return Ok(());
        }
    }
    let camera_config = CameraConfig {
        width,
        height,
        framerate,
        frame_format,
        video_device_index,
        bitrate_kbps,
        cpu_used,
    };
    let (wt_tx, mut wt_rx) = channel::<Vec<u8>>(10);
    let mut video_producer: Box<dyn Producer> = if send_test_pattern {
        Box::new(TestPatternSender::from_config(
            camera_config,
            user_id.clone(),
            wt_tx.clone(),
        ))
    } else {
        Box::new(CameraDaemon::from_config(
            camera_config,
            user_id.clone(),
            wt_tx.clone(),
        ))
    };
    video_producer.start().expect("failed to start camera");
    let mut microphone = MicrophoneDaemon::default();
    if let Some(audio_device) = audio_device {
        microphone
            .start(wt_tx, audio_device, user_id.clone())
            .expect("failed to start microphone");
    }

    loop {
        let data = tokio::select! {
            data = wt_rx.recv() => data,
            () = &mut shutdown => {
                client.close().await;
                println!("Stopped.");
                return Ok(());
            }
            reason = client.lost() => {
                // No reconnect: exit non-zero so systemd (or the user)
                // restarts the CLI, which joins again from scratch.
                bail!("lost the relay connection ({reason}); exiting");
            }
        };
        let Some(data) = data else { break };
        tracing::debug!("Received packet from producer: {} bytes", data.len());
        if let Err(e) = client.send_packet(data).await {
            tracing::debug!("Packet not sent: {e}");
        }
    }
    client.close().await;
    Ok(())
}
