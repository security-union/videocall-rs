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

/// Full-bleed media band carrying the media-plane story: a relay globe video
/// framed on black, with a contained left-set header and mono corner captions.
/// The globe loops; the existing `RelayGlobeArt` SVG is the reduced-motion /
/// no-JS fallback (and the webm is never fetched in that path).
#[component]
pub fn GlobeBand() -> impl IntoView {
    view! {
        <section class="media-band" aria-labelledby="globe-title">
            <div class="max-w-content mx-auto px-6 md:px-10 mb-8 md:mb-10">
                <RevealOnView class="">
                    <p class="section-index" aria-hidden="true">"03 — Mesh plane"</p>
                    <h2 id="globe-title" class="text-h2 text-fg mt-4 max-w-4xl">
                        "Relays around the world, one mesh"
                    </h2>
                </RevealOnView>
            </div>

            <div class="media-frame">
                <MediaVideo
                    poster="/videos/relay-globe-poster.jpg"
                    src="/videos/relay-globe.webm"
                    rotation=""
                    loop_video=true
                >
                    <RelayGlobeArt/>
                </MediaVideo>
                <span class="media-caption absolute bottom-3 right-3 text-right" aria-hidden="true">
                    "One publisher · every subscriber"
                </span>
            </div>

            // How the media plane actually uses NATS. A contained editorial row
            // in the site voice: mono micro-labels + one plain fact each,
            // hairline seams only (no cards, no borders, no bento). Width is
            // capped near the frame's optical width and centered with it — the
            // band keeps ONE alignment system. Items keep vertical padding at
            // every breakpoint so the top hairline never crowds the labels.
            <div class="max-w-4xl mx-auto px-6 md:px-10 mt-8 md:mt-10">
                <RevealOnView class="">
                    <ul class="grid md:grid-cols-3 border-t border-line divide-y md:divide-y-0 md:divide-x divide-line">
                        <li class="py-6 md:px-8 md:first:pl-0 md:last:pr-0">
                            <p class="section-index text-sm">"Subject per meeting"</p>
                            <p class="text-base text-fg-2 leading-loose mt-3 max-w-sm">
                                "Each meeting is a NATS subject. A relay publishes a frame once and every relay subscribed to that meeting receives it."
                            </p>
                        </li>
                        <li class="py-6 md:px-8 md:first:pl-0 md:last:pr-0">
                            <p class="section-index text-sm">"Relay to relay"</p>
                            <p class="text-base text-fg-2 leading-loose mt-3 max-w-sm">
                                "Participants can land on different relay servers anywhere in the world. NATS carries the media between them, so they still share one meeting."
                            </p>
                        </li>
                        <li class="py-6 md:px-8 md:first:pl-0 md:last:pr-0">
                            <p class="section-index text-sm">"Scale out"</p>
                            <p class="text-base text-fg-2 leading-loose mt-3 max-w-sm">
                                "Add relay servers to add capacity. WebSocket and WebTransport relays scale independently behind a load balancer. Demonstrated with 1000 participants in a single meeting."
                            </p>
                        </li>
                    </ul>

                    // Attribution, done the way vendors do it: a quiet eyebrow
                    // and the official mark, linked. The color logo's white
                    // letterforms read cleanly on the near-black ground.
                    <div class="border-t border-line mt-2 pt-8 pb-2 flex items-center justify-center gap-5">
                        <span class="section-index">"Powered by"</span>
                        <a
                            href="https://nats.io"
                            target="_blank"
                            rel="noopener noreferrer"
                            class="opacity-80 hover:opacity-100 transition-opacity"
                            aria-label="NATS.io"
                        >
                            <img
                                src="/images/nats-horizontal-color.png"
                                alt="NATS"
                                width="360"
                                height="93"
                                loading="lazy"
                                decoding="async"
                                class="h-7 w-auto"
                            />
                        </a>
                    </div>
                </RevealOnView>
            </div>
        </section>
    }
}

