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

/// System bento band: an asymmetric grid where every tile maps to a real
/// subsystem that ships in the repo — the Dioxus web client, the transport
/// with QUIC + WebSocket fallback, the Opus/NetEQ audio pipeline, the pure-Rust
/// VP9 codec, the metrics stack, and the NATS pub/sub backbone. Each animation
/// is pure CSS / inline SVG (aria-hidden decoration); the caption + title +
/// factual one-liner carry the meaning.
#[component]
pub fn SystemSection() -> impl IntoView {
    view! {
        <section id="system" aria-labelledby="system-title" class="px-6 md:px-10 py-24 md:py-32">
            <div class="max-w-content mx-auto">
                <RevealOnView class="">
                    <p class="section-index" aria-hidden="true">"02 — System"</p>
                    <h2 id="system-title" class="text-h2 text-fg mt-4">"One system, end to end"</h2>
                    <p class="text-body-lg text-fg-2 mt-4 max-w-2xl">
                        "Not a codec you wire up yourself. A relay that forwards end-to-end-encrypted media over a NATS backbone, a browser and native and mobile client, the pure-Rust audio and video pipeline, and the metrics to see it all working."
                    </p>
                </RevealOnView>

                // 12-col bento; the NATS fan-out thesis tile is tall and leads.
                // The grid lives on a plain div INSIDE the island: children passed
                // across an island boundary are wrapped in an inline
                // <leptos-children> element, so grid classes on the island itself
                // would see a single 99px-wide grid item instead of the tiles.
                <RevealOnView class="mt-12">
                <div class="grid gap-px bg-line border border-line rounded-panel overflow-hidden md:grid-cols-2 lg:grid-cols-12">
                    <BentoTile
                        caption="MESSAGING · NATS PUB/SUB"
                        title="Relays around the world, one mesh"
                        body="A NATS pub/sub backbone moves end-to-end-encrypted media between relay servers around the world. One publisher, every subscriber."
                        span="md:col-span-2 lg:col-span-7 lg:row-span-2"
                    >
                        <RelayGlobeArt/>
                    </BentoTile>

                    <BentoTile
                        caption="AUDIO · OPUS + NETEQ"
                        title="First-class audio"
                        body="Opus encode in pure Rust, with a NetEQ adaptive jitter buffer in every browser to hold a call together on a bad network."
                        span="lg:col-span-5"
                    >
                        <WaveformArt/>
                    </BentoTile>

                    <BentoTile
                        caption="TRANSPORT · WEBTRANSPORT / QUIC"
                        title="QUIC, with a WebSocket fallback"
                        body="WebTransport over QUIC where the network allows it, an automatic WebSocket fallback where it does not. No ICE, STUN, TURN, or SDP."
                        span="lg:col-span-5"
                    >
                        <TransportArt/>
                    </BentoTile>

                    <BentoTile
                        caption="WEB UI · DIOXUS + WASM"
                        title="Browser client"
                        body="A meeting client compiled to WebAssembly with Dioxus. No install."
                        span="md:col-span-1 lg:col-span-4"
                    >
                        <WebUiArt/>
                    </BentoTile>

                    <BentoTile
                        caption="VIDEO · PURE-RUST VP9"
                        title="VP9 in pure Rust"
                        body="A VP9 encoder and decoder written in Rust. No native codec dependency."
                        span="md:col-span-1 lg:col-span-4"
                    >
                        <VideoArt/>
                    </BentoTile>

                    <BentoTile
                        caption="METRICS · PROMETHEUS / GRAFANA"
                        title="Observable by default"
                        body="Latency, bitrate, and connection health export to Prometheus and Grafana."
                        span="md:col-span-2 lg:col-span-4"
                    >
                        <SparklineArt/>
                    </BentoTile>
                </div>
                </RevealOnView>
            </div>
        </section>
    }
}

#[component]
fn BentoTile(
    caption: &'static str,
    title: &'static str,
    body: &'static str,
    span: &'static str,
    children: Children,
) -> impl IntoView {
    view! {
        <div class=format!(
            "relative bg-bg-s1 hover:bg-bg-s2 transition-colors p-5 md:p-6 pb-9 flex flex-col gap-3 min-h-[200px] {span}",
        )>
            <h3 class="text-h3 text-fg">{title}</h3>
            <p class="text-sm text-fg-2 leading-relaxed max-w-md">{body}</p>
            <div class="flex-1 flex items-center justify-center py-2 min-h-[80px]" aria-hidden="true">
                {children()}
            </div>
            <span class="section-index absolute bottom-3 left-5 md:left-6" aria-hidden="true">
                {caption}
            </span>
        </div>
    }
}

