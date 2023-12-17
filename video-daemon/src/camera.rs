use crate::video_encoder::Frame;
use crate::video_encoder::VideoEncoderBuilder;
use crate::yuyv_format::YuyvFormat;
use anyhow::{Result};
use nokhwa::utils::RequestedFormat;
use nokhwa::utils::RequestedFormatType;
use nokhwa::Buffer;
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
use tracing::{debug, error, info, warn};

use types::protos::media_packet::media_packet::MediaType;
use types::protos::media_packet::{MediaPacket, VideoMetadata};
use types::protos::packet_wrapper::{packet_wrapper::PacketType, PacketWrapper};

type CameraPacket = (Buffer, u128);

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

#[derive(Copy, Clone, Debug)]
pub struct CameraConfig {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub video_device_index: usize,
    pub frame_format: FrameFormat,
}

pub struct CameraDaemon {
    config: CameraConfig,
    user_id: String,
    fps_rx: Option<mpsc::Receiver<u128>>,
    fps_tx: Arc<mpsc::Sender<u128>>,
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
        let (fps_tx, fps_rx) = mpsc::channel(5);
        let (cam_tx, cam_rx) = mpsc::channel(100);
        CameraDaemon {
            config,
            user_id,
            fps_rx: Some(fps_rx),
            fps_tx: Arc::new(fps_tx),
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
        let fps = self.fps_thread();
        self.handles.push(fps);
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
        let video_device_index = self.config.video_device_index as u32;
        let quit = self.quit.clone();
        Ok(std::thread::spawn(move || {
            info!("Camera opened... waiting for frames");
            let mut camera = Camera::new(
                CameraIndex::Index(video_device_index),
                RequestedFormat::new::<YuyvFormat>(RequestedFormatType::Closest(
                    CameraFormat::new_from(width, height, frame_format, framerate),
                )),
            )
            .unwrap();
            camera.open_stream().unwrap();
            while let Ok(frame) = camera.frame() {
                if quit.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                if let Err(e) = cam_tx.try_send(Some((frame, since_the_epoch().as_millis()))) {
                    error!("error sending image {}", e);
                }
            }
        }))
    }

    fn encoder_thread(&mut self) -> JoinHandle<()> {
        let fps_tx = self.fps_tx.clone();
        let mut cam_rx = self.cam_rx.take().unwrap();
        let quic_tx = self.quic_tx.clone();
        let quit = self.quit.clone();
        let width = self.config.width;
        let height = self.config.height;
        let user_id = self.user_id.clone();
        std::thread::spawn(move || {
            let start = Instant::now();
            let mut video_encoder = VideoEncoderBuilder::default()
                .set_resolution(width, height)
                .build()
                .unwrap();
            video_encoder.update_bitrate(50_000).unwrap();
            while let Some(data) = cam_rx.blocking_recv() {
                if quit.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let (image, age) = data.unwrap();

                // transform image to 420 format
                let image = convert_yuyv_to_i420(image.buffer(), width as usize, height as usize);

                // If age older than threshold, throw it away.
                let image_age = since_the_epoch().as_millis() - age;
                if image_age > THRESHOLD_MILLIS {
                    debug!("throwing away old image with age {} ms", image_age);
                    continue;
                }
                let time = start.elapsed();
                let encoding_time = Instant::now();
                let frames = video_encoder
                    .encode(
                        (time.as_millis() + time.subsec_millis() as u128) as i64,
                        &image,
                    )
                    .unwrap();
                debug!("encoding took {:?}", encoding_time.elapsed());
                for frame in frames {
                    let packet_wrapper = transform_video_chunk(&frame, &user_id);
                    if let Err(e) = quic_tx.try_send(packet_wrapper.write_to_bytes().unwrap()) {
                        error!("Unable to send packet: {:?}", e);
                    } else if let Err(e) = fps_tx.try_send(since_the_epoch().as_millis()) {
                        error!("Unable to send fps: {:?}", e);
                    }
                }
            }
        })
    }

    fn fps_thread(&mut self) -> JoinHandle<()> {
        let mut fps_rx = self.fps_rx.take().unwrap();
        let quit = self.quit.clone();
        std::thread::spawn(move || {
            let mut num_frames = 0;
            let mut now_plus_1 = since_the_epoch().as_millis() + 1000;
            warn!("Starting fps loop");
            while let Some(dur) = fps_rx.blocking_recv() {
                if quit.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                if now_plus_1 < dur {
                    warn!("FPS: {:?}", num_frames);
                    num_frames = 0;
                    now_plus_1 = since_the_epoch().as_millis() + 1000;
                } else {
                    num_frames += 1;
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

fn convert_yuyv_to_i420(yuyv: &[u8], width: usize, height: usize) -> Vec<u8> {
    assert!(
        width % 2 == 0 && height % 2 == 0,
        "Width and height must be even numbers."
    );

    let mut i420 = vec![0u8; width * height + 2 * (width / 2) * (height / 2)];
    let (y_plane, uv_plane) = i420.split_at_mut(width * height);
    let (u_plane, v_plane) = uv_plane.split_at_mut(uv_plane.len() / 2);

    for y in 0..height {
        for x in (0..width).step_by(2) {
            let base_index = (y * width + x) * 2;
            let y0 = yuyv[base_index];
            let u = yuyv[base_index + 1];
            let y1 = yuyv[base_index + 2];
            let v = yuyv[base_index + 3];

            y_plane[y * width + x] = y0;
            y_plane[y * width + x + 1] = y1;

            if y % 2 == 0 {
                u_plane[y / 2 * (width / 2) + x / 2] = u;
                v_plane[y / 2 * (width / 2) + x / 2] = v;
            }
        }
    }

    i420
}
