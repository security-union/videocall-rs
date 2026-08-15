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

use crate::components::Reveal::RevealOnView;
use leptos::prelude::*;

/// System band, told as an exploded 2.5D stack rather than a flat spec list.
/// Five isometric slabs — clients on top, control plane at the bottom — read in
/// data-flow order, and on desktop a scroll-spy lights the slab that matches the
/// description block nearest the viewport centre. Without JavaScript, under
/// `prefers-reduced-motion`, and on mobile the whole stack renders fully lit and
/// the blocks read as a plain document (the honest, static fallback).
///
/// The five layers, top (nearest the user) to bottom (nearest the metal):
///   01 Clients · 02 Media pipeline · 03 Transport · 04 Media plane · 05 Control
/// Every fact below matches a verified claim already made elsewhere on the site.
const LAYERS: [(&str, &str, &str, &str); 5] = [
    (
        "01",
        "Clients",
        "Browser to embedded board",
        "A Dioxus web client in the browser, compiled to WebAssembly — no install. videocall-cli streams from embedded Linux boards like the Raspberry Pi and Jetson.",
    ),
    (
        "02",
        "Media pipeline",
        "Codecs written in Rust",
        "Opus audio and VP9 video, encoded and decoded in pure Rust. A NetEQ adaptive jitter buffer runs in every browser client.",
    ),
    (
        "03",
        "Transport",
        "QUIC first, WebSocket as backup",
        "WebTransport over QUIC where the network allows it, an automatic WebSocket fallback where it does not. No ICE, STUN, TURN, or SDP.",
    ),
    (
        "04",
        "Media plane",
        "One publisher, every relay",
        "Relay servers forward every media frame over the NATS mesh. One publisher, every subscriber — the mesh in the band above.",
    ),
    (
        "05",
        "Control plane",
        "Auth and lifecycle, kept separate",
        "A separate meeting-api handles auth and SSO, meeting lifecycle, host controls, and the waiting room. Prometheus metrics export across the stack.",
    ),
];

#[component]
pub fn SystemSection() -> impl IntoView {
    view! {
        <section id="system" aria-labelledby="system-title" class="px-6 md:px-10 py-16 md:py-24">
            <div class="max-w-content mx-auto">
                <RevealOnView class="">
                    <p class="section-index" aria-hidden="true">"03 — System"</p>
                    <h2 id="system-title" class="text-h2 text-fg mt-4">"One system, end to end"</h2>
                    <p class="text-body-lg text-fg-2 mt-4 max-w-xl">
                        "A media plane forwards media over NATS. A separate control plane handles auth, meeting lifecycle, host controls, and the waiting room. Browser, native, and CLI clients. Pure-Rust Opus and VP9. Prometheus metrics."
                    </p>
                </RevealOnView>

                <SystemStack/>
            </div>
        </section>
    }
}

/// The interactive layered stack. One island: a sticky SVG of five isometric
/// slabs beside five description blocks. A single `IntersectionObserver` watches
/// the blocks (desktop, motion allowed) and drives one shared `active` signal;
/// CSS does every visual change off the `data-active` attribute it writes.
///
/// `active = None` is the neutral, all-lit state. It is what SSR renders (the
/// attribute is simply absent), what no-JS keeps, and what the island leaves in
/// place under reduced-motion or below the `lg` breakpoint — so the fallback is
/// always a fully-lit, static stack with no dimming and no animation.
#[island]
pub fn SystemStack() -> impl IntoView {
    // Active layer index; None ≡ every slab lit (the SSR / fallback state).
    let active = RwSignal::new(Option::<usize>::None);

    // One node ref per description block — the scroll-spy's observation targets.
    let block_refs: [NodeRef<leptos::html::Div>; 5] = [
        NodeRef::new(),
        NodeRef::new(),
        NodeRef::new(),
        NodeRef::new(),
        NodeRef::new(),
    ];

    #[cfg(feature = "hydrate")]
    {
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;

        Effect::new(move |_| {
            let Some(win) = web_sys::window() else {
                return;
            };

            // Reduced-motion: wire nothing; the stack stays fully lit and still.
            if let Ok(Some(mql)) = win.match_media("(prefers-reduced-motion: reduce)") {
                if mql.matches() {
                    return;
                }
            }

            // Scroll-spy is a desktop (lg+) affordance. On narrow viewports the
            // stack renders once, non-interactive, all layers lit.
            match win.match_media("(min-width: 1024px)") {
                Ok(Some(mql)) if mql.matches() => {}
                _ => return,
            }

            // Reading every ref subscribes this effect to their mounts, so it
            // re-runs and proceeds once the blocks hydrate.
            let mut els = Vec::with_capacity(block_refs.len());
            for r in block_refs.iter() {
                let Some(el) = r.get() else {
                    return;
                };
                els.push(el);
            }

            // The block whose centre band the viewport centre crosses becomes
            // active. A thin center strip (rootMargin shrinks the root to ~4%
            // tall through the middle) keeps at most one block intersecting.
            let cb = Closure::wrap(Box::new(
                move |entries: js_sys::Array, _obs: web_sys::IntersectionObserver| {
                    for entry in entries.iter() {
                        let entry: web_sys::IntersectionObserverEntry = entry.unchecked_into();
                        if entry.is_intersecting() {
                            if let Some(idx) = entry
                                .target()
                                .get_attribute("data-i")
                                .and_then(|s| s.parse::<usize>().ok())
                            {
                                active.set(Some(idx));
                            }
                        }
                    }
                },
            )
                as Box<dyn FnMut(js_sys::Array, web_sys::IntersectionObserver)>);

            let opts = web_sys::IntersectionObserverInit::new();
            opts.set_root_margin("-48% 0px -48% 0px");
            if let Ok(io) =
                web_sys::IntersectionObserver::new_with_options(cb.as_ref().unchecked_ref(), &opts)
            {
                for el in &els {
                    io.observe(el);
                }
                // A persistent scroll-spy for a page-lifetime section: keep the
                // observer and its callback alive; nothing to disconnect.
                cb.forget();
                std::mem::forget(io);
            }
        });
    }

    // `data-active` is present only once the scroll-spy sets an index. Absent on
    // SSR / no-JS / reduced-motion / mobile → CSS leaves every slab fully lit.
    let data_active = move || active.get().map(|i| i.to_string());

    view! {
        <div class="stack-scrolly mt-10 md:mt-14" data-active=data_active>
            <div class="stack-sticky" aria-hidden="true">
                <StackSvg/>
            </div>
            <div class="stack-blocks">
                {LAYERS
                    .iter()
                    .enumerate()
                    .map(|(i, (idx, label, title, fact))| {
                        view! {
                            <div class="stack-block" data-i=i.to_string() node_ref=block_refs[i]>
                                <p class="section-index stack-block-label">
                                    {*idx}" — "{*label}
                                </p>
                                <h3 class="text-h3 text-fg mt-3">{*title}</h3>
                                <p class="text-sm text-fg-2 leading-relaxed mt-3 max-w-md">
                                    {*fact}
                                </p>
                            </div>
                        }
                    })
                    .collect_view()}
            </div>
        </div>
    }
}

