use tokio::sync::mpsc::channel;
use videocall_daemon::producers::{
    camera::{CameraConfig, CameraDaemon},
    microphone::MicrophoneDaemon,
    producer::Producer,
    test_pattern_sender::TestPatternSender,
};

use videocall_daemon::{cli_args::Streaming, quic::Client};

pub async fn stream(opt: Streaming) {
    // Parse resolution
    let resolution: Vec<&str> = opt.resolution.split('x').collect();
    if resolution.len() != 2 {
        panic!("invalid resolution: {}", opt.resolution);
    }
    let width = resolution[0].parse::<u32>().expect("invalid width");
    let height = resolution[1].parse::<u32>().expect("invalid height");
    let framerate = opt.fps;
    // validate framerate
    if framerate != 10 && framerate != 15 {
        panic!(
            "invalid framerate: {}, we currently support only [10, 15]",
            framerate
        );
    }
    let user_id = opt.user_id.clone();
    let video_device_index = opt.video_device_index.clone();
    let send_test_pattern = opt.test_pattern;
    let audio_device = opt.audio_device.clone();
    let mut client = Client::new(opt);
    client.connect().await.expect("failed to connect");
    let camera_config = CameraConfig {
        width,
        height,
        framerate,
        frame_format: nokhwa::utils::FrameFormat::YUYV,
        video_device_index,
    };
    let (quic_tx, mut quic_rx) = channel::<Vec<u8>>(10);
    let mut video_producer: Box<dyn Producer> = if send_test_pattern {
        Box::new(TestPatternSender::from_config(
            camera_config,
            user_id.clone(),
            quic_tx.clone(),
        ))
    } else {
        Box::new(CameraDaemon::from_config(
            camera_config,
            user_id.clone(),
            quic_tx.clone(),
        ))
    };
    video_producer.start().expect("failed to start camera");
    let mut microphone = MicrophoneDaemon::default();
    if let Some(audio_device) = audio_device {
        microphone
            .start(quic_tx, audio_device, user_id.clone())
            .expect("failed to start microphone");
    }

    while let Some(data) = quic_rx.recv().await {
        if let Err(e) = client.send_packet(data).await {
            tracing::error!("Failed to send packet: {}", e);
        }
    }
}
