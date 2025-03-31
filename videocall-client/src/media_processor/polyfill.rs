use js_sys::{Function, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{MediaStreamTrack, ReadableStream, ReadableStreamDefaultReader};

use crate::media_processor::MediaFrameReader;

// JavaScript polyfill to be injected into the page
const POLYFILL_JS: &str = r#"
if (!self.MediaStreamTrackProcessor) {
  self.MediaStreamTrackProcessor = class MediaStreamTrackProcessor {
    constructor({track}) {
      if (track.kind == "video") {
        this.readable = new ReadableStream({
          async start(controller) {
            this.video = document.createElement("video");
            this.video.srcObject = new MediaStream([track]);
            await Promise.all([this.video.play(), new Promise(r => this.video.onloadedmetadata = r)]);
            this.track = track;
            this.canvas = new OffscreenCanvas(this.video.videoWidth, this.video.videoHeight);
            this.ctx = this.canvas.getContext('2d', {desynchronized: true});
            this.t1 = performance.now();
          },
          async pull(controller) {
            while (performance.now() - this.t1 < 1000 / track.getSettings().frameRate) {
              await new Promise(r => requestAnimationFrame(r));
            }
            this.t1 = performance.now();
            this.ctx.drawImage(this.video, 0, 0);
            controller.enqueue(new VideoFrame(this.canvas, {timestamp: this.t1}));
          }
        });
      } else if (track.kind == "audio") {
        this.readable = new ReadableStream({
          async start(controller) {
            this.ac = new AudioContext;
            this.arrays = [];
            function worklet() {
              registerProcessor("mstp-shim", class Processor extends AudioWorkletProcessor {
                  process(input) { this.port.postMessage(input); return true; }
              });
            }
            await this.ac.audioWorklet.addModule(`data:text/javascript,(${worklet.toString()})()`);
            this.node = new AudioWorkletNode(this.ac, "mstp-shim");
            this.ac.createMediaStreamSource(new MediaStream([track])).connect(this.node);
            this.node.port.addEventListener("message", ({data}) => data[0][0] && this.arrays.push(data));
          },
          async pull(controller) {
            while (!this.arrays.length) await new Promise(r => this.node.port.onmessage = r);
            const [channels] = this.arrays.shift();
            const joined = new Float32Array(channels.reduce((a, b) => a + b.length, 0));
            channels.reduce((offset, a) => (joined.set(a, offset), offset + a.length), 0);
            controller.enqueue(new AudioData({
              format: "f32-planar",
              sampleRate: this.ac.sampleRate,
              numberOfFrames: channels[0].length,
              numberOfChannels: channels.length,
              timestamp: this.ac.currentTime * 1e6 | 0,
              data: joined,
              transfer: [joined.buffer]
            }));
          }
        });
      }
    }
  };
}

window.__videoCallPolyfillRegistry = window.__videoCallPolyfillRegistry || {};

window.__videoCallPolyfill = {
  createProcessor(id, trackKind, track) {
    try {
      // Create processor with track
      const processor = new MediaStreamTrackProcessor({track});
      window.__videoCallPolyfillRegistry[id] = processor;
      return true;
    } catch (e) {
      console.error("Polyfill error:", e);
      return false;
    }
  },
  
  destroyProcessor(id) {
    const processor = window.__videoCallPolyfillRegistry[id];
    if (processor) {
      // Clean up resources
      if (processor.ac) {
        processor.ac.close();
      }
      if (processor.video) {
        processor.video.srcObject = null;
      }
      delete window.__videoCallPolyfillRegistry[id];
      return true;
    }
    return false;
  }
};
"#;

/// Implementation of MediaFrameReader using a JavaScript polyfill for browsers
/// that don't support MediaStreamTrackProcessor natively
pub struct PolyfillMediaFrameReader {
    id: String,
    reader: ReadableStreamDefaultReader,
    track_kind: String,
}

impl PolyfillMediaFrameReader {
    pub fn new(track: &MediaStreamTrack) -> Result<Self, JsValue> {
        // Initialize the polyfill if it hasn't been already
        Self::ensure_polyfill_initialized()?;

        let track_kind = track.kind();
        let id = format!("polyfill_{}", js_sys::Math::random());

        // Create the polyfill processor
        let window = web_sys::window().expect("no global window exists");
        let polyfill = js_sys::Reflect::get(&window, &JsValue::from_str("__videoCallPolyfill"))?;
        let create_fn = Reflect::get(&polyfill, &JsValue::from_str("createProcessor"))?
            .dyn_into::<Function>()?;

        let success = create_fn
            .call3(
                &JsValue::NULL,
                &JsValue::from_str(&id),
                &JsValue::from_str(&track_kind),
                track,
            )?
            .as_bool()
            .unwrap_or(false);

        if !success {
            return Err(JsValue::from_str("Failed to create polyfill processor"));
        }

        // Get the registry and processor
        let registry =
            js_sys::Reflect::get(&window, &JsValue::from_str("__videoCallPolyfillRegistry"))?;
        let processor = Reflect::get(&registry, &JsValue::from_str(&id))?;

        // Get the readable stream
        let readable = Reflect::get(&processor, &JsValue::from_str("readable"))?
            .dyn_into::<ReadableStream>()?;
        let reader = readable
            .get_reader()
            .unchecked_into::<ReadableStreamDefaultReader>();

        Ok(Self {
            id,
            reader,
            track_kind,
        })
    }

    fn ensure_polyfill_initialized() -> Result<(), JsValue> {
        let window = web_sys::window().expect("no global window exists");
        let has_polyfill =
            js_sys::Reflect::has(&window, &JsValue::from_str("__videoCallPolyfill"))?;

        if !has_polyfill {
            // Add additional code to ensure cleanup of polyfill instances
            js_sys::eval(
                r#"
                // Track active polyfill instances for cleanup
                window.__videoCallPolyfillActiveIds = window.__videoCallPolyfillActiveIds || new Set();
                
                // Add cleanup on page unload to prevent memory leaks
                if (!window.__videoCallPolyfillCleanupRegistered) {
                    window.__videoCallPolyfillCleanupRegistered = true;
                    window.addEventListener('beforeunload', () => {
                        if (window.__videoCallPolyfillRegistry && window.__videoCallPolyfillActiveIds) {
                            for (const id of window.__videoCallPolyfillActiveIds) {
                                const processor = window.__videoCallPolyfillRegistry[id];
                                if (processor && processor.readable) {
                                    // Force cancel on any pending readers
                                    try {
                                        if (processor.readable.locked) {
                                            processor.readable.cancel();
                                        }
                                    } catch (e) {}
                                    delete window.__videoCallPolyfillRegistry[id];
                                }
                            }
                            window.__videoCallPolyfillActiveIds.clear();
                        }
                    });
                }
                "#,
            )?;
            
            // Inject the polyfill
            let document = window.document().expect("no document exists");

            let script = document.create_element("script")?;
            script.set_text_content(Some(POLYFILL_JS));
            let head = document.head().expect("no head element");
            head.append_child(&script)?;
        }

        Ok(())
    }

    /// Get the track kind (audio or video)
    pub fn track_kind(&self) -> &str {
        &self.track_kind
    }
}

impl MediaFrameReader for PolyfillMediaFrameReader {
    fn read_frame(&self) -> JsValue {
        self.reader.read().into()
    }

    fn close(&self) -> Result<(), JsValue> {
        // We don't try to handle the Promise result, just initiate the cancel
        let _ = self.reader.cancel();

        // Clean up the polyfill processor
        let window = web_sys::window().expect("no global window exists");
        let polyfill = js_sys::Reflect::get(&window, &JsValue::from_str("__videoCallPolyfill"))?;
        let destroy_fn = Reflect::get(&polyfill, &JsValue::from_str("destroyProcessor"))?
            .dyn_into::<Function>()?;

        destroy_fn.call1(&JsValue::NULL, &JsValue::from_str(&self.id))?;
        
        // Remove this ID from the active set
        js_sys::eval(&format!(
            "window.__videoCallPolyfillActiveIds && window.__videoCallPolyfillActiveIds.delete('{}');",
            self.id
        ))?;

        Ok(())
    }

    fn track_kind(&self) -> &str {
        &self.track_kind
    }
}

/// Creates a polyfill processor for a MediaStreamTrack and returns its reader
pub fn create_processor(track: &MediaStreamTrack) -> Result<ReadableStreamDefaultReader, JsValue> {
    ensure_polyfill_initialized()?;

    let track_kind = track.kind();
    let id = format!("polyfill_{}", js_sys::Math::random());

    // Create the polyfill processor
    let window = web_sys::window().expect("no global window exists");
    let polyfill = js_sys::Reflect::get(&window, &JsValue::from_str("__videoCallPolyfill"))?;
    let create_fn =
        Reflect::get(&polyfill, &JsValue::from_str("createProcessor"))?.dyn_into::<Function>()?;

    let success = create_fn
        .call3(
            &JsValue::NULL,
            &JsValue::from_str(&id),
            &JsValue::from_str(&track_kind),
            track,
        )?
        .as_bool()
        .unwrap_or(false);

    if !success {
        return Err(JsValue::from_str("Failed to create polyfill processor"));
    }

    // Get the registry and processor
    let registry =
        js_sys::Reflect::get(&window, &JsValue::from_str("__videoCallPolyfillRegistry"))?;
    let processor = Reflect::get(&registry, &JsValue::from_str(&id))?;

    // Get the readable stream
    let readable =
        Reflect::get(&processor, &JsValue::from_str("readable"))?.dyn_into::<ReadableStream>()?;
    let reader = readable
        .get_reader()
        .unchecked_into::<ReadableStreamDefaultReader>();

    Ok(reader)
}

/// Ensures the polyfill is initialized
fn ensure_polyfill_initialized() -> Result<(), JsValue> {
    let window = web_sys::window().expect("no global window exists");
    let has_polyfill = js_sys::Reflect::has(&window, &JsValue::from_str("__videoCallPolyfill"))?;

    if !has_polyfill {
        // Inject the polyfill
        let document = window.document().expect("no document exists");

        let script = document.create_element("script")?;
        script.set_text_content(Some(POLYFILL_JS));
        let head = document.head().expect("no head element");
        head.append_child(&script)?;
    }

    Ok(())
}
