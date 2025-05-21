use std::{cell::RefCell, rc::Rc};

use js_sys::{Array, Function};
use serde::Serialize;
use wasm_bindgen_futures::JsFuture;
use web_sys::{AudioContext, AudioWorkletNode, AudioWorkletNodeOptions, MessagePort};

use wasm_bindgen::prelude::*;

use gloo_utils::format::JsValueSerdeExt;

#[derive(Serialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct EncoderInitOptions {
    // Sample rate of input
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_sample_rate: Option<u32>,

    // 2048 = Voice (Lower fidelity)
    // 2049 = Full Band Audio (Highest fidelity)
    // 2051 = Restricted Low Delay (Lowest latency)
    // Default: 2049
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_application: Option<u32>,

    // Specified in ms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_frame_size: Option<u32>,

    // Desired encoding sample rate. Audio will be resampled
    // Default: 48000
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_sample_rate: Option<u32>,

    // Tradeoff latency with overhead. Default: 40
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_frames_per_page: Option<u32>,

    // Default: 1
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number_of_channels: Option<u32>,

    // Value between 0 and 10 inclusive. 10 being highest
    // quality. Default: 10
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resample_quality: Option<u32>,

    // Default: 50000
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_bit_rate: Option<u32>,
}

#[derive(Serialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct DecoderInitOptions {
    // Desired decoder sample rate.
    // Default: 48000
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoder_sample_rate: Option<u32>,

    // Desired output sample rate. Audio will be resampled
    // Default: 48000
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_buffer_sample_rate: Option<u32>,

    // Value between 0 and 10 inclusive. 10 being highest quality.
    // Default: 0
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resample_quality: Option<u32>,

    // Default: 1
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number_of_channels: Option<u32>,
}

#[derive(Serialize, Debug)]
#[serde(tag = "command", rename_all = "camelCase")]
pub enum CodecMessages<Options: Serialize> {
    Init {
        #[serde(flatten)]
        options: Option<Options>,
    },
    Start,
    Stop,
    Flush,
    Close,
    Done,
    Decode {
        pages: Vec<u8>,
    },
}

pub type EncoderMessages = CodecMessages<EncoderInitOptions>;
pub type DecoderMessages = CodecMessages<DecoderInitOptions>;

/// Struct to describe a Audio Encoder or Decoder through the use of an AudioWorklet
#[derive(Clone, Default)]
pub struct AudioWorkletCodec {
    inner: Rc<RefCell<Option<AudioWorkletNode>>>,
}

impl AudioWorkletCodec {
    /// Instantiates a AudioWorkletNode under the provided context using the provided script name
    /// and registered processor name
    pub async fn create_node(
        &self,
        context: &AudioContext,
        script_path: &str,
        name: &str,
        channels: u32,
    ) -> Result<AudioWorkletNode, JsValue> {
        let _ = JsFuture::from(context.audio_worklet()?.add_module(script_path)?).await;

        let mut options = AudioWorkletNodeOptions::new();
        options.number_of_inputs(channels);
        options.number_of_outputs(channels);
        options.output_channel_count(
            &vec![channels]
                .into_iter()
                .map(JsValue::from)
                .collect::<Array>(),
        );

        let node = AudioWorkletNode::new_with_options(context, name, &options)?;
        let _ = self.inner.replace(Some(node));
        Ok(self.inner.borrow().to_owned().unwrap())
    }

    pub fn is_instantiated(&self) -> bool {
        self.inner.borrow().is_some()
    }

    pub fn start(&self) -> Result<(), JsValue> {
        self.send_message(EncoderMessages::Start)
    }

    pub fn stop(&self) -> Result<(), JsValue> {
        self.send_message(EncoderMessages::Stop)
    }

    pub fn destroy(&self) {
        let _ = self.send_message(EncoderMessages::Close);
        let _ = self.send_message(EncoderMessages::Flush);
        let _ = self.send_message(EncoderMessages::Done);
        let _ = self.inner.borrow_mut().take();
    }

    pub fn set_onmessage(&self, handler: &Function) {
        if let Some(port) = self.get_port() {
            port.set_onmessage(Some(&handler));
        }
    }

    pub fn send_message<T: Serialize>(&self, message: T) -> Result<(), JsValue> {
        self.get_port()
            .ok_or(JsValue::from_str("AudioWorkletNode is not instantiated"))
            .and_then(|port| {
                JsValue::from_serde(&message)
                    .map_err(|e| JsValue::from_str(&e.to_string()))
                    .and_then(|val| port.post_message(&val))
            })
    }

    fn get_port(&self) -> Option<MessagePort> {
        self.inner
            .borrow()
            .as_ref()
            .and_then(|node| node.port().ok())
    }
}
