use futures::future::FutureExt;
use js_sys::{Function, JsString, Object, Promise, Reflect};
use std::future::Future;
use std::pin::Pin;
use wasm_bindgen::{prelude::*, JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AudioContext, AudioData, HtmlVideoElement, MediaStream, MediaStreamTrack, ReadableStream,
    ReadableStreamDefaultReader, VideoFrame,
};

use crate::media_processor::{MediaFrame, MediaFrameReader};

// JavaScript polyfill to be injected into the page
const POLYFILL_JS: &str = r#"
class VideoTrackPolyfill {
  constructor(track) {
    this.track = track;
    this.readable = new ReadableStream({
      async start(controller) {
        this.video = document.createElement("video");
        this.video.srcObject = new MediaStream([track]);
        this.video.muted = true;
        await Promise.all([
          this.video.play(),
          new Promise(r => this.video.onloadedmetadata = r)
        ]);
        this.canvas = new OffscreenCanvas(
          this.video.videoWidth || 640,
          this.video.videoHeight || 480
        );
        this.ctx = this.canvas.getContext('2d', {desynchronized: true});
        this.t1 = performance.now();
        this.frameRate = track.getSettings().frameRate || 30;
      },
      async pull(controller) {
        while (performance.now() - this.t1 < 1000 / this.frameRate) {
          await new Promise(r => requestAnimationFrame(r));
        }
        this.t1 = performance.now();
        this.ctx.drawImage(this.video, 0, 0);
        controller.enqueue(new VideoFrame(this.canvas, {timestamp: this.t1}));
      },
      cancel() {
        if (this.video) {
          this.video.srcObject = null;
        }
      }
    });
  }
}

class AudioTrackPolyfill {
  constructor(track) {
    this.track = track;
    this.readable = new ReadableStream({
      async start(controller) {
        this.ac = new AudioContext();
        this.arrays = [];
        
        const worklet = () => {
          registerProcessor("mstp-shim", class Processor extends AudioWorkletProcessor {
            process(input) {
              this.port.postMessage(input);
              return true;
            }
          });
        };
        
        const workletUrl = URL.createObjectURL(new Blob(
          [`(${worklet.toString()})()`],
          {type: 'text/javascript'}
        ));
        
        await this.ac.audioWorklet.addModule(workletUrl);
        this.node = new AudioWorkletNode(this.ac, "mstp-shim");
        this.stream = new MediaStream([track]);
        this.source = this.ac.createMediaStreamSource(this.stream);
        this.source.connect(this.node);
        
        this.node.port.addEventListener("message", ({data}) => {
          if (data && data[0] && data[0][0]) {
            this.arrays.push(data);
          }
        });
        this.node.port.start();
      },
      async pull(controller) {
        while (!this.arrays.length) {
          await new Promise(r => setTimeout(r, 10));
        }
        
        const [channels] = this.arrays.shift();
        const joined = new Float32Array(channels.reduce((a, b) => a + b.length, 0));
        channels.reduce((offset, a) => {
          joined.set(a, offset);
          return offset + a.length;
        }, 0);
        
        controller.enqueue(new AudioData({
          format: "f32-planar",
          sampleRate: this.ac.sampleRate,
          numberOfFrames: channels[0].length,
          numberOfChannels: channels.length,
          timestamp: this.ac.currentTime * 1e6 | 0,
          data: joined,
        }));
      },
      cancel() {
        if (this.source) {
          this.source.disconnect();
        }
        if (this.ac) {
          this.ac.close();
        }
      }
    });
  }
}

window.__videoCallPolyfillRegistry = window.__videoCallPolyfillRegistry || {};

window.__videoCallPolyfill = {
  createProcessor(id, trackKind, track) {
    try {
      const processor = trackKind === 'video' 
        ? new VideoTrackPolyfill(track)
        : new AudioTrackPolyfill(track);
      
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
        let id = format!("polyfill_{}", js_sys::Math::random().to_string());

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
    let id = format!("polyfill_{}", js_sys::Math::random().to_string());

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
