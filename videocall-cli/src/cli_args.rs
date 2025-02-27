use std::str::FromStr;

use clap::{ArgGroup, Args, Parser, Subcommand};
use thiserror::Error;
use url::Url;
use videocall_nokhwa::utils::FrameFormat;

/// Video Call CLI
///
/// This cli connects to the videocall.rs and streams audio and video to the specified meeting.
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
    /// Stream audio and video to the specified meeting.
    Stream(Stream),

    /// Information mode to list cameras, formats, and resolutions.
    Info(Info),
}

#[derive(Args, Debug, Clone)]
pub struct Stream {
    /// URL to connect to.
    #[clap(long = "url", default_value = "https://transport.rustlemania.com")]
    pub url: Url,

    #[clap(long = "user-id")]
    pub user_id: String,

    #[clap(long = "meeting-id")]
    pub meeting_id: String,

    /// Specify which camera to use, either by index number or name.
    ///
    /// Examples:
    ///   --video-device-index 0    # Use first camera
    ///   --video-device-index "HD WebCam"  # Use camera by name
    ///
    /// If not provided, the program will list all available cameras.
    /// You can also see available cameras by running:
    ///   videocall-cli info --list-cameras
    ///
    /// Note for MacOS users: You must use the device UUID instead of the human-readable name.
    /// The UUID can be found in the "Extras" field when listing cameras.
    #[clap(long = "video-device-index", short = 'v')]
    pub video_device_index: Option<IndexKind>,

    #[clap(long = "audio-device", short = 'a')]
    pub audio_device: Option<String>,

    /// Resolution in WIDTHxHEIGHT format (e.g., 1920x1080)
    #[clap(long = "resolution", short = 'r')]
    #[clap(default_value = "1280x720")]
    pub resolution: String,

    /// Frame rate for the video stream.
    #[clap(long = "fps")]
    #[clap(default_value = "30")]
    pub fps: u32,

    #[clap(long = "bitrate-kbps")]
    #[clap(default_value = "500")]
    pub bitrate_kbps: u32,

    /// Controls the speed vs. quality tradeoff for VP9 encoding.
    ///
    /// The value ranges from `0` (slowest, best quality) to `15` (fastest, lowest quality).
    ///
    /// The cli does not allow selecting values below 4 because they are useless for realtime streaming.
    ///
    /// ## Valid Values:
    /// - `4` to `8`: **Fast encoding**, lower quality (good for real-time streaming, live video).
    /// - `9` to `15`: **Very fast encoding**, lowest quality, largest files (for ultra-low-latency applications).
    ///
    /// videocall-cli --vp9-cpu-used 5  # Fast encoding, good for live streaming
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u8).range(4..=15))]
    pub vp9_cpu_used: u8,

    /// Frame format to use for the video stream.
    /// Different cameras support different formats.
    /// Please use the `info` subcommand to list supported formats for a specific camera.
    #[arg(long, default_value_t = FrameFormat::NV12, value_parser = parse_frame_format)]
    pub frame_format: FrameFormat,

    /// Perform NSS-compatible TLS key logging to the file specified in `SSLKEYLOGFILE`.
    #[clap(long = "debug-keylog")]
    pub keylog: bool,

    /// Send test pattern instead of camera video.
    #[clap(long = "debug-send-test-pattern")]
    pub send_test_pattern: bool,

    /// This is for ensuring that we can open the camera and encode video
    #[clap(long = "debug-offline-streaming-test")]
    pub local_streaming_test: bool,
}

fn parse_frame_format(s: &str) -> Result<FrameFormat, String> {
    match s {
        "NV12" => Ok(FrameFormat::NV12),
        // TODO: Merge MR with MacOS BGRA support
        // "BGRA" => Ok(FrameFormat::BGRA),
        "YUYV" => Ok(FrameFormat::YUYV),
        _ => Err("Invalid frame format, please use one of [NV12, BGRA, YUYV]".to_string()),
    }
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
