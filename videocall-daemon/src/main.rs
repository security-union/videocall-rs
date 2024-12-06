use clap::Parser;

use tokio::sync::mpsc::channel;
use video_call_daemon::camera::{CameraConfig, CameraDaemon};
use video_call_daemon::microphone::MicrophoneDaemon;
use video_call_daemon::quic::{Client, ClientError, Opt};

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
    let video_device_index = opt.video_device_index;
    let audio_device = opt.audio_device.clone();
    let mut client = Client::new(opt).expect("failed to create client");
    client.connect().await.expect("failed to connect");

    let camera_config = CameraConfig {
        width: 640,
        height: 480,
        framerate: 15,
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
        if let Err(e) = client.send(data).await {
            match e {
                ClientError::OversizedPacket(size) => {
                    tracing::error!(
                        "packet size {} exceeds maximum packet size {}",
                        size,
                        client.max_packet_size
                    );
                }
                ClientError::NotConnected => {
                    tracing::error!("not connected, attempting to reconnect");
                    client.connect().await.expect("failed to connect");
                }
                _ => {
                    panic!("failed to send data: {}", e);
                }
            }
        }
    }
}
