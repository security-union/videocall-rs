#![feature(once_cell)]

mod client;
mod connection;
mod constants;
mod crypto;
mod decode;
mod encode;
mod media_devices;
mod wrappers;

pub use client::{VideoCallClient, VideoCallClientOptions};
pub use encode::{CameraEncoder, MicrophoneEncoder, ScreenEncoder};
pub use media_devices::{MediaDeviceAccess, MediaDeviceList};
