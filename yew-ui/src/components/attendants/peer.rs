use crate::model::AudioSampleFormatWrapper;
use crate::model::EncodedVideoChunkTypeWrapper;
use js_sys::*;
use types::protos::media_packet::media_packet;
use types::protos::media_packet::MediaPacket;
use web_sys::*;

pub struct Peer {
    pub video_decoder: VideoDecoder,
    pub audio_output: Box<dyn FnMut(AudioData)>,
    pub waiting_for_video_keyframe: bool,
    pub waiting_for_audio_keyframe: bool,
}

impl Peer {
    pub fn handle_media_packet(&mut self, packet: MediaPacket) {
        match packet.media_type.enum_value() {
            Ok(media_packet::MediaType::VIDEO) => {
                let video_data =
                    Uint8Array::new_with_length(packet.data.len().try_into().unwrap());
                let chunk_type = EncodedVideoChunkTypeWrapper::from(packet.frame_type).0;
                video_data.copy_from(&packet.data.into_boxed_slice());
                let mut video_chunk =
                    EncodedVideoChunkInit::new(&video_data, packet.timestamp, chunk_type);
                video_chunk.duration(packet.duration);
                let encoded_video_chunk = EncodedVideoChunk::new(&video_chunk).unwrap();
                if self.waiting_for_video_keyframe && chunk_type == EncodedVideoChunkType::Key
                    || !self.waiting_for_video_keyframe
                {
                    self.video_decoder.decode(&encoded_video_chunk);
                    self.waiting_for_video_keyframe = false;
                }
            }
            Ok(media_packet::MediaType::AUDIO) => {
                let audio_data = packet.data;
                let audio_data_js: js_sys::Uint8Array =
                    js_sys::Uint8Array::new_with_length(audio_data.len() as u32);
                audio_data_js.copy_from(&audio_data.as_slice());

                let audio_data = AudioData::new(&AudioDataInit::new(
                    &audio_data_js.into(),
                    AudioSampleFormatWrapper::from(
                        packet.audio_metadata.audio_format.clone(),
                    )
                    .0,
                    packet.audio_metadata.audio_number_of_channels,
                    packet.audio_metadata.audio_number_of_frames,
                    packet.audio_metadata.audio_sample_rate,
                    packet.timestamp,
                ))
                .unwrap();
                (self.audio_output)(audio_data);
            }
            // TODO: Handle unknown Media type
            Err(_) => (),
        }
    }
}
