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
    #[clap(default_value = "1280x720")]
    pub resolution: String,

    /// Frames per second (e.g. 10, 15, 30, 60)
    #[clap(long = "fps")]
    #[clap(default_value = "15")]
    pub fps: u32,

    #[clap(long = "bitrate-kbps")]
    #[clap(default_value = "500")]
    pub bitrate_kbps: u32,

    /// Send test pattern instead of camera video.
    #[clap(long = "send-test-pattern", short = 't')]
    pub test_pattern: bool,

    /// This is for ensuring that we can open the camera and encode video
    #[clap(long = "offline-streaming-test")]
    pub local_streaming_test: bool,

    /// Controls the speed vs. quality tradeoff for VP9 encoding.
    ///
    /// The value ranges from `0` (slowest, best quality) to `15` (fastest, lowest quality).
    ///
    /// ## Valid Values:
    /// - `0` to `3`: **Balanced** speed and quality (recommended for file-based encoding, YouTube, VOD).
    /// - `4` to `8`: **Fast encoding**, lower quality (good for real-time streaming, WebRTC, live video).
    /// - `9` to `15`: **Very fast encoding**, lowest quality, largest files (for ultra-low-latency applications).
    ///
    /// videocall-daemon --cpu-used 5  # Fast encoding, good for live streaming
    /// videocall-daemon --cpu-used 0  # High-quality encoding, reasonable speed
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u8).range(0..=15))]
    pub cpu_used: u8,
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
