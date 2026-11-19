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

//! Microphone encode pipeline: a capture AudioWorklet feeding an Opus
//! encoder Web Worker.
//!
//! ```text
//! mic ─> AudioWorkletNode "mic-capture" ──MessageChannel──> audio_encoder_worker
//!        (micCaptureWorklet.js, copies PCM)                (resample, 20 ms, ropus)
//!                                                                   │ {frame, data}
//!                                                                   v
//!                                                  main thread: stamp, RED, encrypt, send
//! ```
//!
//! [`EncoderWorker`] is the worker, one per `MicrophoneEncoder`, kept across
//! capture sessions so unmute and device switches do not refetch or
//! recompile its wasm. [`AudioWorkletCodec`] is one capture session: its
//! worklet node and its session on the worker. The worklet and the worker
//! talk over a `MessageChannel`, so PCM never crosses the main thread.

use futures::channel::oneshot;
use futures::future::{select, Either};
use gloo_timers::future::TimeoutFuture;
use js_sys::{Array, Uint8Array};
use serde::Serialize;
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AudioContext, AudioWorkletNode, AudioWorkletNodeOptions, ChannelCountMode, ErrorEvent,
    MessageChannel, MessageEvent, MessagePort, Worker,
};

const CAPTURE_WORKLET_CODE: &str = include_str!("scripts/micCaptureWorklet.js");
const CAPTURE_PROCESSOR_NAME: &str = "mic-capture";
/// `<link id=...>` whose `href` is the trunk-built worker loader.
const ENCODER_WORKER_LINK_ID: &str = "audio-encoder-worker";
/// How long the first worker of an `EncoderWorker` may take, from spawn to
/// ready: downloading and compiling its wasm on a slow phone and a slow
/// network. Counted from the spawn, which happens when the microphone
/// encoder is built, so a page that waits before unmuting spends little of
/// it at unmute.
const WORKER_FIRST_LOAD_TIMEOUT_MS: f64 = 60_000.0;
/// The same for a respawn after a failure, when the files are cached.
const WORKER_RELOAD_TIMEOUT_MS: f64 = 10_000.0;

/// Encoder settings sent to the worker's `init`.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EncoderInitOptions {
    /// Rate of the PCM the worklet captures (the AudioContext's rate).
    pub input_sample_rate: u32,
    /// Opus target bitrate in bits per second.
    pub bitrate_bps: u32,
    /// Opus discontinuous transmission.
    pub dtx: bool,
}

/// One encoded packet from the worker.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    /// The encoder's frame index, counting frames DTX did not send.
    pub frame_index: u64,
    /// The Opus packet.
    pub data: Uint8Array,
}

/// Why a capture session did not start.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum CodecStartError {
    /// The session was stopped (mute, device switch, teardown) while it was
    /// starting. Not a failure; nothing to report.
    Cancelled,
    /// Setup failed.
    Failed(String),
}

impl fmt::Display for CodecStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => write!(f, "cancelled"),
            Self::Failed(message) => write!(f, "{message}"),
        }
    }
}

impl From<JsValue> for CodecStartError {
    fn from(value: JsValue) -> Self {
        Self::Failed(format!("{value:?}"))
    }
}

#[derive(Serialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
enum WorkerMsg {
    Init {
        session: u32,
        #[serde(flatten)]
        options: EncoderInitOptions,
    },
    #[serde(rename_all = "camelCase")]
    Reconfigure {
        session: u32,
        bitrate_bps: u32,
    },
    Stop {
        session: u32,
    },
}

type PacketHandler = Box<dyn FnMut(EncodedPacket)>;
type ErrorHandler = Box<dyn FnMut(String)>;

struct ActiveSession {
    id: u32,
    on_packet: Option<PacketHandler>,
    on_error: Option<ErrorHandler>,
}

