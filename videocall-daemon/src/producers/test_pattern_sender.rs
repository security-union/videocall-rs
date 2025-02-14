use crate::cli_args::IndexKind;
use crate::producers::camera::since_the_epoch;
use crate::producers::camera::transform_video_chunk;
use crate::producers::camera::THRESHOLD_MILLIS;
use crate::producers::producer::Producer;
use crate::video_encoder::VideoEncoderBuilder;
use anyhow::Result;
use image::imageops::FilterType;
use image::ImageBuffer;
use image::ImageReader;
use image::Rgb;
use nokhwa::utils::FrameFormat;
use protobuf::Message;
use std::fs::read;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::{debug, error};

use tokio::sync::mpsc::{self, Sender};

use super::camera::CameraConfig;
use super::camera::CameraPacket;
use super::encoder_thread::encoder_thread;

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
    pub fn from_config(config: CameraConfig, user_id: String, quic_tx: Sender<Vec<u8>>) -> Self {
        let (cam_tx, cam_rx) = mpsc::channel(100);
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

    fn camera_thread(&self) -> Result<JoinHandle<()>> {
        let quit = self.quit.clone();
        let cam_tx = self.cam_tx.clone();
        let frame_format = FrameFormat::NV12;
        let interval = Duration::from_millis(1000 / self.config.framerate as u64);
        let mut frames = vec![];

        for i in 100..200 {
            let path = format!("images/sample_video_save/output_{}.jpg", i);
            let img = read(path).unwrap();
            let img = ImageReader::new(std::io::Cursor::new(img))
                .with_guessed_format()
                .unwrap();
            // Transform the image to NV12 format
            let img = img.decode().unwrap();
            // Resize the image to value in config
            let img = img.resize_exact(self.config.width, self.config.height, FilterType::Nearest);
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
