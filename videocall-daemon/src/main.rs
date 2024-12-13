use clap::Parser;

use tokio::sync::mpsc::channel;
use videocall_daemon::camera::{CameraConfig, CameraDaemon};
use videocall_daemon::microphone::MicrophoneDaemon;
use videocall_daemon::quic::{Client, Opt};

#[tokio::main]
async fn main() {
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .finish(),
    )
    .unwrap();
    let opt = Opt::parse();

    // Parse resolution
    let resolution: Vec<&str> = opt.resolution.split('x').collect();
    if resolution.len() != 2 {
        panic!("invalid resolution: {}", opt.resolution);
    }
    let width = resolution[0].parse::<u32>().expect("invalid width");
    let height = resolution[1].parse::<u32>().expect("invalid height");
    let framerate = opt.fps;
    // validate framerate
    if framerate != 10 && framerate != 15 && framerate != 30 && framerate != 60 {
        panic!("invalid framerate: {}", framerate);
    }
    let user_id = opt.user_id.clone();
    let video_device_index = opt.video_device_index;
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
    let mut camera = CameraDaemon::from_config(camera_config, user_id.clone(), quic_tx.clone());
    camera.start().expect("failed to start camera");
    let mut microphone = MicrophoneDaemon::default();
    if let Some(audio_device) = audio_device {
        microphone
            .start(quic_tx, audio_device, user_id)
            .expect("failed to start microphone");
    }
    while let Some(data) = quic_rx.recv().await {
        if let Err(e) = client.send_packet(data).await {
            tracing::error!("Failed to send packet: {}", e);
        }
    }
}
