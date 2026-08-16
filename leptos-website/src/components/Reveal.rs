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

/// Shared scroll-reveal island. Wrap a section header (eyebrow / h2 / lede) and
/// its direct children rise + fade into view once, staggered, on first scroll.
///
/// Motion is opt-in and fails safe: the wrapper only gains the `reveal-armed`
/// class after the island hydrates and confirms motion is allowed, so without
/// JavaScript — or under `prefers-reduced-motion` — the content renders in its
/// final visible state. A single `IntersectionObserver` toggles `in-view` and
/// disconnects each element after its first reveal, so nothing keeps observing.
#[island]
pub fn RevealOnView(
    /// Extra classes for the reveal wrapper — spacing only (e.g. `"mt-12"`).
    /// Do NOT put grid/flex layout classes here: island children arrive
    /// wrapped in a `<leptos-children>` element, so a grid on the wrapper
    /// would see one child. `leptos-children` is `display: contents` (see
    /// input.css), which lets the stagger selectors reach the real items, but
    /// layout containers belong on a plain div inside the island (see
    /// System.rs). Required (islands can't serialize an optional/defaulted
    /// prop cleanly), so every call site states it explicitly.
    #[prop(into)]
    class: String,
    children: Children,
) -> impl IntoView {
    let node: NodeRef<leptos::html::Div> = NodeRef::new();

    #[cfg(feature = "hydrate")]
    {
        use std::cell::RefCell;
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

            // Respect reduced-motion: reveal immediately, no observer, no motion.
            if let Ok(Some(mql)) = win.match_media("(prefers-reduced-motion: reduce)") {
                if mql.matches() {
                    return;
                }
            }

            // Arm the hidden initial state now that motion is confirmed.
            let _ = el.class_list().add_1("reveal-armed");

            // Hold the closure in a cell the callback also owns (an Rc cycle), so
            // it stays alive until the one-shot reveal fires. On reveal we
            // disconnect the observer and drop the closure by clearing the cell,
            // so both the browser-side observer and the Rust closure are freed —
            // nothing is leaked for the page lifetime.
            type RevealClosure = Closure<dyn FnMut(js_sys::Array, web_sys::IntersectionObserver)>;
            let cell: Rc<RefCell<Option<RevealClosure>>> = Rc::new(RefCell::new(None));
            let cell_inner = cell.clone();
            *cell.borrow_mut() = Some(Closure::wrap(Box::new(
                move |entries: js_sys::Array, observer: web_sys::IntersectionObserver| {
                    let mut revealed = false;
                    for entry in entries.iter() {
                        let entry: web_sys::IntersectionObserverEntry = entry.unchecked_into();
                        if entry.is_intersecting() {
                            let _ = entry.target().class_list().add_1("in-view");
                            revealed = true;
                        }
                    }
                    if revealed {
                        observer.disconnect();
                        // Break the cycle; the closure drops after this returns.
                        let _ = cell_inner.borrow_mut().take();
                    }
                },
            )
                as Box<dyn FnMut(js_sys::Array, web_sys::IntersectionObserver)>));

            // Scope the borrow so the `Ref` drops before `cell` does at the end
            // of the effect; passing the callback to the observer is all we need
            // it for. The Rc<RefCell<..>> lives on via the closure's own clone.
            let observer = {
                let borrowed = cell.borrow();
                let callback = borrowed.as_ref().unwrap().as_ref().unchecked_ref();
                web_sys::IntersectionObserver::new(callback)
            };
            if let Ok(observer) = observer {
                observer.observe(&el);
            }
        });
    }

    view! {
        <div node_ref=node class=format!("reveal {class}")>
            {children()}
        </div>
    }
}
