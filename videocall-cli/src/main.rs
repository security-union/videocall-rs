use clap::Parser;
mod modes;

use modes::info::get_info;
use modes::stream::stream;
use tracing::debug;
use tracing::level_filters::LevelFilter;
use videocall_cli::cli_args::{Info, Mode, Opt};

async fn initialize() {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    // Wrap the sender in an Arc<Mutex<Option>> to allow mutable access in the closure.
    let sender_lock = std::sync::Arc::new(std::sync::Mutex::new(Some(sender)));
    debug!("Asking for permission to camera");
    videocall_nokhwa::nokhwa_initialize(move |x| {
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
        panic!("User denied permission to camera, can't stream without camera");
    } else {
        debug!("Permission granted to camera");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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

    #[cfg(target_os = "macos")]
    println!("*Attention: to select a camera on MacOS please use the UUID in the Extras field as the name as opposed to the actual Name.\n");

    let mut opt = Opt::parse();

    // if os is mac os we need to ask for permission for camera and microphone
    initialize().await;

    match opt.mode {
        Mode::Stream(ref mut s) => {
            // If video device index is None, show available cameras and exit
            match s.video_device_index.clone() {
                None => {
                    println!("No camera selected. Available cameras:");
                    get_info(Info {
                        list_cameras: true,
                        list_formats: None,
                        list_resolutions: None,
                    })
                    .await?;
                    println!("\nPlease run the command again with --video-device-index <INDEX>");
                    return Ok(());
                }
                Some(_index) => {
                    stream(s.clone()).await;
                }
            }
        }
        Mode::Info(i) => {
            get_info(i).await?;
        }
    };

    Ok(())
}