#[derive(Default)]
struct WorkerShared {
    worker: Option<Worker>,
    ready: bool,
    waiters: Vec<oneshot::Sender<Result<(), String>>>,
    session: Option<ActiveSession>,
    next_session: u32,
    /// `Date.now()` by which the current worker must be ready.
    ready_deadline_ms: f64,
    /// Whether a worker was spawned before (later loads come from cache).
    spawned_before: bool,
    _on_message: Option<Closure<dyn FnMut(MessageEvent)>>,
    _on_error: Option<Closure<dyn FnMut(ErrorEvent)>>,
}

/// The encoder Web Worker, shared by the capture sessions of one
/// microphone encoder. Spawned by [`Self::prefetch`] or on first use;
/// respawned after a failure.
#[derive(Clone, Default)]
pub struct EncoderWorker {
    shared: Rc<RefCell<WorkerShared>>,
}

impl EncoderWorker {
    /// Start loading the worker now, so the first session does not wait for
    /// the download. A failure here is logged; the first session retries.
    // Called only on wasm32 (web-sys panics natively).
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub fn prefetch(&self) {
        if self.shared.borrow().worker.is_some() {
            return;
        }
        if let Err(e) = self.spawn() {
            log::warn!("audio encoder worker prefetch failed: {e:?}");
        }
    }

    /// Wait until the worker has loaded its wasm, spawning it if needed.
    async fn ready(&self) -> Result<(), String> {
        let receiver = {
            let mut shared = self.shared.borrow_mut();
            if shared.worker.is_none() {
                drop(shared);
                self.spawn().map_err(|e| format!("{e:?}"))?;
                shared = self.shared.borrow_mut();
            }
            if shared.ready {
                return Ok(());
            }
            let (sender, receiver) = oneshot::channel();
            shared.waiters.push(sender);
            receiver
        };
        let remaining_ms = (self.shared.borrow().ready_deadline_ms - js_sys::Date::now())
            .clamp(0.0, WORKER_FIRST_LOAD_TIMEOUT_MS);
        match select(receiver, TimeoutFuture::new(remaining_ms as u32)).await {
            Either::Left((Ok(result), _)) => result,
            Either::Left((Err(_), _)) => Err("audio encoder worker went away".to_string()),
            Either::Right(_) => {
                let message = "audio encoder worker did not load in time".to_string();
                self.fail(&message);
                Err(message)
            }
        }
    }

