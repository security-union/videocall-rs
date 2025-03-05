use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    AudioContext, HtmlCanvasElement, HtmlVideoElement, MediaStream, MediaStreamTrack, 
    ReadableStream, ReadableStreamDefaultReader, VideoFrame, AudioData
};
use js_sys::{Object, Reflect, Function, Promise, Array, Float32Array};
use gloo_utils::window;
use std::rc::Rc;
use std::cell::RefCell;

/// A custom implementation of MediaStreamTrackProcessor that processes MediaStreamTrack objects
/// and produces a ReadableStream of frames.
/// 
/// This implementation uses JavaScript eval to create the equivalent functionality of the
/// MediaStreamTrackProcessor polyfill.
#[wasm_bindgen]
pub struct CustomMediaStreamTrackProcessor {
    track: MediaStreamTrack,
    readable: ReadableStream,
}

/// Initialization object for CustomMediaStreamTrackProcessor
#[wasm_bindgen]
pub struct CustomMediaStreamTrackProcessorInit {
    track: MediaStreamTrack,
}

#[wasm_bindgen]
impl CustomMediaStreamTrackProcessorInit {
    /// Create a new CustomMediaStreamTrackProcessorInit with the given track
    #[wasm_bindgen(constructor)]
    pub fn new(track: &MediaStreamTrack) -> Self {
        Self {
            track: track.clone(),
        }
    }
}

// Declare the eval function for calling JavaScript directly
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = window, js_name = eval)]
    fn eval_js(js: &str) -> JsValue;
}

