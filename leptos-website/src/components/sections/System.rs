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

/// System band, told as a quiet engineering spec sheet rather than a bento of
/// glowing tiles: a header, then one row per subsystem — a mono label paired
/// with a one-line fact, rows separated by hairline seams. Two subsystems carry
/// a small monochrome inline diagram (the transport and the control plane); the
/// media plane's diagram now lives, animated, in the globe band above, so here
/// it is a single row that points at it.
#[component]
pub fn SystemSection() -> impl IntoView {
    view! {
        <section id="system" aria-labelledby="system-title" class="px-6 md:px-10 py-16 md:py-24">
            <div class="max-w-content mx-auto">
                <RevealOnView class="">
                    <p class="section-index" aria-hidden="true">"03 — System"</p>
                    <h2 id="system-title" class="text-h2 text-fg mt-4">"One system, end to end"</h2>
                    <p class="text-body-lg text-fg-2 mt-4 max-w-xl">
                        "Not a codec you wire up yourself. A media plane that forwards encrypted-in-transit media, a separate control plane for auth and host controls, a browser and native client, the pure-Rust audio and video pipeline, and the metrics to see it all working."
                    </p>
                </RevealOnView>

                // The spec list. Rows are separated by hairline `.rule` seams —
                // no cell borders, no backgrounds, no cards.
                <div class="mt-10 md:mt-12 border-t border-line">
                    <SpecRow
                        label="Media plane · NATS pub/sub"
                        fact="actix-api relay servers forward media over a NATS pub/sub backbone, worldwide. One publisher, every subscriber — the mesh above."
                    />
                    <div class="rule"></div>
                    <SpecRow
                        label="Transport · WebTransport / QUIC"
                        fact="WebTransport over QUIC where the network allows it, an automatic WebSocket fallback where it does not. No ICE, STUN, TURN, or SDP."
                    >
                        <TransportArt/>
                    </SpecRow>
                    <div class="rule"></div>
                    <SpecRow
                        label="Control plane · meeting-api"
                        fact="Login, meeting lifecycle, host controls, and the waiting room live in a separate Axum API. The media plane just moves media frames."
                    >
                        <ControlPlaneArt/>
                    </SpecRow>
                    <div class="rule"></div>
                    <SpecRow
                        label="Audio · Opus + NetEQ"
                        fact="Opus encode in pure Rust, with a NetEQ adaptive jitter buffer in every browser to hold a call together on a bad network."
                    />
                    <div class="rule"></div>
                    <SpecRow
                        label="Video · pure-Rust VP9"
                        fact="A VP9 encoder and decoder written in Rust. No native codec dependency."
                    />
                    <div class="rule"></div>
                    <SpecRow
                        label="Web UI · Dioxus + WASM"
                        fact="A meeting client compiled to WebAssembly with Dioxus. No install."
                    />
                    <div class="rule"></div>
                    <SpecRow
                        label="Metrics · Prometheus / Grafana"
                        fact="Latency, bitrate, and connection health export to Prometheus and Grafana."
                    />
                </div>
            </div>
        </section>
    }
}

/// One subsystem row: a mono label on the left, a one-line fact on the right,
/// and an optional small monochrome diagram beneath the fact.
#[component]
fn SpecRow(
    label: &'static str,
    fact: &'static str,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    view! {
        <div class="grid md:grid-cols-12 gap-x-8 gap-y-3 py-6 md:py-7 items-start">
            <p class="section-index md:col-span-4 lg:col-span-3">{label}</p>
            <div class="md:col-span-8 lg:col-span-9">
                <p class="text-sm text-fg-2 leading-relaxed max-w-2xl">{fact}</p>
                {children
                    .map(|c| {
                        view! {
                            <div class="mt-5 max-w-xs" aria-hidden="true">
                                {c()}
                            </div>
                        }
                    })}
            </div>
        </div>
    }
}

/// Transport diagram: a QUIC path (active, marching dashes) above a WebSocket
/// fallback path (dim, static), between a client and a relay endpoint. Fully
/// monochrome — the accent lives only in live indicators and the media, not in
/// an inline spec diagram.
#[component]
fn TransportArt() -> impl IntoView {
    view! {
        <svg viewBox="0 0 240 104" class="w-full h-full max-h-20" fill="none" aria-hidden="true">
            <rect x="6" y="42" width="16" height="20" rx="2" fill="var(--fg-3)"></rect>
            <rect x="218" y="42" width="16" height="20" rx="2" fill="var(--fg-3)"></rect>

            // QUIC — active primary path.
            <path
                class="packet-path"
                d="M22 52 C86 22, 154 22, 218 52"
                stroke="var(--fg-2)"
                stroke-width="1.5"
            ></path>
            // WebSocket — dim static fallback path.
            <path
                d="M22 52 C86 82, 154 82, 218 52"
                stroke="var(--fg-4)"
                stroke-width="1.5"
                stroke-dasharray="3 5"
            ></path>

            <text x="120" y="14" text-anchor="middle" class="font-mono" font-size="8" letter-spacing="1" fill="var(--fg-2)">"QUIC"</text>
            <text x="120" y="99" text-anchor="middle" class="font-mono" font-size="8" letter-spacing="1" fill="var(--fg-3)">"WS FALLBACK"</text>
        </svg>
    }
}

/// Control-plane diagram: a queued waiting room on the left, the meeting-api
/// gate (dashed control boundary) in the middle, and one admitted participant
/// past it on the right. Monochrome — the admit path marches through the gate,
/// the admitted node breathes on opacity only.
#[component]
fn ControlPlaneArt() -> impl IntoView {
    view! {
        <svg viewBox="0 0 240 100" class="w-full h-full max-h-20" fill="none" aria-hidden="true">
            // Waiting room — three queued participants.
            <circle cx="26" cy="50" r="5" fill="var(--fg-3)"></circle>
            <circle cx="46" cy="50" r="5" fill="var(--fg-3)"></circle>
            <circle cx="66" cy="50" r="5" fill="var(--fg-3)"></circle>

            // Admit path marching through the gate.
            <path class="packet-path" d="M74 50 H160" stroke="var(--fg-3)" stroke-width="1.25"></path>

            // The meeting-api control boundary.
            <line x1="120" y1="20" x2="120" y2="80" stroke="var(--line-strong)" stroke-width="1.5" stroke-dasharray="3 5"></line>

            // Admitted participant, past the gate.
            <circle class="source-node" cx="176" cy="50" r="6" fill="var(--fg-2)"></circle>

            <text x="120" y="12" text-anchor="middle" class="font-mono" font-size="8" letter-spacing="1" fill="var(--fg-3)">"ADMIT"</text>
        </svg>
    }
}
