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

/// Gate condition for the 1-second interval emit: decide whether to broadcast a
/// render-FPS metric for a window that painted `frames` rAF callbacks while the
/// page's `document.hidden` was `page_hidden`.
///
/// Emit when either:
/// - `frames > 0` — a real rAF measurement, always worth reporting; or
/// - the page is in the *foreground* (`!page_hidden`) — even a 0-frame window,
///   because a foreground second with zero rAF callbacks is genuine rendering
///   pressure (the main thread was blocked for ~1 s). Emitting `Some(0.0)`
///   there is CORRECT: it lets the decode-budget controller see the collapse
///   and, if it sustains, take a protective step-down (a median of 0 fps is
///   `<= FPS_SEVERE`).
///
/// Skip ONLY when `frames == 0` AND `page_hidden` is true: a backgrounded tab
/// has its rAF loop paused by the browser, so 0 frames is expected there and is
/// NOT a collapse. Reporting `Some(0.0)` for a hidden tab would be misread as
/// "FPS collapsed → step down", a false down-step even though nothing is wrong.
///
/// Downstream effect of a skip: the Dioxus decode-budget control loop
/// (`attendants.rs`) only closes a sample bucket when a `client_render_fps`
/// event arrives — with no event it `continue`s without pushing a sample, so
/// the sample window does not advance and no step decision runs. The cap is
/// therefore held while the tab is hidden, which is the correct behaviour for a
/// backgrounded page. (It is NOT that a `None`-fps sample enters the window;
/// the loop simply does not advance.)
///
/// `page_hidden` is passed in (rather than read here) so this predicate stays
/// pure and host-testable; the wasm interval closure reads `document.hidden()`
/// each tick and forwards it. Extracted (and NOT `#[cfg(target_arch =
/// "wasm32")]`-gated) so the closure calls exactly this predicate and host unit
/// tests can pin its behaviour directly.
pub fn should_emit_render_fps(frames: u32, page_hidden: bool) -> bool {
    frames > 0 || !page_hidden
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
        // Cache the Document handle once (stable for the page lifetime) so the
        // interval closure can read live tab visibility each tick without
        // re-walking window()->document(). `None` is impossible in a browser;
        // if it ever were, the gate below treats the page as visible.
        let document_for_interval = window.document();
        let interval_closure = Closure::<dyn FnMut()>::new(move || {
            let frames = count_for_interval.get();
            count_for_interval.set(0);
            // Read live page visibility each tick. A hidden tab pauses rAF, so a
            // 0-frame window there is expected and must NOT be reported as a
            // collapse; a foreground 0-frame window is genuine rendering
            // pressure the decode-budget controller must see. `None` document
            // defaults to "visible" (matches health_reporter's convention), so
            // unknown visibility still surfaces the 0. The gate condition lives
            // in `should_emit_render_fps` so it is host-testable; see that
            // function for the full rationale.
            let page_hidden = document_for_interval
                .as_ref()
                .map(|d| d.hidden())
                .unwrap_or(false);
            if should_emit_render_fps(frames, page_hidden) {
                emit_render_fps(frames as f64);
            }
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

    // ── visibility-aware emit gate ────────────────────────────────────────────
    //
    // The 1-second interval closure emits a render-FPS metric iff
    // `should_emit_render_fps(frames, page_hidden)` is true, where `page_hidden`
    // is the live `document.hidden()` read that tick. The predicate must:
    //   - always emit when frames were painted (`frames > 0`), regardless of
    //     visibility (a real measurement is always worth reporting);
    //   - emit a 0-frame window when the page is in the FOREGROUND — a genuine
    //     rendering-pressure event the decode-budget controller must see (a
    //     sustained median of 0 fps is `<= FPS_SEVERE` → protective step-down);
    //   - skip a 0-frame window ONLY when the page is HIDDEN — a backgrounded
    //     tab pauses rAF, so 0 frames is expected and must NOT be reported as a
    //     collapse (which would trigger a false down-step).
    //
    // These tests pin that CONDITION by calling the exact production predicate
    // the interval closure uses as its guard
    // (`if should_emit_render_fps(frames, page_hidden) { emit_render_fps(..) }`).
    // The full 2x2 matrix below fails under every single-operator mutation of
    // `frames > 0 || !page_hidden`: dropping the `frames` term breaks the
    // hidden+nonzero case, dropping the visibility term breaks the
    // foreground+zero case, flipping `||`→`&&` breaks the hidden+nonzero case,
    // and always-true / always-false break the hidden-zero / foreground-zero
    // cases respectively.

    /// Zero frames while the page is HIDDEN must NOT satisfy the gate: a
    /// backgrounded tab pauses rAF, so 0 frames is expected, not a collapse.
    /// A `Some(0.0)` here cannot be distinguished by the budget controller from
    /// a true FPS collapse, so it must be skipped.
    #[test]
    fn zero_frames_hidden_page_does_not_satisfy_gate() {
        assert!(
            !should_emit_render_fps(0, true),
            "gate must not fire for zero frames on a hidden tab"
        );
    }

    /// Zero frames while the page is in the FOREGROUND MUST satisfy the gate:
    /// a foreground second with no rAF callbacks is genuine rendering pressure
    /// (main thread blocked ~1 s), which the budget controller must see so it
    /// can take a protective step-down if it sustains.
    #[test]
    fn zero_frames_foreground_page_satisfies_gate() {
        assert!(
            should_emit_render_fps(0, false),
            "gate MUST fire for a foreground 0-frame window (real rendering pressure)"
        );
    }

    /// One or more frames MUST satisfy the gate regardless of visibility — a
    /// real rAF measurement is always worth reporting.
    #[test]
    fn nonzero_frames_satisfies_gate_regardless_of_visibility() {
        for frames in [1u32, 30, 60, 120] {
            for page_hidden in [false, true] {
                assert!(
                    should_emit_render_fps(frames, page_hidden),
                    "frame count {frames} (page_hidden={page_hidden}) must pass the gate \
                     and trigger emit_render_fps"
                );
            }
        }
    }

    /// The value emitted when the gate passes is `frames as f64` — verify the
    /// cast is lossless for plausible frame counts (1–240).
    #[test]
    fn frames_cast_to_f64_is_lossless_for_normal_rates() {
        for frames in [1u32, 30, 60, 120, 240] {
            let fps = frames as f64;
            assert!(
                (fps - f64::from(frames)).abs() < 1e-9,
                "u32→f64 cast must be lossless for frame count {frames}"
            );
        }
    }
}
