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

/// The hero's single aliveness island: a mock of a live videocall.rs call.
///
/// The whole visual — speaking glow, audio meters, REC blink, LIVE dot — is
/// pure CSS keyframes/transitions. This island does only two things on a timer:
/// advance the round-robin "speaking" tile (a class toggle the CSS transitions)
/// and random-walk the telemetry readout. Both timers are paused while the
/// panel is off-screen or the tab is hidden, and never start at all under
/// `prefers-reduced-motion` — the SSR markup already renders the final static
/// frame (rover speaking, fixed telemetry), so hydration only *starts* motion
/// and there is no layout shift.
#[island]
pub fn LiveCallPanel() -> impl IntoView {
    // Initial signal values ARE the reduced-motion / static final frame, so SSR
    // and the reduced path render identically with no flash.
    let active = RwSignal::new(0usize); // rover tile speaking
    let rtt = RwSignal::new(24i32); // ms
    let mbps10 = RwSignal::new(21i32); // tenths of Mbps -> 2.1
    let jitter = RwSignal::new(4i32); // ms
    let rec = RwSignal::new(41i32); // seconds -> 00:41

    let node: NodeRef<leptos::html::Figure> = NodeRef::new();

    #[cfg(feature = "hydrate")]
    {
        use std::cell::Cell;
        use std::rc::Rc;
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;

        // Random walk one reading within a believable band; step is the max
        // per-tick delta. Illustrative telemetry, never a measured benchmark.
        fn walk(cur: i32, lo: i32, hi: i32, step: i32) -> i32 {
            let span = (2 * step + 1) as f64;
            let delta = (js_sys::Math::random() * span).floor() as i32 - step;
            (cur + delta).clamp(lo, hi)
        }

        Effect::new(move |_| {
            let Some(el) = node.get() else {
                return;
            };
            let Some(win) = web_sys::window() else {
                return;
            };

            // Reduced motion: keep the static frame, start no timers.
            if let Ok(Some(mql)) = win.match_media("(prefers-reduced-motion: reduce)") {
                if mql.matches() {
                    return;
                }
            }
            let Some(doc) = win.document() else {
                return;
            };

            // The fleet's camera feeds live inside this panel; the same start/stop
            // lifecycle below drives every feed's playback (no separate observer).
            // Empty under no-JS/reduced-motion paths, so an empty Vec is fine — each
            // feed's poster (a static frame) simply stays.
            let videos: Rc<Vec<web_sys::HtmlVideoElement>> = Rc::new({
                let mut v = Vec::new();
                if let Ok(list) = el.query_selector_all(".lcp-feed") {
                    for i in 0..list.length() {
                        if let Some(node) = list.item(i) {
                            if let Ok(video) = node.dyn_into::<web_sys::HtmlVideoElement>() {
                                v.push(video);
                            }
                        }
                    }
                }
                v
            });

            let glow_id: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));
            let tel_id: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));
            let on_screen = Rc::new(Cell::new(false));

            // 2200ms: advance the speaking tile round-robin.
            let glow_cb: Rc<Closure<dyn FnMut()>> = Rc::new(Closure::wrap(Box::new(move || {
                active.update(|a| *a = (*a + 1) % 4);
            })
                as Box<dyn FnMut()>));

            // 1000ms: random-walk the four readouts, tick the REC clock.
            let tel_cb: Rc<Closure<dyn FnMut()>> = Rc::new(Closure::wrap(Box::new(move || {
                rtt.update(|v| *v = walk(*v, 18, 34, 3));
                mbps10.update(|v| *v = walk(*v, 18, 26, 2));
                jitter.update(|v| *v = walk(*v, 2, 9, 2));
                rec.update(|v| *v = (*v + 1) % 3600);
            })
                as Box<dyn FnMut()>));

            let start: Rc<dyn Fn()> = {
                let win = win.clone();
                let glow_cb = glow_cb.clone();
                let tel_cb = tel_cb.clone();
                let glow_id = glow_id.clone();
                let tel_id = tel_id.clone();
                let videos = videos.clone();
                Rc::new(move || {
                    for v in videos.iter() {
                        // `play()` returns a Promise that can reject (e.g. the
                        // tab lost autoplay eligibility); nothing to do, drop it.
                        let _ = v.play();
                    }
                    if glow_id.get().is_none() {
                        if let Ok(id) = win.set_interval_with_callback_and_timeout_and_arguments_0(
                            (*glow_cb).as_ref().unchecked_ref(),
                            2200,
                        ) {
                            glow_id.set(Some(id));
                        }
                    }
                    if tel_id.get().is_none() {
                        if let Ok(id) = win.set_interval_with_callback_and_timeout_and_arguments_0(
                            (*tel_cb).as_ref().unchecked_ref(),
                            1000,
                        ) {
                            tel_id.set(Some(id));
                        }
                    }
                })
            };
            let stop: Rc<dyn Fn()> = {
                let win = win.clone();
                let glow_id = glow_id.clone();
                let tel_id = tel_id.clone();
                let videos = videos.clone();
                Rc::new(move || {
                    for v in videos.iter() {
                        let _ = v.pause();
                    }
                    if let Some(id) = glow_id.take() {
                        win.clear_interval_with_handle(id);
                    }
                    if let Some(id) = tel_id.take() {
                        win.clear_interval_with_handle(id);
                    }
                })
            };

            // Pause/resume with viewport visibility.
            let io_cb = Closure::wrap(Box::new({
                let on_screen = on_screen.clone();
                let start = start.clone();
                let stop = stop.clone();
                move |entries: js_sys::Array, _obs: web_sys::IntersectionObserver| {
                    let mut visible = false;
                    for entry in entries.iter() {
                        let entry: web_sys::IntersectionObserverEntry = entry.unchecked_into();
                        if entry.is_intersecting() {
                            visible = true;
                        }
                    }
                    on_screen.set(visible);
                    if visible {
                        start();
                    } else {
                        stop();
                    }
                }
            })
                as Box<dyn FnMut(js_sys::Array, web_sys::IntersectionObserver)>);
            if let Ok(io) = web_sys::IntersectionObserver::new(io_cb.as_ref().unchecked_ref()) {
                io.observe(&el);
                // Page-lifetime observer for a page-lifetime panel.
                io_cb.forget();
                std::mem::forget(io);
            }

            // Pause/resume with tab visibility (only resume if also on-screen).
            let vis_cb = Closure::wrap(Box::new({
                let doc = doc.clone();
                let on_screen = on_screen.clone();
                let start = start.clone();
                let stop = stop.clone();
                move || {
                    if doc.hidden() {
                        stop();
                    } else if on_screen.get() {
                        start();
                    }
                }
            }) as Box<dyn FnMut()>);
            let _ = doc.add_event_listener_with_callback(
                "visibilitychange",
                vis_cb.as_ref().unchecked_ref(),
            );
            vis_cb.forget();
        });
    }

    // ---- markup: SSR renders the final static frame; hydration adds motion --
    let telemetry = move || {
        format!(
            "RTT {} ms · {}.{} Mbps · JITTER {} ms",
            rtt.get(),
            mbps10.get() / 10,
            mbps10.get() % 10,
            jitter.get(),
        )
    };
    let rec_clock = move || format!("REC {:02}:{:02}", rec.get() / 60, rec.get() % 60);

    // Tile config: (caption, feed) — the four-robot Mars fleet, each tile a live
    // camera feed. UNIT 01 keeps its original mascot clip; 02–04 are field footage.
    let tiles = [
        ("UNIT 01", TileSubject::Rover),
        (
            "UNIT 02 · FPV",
            TileSubject::Feed {
                src: "/videos/unit-02-fpv.webm",
                poster: "/videos/unit-02-fpv-poster.jpg",
            },
        ),
        (
            "UNIT 03 · SCOUT",
            TileSubject::Feed {
                src: "/videos/unit-03-aerial.webm",
                poster: "/videos/unit-03-aerial-poster.jpg",
            },
        ),
        (
            "UNIT 04 · MAST",
            TileSubject::Feed {
                src: "/videos/unit-04-mast.webm",
                poster: "/videos/unit-04-mast-poster.jpg",
            },
        ),
    ];

    view! {
        <figure
            node_ref=node
            role="img"
            aria-label="An animated mock of a live videocall.rs call: four robot camera feeds from a field fleet, with live audio meters and connection telemetry."
            class="lcp-cv border-y border-line bg-bg-code overflow-hidden"
        >
            // Header row.
            <div
                class="flex items-center justify-between gap-3 px-4 py-2.5 border-b border-line font-mono text-[11px] md:text-xs text-fg-3"
                aria-hidden="true"
            >
                <span class="flex items-center gap-2">
                    <span class="live-dot"></span>
                    <span class="text-signal tracking-wider">"LIVE"</span>
                    <span class="hidden sm:inline">"control-room"</span>
                </span>
                <span class="tabular-nums tracking-wide">{telemetry}</span>
            </div>

            // Participant tiles.
            <div class="grid grid-cols-2 lg:grid-cols-4 gap-px bg-line" aria-hidden="true">
                {tiles
                    .into_iter()
                    .enumerate()
                    .map(|(idx, (caption, subject))| {
                        view! {
                            <div
                                class="lcp-tile relative bg-bg-s1 border border-line aspect-[4/3] overflow-hidden"
                                class:is-active=move || active.get() == idx
                            >
                                <div class="absolute inset-0 flex items-center justify-center bg-bg-s2">
                                    {subject.into_view()}
                                </div>

                                {(idx == 0)
                                    .then(|| {
                                        view! {
                                            <span class="absolute top-1.5 right-1.5 flex items-center gap-1 font-mono text-[9px] text-signal">
                                                <span class="rec-dot">"\u{25CF}"</span>
                                                "REC"
                                            </span>
                                        }
                                    })}

                                <span class="absolute bottom-1.5 left-2 font-mono text-[10px] text-fg-3 tracking-wider">
                                    {caption}
                                </span>

                                <div class="absolute bottom-1.5 right-2 flex items-end gap-[3px] h-5">
                                    <span class="lcp-bar w-[3px] h-full bg-fg-3"></span>
                                    <span class="lcp-bar w-[3px] h-full bg-fg-3"></span>
                                    <span class="lcp-bar w-[3px] h-full bg-fg-3"></span>
                                    <span class="lcp-bar w-[3px] h-full bg-fg-3"></span>
                                </div>
                            </div>
                        }
                    })
                    .collect_view()}
            </div>

            // Footer row.
            <div
                class="flex items-center justify-between gap-3 px-4 py-2.5 border-t border-line font-mono text-[11px] md:text-xs text-fg-3"
                aria-hidden="true"
            >
                <span class="tracking-wide truncate">"4 CONNECTED · WEBTRANSPORT / QUIC · TLS 1.3"</span>
                <span class="flex items-center gap-1.5 flex-shrink-0">
                    <span class="rec-dot text-signal">"\u{25CF}"</span>
                    <span class="tabular-nums">{rec_clock}</span>
                </span>
            </div>
        </figure>
    }
}