/// ~24 mirrored bars pulsing around a centerline — the audio pipeline.
#[component]
fn WaveformArt() -> impl IntoView {
    view! {
        <div class="flex items-center justify-center gap-[3px] h-16 w-full max-w-sm">
            {(0..24)
                .map(|_| view! { <span class="wave-bar w-[3px] h-full bg-fg-3 rounded-full"></span> })
                .collect_view()}
        </div>
    }
}

/// NATS pub/sub as a relay globe: an orthographic wireframe world with relay
/// nodes at real city positions, media arcing between them along the marching
/// packet-dash idiom. New York is the publisher (oxide, pulsing); the rest are
/// subscriber relays that flash as media arrives.
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
        <svg viewBox="-120 -120 240 240" class="w-full h-full max-h-56" fill="none" aria-hidden="true">
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

/// Transport tile: a QUIC path (active, marching dashes) above a WebSocket
/// fallback path (dim, static), between a client and a relay endpoint.
#[component]
fn TransportArt() -> impl IntoView {
    view! {
        <svg viewBox="0 0 240 104" class="w-full h-full max-h-32" fill="none" aria-hidden="true">
            <rect x="6" y="42" width="16" height="20" rx="2" fill="var(--fg-3)"></rect>
            <rect x="218" y="42" width="16" height="20" rx="2" fill="var(--fg-3)"></rect>

            // QUIC — active primary path.
            <path
                class="packet-path"
                d="M22 52 C86 22, 154 22, 218 52"
                stroke="var(--signal)"
                stroke-width="1.5"
            ></path>
            // WebSocket — dim static fallback path.
            <path
                d="M22 52 C86 82, 154 82, 218 52"
                stroke="var(--fg-4)"
                stroke-width="1.5"
                stroke-dasharray="3 5"
            ></path>

            <text x="120" y="14" text-anchor="middle" class="font-mono" font-size="8" letter-spacing="1" fill="var(--signal)">"QUIC"</text>
            <text x="120" y="99" text-anchor="middle" class="font-mono" font-size="8" letter-spacing="1" fill="var(--fg-3)">"WS FALLBACK"</text>
        </svg>
    }
}

/// VP9 tile: a mock frame whose macroblocks ripple as they "encode".
#[component]
fn VideoArt() -> impl IntoView {
    view! {
        <svg viewBox="0 0 240 120" class="w-full h-full max-h-32" fill="none" aria-hidden="true">
            <rect x="40" y="14" width="160" height="92" rx="3" stroke="var(--line-strong)" stroke-width="1"></rect>
            {(0..12)
                .map(|i| {
                    let col = i % 4;
                    let row = i / 4;
                    let x = 48 + col * 38;
                    let y = 22 + row * 28;
                    view! {
                        <rect
                            class="vp9-block"
                            x=x
                            y=y
                            width="34"
                            height="24"
                            rx="1.5"
                            fill="var(--fg-3)"
                            style=format!("animation-delay:{}ms", i * -140)
                        ></rect>
                    }
                })
                .collect_view()}
        </svg>
    }
}

/// Web UI tile: a mock Dioxus meeting frame — a participant grid with one
/// active speaker pulsing, over a control bar.
#[component]
fn WebUiArt() -> impl IntoView {
    view! {
        <div class="w-full max-w-[220px] border border-line-strong rounded bg-bg-s2 p-2">
            <div class="grid grid-cols-2 gap-1.5">
                <div class="aspect-video rounded-sm bg-bg-s1 source-node"></div>
                <div class="aspect-video rounded-sm bg-bg-s1"></div>
                <div class="aspect-video rounded-sm bg-bg-s1"></div>
                <div class="aspect-video rounded-sm bg-bg-s1"></div>
            </div>
            <div class="flex items-center justify-center gap-1.5 mt-2">
                <span class="w-1.5 h-1.5 rounded-full bg-fg-3"></span>
                <span class="w-1.5 h-1.5 rounded-full bg-fg-3"></span>
                <span class="w-1.5 h-1.5 rounded-full bg-signal"></span>
            </div>
        </div>
    }
}

/// A latency sparkline that draws in once on reveal, endpoint dot pulsing —
/// the metrics/diagnostics subsystem.
#[component]
fn SparklineArt() -> impl IntoView {
    view! {
        <svg viewBox="0 0 240 100" class="w-full h-full max-h-32" fill="none" aria-hidden="true">
            <polyline
                class="spark-path"
                points="0,72 30,58 60,64 90,42 120,50 150,32 180,46 210,28 240,36"
                stroke="var(--signal)"
                stroke-width="1.5"
                stroke-linecap="round"
                stroke-linejoin="round"
            ></polyline>
            <circle class="spark-dot" cx="240" cy="36" r="3.5" fill="var(--signal)"></circle>
        </svg>
    }
}
