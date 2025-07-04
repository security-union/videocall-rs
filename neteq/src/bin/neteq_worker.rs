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
        console::log_1(&"[neteq-worker] starting".into());
        let self_scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
        let self_scope_clone = self_scope.clone();
        let self_scope_clone_2 = self_scope.clone();
        let self_scope_clone_3 = self_scope.clone();
        let on_message = Closure::wrap(Box::new(move |evt: MessageEvent| {
            match serde_wasm_bindgen::from_value::<WorkerMsg>(evt.data()) {
                Ok(msg) => handle_message(&self_scope_clone, msg),
                Err(e) => console::error_1(&format!("[neteq-worker] bad msg: {:?}", e).into()),
            }
        }) as Box<dyn FnMut(_)>);
        console::log_1(&"[neteq-worker] onmessage".into());

        self_scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        // Eagerly create a default NetEq (48 kHz / mono). If the main thread later sends an
        // explicit Init message, that path will simply be ignored because `NETEQ` is already
        // populated.
        NETEQ.with(|cell| {
            if cell.borrow().is_none() {
                match WebNetEq::new(48_000, 1) {
                    Ok(eq) => {
                        *cell.borrow_mut() = Some(eq);
                        console::log_1(
                            &"[neteq-worker] NetEq auto-initialised (48 kHz/mono)".into(),
                        );
                    }
                    Err(e) => {
                        console::error_2(&"[neteq-worker] auto-init error:".into(), &e);
                    }
                }
            }
        });

        // === Stats interval (1 Hz) ===
        console::log_1(&"[neteq-worker] stats interval".into());
        let stats_cb = Closure::wrap(Box::new(move || {
            console::log_1(&"[neteq-worker] stats".into());
            NETEQ.with(|cell| {
                if let Some(eq) = cell.borrow().as_ref() {
                    match eq.get_statistics() {
                        Ok(js_val) => {
                            let prefix = JsValue::from_str("[neteq-worker] stats: ");
                            console::log_2(&prefix, &js_val);
                        }
                        Err(e) => {
                            console::error_1(
                                &format!("[neteq-worker] stats error: {:?}", e).into(),
                            );
                        }
                    }
                } else {
                    console::log_1(&"[neteq-worker] no eq".into());
                }
            });
        }) as Box<dyn FnMut()>);
        let _ = self_scope_clone_2.set_interval_with_callback_and_timeout_and_arguments_0(
            stats_cb.as_ref().unchecked_ref(),
            1000,
        );
        stats_cb.forget();

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
        let _ = self_scope_clone_3.set_interval_with_callback_and_timeout_and_arguments_0(
            cb.as_ref().unchecked_ref(),
            10,
        );
        cb.forget();

        on_message.forget();
    }

    fn handle_message(scope: &DedicatedWorkerGlobalScope, msg: WorkerMsg) {
        // console::log_1(&format!("[neteq-worker] received message: {:?}", msg).into());
        match msg {
            WorkerMsg::Init {
                sample_rate,
                channels,
            } => {
                console::log_2(
                    &"[neteq-worker] Init received, sr=".into(),
                    &JsValue::from_f64(sample_rate as f64),
                );
                NETEQ.with(|cell| {
                    if cell.borrow().is_none() {
                        match WebNetEq::new(sample_rate, channels) {
                            Ok(eq) => {
                                *cell.borrow_mut() = Some(eq);
                                console::log_1(&"[neteq-worker] NetEq initialised".into());
                            }
                            Err(e) => {
                                console::error_2(&"[neteq-worker] NetEq init error:".into(), &e);
                            }
                        }
                    }
                });
                // Timer to pull audio every 10 ms.
                let cb = Closure::wrap(Box::new(move || {
                    NETEQ.with(|cell| {
                        if let Some(eq) = cell.borrow().as_ref() {
                            console::log_1(&"[neteq-worker] get_audio".into());
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
                // console::log_1(&"[neteq-worker] insert_packet".into());
                NETEQ.with(|cell| {
                    if let Some(eq) = cell.borrow().as_ref() {
                        if let Err(e) = eq.insert_packet(seq, timestamp, &payload) {
                            console::error_1(
                                &format!("[neteq-worker] insert_packet error: {:?}", e).into(),
                            );
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