/// The camera feed inside a fleet tile. Every feed is started/stopped by the
/// island's lifecycle (IntersectionObserver + visibilitychange) and never plays
/// under reduced motion or no-JS, where the poster (a static frame) shows
/// instead. All clips are small looping webm; no `autoplay` attribute.
#[derive(Clone, Copy)]
enum TileSubject {
    /// UNIT 01's mascot clip — a near-square render letterboxed against the tile
    /// with `object-contain` so its dark backdrop reads as the feed.
    Rover,
    /// UNITs 02–04 — real 4:3 field footage that fills the tile with
    /// `object-cover` (matched aspect, so no crop of note and no letterbox).
    Feed {
        src: &'static str,
        poster: &'static str,
    },
}

impl TileSubject {
    fn into_view(self) -> AnyView {
        match self {
            TileSubject::Rover => view! {
                <video
                    class="lcp-feed w-full h-full object-contain p-2"
                    width="640"
                    height="598"
                    src="/videos/rover-wiggle.webm"
                    poster="/images/rover-mascot.png"
                    loop=true
                    muted=true
                    playsinline=true
                    preload="none"
                    aria-hidden="true"
                ></video>
            }
            .into_any(),
            TileSubject::Feed { src, poster } => view! {
                <video
                    class="lcp-feed w-full h-full object-cover"
                    src=src
                    poster=poster
                    loop=true
                    muted=true
                    playsinline=true
                    preload="none"
                    aria-hidden="true"
                ></video>
            }
            .into_any(),
        }
    }
}
