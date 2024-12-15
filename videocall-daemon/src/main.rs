use clap::Parser;

use tokio::sync::mpsc::channel;
use tracing::level_filters::LevelFilter;
use videocall_daemon::camera::{CameraConfig, CameraDaemon};
use videocall_daemon::microphone::MicrophoneDaemon;
use videocall_daemon::quic::{Client, Info, Mode, Opt, Streaming};

#[cfg(target_os = "macos")]
async fn initialize() {
    use tracing::warn;

    let (sender, receiver) = tokio::sync::oneshot::channel();
    // Wrap the sender in an Arc<Mutex<Option>> to allow mutable access in the closure.
    let sender_lock = std::sync::Arc::new(std::sync::Mutex::new(Some(sender)));
    warn!("Asking for permission to camera");
    nokhwa::nokhwa_initialize(move |x| {
        if let Ok(mut sender_option) = sender_lock.lock() {
            // Take the sender out of the Option and send the value
            if let Some(sender) = sender_option.take() {
                let _ = sender.send(x); // Ignore the result to avoid panics
            }
        }
    });

    // Await for the user to accept or deny the permission
    let x = receiver.await.unwrap();
    if !x {
        panic!("User denied permission to camera or microphone");
    }
}

#[tokio::main]
async fn main() {
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(
                tracing_subscriber::EnvFilter::builder()
                    .with_default_directive(LevelFilter::INFO.into())
                    .from_env_lossy(),
            )
            .finish(),
    )
    .unwrap();
    let opt = Opt::parse();
    // if os is mac os we need to ask for permission for camera and microphone
    #[cfg(target_os = "macos")]
    initialize().await;

    match opt.mode {
        Mode::Streaming(s) => {
            stream(s).await;
        }
        Mode::Info(i) => {
            get_info(i).await;
        }
    };
}

async fn stream(opt: Streaming) {
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
    let meeting_id = opt.meeting_id.clone();
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
            .start(quic_tx, audio_device, user_id.clone())
            .expect("failed to start microphone");
    }

    tracing::info!(
 "If you used the default url, the stream is ready at https://app.videocall.rs with meeting id:{} \n** warning: use Chrome or Chromium \n** warning: do no reuse the username {}",
     meeting_id,
     user_id
 );
    while let Some(data) = quic_rx.recv().await {
        if let Err(e) = client.send_packet(data).await {
            tracing::error!("Failed to send packet: {}", e);
        }
    }
}

async fn get_info(_info: Info) {
    panic!("Not implemented yet");
}
