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

#[component]
pub fn DevelopersSection() -> impl IntoView {
    view! {
        <section id="developers" aria-labelledby="developers-title" class="px-6 md:px-10 py-24 md:py-32">
            <div class="max-w-content mx-auto">
                <RevealOnView class="">
                    <p class="section-index" aria-hidden="true">"04 — Developers"</p>
                    <h2 id="developers-title" class="text-h2 text-fg mt-4">"Three ways to ship"</h2>
                    <p class="text-body-lg text-fg-2 mt-4 max-w-2xl">"One system, three surfaces to build on. Library, headless CLI, and native mobile."</p>
                </RevealOnView>

                // Ruled grid: the hairline gap between cells forms a continuous
                // engineering-table seam. On reveal, each card's top hairline
                // draws in left-to-right, staggered across the row.
                <RevealOnView class="mt-12">
                <div class="grid md:grid-cols-3 gap-px bg-line border border-line rounded-panel overflow-hidden">
                    <DeveloperCard
                        index="01"
                        title="videocall-rs"
                        description="Core Rust library. WebTransport and WebSocket transport, end-to-end encryption, and the pure-Rust VP9 and Opus media pipeline."
                        link_text="Explore on GitHub →"
                        link_href="https://github.com/security-union/videocall-rs"
                        delay=0
                    />
                    <DeveloperCard
                        index="02"
                        title="videocall-cli"
                        description="Headless streaming for robotics and IoT. Stream from a Raspberry Pi, Jetson, or a server."
                        link_text="Install from crates.io →"
                        link_href="https://crates.io/crates/videocall-cli"
                        delay=90
                    />
                    <DeveloperCard
                        index="03"
                        title="videocall-sdk"
                        description="iOS and Android bindings over UniFFI. Bring real-time audio and video into a native app."
                        link_text="Read the SDK docs →"
                        link_href="https://github.com/security-union/videocall-rs/blob/main/docs/ARCHITECTURE.md"
                        delay=180
                    />
                </div>
                </RevealOnView>

                // Community strip — mono readouts on a single hairline-ruled panel.
                <div class="panel mt-6 flex flex-col lg:flex-row lg:items-center lg:justify-between gap-6">
                    <div class="max-w-2xl">
                        <h3 class="text-h3 text-fg">"Built in the open"</h3>
                        <p class="text-fg-2 mt-2">"Read the code, file issues, or send a patch."</p>
                        <div class="flex flex-wrap gap-y-2 mt-4">
                            <span class="data pr-4">"490+ COMMITS"</span>
                            <span class="data border-l border-line px-4">"20+ CONTRIBUTORS"</span>
                            <span class="data border-l border-line px-4">"170 FORKS"</span>
                        </div>
                    </div>
                    <a
                        href="https://github.com/security-union/videocall-rs"
                        class="btn-line px-5 py-2.5 self-start lg:self-auto flex-shrink-0"
                    >
                        "View on GitHub"
                    </a>
                </div>
            </div>
        </section>
    }
}

#[component]
fn DeveloperCard(
    index: &'static str,
    title: &'static str,
    description: &'static str,
    link_text: &'static str,
    link_href: &'static str,
    /// Stagger for the top-hairline draw-in, in milliseconds.
    delay: i32,
) -> impl IntoView {
    view! {
        <div class="card-lift relative bg-bg-s1 hover:bg-bg-s2 p-6 md:p-8 h-full flex flex-col">
            <span
                class="draw-x absolute top-0 left-0 right-0 h-px bg-line-strong"
                style=format!("transition-delay:{delay}ms")
                aria-hidden="true"
            ></span>
            <span class="section-index" aria-hidden="true">{index}</span>
            <h3 class="text-h3 text-fg mt-4">{title}</h3>
            <p class="text-sm text-fg-2 leading-relaxed mt-3">{description}</p>
            <a href=link_href class="btn-ghost text-sm mt-auto pt-6">{link_text}</a>
        </div>
    }
}
