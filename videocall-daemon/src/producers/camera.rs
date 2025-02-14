use crate::cli_args::IndexKind;
use crate::video_encoder::Frame;
use anyhow::Result;
use nokhwa::pixel_format::I420Format;
use nokhwa::utils::RequestedFormat;
use nokhwa::utils::RequestedFormatType;
use nokhwa::{
    utils::{ApiBackend, CameraFormat, CameraIndex, FrameFormat},
    Camera,
};
use protobuf::Message;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::{self, Sender};
use tracing::{debug, error, info};

use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{MediaPacket, VideoMetadata};
use videocall_types::protos::packet_wrapper::{packet_wrapper::PacketType, PacketWrapper};

use super::encoder_thread::encoder_thread;
use super::producer::Producer;

pub struct CameraPacket {
    pub data: Vec<u8>,
    pub _format: FrameFormat,
    pub age: u128,
}

impl CameraPacket {
    pub fn new(data: Vec<u8>, format: FrameFormat, age: u128) -> CameraPacket {
        CameraPacket {
            data,
            _format: format,
            age,
        }
    }
}

pub fn transform_video_chunk(frame: &Frame, email: &str) -> PacketWrapper {
    let frame_type = if frame.key {
        "key".to_string()
    } else {
        "delta".to_string()
    };
    let media_packet: MediaPacket = MediaPacket {
        data: frame.data.to_vec(),
        frame_type,
        email: email.to_owned(),
        media_type: MediaType::VIDEO.into(),
        timestamp: since_the_epoch().as_micros() as f64,
        video_metadata: Some(VideoMetadata {
            sequence: frame.pts as u64,
            ..Default::default()
        })
        .into(),
        ..Default::default()
    };
    let data = media_packet.write_to_bytes().unwrap();
    PacketWrapper {
        data,
        email: media_packet.email,
        packet_type: PacketType::MEDIA.into(),
        ..Default::default()
    }
}

pub static THRESHOLD_MILLIS: u128 = 1000;

pub fn since_the_epoch() -> Duration {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap()
}

#[derive(Clone, Debug)]
pub struct CameraConfig {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub video_device_index: IndexKind,
    pub frame_format: FrameFormat,
    pub bitrate_kbps: u32,
    pub cpu_used: u8,
}

pub struct CameraDaemon {
    config: CameraConfig,
    user_id: String,
    cam_rx: Option<mpsc::Receiver<Option<CameraPacket>>>,
    cam_tx: Arc<mpsc::Sender<Option<CameraPacket>>>,
    quic_tx: Arc<Sender<Vec<u8>>>,
    quit: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
}

impl Producer for CameraDaemon {
    fn start(&mut self) -> Result<()> {
        self.handles.push(self.camera_thread()?);
        let encoder = encoder_thread(
            self.cam_rx.take().unwrap(),
            self.quic_tx.clone(),
            self.quit.clone(),
            self.config.clone(),
            self.user_id.clone(),
        );
        self.handles.push(encoder);
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.quit.store(true, std::sync::atomic::Ordering::Relaxed);
        for handle in self.handles.drain(..) {
            handle.join().unwrap();
        }
        Ok(())
    }
}

impl CameraDaemon {
    pub fn from_config(
        config: CameraConfig,
        user_id: String,
        quic_tx: Sender<Vec<u8>>,
    ) -> CameraDaemon {
        let (cam_tx, cam_rx) = mpsc::channel(100);
        CameraDaemon {
            config,
            user_id,
            cam_rx: Some(cam_rx),
            cam_tx: Arc::new(cam_tx),
            quit: Arc::new(AtomicBool::new(false)),
            handles: vec![],
            quic_tx: Arc::new(quic_tx),
        }
    }

    fn camera_thread(&self) -> Result<JoinHandle<()>> {
        let devices = nokhwa::query(ApiBackend::Auto)?;
        for (i, camera_info) in devices.iter().enumerate() {
            info!("AVAILABLE CAMERA DEVICE INDEX {}: {:?}", i, camera_info);
        }
        let cam_tx = self.cam_tx.clone();
        let width = self.config.width;
        let height = self.config.height;
        let framerate = self.config.framerate;
        let frame_format = self.config.frame_format;
        let video_device_index = match &self.config.video_device_index {
            IndexKind::String(s) => CameraIndex::String(s.clone()),
            IndexKind::Index(i) => CameraIndex::Index(*i),
        };
        let quit = self.quit.clone();
        Ok(std::thread::spawn(move || {
            debug!("Camera opened... waiting for frames");
            let mut camera = match Camera::new(
                video_device_index,
                RequestedFormat::new::<I420Format>(RequestedFormatType::Exact(
                    CameraFormat::new_from(width, height, frame_format, framerate),
                )),
            ) {
                Ok(camera) => camera,
                Err(e) => {
                    panic!("{}\n please run 'info --list-formats' to see the available resolutions and fps", e)
                }
            };
            let actual_resolution = camera.resolution();
            camera.open_stream().unwrap();

            // Allocate buffer for raw data based on actual format
            let mut image_buffer =
                vec![
                    0;
                    actual_resolution.width() as usize * actual_resolution.height() as usize * 3
                        / 2
                ];

            let frame_time = Duration::from_millis(1000u64 / framerate as u64);
            let mut last_frame_time = Instant::now();
            loop {
                // use last_frame_time to calculate if we should skip this frame
                let elapsed = last_frame_time.elapsed();
                if elapsed < frame_time {
                    continue;
                }
                last_frame_time = Instant::now();
                let frame = camera.frame().unwrap();
                frame
                    .decode_image_to_buffer::<I420Format>(&mut image_buffer)
                    .unwrap();
                // Check if we should quit
                if quit.load(std::sync::atomic::Ordering::Relaxed) {
                    info!("Quit signal received, exiting frame loop.");
                    return;
                }

                // Try sending the frame over the channel
                if let Err(e) = cam_tx.try_send(Some(CameraPacket::new(
                    image_buffer.clone(),
                    frame_format,
                    since_the_epoch().as_millis(),
                ))) {
                    error!("Error sending image: {}", e);
                }
            }
        }))
    }
}

pub fn buffer_size_i420(width: u32, height: u32) -> u32 {
    width
        .checked_mul(height)
        .and_then(|y_size| y_size.checked_add(y_size / 2)) // Total size = Y + U + V
        .expect("Buffer size calculation overflowed")
}
