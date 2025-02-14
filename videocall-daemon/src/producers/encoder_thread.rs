use std::{
    sync::{atomic::AtomicBool, Arc},
    thread::JoinHandle,
    time::Instant,
};

use crate::{
    producers::camera::{since_the_epoch, transform_video_chunk, CameraPacket, THRESHOLD_MILLIS},
    video_encoder::VideoEncoderBuilder,
};
use protobuf::Message;
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{debug, error};

use super::camera::CameraConfig;

pub fn encoder_thread(
    mut cam_rx: Receiver<Option<CameraPacket>>,
    quic_tx: Arc<Sender<Vec<u8>>>,
    quit: Arc<AtomicBool>,
    camera_config: CameraConfig,
    user_id: String,
) -> JoinHandle<()> {
    let width = camera_config.width;
    let height = camera_config.height;
    std::thread::spawn(move || {
        let mut video_encoder = VideoEncoderBuilder::new(camera_config.framerate)
            .set_resolution(width, height)
            .build()
            .unwrap();
        video_encoder.update_bitrate_kbps(1_000).unwrap();
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