    fn spawn(&self) -> Result<(), JsValue> {
        let worker = create_encoder_worker()?;
        let weak = Rc::downgrade(&self.shared);
        let on_message = Closure::wrap(Box::new(move |event: MessageEvent| {
            if let Some(shared) = weak.upgrade() {
                EncoderWorker { shared }.on_message(event.data());
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        let weak = Rc::downgrade(&self.shared);
        let on_error = Closure::wrap(Box::new(move |event: ErrorEvent| {
            event.prevent_default();
            if let Some(shared) = weak.upgrade() {
                EncoderWorker { shared }.fail(&format!(
                    "audio encoder worker crashed: {}",
                    event.message()
                ));
            }
        }) as Box<dyn FnMut(ErrorEvent)>);
        worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        let mut shared = self.shared.borrow_mut();
        let timeout_ms = if shared.spawned_before {
            WORKER_RELOAD_TIMEOUT_MS
        } else {
            WORKER_FIRST_LOAD_TIMEOUT_MS
        };
        shared.ready_deadline_ms = js_sys::Date::now() + timeout_ms;
        shared.spawned_before = true;
        shared.worker = Some(worker);
        shared.ready = false;
        shared._on_message = Some(on_message);
        shared._on_error = Some(on_error);
        Ok(())
    }

    fn on_message(&self, data: JsValue) {
        let field = |name: &str| js_sys::Reflect::get(&data, &JsValue::from_str(name)).ok();
        let kind = field("type").and_then(|kind| kind.as_string());
        let session = field("session")
            .and_then(|id| id.as_f64())
            .map(|id| id as u32);
        match kind.as_deref() {
            Some("ready") => {
                let waiters = {
                    let mut shared = self.shared.borrow_mut();
                    shared.ready = true;
                    std::mem::take(&mut shared.waiters)
                };
                for waiter in waiters {
                    let _ = waiter.send(Ok(()));
                }
            }
            Some("packet") => {
                let (Some(id), Some(frame), Some(packet)) = (
                    session,
                    field("frame").and_then(|frame| frame.as_f64()),
                    field("data").and_then(|data| data.dyn_into::<Uint8Array>().ok()),
                ) else {
                    return;
                };
                // Run the handler without holding the borrow: it sends the
                // packet, and must be free to touch the worker.
                let Some(mut handler) = self.take_handler(id) else {
                    return;
                };
                handler(EncodedPacket {
                    frame_index: frame as u64,
                    data: packet,
                });
                if let Some(active) = self.shared.borrow_mut().session.as_mut() {
                    if active.id == id && active.on_packet.is_none() {
                        active.on_packet = Some(handler);
                    }
                }
            }
            Some("error") => {
                let message = field("message")
                    .and_then(|message| message.as_string())
                    .unwrap_or_default();
                // A session error ends that session; the worker carries on.
                let ended = {
                    let mut shared = self.shared.borrow_mut();
                    match shared.session.take() {
                        Some(active) if Some(active.id) == session => Some(active),
                        other => {
                            shared.session = other;
                            None
                        }
                    }
                };
                if let Some(mut on_error) = ended.and_then(|active| active.on_error) {
                    on_error(format!("Audio encoder error: {message}"));
                }
            }
            _ => {}
        }
    }

    fn take_handler(&self, id: u32) -> Option<PacketHandler> {
        let mut shared = self.shared.borrow_mut();
        let active = shared.session.as_mut().filter(|active| active.id == id)?;
        active.on_packet.take()
    }

    /// Start a session: the worker builds a fresh encoder and reads PCM
    /// from `port`. Replaces the previous session, if any.
    fn begin(
        &self,
        options: EncoderInitOptions,
        port: &MessagePort,
        on_packet: PacketHandler,
        on_error: ErrorHandler,
    ) -> Result<u32, JsValue> {
        let mut shared = self.shared.borrow_mut();
        let worker = shared
            .worker
            .clone()
            .ok_or_else(|| JsValue::from_str("audio encoder worker is not running"))?;
        let id = shared.next_session;
        shared.next_session = id.wrapping_add(1);
        let init = serde_wasm_bindgen::to_value(&WorkerMsg::Init {
            session: id,
            options,
        })?;
        worker.post_message_with_transfer(&init, &Array::of1(port))?;
        shared.session = Some(ActiveSession {
            id,
            on_packet: Some(on_packet),
            on_error: Some(on_error),
        });
        Ok(id)
    }

    /// End session `id` if it is still the current one.
    fn end(&self, id: u32) {
        let mut shared = self.shared.borrow_mut();
        if shared
            .session
            .as_ref()
            .is_some_and(|active| active.id == id)
        {
            shared.session = None;
        }
        if let Some(worker) = shared.worker.as_ref() {
            if let Ok(stop) = serde_wasm_bindgen::to_value(&WorkerMsg::Stop { session: id }) {
                let _ = worker.post_message(&stop);
            }
        }
    }

    fn reconfigure(&self, id: u32, bitrate_bps: u32) {
        let shared = self.shared.borrow();
        if !shared
            .session
            .as_ref()
            .is_some_and(|active| active.id == id)
        {
            return;
        }
        if let Some(worker) = shared.worker.as_ref() {
            match serde_wasm_bindgen::to_value(&WorkerMsg::Reconfigure {
                session: id,
                bitrate_bps,
            }) {
                Ok(message) => {
                    let _ = worker.post_message(&message);
                }
                Err(e) => log::error!("audio encoder reconfigure: {e:?}"),
            }
        }
    }

    /// The worker crashed or never loaded: terminate it, fail whoever waits
    /// on it, and end the current session with `message`. The next session
    /// spawns a new worker.
    fn fail(&self, message: &str) {
        log::error!("{message}");
        let (waiters, session) = {
            let mut shared = self.shared.borrow_mut();
            if let Some(worker) = shared.worker.take() {
                worker.set_onmessage(None);
                worker.set_onerror(None);
                worker.terminate();
            }
            shared.ready = false;
            // The closures may be running right now (this can be called from
            // the error handler), so they are dropped later, not here.
            (std::mem::take(&mut shared.waiters), shared.session.take())
        };
        for waiter in waiters {
            let _ = waiter.send(Err(message.to_string()));
        }
        if let Some(mut on_error) = session.and_then(|active| active.on_error) {
            on_error(message.to_string());
        }
    }

    /// Terminate the worker. A later session would spawn a new one.
    pub fn terminate(&self) {
        let waiters = {
            let mut shared = self.shared.borrow_mut();
            if let Some(worker) = shared.worker.take() {
                worker.set_onmessage(None);
                worker.set_onerror(None);
                worker.terminate();
            }
            shared.ready = false;
            shared.session = None;
            shared._on_message = None;
            shared._on_error = None;
            std::mem::take(&mut shared.waiters)
        };
        for waiter in waiters {
            let _ = waiter.send(Err("audio encoder worker terminated".to_string()));
        }
    }
}

/// The live parts of one capture session.
struct Parts {
    node: AudioWorkletNode,
    worker: EncoderWorker,
    session: u32,
}

impl Parts {
    fn teardown(self) {
        self.worker.end(self.session);
        close_node(&self.node);
    }
}

/// Stop the worklet's processing and take it out of the graph.
fn close_node(node: &AudioWorkletNode) {
    let close = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&close, &"command".into(), &"close".into());
    if let Ok(port) = node.port() {
        let _ = port.post_message(&close);
    }
    let _ = node.disconnect();
}

#[derive(Default)]
enum Lifecycle {
    #[default]
    Idle,
    /// `create` is loading the worklet module or waiting on the worker.
    Starting,
    Running(Parts),
    /// `destroy` ran; a `create` still in flight must give up.
    Destroyed,
}

/// Handle to one microphone capture session. Clones share it.
///
/// Make a new one per session: `destroy` is final, so a session's cleanup
/// can never tear down the session that replaced it.
#[derive(Clone, Default)]
pub struct AudioWorkletCodec {
    inner: Rc<RefCell<Lifecycle>>,
}

impl AudioWorkletCodec {
    /// Claim the handle for a session that is about to start, so
    /// [`Self::is_instantiated`] is true while `create` is still loading.
    pub fn mark_starting(&self) {
        let mut inner = self.inner.borrow_mut();
        if matches!(*inner, Lifecycle::Idle) {
            *inner = Lifecycle::Starting;
        }
    }

    fn check_starting(&self) -> Result<(), CodecStartError> {
        if matches!(*self.inner.borrow(), Lifecycle::Starting) {
            Ok(())
        } else {
            Err(CodecStartError::Cancelled)
        }
    }

    /// Build the capture worklet, start a session on `worker` and connect
    /// them. Returns the node to connect the microphone into.
    ///
    /// `on_packet` gets each Opus packet (one per 20 ms, fewer during DTX
    /// silence). `on_error` gets a worker or encoder failure, after which
    /// this session is destroyed. [`CodecStartError::Cancelled`] means
    /// [`Self::destroy`] ran while starting.
    pub async fn create(
        &self,
        context: &AudioContext,
        worker: &EncoderWorker,
        options: EncoderInitOptions,
        on_packet: PacketHandler,
        mut on_error: ErrorHandler,
    ) -> Result<AudioWorkletNode, CodecStartError> {
        self.mark_starting();
        self.check_starting()?;
        // After each await, a stop that happened meanwhile wins over any
        // error: the error is most likely the stop's own doing (a
        // terminated worker, a closed context), and is not worth reporting.
        let registered = register_capture_worklet(context).await;
        self.check_starting()?;
        registered?;
        // The worklet's port is attached only once the worker is ready, so
        // PCM never queues up while the worker's wasm loads.
        let ready = worker.ready().await;
        self.check_starting()?;
        ready.map_err(CodecStartError::Failed)?;

        let node_options = AudioWorkletNodeOptions::new();
        node_options.set_number_of_inputs(1);
        node_options.set_number_of_outputs(1);
        node_options.set_output_channel_count(&Array::of1(&JsValue::from(1)));
        // The browser downmixes to mono before process() sees the input.
        node_options.set_channel_count(1);
        node_options.set_channel_count_mode(ChannelCountMode::Explicit);
        let node =
            AudioWorkletNode::new_with_options(context, CAPTURE_PROCESSOR_NAME, &node_options)?;

        let attach = || -> Result<(MessageChannel, MessagePort), JsValue> {
            let channel = MessageChannel::new()?;
            let port = node.port()?;
            Ok((channel, port))
        };
        let (channel, node_port) = match attach() {
            Ok(parts) => parts,
            Err(e) => {
                close_node(&node);
                return Err(e.into());
            }
        };

        // A worker or encoder failure ends this session.
        let codec = self.clone();
        let on_session_error: ErrorHandler = Box::new(move |message: String| {
            on_error(message);
            codec.destroy();
        });
        let session = match worker.begin(options, &channel.port2(), on_packet, on_session_error) {
            Ok(session) => session,
            Err(e) => {
                close_node(&node);
                return Err(e.into());
            }
        };
        let attach_message = js_sys::Object::new();
        let attached = js_sys::Reflect::set(&attach_message, &"command".into(), &"attach".into())
            .and_then(|_| {
                node_port
                    .post_message_with_transferable(&attach_message, &Array::of1(&channel.port1()))
            });
        let parts = Parts {
            node: node.clone(),
            worker: worker.clone(),
            session,
        };
        if let Err(e) = attached {
            parts.teardown();
            return Err(e.into());
        }
        let mut inner = self.inner.borrow_mut();
        if !matches!(*inner, Lifecycle::Starting) {
            drop(inner);
            parts.teardown();
            return Err(CodecStartError::Cancelled);
        }
        *inner = Lifecycle::Running(parts);
        Ok(node)
    }

    /// True from [`Self::mark_starting`] until [`Self::destroy`].
    pub fn is_instantiated(&self) -> bool {
        matches!(
            *self.inner.borrow(),
            Lifecycle::Starting | Lifecycle::Running(_)
        )
    }

    /// Send a new target bitrate to this session's encoder.
    pub fn set_bitrate(&self, bitrate_bps: u32) {
        if let Lifecycle::Running(parts) = &*self.inner.borrow() {
            parts.worker.reconfigure(parts.session, bitrate_bps);
        }
    }

    /// End the session and take the worklet out of the graph. The worker
    /// stays up for the next session. Final: a `create` still in flight on
    /// this handle gives up.
    pub fn destroy(&self) {
        let previous = std::mem::replace(&mut *self.inner.borrow_mut(), Lifecycle::Destroyed);
        if let Lifecycle::Running(parts) = previous {
            parts.teardown();
        }
    }
}

/// Register the capture processor on `context` from the inlined script. Each
/// microphone session has its own AudioContext, so this runs per session.
async fn register_capture_worklet(context: &AudioContext) -> Result<(), JsValue> {
    let blob_parts = Array::of1(&JsValue::from_str(CAPTURE_WORKLET_CODE));
    let blob_options = web_sys::BlobPropertyBag::new();
    blob_options.set_type("application/javascript");
    let blob = web_sys::Blob::new_with_str_sequence_and_options(&blob_parts, &blob_options)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)?;
    let result = match context.audio_worklet()?.add_module(&url) {
        Ok(promise) => JsFuture::from(promise).await.map(|_| ()),
        Err(e) => Err(e),
    };
    let _ = web_sys::Url::revoke_object_url(&url);
    result
}

/// Start the trunk-built `audio_encoder_worker`, found through its
/// `<link id="audio-encoder-worker">` in index.html.
fn create_encoder_worker() -> Result<Worker, JsValue> {
    let document = web_sys::window()
        .and_then(|window| window.document())
        .ok_or_else(|| JsValue::from_str("no document"))?;
    let worker_url = document
        .get_element_by_id(ENCODER_WORKER_LINK_ID)
        .and_then(|link| link.get_attribute("href"))
        .ok_or_else(|| JsValue::from_str("audio-encoder-worker link tag not found"))?;
    Worker::new(&worker_url)
}
