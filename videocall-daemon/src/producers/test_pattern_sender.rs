use crate::producers::camera::since_the_epoch;
use crate::producers::camera::transform_video_chunk;
use crate::producers::camera::THRESHOLD_MILLIS;
use crate::producers::producer::Producer;
use crate::cli_args::IndexKind;
use crate::video_encoder::Frame;
use crate::video_encoder::VideoEncoderBuilder;
use anyhow::Result;
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

use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{MediaPacket, VideoMetadata};
use videocall_types::protos::packet_wrapper::{packet_wrapper::PacketType, PacketWrapper};
use tokio::sync::mpsc::{self, Sender};

use super::camera::CameraConfig;
use super::camera::CameraPacket;



const TARGET_FPS: u64 = 15;

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
    pub fn from_config(_config: CameraConfig, user_id: String, quic_tx:    Sender<Vec<u8>>) -> Self {
        let (cam_tx, cam_rx) = mpsc::channel(100);
        // rewrite res to 1280 × 680
        let config = CameraConfig {
            width: 1280,
            height: 680,
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
            video_encoder.update_bitrate(100_000).unwrap();
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
        // Read from file src/producers/chichen_itza.nv12
        let frame = read("src/producers/chichen_itza.nv12").unwrap(); 
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
                if let Err(e) = cam_tx.try_send(Some(CameraPacket::new(
                    frame.clone(),
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