/// The one media-band island: lazily swaps a `<video>` in over a static
/// fallback, and gates playback so nothing decodes off-screen.
///
/// On the server and with JavaScript off, only the fallback (`children`) is in
/// the DOM — an SVG for the globe, the mascot PNG for the rover — so the page
/// is complete and the webm is never fetched. On hydrate the island checks
/// `prefers-reduced-motion`: if the user asked for less motion it does nothing
/// and the fallback stays; otherwise it flips `activate`, which mounts the
/// `<video>` (its `poster` is the only image that loads). A second effect wires
/// the video once it mounts:
///   - an `IntersectionObserver` plays it only while in view and pauses it
///     off-screen (mirrors the marquee / live-call idiom),
///   - `visibilitychange` pauses it while the tab is hidden,
///   - for a rotation set (rover), `ended` picks a *different* clip uniformly
///     at random, points `src` at it, and plays — so only the active clip is
///     ever fetched, capped at the current + next.
#[island]
pub fn MediaVideo(
    /// Poster shown until the video can paint. Also the reduced-motion image
    /// budget — under motion it is the only asset that loads before play.
    #[prop(into)]
    poster: String,
    /// Initial (and, when `rotation` is empty, only) clip.
    #[prop(into)]
    src: String,
    /// Comma-separated rotation set. Empty means "just loop `src`".
    #[prop(into)]
    rotation: String,
    /// Whether the `<video>` loops itself (globe) or is rotated on `ended`
    /// (rover).
    loop_video: bool,
    /// The reduced-motion / no-JS fallback rendered inside the frame (SVG or
    /// `<img>`). Always present in the DOM; hidden once the video takes over.
    children: Children,
) -> impl IntoView {
    // `activate` starts false so SSR and the reduced-motion path render only the
    // fallback — zero video bytes until motion is confirmed allowed.
    let activate = RwSignal::new(false);
    let video_ref: NodeRef<leptos::html::Video> = NodeRef::new();

    #[cfg(feature = "hydrate")]
    {
        use std::cell::Cell;
        use std::rc::Rc;
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;

        // Effect 1 — decide activation. Reduced-motion keeps the static frame.
        Effect::new(move |_| {
            let Some(win) = web_sys::window() else {
                return;
            };
            if let Ok(Some(mql)) = win.match_media("(prefers-reduced-motion: reduce)") {
                if mql.matches() {
                    return;
                }
            }
            activate.set(true);
        });

        // Effect 2 — wire the video once it mounts (after `activate` flips).
        let clips: Vec<String> = rotation
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        Effect::new(move |_| {
            let Some(video) = video_ref.get() else {
                return;
            };
            let Some(win) = web_sys::window() else {
                return;
            };
            let Some(doc) = win.document() else {
                return;
            };

            video.set_muted(true);
            // `playsinline` keeps iOS Safari from taking the clip fullscreen;
            // set as an attribute (this web-sys build has no `set_plays_inline`).
            let _ = video.set_attribute("playsinline", "");
            video.set_loop(loop_video);

            let on_screen = Rc::new(Cell::new(false));

            // Rover rotation: hop to a different clip at random on `ended`.
            if clips.len() > 1 {
                let idx = Rc::new(Cell::new(0usize));
                let ended_cb = Closure::wrap(Box::new({
                    let video = video.clone();
                    let clips = clips.clone();
                    let idx = idx.clone();
                    move || {
                        let cur = idx.get();
                        // Uniform over the OTHER clips: offset 1..len from the
                        // current index, so the finished clip never repeats and
                        // the remaining clips are equally likely.
                        let offset =
                            1 + (js_sys::Math::random() * (clips.len() - 1) as f64) as usize;
                        let next = (cur + offset.min(clips.len() - 1)) % clips.len();
                        idx.set(next);
                        video.set_src(&clips[next]);
                        video.load();
                        let _ = video.play();
                    }
                }) as Box<dyn FnMut()>);
                let _ = video
                    .add_event_listener_with_callback("ended", ended_cb.as_ref().unchecked_ref());
                ended_cb.forget();
            }

            // Play only while in view; pause off-screen.
            let io_cb = Closure::wrap(Box::new({
                let video = video.clone();
                let on_screen = on_screen.clone();
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
                        let _ = video.play();
                    } else {
                        let _ = video.pause();
                    }
                }
            })
                as Box<dyn FnMut(js_sys::Array, web_sys::IntersectionObserver)>);
            if let Ok(io) = web_sys::IntersectionObserver::new(io_cb.as_ref().unchecked_ref()) {
                io.observe(&video);
                // Page-lifetime observer for a page-lifetime band.
                io_cb.forget();
                std::mem::forget(io);
            }

            // Pause while the tab is hidden; resume only if also on-screen.
            let vis_cb = Closure::wrap(Box::new({
                let doc = doc.clone();
                let video = video.clone();
                let on_screen = on_screen.clone();
                move || {
                    if doc.hidden() {
                        let _ = video.pause();
                    } else if on_screen.get() {
                        let _ = video.play();
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

    // `rotation` and `loop_video` only drive the hydrate-side playback wiring;
    // the SSR / CSR frame renders from the fallback + poster alone.
    #[cfg(not(feature = "hydrate"))]
    let _ = (&rotation, loop_video);

    let poster_v = poster.clone();
    let src_v = src.clone();
    // `contents` wrapper: a single island root that generates no box, so the
    // absolutely-positioned fallback and video still anchor to `.media-frame`.
    view! {
        <div class="contents">
            // Fallback: always in the DOM (SSR / no-JS / reduced-motion), hidden
            // once the video takes over.
            <div class="absolute inset-0" class:hidden=move || activate.get()>
                {children()}
            </div>
            {move || {
                activate
                    .get()
                    .then(|| {
                        view! {
                            <video
                                node_ref=video_ref
                                class="absolute inset-0 w-full h-full object-cover"
                                poster=poster_v.clone()
                                src=src_v.clone()
                                preload="none"
                                aria-hidden="true"
                            ></video>
                        }
                    })
            }}
        </div>
    }
}

/// NATS pub/sub as a relay globe: an orthographic wireframe world with relay
/// nodes at real city positions, media arcing between them along the marching
/// packet-dash idiom. New York is the publisher (oxide, pulsing — the one
/// licensed live-source marker); the rest are subscriber relays that flash as
/// media arrives. Relocated from the retired System bento; it is the globe
/// band's reduced-motion / no-JS fallback.
///
/// Projection (orthographic, R=100), centered at lat0 = 25°N, lon0 = -40°W:
///   Δlon = lon − lon0
///   x =  R·cos(lat)·sin(Δlon)
///   y = −R·(cos(lat0)·sin(lat) − sin(lat0)·cos(lat)·cos(Δlon))
///   visible ⇔ sin(lat0)·sin(lat) + cos(lat0)·cos(lat)·cos(Δlon) > 0
/// The six coordinates below are that formula evaluated once and hardcoded
/// (New York, Mexico City, São Paulo, London, Frankfurt, Lagos — all visible).
#[component]
fn RelayGlobeArt() -> impl IntoView {
    // Projected (x, y, is_source). Source is New York.
    let nodes: [(f64, f64, bool); 6] = [
        (-42.4, -32.5, true), // New York   (40.7, -74.0)
        (-81.0, -9.7, false), // Mexico City(19.4, -99.1)
        (-10.5, 74.6, false), // São Paulo  (-23.5, -46.6)
        (39.9, -50.7, false), // London     (51.5, -0.1)
        (48.2, -51.6, false), // Frankfurt  (50.1, 8.7)
        (68.3, 20.3, false),  // Lagos      (6.5, 3.4)
    ];

    // Media arcs (quadratic béziers) bulging outward from the globe surface:
    // (x0, y0, cx, cy, x1, y1). Control points sit radially outside each chord.
    let arcs: [(f64, f64, f64, f64, f64, f64); 5] = [
        (-42.4, -32.5, -2.0, -66.0, 39.9, -50.7), // New York → London
        (-42.4, -32.5, -82.5, -28.2, -81.0, -9.7), // New York → Mexico City
        (-42.4, -32.5, -45.2, 36.0, -10.5, 74.6), // New York → São Paulo
        (39.9, -50.7, 54.5, -63.3, 48.2, -51.6),  // London → Frankfurt
        (48.2, -51.6, 79.5, -21.4, 68.3, 20.3),   // Frankfurt → Lagos
    ];

    // Longitude meridians (ry = 100, rx shrinking) and latitude parallels
    // (rx = 100, ry shrinking) — the standard orthographic wireframe look.
    let longitudes: [f64; 3] = [26.0, 54.0, 82.0];
    let latitudes: [f64; 2] = [32.0, 64.0];

    view! {
        <svg viewBox="-120 -120 240 240" class="w-full h-full" fill="none" aria-hidden="true">
            // Globe body + graticule (static).
            <circle cx="0" cy="0" r="100" fill="var(--surface-2)" opacity="0.45"></circle>
            {longitudes
                .into_iter()
                .map(|rx| {
                    view! {
                        <ellipse cx="0" cy="0" rx=rx ry="100" stroke="var(--line)" stroke-width="0.75"></ellipse>
                    }
                })
                .collect_view()}
            {latitudes
                .into_iter()
                .map(|ry| {
                    view! {
                        <ellipse cx="0" cy="0" rx="100" ry=ry stroke="var(--line)" stroke-width="0.75"></ellipse>
                    }
                })
                .collect_view()}
            <line x1="-100" y1="0" x2="100" y2="0" stroke="var(--line)" stroke-width="0.75"></line>
            <circle cx="0" cy="0" r="100" stroke="var(--line-strong)" stroke-width="1"></circle>

            // Media arcs — marching dashes from publisher to relays.
            {arcs
                .into_iter()
                .enumerate()
                .map(|(i, (x0, y0, cx, cy, x1, y1))| {
                    view! {
                        <path
                            class="packet-path"
                            d=format!("M{x0} {y0} Q{cx} {cy} {x1} {y1}")
                            stroke="var(--fg-3)"
                            stroke-width="1.25"
                            style=format!("animation-delay:{}ms", i as i32 * -300)
                        ></path>
                    }
                })
                .collect_view()}

            // Relay nodes — publisher pulses oxide; subscribers flash on arrival.
            {nodes
                .into_iter()
                .enumerate()
                .map(|(i, (x, y, is_source))| {
                    if is_source {
                        view! {
                            <circle class="source-node" cx=x cy=y r="4.5" fill="var(--signal)"></circle>
                        }
                        .into_any()
                    } else {
                        view! {
                            <circle
                                class="arrive-node"
                                cx=x
                                cy=y
                                r="3.5"
                                fill="var(--fg-2)"
                                style=format!("animation-delay:{}ms", i as i32 * -260)
                            ></circle>
                        }
                        .into_any()
                    }
                })
                .collect_view()}
        </svg>
    }
}
