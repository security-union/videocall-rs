use crate::cli_args::IndexKind;
use crate::producers::camera::since_the_epoch;
use crate::producers::camera::transform_video_chunk;
use crate::producers::camera::THRESHOLD_MILLIS;
use crate::producers::producer::Producer;
use crate::video_encoder::Frame;
use crate::video_encoder::VideoEncoderBuilder;
use anyhow::Result;
use image::ImageBuffer;
use image::ImageReader;
use image::Rgb;
use nokhwa::pixel_format::I420Format;
use nokhwa::utils::RequestedFormat;
use nokhwa::utils::RequestedFormatType;
use nokhwa::{
    utils::{ApiBackend, CameraFormat, CameraIndex, FrameFormat},
    Camera,
};
use protobuf::Message;
use std::fs::read;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info};

use tokio::sync::mpsc::{self, Sender};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{MediaPacket, VideoMetadata};
use videocall_types::protos::packet_wrapper::{packet_wrapper::PacketType, PacketWrapper};

use super::camera::CameraConfig;
use super::camera::CameraPacket;

const TARGET_FPS: u64 = 30;

pub struct TestPatternSender {
    user_id: String,
    cam_rx: Option<mpsc::Receiver<Option<CameraPacket>>>,
    cam_tx: Arc<mpsc::Sender<Option<CameraPacket>>>,
    quic_tx: Arc<Sender<Vec<u8>>>,
    quit: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    config: CameraConfig,
}

impl TestPatternSender {
    pub fn from_config(_config: CameraConfig, user_id: String, quic_tx: Sender<Vec<u8>>) -> Self {
        let (cam_tx, cam_rx) = mpsc::channel(100);
        // rewrite res to 1280 × 680
        let config = CameraConfig {
            width: 1280,
            height: 720,
            framerate: 15,
            frame_format: FrameFormat::NV12,
            video_device_index: IndexKind::Index(0),
        };
        Self {
            config,
            user_id,
            cam_rx: Some(cam_rx),
            cam_tx: Arc::new(cam_tx),
            quit: Arc::new(AtomicBool::new(false)),
            handles: vec![],
            quic_tx: Arc::new(quic_tx),
        }
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
            video_encoder.update_bitrate(200_000).unwrap();
            let mut sequence = 0;
            // the video encoder only supports I420 format, so whatever the camera gives us, we need to convert it
            while let Some(data) = cam_rx.blocking_recv() {
                if quit.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let CameraPacket {
                    data,
                    _format: _,
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
                    // Frame size kbit
                    let frame_size = frame.data.len() as f64 / 1000f64;
                    debug!("Frame size: {:.2} kbit", frame_size);
                    let packet_wrapper = transform_video_chunk(&frame, &user_id);
                    if let Err(e) = quic_tx.try_send(packet_wrapper.write_to_bytes().unwrap()) {
                        error!("Unable to send packet: {:?}", e);
                    }
                }
            }
        })
    }

    fn camera_thread(&self) -> Result<JoinHandle<()>> {
        let quit = self.quit.clone();
        let cam_tx = self.cam_tx.clone();
        let frame_format = FrameFormat::NV12;
        let interval = Duration::from_millis(1000 / TARGET_FPS);
        // Read the first 10 images from directory: src/producers/sample_video
        let mut frames = vec![];
        for i in 1..100 {
            let path = format!("src/producers/sample_video/output_{}.jpg", i);
            let img = read(path).unwrap();
            let img = ImageReader::new(std::io::Cursor::new(img))
                .with_guessed_format()
                .unwrap();
            // Transform the image to NV12 format
            let img = img.decode().unwrap();
            let img = rgb_to_nv12(&img.to_rgb8());
            frames.push(img);
        }
        let mut iterator = frames.into_iter().cycle();
        // rotate the image
        Ok(std::thread::spawn(move || {
            debug!("Camera opened... waiting for frames");
            loop {
                // Check if we should quit
                if quit.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                // Generate a test pattern
                // Send the frame to the encoder
                // Try sending the frame over the channel
                let next = iterator.next().unwrap();
                if let Err(e) = cam_tx.try_send(Some(CameraPacket::new(
                    next,
                    frame_format,
                    since_the_epoch().as_millis(),
                ))) {
                    error!("Error sending image: {}", e);
                }
                // Sleep for the interval
                std::thread::sleep(interval);
            }
        }))
    }
}

impl Producer for TestPatternSender {
    fn start(&mut self) -> anyhow::Result<()> {
        self.handles.push(self.camera_thread()?);
        let encoder = self.encoder_thread();
        self.handles.push(encoder);
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        todo!()
    }
}

// Function to convert a grayscale image buffer to NV12 format
fn rgb_to_nv12(image: &ImageBuffer<Rgb<u8>, Vec<u8>>) -> Vec<u8> {
    let width = image.width() as usize;
    let height = image.height() as usize;
    let mut nv12_data = vec![0u8; width * height * 3 / 2];

    rgb_to_i420(image.as_raw(), width, height, &mut nv12_data);
    nv12_data
}

pub fn rgb_to_i420(rgb: &[u8], width: usize, height: usize, i420: &mut [u8]) {
    assert!(
        i420.len() >= width * height * 3 / 2,
        "Insufficient I420 buffer size"
    );

    let (y_plane, uv_planes) = i420.split_at_mut(width * height);
    let (u_plane, v_plane) = uv_planes.split_at_mut(width * height / 4);

    for y in 0..height {
        for x in 0..width {
            let rgb_index = (y * width + x) * 3;
            let r = rgb[rgb_index] as f32;
            let g = rgb[rgb_index + 1] as f32;
            let b = rgb[rgb_index + 2] as f32;

            // Calculate Y, U, V components
            let y_value = (0.257 * r + 0.504 * g + 0.098 * b + 16.0).round() as u8;
            let u_value = (-0.148 * r - 0.291 * g + 0.439 * b + 128.0).round() as u8;
            let v_value = (0.439 * r - 0.368 * g - 0.071 * b + 128.0).round() as u8;

            y_plane[y * width + x] = y_value;

            if y % 2 == 0 && x % 2 == 0 {
                let uv_index = (y / 2) * (width / 2) + (x / 2);
                u_plane[uv_index] = u_value;
                v_plane[uv_index] = v_value;
            }
        }
    }
}
