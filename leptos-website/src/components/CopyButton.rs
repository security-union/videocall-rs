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

/// Quiet monochrome copy control, reused once per terminal block.
///
/// Fails safe like the other islands on this page: the button ships hidden
/// (`display:none`) in the SSR HTML and only reveals itself after the island
/// hydrates and confirms `navigator.clipboard` exists. So without JavaScript —
/// or on a browser lacking the async clipboard API — there is simply no dead
/// control in the corner; the code block is still there to select by hand.
///
/// On click it writes `text` to the clipboard and flips the mono label from
/// `COPY` to `COPIED` for 1.5s, then back. All browser interop lives inside the
/// `hydrate`-gated effect so the `ssr` and `csr` builds carry no `web-sys`.
#[island]
pub fn CopyButton(
    /// The exact text placed on the clipboard — pass the block's command(s)
    /// verbatim so what copies matches what the reader sees.
    #[prop(into)]
    text: String,
) -> impl IntoView {
    // Both start in their fail-safe state: hidden, and showing "COPY". The
    // hydrate effect is the only thing that ever arms the button.
    let armed = RwSignal::new(false);
    let copied = RwSignal::new(false);
    let node: NodeRef<leptos::html::Button> = NodeRef::new();

    // `text` is consumed only by the hydrate-gated effect below; the ssr/csr
    // builds render the (hidden) button without it.
    #[cfg(not(feature = "hydrate"))]
    let _ = &text;

    #[cfg(feature = "hydrate")]
    {
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;

        Effect::new(move |_| {
            let Some(btn) = node.get() else {
                return;
            };
            let Some(win) = web_sys::window() else {
                return;
            };

            // No async clipboard API (older/locked-down browsers): stay hidden
            // rather than offer a button that silently does nothing.
            let clipboard = win.navigator().clipboard();

            // Clipboard is available — reveal the control.
            armed.set(true);

            let text = text.clone();
            let click = Closure::wrap(Box::new(move || {
                // write_text returns a Promise; the copy is fire-and-forget and
                // the label flip is optimistic — a rejected write is rare and
                // purely cosmetic, and awaiting it would only add a JsFuture dep.
                let _ = clipboard.write_text(&text);
                copied.set(true);

                // Flip back to COPY after 1.5s. One tiny closure per click is
                // dropped on the floor via forget(); clicks are rare enough that
                // this matches the forget() idiom the other islands already use.
                let reset = Closure::wrap(Box::new(move || {
                    copied.set(false);
                }) as Box<dyn FnMut()>);
                let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(
                    reset.as_ref().unchecked_ref(),
                    1500,
                );
                reset.forget();
            }) as Box<dyn FnMut()>);

            let _ = btn.add_event_listener_with_callback("click", click.as_ref().unchecked_ref());
            click.forget();
        });
    }

    view! {
        <button
            node_ref=node
            type="button"
            class:hidden=move || !armed.get()
            class="absolute top-3 right-3 data hover:text-fg transition-colors cursor-pointer"
            aria-live="polite"
        >
            {move || if copied.get() { "COPIED" } else { "COPY" }}
        </button>
    }
}
