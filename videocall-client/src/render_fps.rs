/*
 * Copyright 2026 Security Union LLC
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

//! rAF (requestAnimationFrame) cadence measurement → diagnostic bus.
//!
//! Counts how many rAF callbacks fire per second on the main thread.
//! Emits `client_render_fps` to the diagnostic bus at 1 Hz.

use log::debug;
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};

pub const SUBSYSTEM: &str = "client_perf";
pub const METRIC_RENDER_FPS: &str = "client_render_fps";

pub fn build_render_fps_event(fps: f64) -> DiagEvent {
    DiagEvent {
        subsystem: SUBSYSTEM,
        stream_id: None,
        ts_ms: now_ms(),
        metrics: vec![metric!(METRIC_RENDER_FPS, fps)],
    }
}

pub fn emit_render_fps(fps: f64) -> bool {
    let event = build_render_fps_event(fps);
    match global_sender().try_broadcast(event) {
        Ok(_) => true,
        Err(e) => {
            debug!("render_fps: failed to broadcast metric: {e}");
            false
        }
    }
}

#[cfg(target_arch = "wasm32")]
use std::cell::Cell;
#[cfg(target_arch = "wasm32")]
use std::rc::Rc;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::{prelude::Closure, JsCast};

#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
pub struct RenderFpsObserver {
    _raf_closure: Closure<dyn FnMut()>,
    _interval_closure: Closure<dyn FnMut()>,
    interval_handle: i32,
    raf_id: Rc<Cell<i32>>,
}

#[cfg(target_arch = "wasm32")]
#[allow(clippy::type_complexity)]
impl RenderFpsObserver {
    pub fn start() -> Option<Self> {
        use std::cell::RefCell;

        let window = web_sys::window()?;
        let frame_count: Rc<Cell<u32>> = Rc::new(Cell::new(0));
        let raf_id: Rc<Cell<i32>> = Rc::new(Cell::new(0));

        // Self-referencing rAF loop via Rc<RefCell<Option<Closure>>>.
        let count_for_raf = frame_count.clone();
        let raf_id_for_loop = raf_id.clone();
        let raf_cb: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
        let raf_cb_clone = raf_cb.clone();
        let window_for_loop = window.clone();

        let closure = Closure::<dyn FnMut()>::new(move || {
            count_for_raf.set(count_for_raf.get() + 1);
            if let Some(ref cb) = *raf_cb_clone.borrow() {
                if let Ok(id) = window_for_loop.request_animation_frame(cb.as_ref().unchecked_ref())
                {
                    raf_id_for_loop.set(id);
                }
            }
        });

        let first_id = window
            .request_animation_frame(closure.as_ref().unchecked_ref())
            .ok()?;
        raf_id.set(first_id);
        *raf_cb.borrow_mut() = Some(closure);

        // 1-second interval: read frame count, emit metric, reset counter.
        let count_for_interval = frame_count.clone();
        let interval_closure = Closure::<dyn FnMut()>::new(move || {
            let frames = count_for_interval.get();
            count_for_interval.set(0);
            emit_render_fps(frames as f64);
        });

        let interval_handle = window
            .set_interval_with_callback_and_timeout_and_arguments_0(
                interval_closure.as_ref().unchecked_ref(),
                1000,
            )
            .ok()?;

        // Prevent the self-referencing closure from being dropped by capturing
        // the Rc in a dummy closure held by the struct.
        let raf_handle_closure = Closure::<dyn FnMut()>::new(move || {
            let _ = &raf_cb;
        });

        Some(Self {
            _raf_closure: raf_handle_closure,
            _interval_closure: interval_closure,
            interval_handle,
            raf_id,
        })
    }
}

#[cfg(target_arch = "wasm32")]
impl Drop for RenderFpsObserver {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            window.clear_interval_with_handle(self.interval_handle);
            window.cancel_animation_frame(self.raf_id.get()).ok();
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default)]
pub struct RenderFpsObserver;

#[cfg(not(target_arch = "wasm32"))]
impl RenderFpsObserver {
    pub fn start() -> Option<Self> {
        Some(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use videocall_diagnostics::MetricValue;

    #[test]
    fn metric_constant_names() {
        assert_eq!(METRIC_RENDER_FPS, "client_render_fps");
        assert_eq!(SUBSYSTEM, "client_perf");
    }

    #[test]
    fn build_render_fps_event_shape() {
        let event = build_render_fps_event(58.5);
        assert_eq!(event.subsystem, SUBSYSTEM);
        assert!(event.stream_id.is_none());
        assert_eq!(event.metrics.len(), 1);
        let m = &event.metrics[0];
        assert_eq!(m.name, METRIC_RENDER_FPS);
        match &m.value {
            MetricValue::F64(v) => assert!((v - 58.5).abs() < 1e-9),
            other => panic!("expected F64, got {other:?}"),
        }
    }

    #[test]
    fn emit_render_fps_returns_false_when_bus_closed() {
        let ok = emit_render_fps(60.0);
        assert!(!ok);
    }

    #[test]
    fn native_stub_can_be_started() {
        let _ = RenderFpsObserver::start().expect("native stub should start");
    }
}
