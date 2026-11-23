// SPDX-License-Identifier: MIT OR Apache-2.0

//! Small questions about the device the page runs on.

/// Whether a text field may take focus on its own when it appears.
///
/// On a phone or tablet the primary pointer is a finger (`pointer: coarse`),
/// and focusing a field raises the on-screen keyboard, which covers the
/// button the person came to press. There they tap the field themselves.
/// With a mouse or trackpad autofocus saves a click, so it stays.
pub fn autofocus_allowed() -> bool {
    let coarse = web_sys::window()
        .and_then(|window| window.match_media("(pointer: coarse)").ok().flatten())
        .is_some_and(|query| query.matches());
    !coarse
}

/// Whether the CSS media query `query` matches, kept current.
///
/// The value is read once when the component first renders, then updated
/// only by the query's `change` event, which fires when the answer flips
/// (crossing a breakpoint, rotating a phone). Nothing runs per frame. The
/// listener is removed when the component unmounts.
pub fn use_media_query(query: &'static str) -> dioxus::prelude::ReadSignal<bool> {
    use dioxus::prelude::*;
    use std::rc::Rc;
    use wasm_bindgen::{closure::Closure, JsCast};

    let list = use_hook(|| {
        web_sys::window()
            .and_then(|window| window.match_media(query).ok().flatten())
            .map(Rc::new)
    });
    let mut matches = use_signal(|| list.as_ref().is_some_and(|list| list.matches()));
    let listener = use_hook(|| {
        let list = list.clone()?;
        let source = list.clone();
        let closure = Closure::<dyn FnMut()>::new(move || {
            let now = source.matches();
            if *matches.peek() != now {
                matches.set(now);
            }
        });
        list.add_event_listener_with_callback("change", closure.as_ref().unchecked_ref())
            .ok()?;
        Some(Rc::new((list, closure)))
    });
    use_drop(move || {
        if let Some(listener) = &listener {
            let (list, closure) = &**listener;
            let _ = list
                .remove_event_listener_with_callback("change", closure.as_ref().unchecked_ref());
        }
    });
    matches.into()
}

/// The phone layout's breakpoint, as in `styles/features/call.css` and
/// `sidebars.css` (`max-width: 37.49em`, under 600px).
pub const PHONE_LAYOUT_QUERY: &str = "(max-width: 37.49em)";
