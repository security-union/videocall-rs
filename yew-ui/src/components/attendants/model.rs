use std::collections::HashMap;

use crate::constants::ACTIX_WEBSOCKET;
use crate::constants::VIDEO_CODEC;
use crate::model::configure_audio_context;
use crate::model::transform_audio_chunk;
use crate::model::MediaPacketWrapper;
use anyhow::anyhow;
use gloo_console::log;
use protobuf::Message;
use types::protos::rust::media_packet::MediaPacket;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use yew::Callback;

use super::peer::Peer;
use web_sys::*;
use yew_websocket::websocket::{WebSocketService, WebSocketStatus, WebSocketTask};

type PeerMap = HashMap<String, Peer>;

#[derive(Clone, Copy)]
pub enum State {
    Created,
    Connecting,
    Connected,
    Disconnected,
}

pub struct Model {
    ws: Option<WebSocketTask>,
    state: State,
    connected_peers: PeerMap,
    outbound_audio_buffer: [u8; 2000],
}

pub struct ConnectArgs {
    pub callback: Callback<MediaPacketWrapper>,
    pub notification: Callback<WebSocketStatus>,
    pub meeting_id: String,
    pub email: String,
}

impl Model {
    pub fn new() -> Self {
        let connected_peers: PeerMap = HashMap::new();
        Self {
            ws: None,
            state: State::Created,
            connected_peers,
            outbound_audio_buffer: [0; 2000],
        }
    }

    pub fn connected_peers(&self) -> &PeerMap {
        &self.connected_peers
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn connect(&mut self, args: ConnectArgs) {
        if let State::Connected = self.state {
            return;
        }
        let url = format!(
            "{}/{}/{}",
            ACTIX_WEBSOCKET.to_string(),
            args.email,
            args.meeting_id
        );
        let task = WebSocketService::connect(&url, args.callback, args.notification).unwrap();
        self.ws = Some(task);
        self.state = State::Connecting;
    }

    pub fn disconnect(&mut self) {
        self.ws.take();
        self.state = State::Disconnected;
    }

    pub fn connection_succeed(&mut self) {
        if let State::Connecting = self.state {
            self.state = State::Connected;
        }
    }

    pub fn send_video_packet(&mut self, packet: MediaPacket) {
        if let Some(ws) = self.ws.as_mut() {
            let bytes = packet.write_to_bytes().map_err(|w| anyhow!("{:?}", w));
            ws.send_binary(bytes);
        }
    }

    pub fn send_audio_packet(&mut self, email: String, audio_frame: AudioData) {
        if let Some(ws) = self.ws.as_mut() {
            let mut buffer = self.outbound_audio_buffer;
            let packet = transform_audio_chunk(&audio_frame, &mut buffer, &email);
            let bytes = packet.write_to_bytes().map_err(|w| anyhow!("{:?}", w));
            ws.send_binary(bytes);
            audio_frame.close();
        }
    }

    pub fn peer_connected(&self, peer_email: &str) -> bool {
        self.connected_peers.contains_key(peer_email)
    }

    pub fn get_peer_mut(&mut self, peer_email: &str) -> Option<&mut Peer> {
        self.connected_peers.get_mut(peer_email)
    }

    pub fn register_peer(&mut self, packet: MediaPacket) {
        // TODO: This method is too long, most of the logic could be moved to the Peer
        let peer_email = packet.email.clone();
        let audio_output = {
            let audio_stream_generator =
                MediaStreamTrackGenerator::new(&MediaStreamTrackGeneratorInit::new(&"audio"))
                    .unwrap();
            // The audio context is used to reproduce audio.
            let _audio_context = configure_audio_context(&audio_stream_generator);
            Box::new(move |audio_data: AudioData| {
                let writable = audio_stream_generator.writable();
                if writable.locked() {
                    return;
                }
                writable.get_writer().map_or_else(
                    |e| log!("error", e),
                    |writer| {
                        wasm_bindgen_futures::spawn_local(async move {
                            JsFuture::from(writer.ready())
                                .await
                                .map_or((), |e| log!("write chunk error ", e));

                            JsFuture::from(writer.write_with_chunk(&audio_data))
                                .await
                                .map_or((), |e| log!("write chunk error ", e));
                            writer.release_lock();
                        });
                    },
                );
            }) as Box<dyn FnMut(AudioData)>
        };
        let error_video = Closure::wrap(Box::new(move |e: JsValue| {
            log!(&e);
        }) as Box<dyn FnMut(JsValue)>);
        let video_output = Closure::wrap(Box::new(move |original_chunk: JsValue| {
            let chunk = Box::new(original_chunk);
            let video_chunk = chunk.unchecked_into::<VideoFrame>();
            let width = video_chunk.coded_width();
            let height = video_chunk.coded_height();
            let render_canvas = window()
                .unwrap()
                .document()
                .unwrap()
                .get_element_by_id(&peer_email.clone())
                .unwrap()
                .unchecked_into::<HtmlCanvasElement>();
            render_canvas.set_width(width as u32);
            render_canvas.set_height(height as u32);
            let ctx = render_canvas
                .get_context("2d")
                .unwrap()
                .unwrap()
                .unchecked_into::<CanvasRenderingContext2d>();
            let video_chunk = video_chunk.unchecked_into::<HtmlImageElement>();
            if let Err(e) = ctx.draw_image_with_html_image_element(&video_chunk, 0.0, 0.0) {
                log!("error ", e);
            }
        }) as Box<dyn FnMut(JsValue)>);
        let video_decoder = VideoDecoder::new(&VideoDecoderInit::new(
            error_video.as_ref().unchecked_ref(),
            video_output.as_ref().unchecked_ref(),
        ))
        .unwrap();
        video_decoder.configure(&VideoDecoderConfig::new(&VIDEO_CODEC));

        self.connected_peers.insert(
            packet.email.clone(),
            Peer {
                video_decoder,
                audio_output,
                waiting_for_video_keyframe: true,
                waiting_for_audio_keyframe: true,
            },
        );
        video_output.forget();
        error_video.forget();
    }
}