#[wasm_bindgen]
impl CustomMediaStreamTrackProcessor {
    /// Create a new CustomMediaStreamTrackProcessor from an initialization object
    #[wasm_bindgen(constructor)]
    pub fn new(init: &CustomMediaStreamTrackProcessorInit) -> Result<CustomMediaStreamTrackProcessor, JsValue> {
        let track = init.track.clone();
        
        // Track kind for branching logic
        let kind = track.kind();
        
        let js_code = format!(r#"
        (function() {{
            // Safe reference to the track
            const track = document.createMediaStreamTrackProcessor_track;
            
            // Create and return a new ReadableStream with proper constructor
            const readable = new ReadableStream({{
                async start(controller) {{
                    try {{
                        if ("{kind}" === "video") {{
                            this.video = document.createElement("video");
                            this.video.srcObject = new MediaStream([track]);
                            this.video.autoplay = true;
                            this.video.muted = true;
                            this.video.playsInline = true;
                            
                            // Wait for video to load
                            await new Promise(resolve => {{
                                this.video.onloadedmetadata = () => resolve();
                                // Fallback if metadata never fires
                                setTimeout(resolve, 1000);
                            }});
                            
                            try {{
                                await this.video.play();
                            }} catch (e) {{
                                console.warn("Video play failed, continuing anyway:", e);
                            }}
                            
                            // Setup canvas for frame capture
                            this.canvas = document.createElement("canvas");
                            this.canvas.width = this.video.videoWidth || 640;
                            this.canvas.height = this.video.videoHeight || 480;
                            this.ctx = this.canvas.getContext('2d', {{desynchronized: true}});
                            this.t1 = performance.now();
                        }} else if ("{kind}" === "audio") {{
                            try {{
                                // Setup audio processing
                                const AudioContextClass = window.AudioContext || window.webkitAudioContext;
                                if (AudioContextClass) {{
                                    this.ac = new AudioContextClass({{sampleRate: 48000}});
                                    this.mediaStream = new MediaStream([track]);
                                    this.source = this.ac.createMediaStreamSource(this.mediaStream);
                                    
                                    // Create a processor node for audio data
                                    this.processor = this.ac.createScriptProcessor(1024, 1, 1);
                                    this.source.connect(this.processor);
                                    this.processor.connect(this.ac.destination);
                                    
                                    // Buffer to store audio data
                                    this.buffer = [];
                                    
                                    // Setup audio processor
                                    this.processor.onaudioprocess = (e) => {{
                                        const inputData = e.inputBuffer.getChannelData(0);
                                        // Store a copy of the data
                                        this.buffer.push(new Float32Array(inputData));
                                        // Keep buffer to a reasonable size
                                        if (this.buffer.length > 10) {{
                                            this.buffer.shift();
                                        }}
                                    }};
                                }}
                            }} catch(e) {{
                                console.error("Audio context creation failed:", e);
                            }}
                            this.t1 = performance.now();
                        }} else {{
                            throw new Error("Unsupported track kind: " + "{kind}");
                        }}
                    }} catch (e) {{
                        console.error("Error in stream start:", e);
                        controller.error(e);
                    }}
                }},
                async pull(controller) {{
                    try {{
                        if ("{kind}" === "video") {{
                            if (!this.video || !this.ctx) {{
                                throw new Error("Video or canvas context not available");
                            }}
                            
                            const frameRate = (track.getSettings && track.getSettings().frameRate) || 30;
                            // Wait until it's time for the next frame
                            while (performance.now() - this.t1 < 1000 / frameRate) {{
                                await new Promise(r => setTimeout(r, 5));
                            }}
                            
                            this.t1 = performance.now();
                            
                            try {{
                                // Draw the current video frame to the canvas
                                this.ctx.drawImage(this.video, 0, 0);
                                
                                // Create and enqueue a frame
                                if (typeof VideoFrame !== 'undefined') {{
                                    // If VideoFrame API is available
                                    const videoFrame = new VideoFrame(this.canvas, {{timestamp: this.t1}});
                                    controller.enqueue(videoFrame);
                                }} else {{
                                    // Fallback if VideoFrame API is not available
                                    const imageData = this.ctx.getImageData(0, 0, this.canvas.width, this.canvas.height);
                                    controller.enqueue({{
                                        type: 'video',
                                        timestamp: this.t1,
                                        width: this.canvas.width,
                                        height: this.canvas.height,
                                        data: imageData.data
                                    }});
                                }}
                            }} catch (drawError) {{
                                console.error("Error drawing video:", drawError);
                                // Return a blank frame to avoid breaking the stream
                                controller.enqueue({{
                                    type: 'video',
                                    timestamp: this.t1,
                                    width: this.canvas.width || 640,
                                    height: this.canvas.height || 480,
                                    data: new Uint8Array((this.canvas.width || 640) * (this.canvas.height || 480) * 4)
                                }});
                            }}
                        }} else if ("{kind}" === "audio") {{
                            // Create proper AudioData
                            try {{
                                if (typeof AudioData !== 'undefined' && this.buffer && this.buffer.length > 0) {{
                                    // Get the latest audio data from our buffer
                                    const audioBuffer = this.buffer.shift();
                                    if (audioBuffer) {{
                                        // Create proper AudioData object
                                        const numChannels = 1; // Mono audio
                                        const sampleRate = this.ac ? this.ac.sampleRate : 48000;
                                        const format = "f32-planar"; // Float32 planar format
                                        
                                        // AudioData constructor parameters
                                        const audioDataInit = {{
                                            format: format,
                                            sampleRate: sampleRate,
                                            numberOfFrames: audioBuffer.length,
                                            numberOfChannels: numChannels,
                                            timestamp: this.t1,
                                            data: audioBuffer
                                        }};
                                        
                                        // Create AudioData object
                                        const audioData = new AudioData(audioDataInit);
                                        controller.enqueue(audioData);
                                    }} else {{
                                        // Wait for audio data to be available
                                        await new Promise(r => setTimeout(r, 10));
                                    }}
                                }} else {{
                                    // If AudioData API is not available, use a fallback
                                    // Create a simple fallback that matches the expected interface
                                    const sampleRate = this.ac ? this.ac.sampleRate : 48000;
                                    const numberOfFrames = 1024;
                                    const numberOfChannels = 1;
                                    
                                    // Create a Float32Array filled with zeros as placeholder audio
                                    const audioData = new Float32Array(numberOfFrames);
                                    
                                    // Compatibility version for browsers without AudioData
                                    const fallbackAudioData = {{
                                        format: "f32-planar",
                                        sampleRate: sampleRate,
                                        numberOfFrames: numberOfFrames,
                                        numberOfChannels: numberOfChannels,
                                        timestamp: this.t1,
                                        duration: numberOfFrames / sampleRate * 1000000,
                                        allocationSize: function() {{ return numberOfFrames * 4; }},
                                        copyTo: function() {{ /* No-op */ }},
                                        clone: function() {{ return this; }},
                                        close: function() {{ /* No-op */ }},
                                        data: audioData
                                    }};
                                    
                                    // Send it to the controller
                                    controller.enqueue(fallbackAudioData);
                                    
                                    // Wait before next pull
                                    await new Promise(r => setTimeout(r, 10));
                                }}
                            }} catch (audioError) {{
                                console.error("Error processing audio:", audioError);
                                // Wait before retry
                                await new Promise(r => setTimeout(r, 100));
                            }}
                        }}
                    }} catch (e) {{
                        console.error("Error in stream pull:", e);
                        controller.error(e);
                    }}
                }}
            }});
            
            return readable;
        }})()
        "#);
        
        // Store the track in a global variable to ensure it's accessible to our JS code
        let window = web_sys::window().ok_or_else(|| JsValue::from_str("No window found"))?;
        let document = window.document().ok_or_else(|| JsValue::from_str("No document found"))?;
        
        // Store the track in a property on the document (safer than global)
        js_sys::Reflect::set(
            &document,
            &JsValue::from_str("createMediaStreamTrackProcessor_track"),
            &track,
        )?;
        
        // Evaluate the JavaScript code
        let readable_js = eval_js(&js_code);
        let readable = readable_js.dyn_into::<ReadableStream>()?;
        
        // Clean up the global reference
        js_sys::Reflect::set(
            &document,
            &JsValue::from_str("createMediaStreamTrackProcessor_track"),
            &JsValue::NULL,
        )?;
        
        Ok(CustomMediaStreamTrackProcessor {
            track,
            readable,
        })
    }
    
    /// Gets the ReadableStream for this processor
    pub fn readable(&self) -> ReadableStream {
        self.readable.clone()
    }
} 