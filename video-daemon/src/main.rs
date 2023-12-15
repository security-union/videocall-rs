use clap::Parser;

use tokio::sync::mpsc::channel;
use video_daemon::camera::{CameraConfig, CameraDaemon};
use video_daemon::quic::{Client, Opt};

#[tokio::main]
async fn main() {
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .finish(),
    )
    .unwrap();
    let opt = Opt::parse();
    let user_id = opt.user_id.clone();
    let mut client = Client::new(opt).expect("failed to create client");
    client.connect().await.expect("failed to connect");
    let camera_config = CameraConfig {
        width: 640,
        height: 480,
        framerate: 30,
        frame_format: nokhwa::utils::FrameFormat::YUYV,
        video_device_index: 0,
    };
    let (quic_tx, mut quic_rx) = channel::<Vec<u8>>(10);
    let mut camera = CameraDaemon::from_config(camera_config, user_id, quic_tx);
    camera.start().expect("failed to start camera");
    while let Some(data) = quic_rx.recv().await {
        if let Err(e) = client.send(data).await {
            panic!("failed to send data: {}", e);
        }
    }
}
