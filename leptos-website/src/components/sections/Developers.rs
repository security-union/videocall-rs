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
                <RevealOnView>
                    <p class="section-index" aria-hidden="true">"03 — Developers"</p>
                    <h2 id="developers-title" class="text-h2 text-fg mt-4">"Three ways to ship"</h2>
                    <p class="text-body-lg text-fg-2 mt-4 max-w-2xl">"One transport, three surfaces to build on."</p>
                </RevealOnView>

                // Ruled grid: the hairline gap between cells forms a continuous
                // engineering-table seam.
                <div class="grid md:grid-cols-3 gap-px bg-line border border-line rounded-panel overflow-hidden mt-12">
                    <DeveloperCard
                        index="01"
                        title="videocall-rs"
                        description="Core Rust library. WebTransport support, WebSocket fallback, and low-level media control."
                        link_text="Explore on GitHub →"
                        link_href="https://github.com/security-union/videocall-rs"
                    />
                    <DeveloperCard
                        index="02"
                        title="videocall-cli"
                        description="Headless streaming for robotics and IoT. Stream from a Raspberry Pi, Jetson, or a server."
                        link_text="Install from crates.io →"
                        link_href="https://crates.io/crates/videocall-cli"
                    />
                    <DeveloperCard
                        index="03"
                        title="WebTransport"
                        description="QUIC transport with automatic WebSocket fallback."
                        link_text="Read the transport docs →"
                        link_href="https://github.com/security-union/videocall-rs/blob/main/docs/ARCHITECTURE.md"
                    />
                </div>

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
) -> impl IntoView {
    view! {
        <div class="bg-bg-s1 hover:bg-bg-s2 transition-colors p-6 md:p-8 h-full flex flex-col">
            <span class="section-index" aria-hidden="true">{index}</span>
            <h3 class="text-h3 text-fg mt-4">{title}</h3>
            <p class="text-sm text-fg-2 leading-relaxed mt-3">{description}</p>
            <a href=link_href class="btn-ghost text-sm mt-auto pt-6">{link_text}</a>
        </div>
    }
}