/// The exploded stack itself: five isometric slabs (flat parallelogram lid +
/// two thin side faces), stacked with vertical gaps and joined by thin vertical
/// connectors down which data flows. Purely geometric and decorative — the SVG
/// is `aria-hidden`; the description blocks are the real content. Every stateful
/// visual (lift, dim, oxide edge, marching connector) is CSS keyed off the
/// parent `data-active`, so this markup is static.
#[component]
fn StackSvg() -> impl IntoView {
    // 2.5D projection constants. The lid is a parallelogram shifted right by
    // SKEW over its depth; THICK is the visible side-face height; PITCH is the
    // slab-to-slab vertical stride (bigger than the slab so gaps show).
    const X0: f64 = 56.0;
    const W: f64 = 200.0;
    const SKEW: f64 = 32.0;
    const DEPTH: f64 = 40.0;
    const THICK: f64 = 13.0;
    const PITCH: f64 = 84.0;
    const Y0: f64 = 22.0;
    let conn_x = X0 + W / 2.0;

    view! {
        <svg
            class="stack-svg"
            viewBox="0 0 320 424"
            fill="none"
            role="presentation"
            aria-hidden="true"
        >
            // Connectors first, so the slabs paint over their ends.
            {(0..4)
                .map(|i| {
                    let y_bottom = Y0 + i as f64 * PITCH + DEPTH + THICK;
                    let y_next = Y0 + (i as f64 + 1.0) * PITCH;
                    view! {
                        <line
                            class="stack-connector"
                            data-i=i.to_string()
                            x1=conn_x
                            y1=y_bottom
                            x2=conn_x
                            y2=y_next
                        ></line>
                    }
                })
                .collect_view()}

            {(0..5)
                .map(|i| {
                    let y = Y0 + i as f64 * PITCH;
                    // Lid parallelogram (top-left, top-right, bottom-right, bottom-left).
                    let (tlx, tly) = (X0 + SKEW, y);
                    let (trx, try_) = (X0 + SKEW + W, y);
                    let (brx, bry) = (X0 + W, y + DEPTH);
                    let (blx, bly) = (X0, y + DEPTH);
                    // Dropped edges for the two side faces.
                    let (brdx, brdy) = (X0 + W, y + DEPTH + THICK);
                    let (bldx, bldy) = (X0, y + DEPTH + THICK);
                    let (trdx, trdy) = (X0 + SKEW + W, y + THICK);

                    let front = format!(
                        "M{blx} {bly} L{brx} {bry} L{brdx} {brdy} L{bldx} {bldy} Z",
                    );
                    let right = format!(
                        "M{trx} {try_} L{brx} {bry} L{brdx} {brdy} L{trdx} {trdy} Z",
                    );
                    let lid = format!(
                        "M{tlx} {tly} L{trx} {try_} L{brx} {bry} L{blx} {bly} Z",
                    );
                    view! {
                        <g class="stack-slab" data-i=i.to_string()>
                            // Front face (darkest) and right face (mid) give thickness.
                            <path d=front fill="var(--bg)" stroke="var(--line)" stroke-width="1"></path>
                            <path
                                d=right
                                fill="var(--surface-1)"
                                stroke="var(--line)"
                                stroke-width="1"
                            ></path>
                            // Lid (lightest) doubles as the oxide-able active edge.
                            <path
                                class="stack-edge"
                                d=lid
                                fill="var(--surface-2)"
                                stroke="var(--line-strong)"
                                stroke-width="1"
                            ></path>
                            <text
                                class="font-mono"
                                x=X0 - 26.0
                                y=y + DEPTH - 2.0
                                font-size="11"
                                letter-spacing="1"
                                fill="var(--fg-3)"
                            >
                                {LAYERS[i].0}
                            </text>
                        </g>
                    }
                })
                .collect_view()}
        </svg>
    }
}
