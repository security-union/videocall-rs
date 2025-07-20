/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use yew::prelude::*;
use web_sys::HtmlAudioElement;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::AudioContext;
use web_sys::AnalyserNode;
use web_sys::AudioContextState;
use web_sys::MediaStreamAudioSourceNode;
use wasm_bindgen_futures::JsFuture;
use std::collections::VecDeque;

#[derive(Properties, PartialEq)]
pub struct AudioDiagnosticsProps {
    pub stream: Option<web_sys::MediaStream>,
    pub is_local: bool,
    pub peer_id: Option<String>,
}

pub enum AudioDiagnosticsMsg {
    UpdateMeter(f32),
    UpdateStats(AudioStats),
    Error(String),
}

#[derive(Default, Debug, Clone)]
pub struct AudioStats {
    pub volume: f32,
    pub sample_rate: f32,
    pub packet_loss: f32,
    pub jitter: f32,
    pub latency: f32,
    pub codec: String,
}

pub struct AudioDiagnostics {
    audio_ctx: Option<AudioContext>,
    analyser: Option<AnalyserNode>,
    source: Option<MediaStreamAudioSourceNode>,
    stats: AudioStats,
    volume_history: VecDeque<f32>,
    animation_id: Option<i32>,
}

impl Component for AudioDiagnostics {
    type Message = AudioDiagnosticsMsg;
    type Properties = AudioDiagnosticsProps;

    fn create(_ctx: &Context<Self>) -> Self {
        Self {
            audio_ctx: None,
            analyser: None,
            source: None,
            stats: AudioStats::default(),
            volume_history: VecDeque::with_capacity(60), // Store last 60 readings (1 second at 60fps)
            animation_id: None,
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            AudioDiagnosticsMsg::UpdateMeter(volume) => {
                self.volume_history.push_back(volume);
                if self.volume_history.len() > 60 {
                    self.volume_history.pop_front();
                }
                true
            }
            AudioDiagnosticsMsg::UpdateStats(stats) => {
                self.stats = stats;
                true
            }
            AudioDiagnosticsMsg::Error(_) => {
                // Handle error
                false
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let avg_volume = self.volume_history.iter().sum::<f32>() / self.volume_history.len().max(1) as f32;
        
        html! {
            <div class="audio-diagnostics">
                <h3>{
                    if ctx.props().is_local {
                        "Local Audio"
                    } else {
                        format!("Audio: {}", ctx.props().peer_id.as_deref().unwrap_or("Unknown"))
                    }
                }</h3>
                <div class="audio-meter">
                    <div class="audio-meter-bar" style={format!("width: {}%; height: 20px; background-color: {}", 
                        (avg_volume * 100.0).min(100.0),
                        if avg_volume > 0.8 { "#ef4444" } 
                        else if avg_volume > 0.5 { "#f59e0b" } 
                        else { "#10b981" })}
                    ></div>
                </div>
                <div class="audio-stats">
                    <div class="stat">
                        <span class="stat-label">{"Volume:"}</span>
                        <span class="stat-value">{
                            format!("{:.1}%", avg_volume * 100.0)
                        }</span>
                    </div>
                    <div class="stat">
                        <span class="stat-label">{"Packet Loss:"}</span>
                        <span class="stat-value">{
                            format!("{:.1}%", self.stats.packet_loss * 100.0)
                        }</span>
                    </div>
                    <div class="stat">
                        <span class="stat-label">{"Jitter:"}</span>
                        <span class="stat-value">{
                            format!("{:.1}ms", self.stats.jitter)
                        }</span>
                    </div>
                    <div class="stat">
                        <span class="stat-label">{"Latency:"}</span>
                        <span class="stat-value">{
                            format!("{:.1}ms", self.stats.latency)
                        }</span>
                    </div>
                    <div class="stat">
                        <span class="stat-label">{"Codec:"}</span>
                        <span class="stat-value">{
                            &self.stats.codec
                        }</span>
                    </div>
                </div>
            </div>
        }
    }

    fn rendered(&mut self, ctx: &Context<Self>, first_render: bool) {
        if first_render && ctx.props().stream.is_some() {
            self.setup_audio_analysis(ctx);
        }
    }

    fn destroy(&mut self, _ctx: &Context<Self>) {
        if let Some(id) = self.animation_id.take() {
            let _ = web_sys::window()
                .unwrap()
                .cancel_animation_frame(id);
        }
        if let Some(ctx) = self.audio_ctx.take() {
            let _ = ctx.close();
        }
    }
}

impl AudioDiagnostics {
    fn setup_audio_analysis(&mut self, ctx: &Context<Self>) {
        let window = web_sys::window().expect("no global `window` exists");
        let document = window.document().expect("should have a document on window");
        
        // Create audio context
        let audio_ctx = web_sys::AudioContext::new().expect("could not create audio context");
        let analyser = audio_ctx
            .create_analyser()
            .expect("could not create analyser");
            
        analyser.set_fft_size(256);
        
        // Connect audio source
        if let Some(stream) = &ctx.props().stream {
            let source = audio_ctx
                .create_media_stream_source(stream)
                .expect("could not create media stream source");
                
            source.connect_with_audio_node(&analyser)
                .expect("could not connect source to analyser");
                
            self.source = Some(source);
        }
        
        self.audio_ctx = Some(audio_ctx);
        self.analyser = Some(analyser);
        
        // Start animation loop
        let link = ctx.link().clone();
        let closure = Closure::wrap(Box::new(move || {
            if let Some(analyser) = &self.analyser {
                let mut data_array = vec![0u8; analyser.frequency_bin_count() as usize];
                analyser.get_byte_frequency_data(&mut data_array);
                
                // Calculate average volume
                let sum: u32 = data_array.iter().map(|&x| x as u32).sum();
                let avg = sum as f32 / data_array.len() as f32;
                let normalized = (avg / 255.0).powf(2.0); // Square to make it more visible
                
                link.send_message(AudioDiagnosticsMsg::UpdateMeter(normalized));
            }
            
            // Schedule next frame
            let link = link.clone();
            let id = window.request_animation_frame(
                wasm_bindgen::prelude::Closure::once_into_js(move || {
                    link.send_message(AudioDiagnosticsMsg::UpdateMeter(0.0));
                })
                .unchecked_into()
            );
            
            self.animation_id = Some(id);
        }) as Box<dyn FnMut()>);
        
        // Start the animation loop
        window.request_animation_frame(closure.as_ref().unchecked_ref());
        std::mem::forget(closure);
    }
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console)]
    fn log(s: &str);
}
