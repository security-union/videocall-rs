use crate::cli_args::IndexKind;
use crate::video_encoder::Frame;
use crate::video_encoder::VideoEncoderBuilder;
use anyhow::Result;
use nokhwa::pixel_format::I420Format;
use nokhwa::utils::RequestedFormat;
use nokhwa::utils::RequestedFormatType;

use nokhwa::utils::Resolution;
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

const TARGET_FPS: u64 = 15;

struct CameraPacket {
    data: Vec<u8>,
    format: FrameFormat,
    age: u128,
}

impl CameraPacket {
    pub fn new(data: Vec<u8>, format: FrameFormat, age: u128) -> CameraPacket {
        CameraPacket { data, format, age }
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

static THRESHOLD_MILLIS: u128 = 1000;

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

    pub fn start(&mut self) -> Result<()> {
        self.handles.push(self.camera_thread()?);
        let encoder = self.encoder_thread();
        self.handles.push(encoder);
        Ok(())
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
        let video_device_index_clone = video_device_index.clone();
        let quit = self.quit.clone();
        Ok(std::thread::spawn(move || {
            debug!("Camera opened... waiting for frames");
            let mut camera = Camera::new(
                video_device_index,
                RequestedFormat::new::<I420Format>(RequestedFormatType::Closest(
                    CameraFormat::new_from(width, height, frame_format, framerate),
                )),
            )
            .or_else(|e| {
                error!("Failed to open camera with closest format: {}", e);
                Camera::new(
                    video_device_index_clone,
                    RequestedFormat::new::<I420Format>(
                        RequestedFormatType::AbsoluteHighestFrameRate,
                    ),
                )
            })
            .unwrap();
            let actual_format = camera.camera_format();
            camera.open_stream().unwrap();

            // Allocate buffer for raw data based on actual format
            let mut image_buffer = vec![0; buffer_size_i420(actual_format.resolution().width(), actual_format.resolution().height()) as usize];

            // This loop should run at most at 30 fps, if actual fps is higher we should skip frames
            let frame_time = Duration::from_millis(1000u64 / TARGET_FPS);
            let mut last_frame_time = Instant::now();
            loop {
                // Try writing a frame to the buffer
                if let Err(e) = camera.write_frame_to_buffer::<I420Format>(&mut image_buffer) {
                    error!("Failed to write frame to buffer: {}", e);
                    break;
                }

                // use last_frame_time to calculate if we should skip this frame
                let elapsed = last_frame_time.elapsed();
                if elapsed < frame_time {
                    continue;
                }
                last_frame_time = Instant::now();

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

    fn encoder_thread(&mut self) -> JoinHandle<()> {
        let mut cam_rx = self.cam_rx.take().unwrap();
        let quic_tx = self.quic_tx.clone();
        let quit = self.quit.clone();
        let width = self.config.width;
        let height = self.config.height;
        let user_id = self.user_id.clone();
        std::thread::spawn(move || {
            let mut video_encoder = VideoEncoderBuilder::default()
                .set_resolution(width, height)
                .build()
                .unwrap();
            video_encoder.update_bitrate(100_000).unwrap();
            let mut sequence = 0;
            // the video encoder only supports I420 format, so whatever the camera gives us, we need to convert it
            while let Some(data) = cam_rx.blocking_recv() {
                if quit.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let CameraPacket {
                    data,
                    format: _,
                    age,
                } = data.unwrap();

                // If age older than threshold, throw it away.
                let image_age = since_the_epoch().as_millis() - age;
                if image_age > THRESHOLD_MILLIS {
                    debug!("throwing away old image with age {} ms", image_age);
                    continue;
                }
                let encoding_time = Instant::now();
                let frames = match video_encoder.encode(sequence, data.as_slice()) {
                    Ok(frames) => frames,
                    Err(e) => {
                        error!("Error encoding frame: {:?}", e);
                        continue;
                    }
                };
                sequence += 1;
                debug!("encoding took {:?}", encoding_time.elapsed());
                for frame in frames {
                    let packet_wrapper = transform_video_chunk(&frame, &user_id);
                    if let Err(e) = quic_tx.try_send(packet_wrapper.write_to_bytes().unwrap()) {
                        error!("Unable to send packet: {:?}", e);
                    }
                }
            }
        })
    }

    pub fn stop(&mut self) -> Result<()> {
        self.quit.store(true, std::sync::atomic::Ordering::Relaxed);
        for handle in self.handles.drain(..) {
            handle.join().unwrap();
        }
        Ok(())
    }
}


fn buffer_for_format(actual_format: &CameraFormat) -> Vec<u8> {
    let resolution = actual_format.resolution();
    let width = resolution.width();
    let height = resolution.height();

    // Calculate the required buffer size safely
    let buffer_size: Option<u32> = match actual_format.format() {
        FrameFormat::YUYV => width.checked_mul(height).unwrap().checked_mul(2),
        FrameFormat::BGRA => width.checked_mul(height).unwrap().checked_mul(4),
        FrameFormat::RAWRGB => width.checked_mul(height).unwrap().checked_mul(3),
        FrameFormat::NV12 => width
            .checked_mul(height).unwrap()
            .checked_add(width.checked_mul(height).unwrap().checked_div(2).unwrap()),
        _ => panic!("Unsupported format: {:?}", actual_format.format()),
    };

    // Handle potential overflow or other size calculation errors
    let buffer_size = match buffer_size {
        Some(size) => size,
        None => panic!("Buffer size calculation overflowed or is invalid."),
    };

    // Allocate the buffer
    vec![0u8; buffer_size as usize]
}

fn buffer_size_i420(width: u32, height: u32) -> u32 {
    width
        .checked_mul(height)
        .and_then(|y_size| y_size.checked_add(y_size / 2)) // Total size = Y + U + V
        .expect("Buffer size calculation overflowed")
}

