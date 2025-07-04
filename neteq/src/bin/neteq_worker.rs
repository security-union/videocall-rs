/*
 * NetEq worker for browser: receives RTP-like Opus packets, feeds them to NetEq,
 * periodically pulls PCM frames and posts them back to main thread.
 */
#![cfg_attr(target_arch = "wasm32", no_main)]

#[cfg(target_arch = "wasm32")]
mod wasm_worker {
    use neteq::WebNetEq;
    use serde::{Deserialize, Serialize};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use web_sys::{console, DedicatedWorkerGlobalScope, MessageEvent};

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "cmd", rename_all = "camelCase")]
    enum WorkerMsg {
        Init {
            sample_rate: u32,
            channels: u8,
        },
        /// Insert an encoded packet
        Insert {
            seq: u16,
            timestamp: u32,
            #[serde(with = "serde_bytes")]
            payload: Vec<u8>,
        },
        Flush,
        Clear,
        Close,
    }

    thread_local! {
        static NETEQ: std::cell::RefCell<Option<WebNetEq>> = const { std::cell::RefCell::new(None) };
    }

    #[wasm_bindgen(start)]
    pub fn start() {
        console_error_panic_hook::set_once();
        let self_scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
        let self_scope_clone = self_scope.clone();
        let on_message = Closure::wrap(Box::new(move |evt: MessageEvent| {
            match serde_wasm_bindgen::from_value::<WorkerMsg>(evt.data()) {
                Ok(msg) => handle_message(&self_scope_clone, msg),
                Err(e) => console::error_1(&format!("[neteq-worker] bad msg: {:?}", e).into()),
            }
        }) as Box<dyn FnMut(_)>);

        self_scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        on_message.forget();
    }

    fn handle_message(scope: &DedicatedWorkerGlobalScope, msg: WorkerMsg) {
        match msg {
            WorkerMsg::Init {
                sample_rate,
                channels,
            } => {
                NETEQ.with(|cell| {
                    if cell.borrow().is_none() {
                        match WebNetEq::new(sample_rate, channels) {
                            Ok(eq) => {
                                *cell.borrow_mut() = Some(eq);
                                console::log_1(&"[neteq-worker] NetEq initialised".into());
                            }
                            Err(e) => console::error_1(&e),
                        }
                    }
                });
                // Timer to pull audio every 10 ms.
                let cb = Closure::wrap(Box::new(move || {
                    NETEQ.with(|cell| {
                        if let Some(eq) = cell.borrow().as_ref() {
                            if let Ok(pcm) = eq.get_audio() {
                                let sab = js_sys::Array::of1(&pcm.buffer());
                                let _ = js_sys::global()
                                    .unchecked_into::<DedicatedWorkerGlobalScope>()
                                    .post_message_with_transfer(&pcm, &sab);
                            }
                        }
                    });
                }) as Box<dyn FnMut()>);
                let _ = scope.set_interval_with_callback_and_timeout_and_arguments_0(
                    cb.as_ref().unchecked_ref(),
                    10,
                );
                cb.forget();
            }
            WorkerMsg::Insert {
                seq,
                timestamp,
                payload,
            } => {
                NETEQ.with(|cell| {
                    if let Some(eq) = cell.borrow().as_ref() {
                        if let Err(e) = eq.insert_packet(seq, timestamp, &payload) {
                            console::error_1(&e);
                        }
                    }
                });
            }
            WorkerMsg::Flush => {}
            WorkerMsg::Clear => {
                NETEQ.with(|cell| cell.borrow_mut().take());
            }
            WorkerMsg::Close => {
                scope.close();
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    println!("neteq_worker is only compiled for wasm32 target");
}
