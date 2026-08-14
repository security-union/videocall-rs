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

use leptos::prelude::*;

/// A single count-up readout for the Adoption band.
///
/// The signal starts AT the final value, so SSR, no-JS, and reduced-motion all
/// render the real number and never a zero. Only when hydration runs, motion is
/// allowed, and the readout first scrolls into view does the island reset to 0
/// and animate up once (easeOutCubic over ~900 ms) via `requestAnimationFrame`,
/// then land exactly on `target` and disconnect its observer — one shot, no
/// lingering timers. `tabular-nums` keeps the digits from jittering as they run.
#[island]
pub fn CountUp(
    /// Final numeric value the readout counts up to (e.g. `1.7`, `170.0`).
    target: f64,
    /// Decimal places to render — `0` for integers, `1` for "1.7K".
    decimals: u8,
    /// Suffix appended after the number (e.g. "K", "+", or "").
    #[prop(into)]
    suffix: String,
) -> impl IntoView {
    let dp = decimals as usize;
    // Initial value is the final frame — the number is always correct with no JS.
    let display = RwSignal::new(format!("{target:.dp$}{suffix}"));

    let node: NodeRef<leptos::html::Span> = NodeRef::new();

    #[cfg(feature = "hydrate")]
    {
        use std::cell::{Cell, RefCell};
        use std::rc::Rc;
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;

        Effect::new(move |_| {
            let Some(el) = node.get() else {
                return;
            };
            let Some(win) = web_sys::window() else {
                return;
            };

            // Reduced motion: leave the final value in place, start nothing.
            if let Ok(Some(mql)) = win.match_media("(prefers-reduced-motion: reduce)") {
                if mql.matches() {
                    return;
                }
            }

            // Motion allowed: reset to zero now (still off-screen for section 06),
            // then count up on first reveal.
            let suffix = suffix.clone();
            display.set(format!("{:.dp$}{suffix}", 0.0));

            // Cell holding the recursive rAF closure so each frame can re-arm it.
            type FrameCell = Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>>;

            let started = Rc::new(Cell::new(false));
            let raf: FrameCell = Rc::new(RefCell::new(None));

            // Recursive rAF: eased 0 -> target over 900 ms, then snap to exact.
            let run: Rc<dyn Fn()> = {
                let win = win.clone();
                let raf = raf.clone();
                Rc::new(move || {
                    let start: Rc<Cell<f64>> = Rc::new(Cell::new(f64::NAN));
                    let win_frame = win.clone();
                    let raf_frame = raf.clone();
                    let suffix = suffix.clone();
                    let frame = Closure::wrap(Box::new(move |now: f64| {
                        if start.get().is_nan() {
                            start.set(now);
                        }
                        let t = ((now - start.get()) / 900.0).clamp(0.0, 1.0);
                        let eased = 1.0 - (1.0 - t).powi(3);
                        display.set(format!("{:.dp$}{suffix}", target * eased));
                        if t < 1.0 {
                            if let Some(cb) = raf_frame.borrow().as_ref() {
                                let _ =
                                    win_frame.request_animation_frame(cb.as_ref().unchecked_ref());
                            }
                        } else {
                            display.set(format!("{target:.dp$}{suffix}"));
                            // Done — drop the closure to free it.
                            let _ = raf_frame.borrow_mut().take();
                        }
                    }) as Box<dyn FnMut(f64)>);
                    let _ = win.request_animation_frame(frame.as_ref().unchecked_ref());
                    *raf.borrow_mut() = Some(frame);
                })
            };

            // One-shot IntersectionObserver: kick the animation on first reveal.
            let io_cb = Closure::wrap(Box::new({
                let started = started.clone();
                let run = run.clone();
                move |entries: js_sys::Array, obs: web_sys::IntersectionObserver| {
                    let visible = entries.iter().any(|e| {
                        e.unchecked_into::<web_sys::IntersectionObserverEntry>()
                            .is_intersecting()
                    });
                    if visible && !started.get() {
                        started.set(true);
                        run();
                        obs.disconnect();
                    }
                }
            })
                as Box<dyn FnMut(js_sys::Array, web_sys::IntersectionObserver)>);
            if let Ok(io) = web_sys::IntersectionObserver::new(io_cb.as_ref().unchecked_ref()) {
                io.observe(&el);
                io_cb.forget();
                std::mem::forget(io);
            }
        });
    }

    view! {
        <span node_ref=node class="tabular-nums">{move || display.get()}</span>
    }
}
