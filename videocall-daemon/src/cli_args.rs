use std::str::FromStr;

use clap::{ArgGroup, Args, Parser, Subcommand};
use thiserror::Error;
use url::Url;

/// Video Call Daemon
///
/// This daemon connects to the videocall.rs and streams audio and video to the specified meeting.
///
/// You can watch the video at https://videocall.rs/meeting/{user_id}/{meeting_id}
#[derive(Parser, Debug)]
#[clap(name = "client")]
pub struct Opt {
    #[clap(subcommand)]
    pub mode: Mode,
}

#[derive(Clone, Debug)]
pub enum IndexKind {
    String(String),
    Index(u32),
}

#[derive(Error, Debug)]
pub enum ParseIndexKindError {
    #[error("Invalid index value: {0}")]
    InvalidIndex(String),
}

impl FromStr for IndexKind {
    type Err = ParseIndexKindError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(index) = s.parse::<u32>() {
            Ok(IndexKind::Index(index))
        } else {
            Ok(IndexKind::String(s.to_string()))
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum Mode {
    /// Streaming mode with all the current options.
    Streaming(Streaming),

    /// Information mode to list cameras, formats, and resolutions.
    Info(Info),

    /// Test the camera on a window
    TestCamera(TestCamera),
}

#[derive(Args, Debug, Clone)]
pub struct Streaming {
    /// Perform NSS-compatible TLS key logging to the file specified in `SSLKEYLOGFILE`.
    #[clap(long = "keylog")]
    pub keylog: bool,

    /// URL to connect to.
    #[clap(long = "url", default_value = "https://transport.rustlemania.com")]
    pub url: Url,

    #[clap(long = "user-id")]
    pub user_id: String,

    #[clap(long = "meeting-id")]
    pub meeting_id: String,

    /// You can specify the video device index by index or by name.
    ///
    /// If you specify the index, it will be used directly.
    ///
    /// If you specify the name, it will be matched against the camera names.
    #[clap(long = "video-device-index", short = 'v')]
    pub video_device_index: IndexKind,

    #[clap(long = "audio-device", short = 'a')]
    pub audio_device: Option<String>,

    /// Resolution in WIDTHxHEIGHT format (e.g., 1920x1080)
    #[clap(long = "resolution", short = 'r')]
    pub resolution: String,

    /// Frames per second (e.g. 10, 30, 60)
    #[clap(long = "fps")]
    pub fps: u32,

    /// Send test pattern instead of camera video.
    #[clap(long = "send-test-pattern", short = 't')]
    pub test_pattern: bool,

    /// This is for ensuring that we can open the camera and encode video
    #[clap(long = "offline-streaming-test")]
    pub local_streaming_test: bool,
}

#[derive(Args, Debug)]
#[clap(group = ArgGroup::new("info").required(true))]
pub struct Info {
    /// List available cameras.
    #[clap(long = "list-cameras", group = "info")]
    pub list_cameras: bool,

    /// List supported formats for a specific camera using the index from `list-cameras`
    #[clap(long = "list-formats", group = "info")]
    pub list_formats: Option<IndexKind>, // Camera index

    /// List supported resolutions for a specific camera using the index from `list-cameras`
    #[clap(long = "list-resolutions", group = "info")]
    pub list_resolutions: Option<IndexKind>, // Camera index
}

#[derive(Args, Debug, Clone)]
pub struct TestCamera {
    #[clap(long = "video-device-index")]
    pub video_device_index: IndexKind,
}
