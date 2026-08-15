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
        <section id="developers" aria-labelledby="developers-title" class="px-6 md:px-10 py-16 md:py-24">
            <div class="max-w-content mx-auto">
                <RevealOnView class="">
                    <p class="section-index" aria-hidden="true">"05 — Developers"</p>
                    <h2 id="developers-title" class="text-h2 text-fg mt-4">"Three ways to ship"</h2>
                    <p class="text-body-lg text-fg-2 mt-4 max-w-2xl">"One system, three surfaces to build on. The full stack, a headless CLI, and the embeddable client crate."</p>
                </RevealOnView>

                // Three surfaces as numbered editorial rows, separated by
                // hairline seams — no cards, no cell borders, no hover lift.
                <div class="mt-10 md:mt-12 border-t border-line">
                    <DeveloperRow
                        index="01"
                        title="videocall-rs"
                        description="Core Rust library. WebTransport and WebSocket transport, end-to-end encryption, and the pure-Rust VP9 and Opus media pipeline."
                        link_text="Explore on GitHub →"
                        link_href="https://github.com/security-union/videocall-rs"
                    />
                    <div class="rule"></div>
                    <DeveloperRow
                        index="02"
                        title="videocall-cli"
                        description="Headless streaming for robotics and IoT. Stream from a Raspberry Pi, Jetson, or a server."
                        link_text="Install from crates.io →"
                        link_href="https://crates.io/crates/videocall-cli"
                    />
                    <div class="rule"></div>
                    <DeveloperRow
                        index="03"
                        title="videocall-client"
                        description="Embed the client in your own web app. The transport, end-to-end encryption, and media pipeline as a Rust crate, compiled to WebAssembly."
                        link_text="Install from crates.io →"
                        link_href="https://crates.io/crates/videocall-client"
                    />
                </div>

                // Community strip — a bare editorial row with a hairline
                // top-seam, mono readouts, and a single line to GitHub.
                <div class="border-t border-line pt-8 mt-8 flex flex-col lg:flex-row lg:items-center lg:justify-between gap-6">
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
fn DeveloperRow(
    index: &'static str,
    title: &'static str,
    description: &'static str,
    link_text: &'static str,
    link_href: &'static str,
) -> impl IntoView {
    view! {
        <div class="grid md:grid-cols-12 gap-x-8 gap-y-3 py-8 items-baseline">
            <div class="md:col-span-4 lg:col-span-3 flex items-baseline gap-3">
                <span class="section-index" aria-hidden="true">{index}</span>
                <h3 class="text-h3 text-fg">{title}</h3>
            </div>
            <p class="md:col-span-5 lg:col-span-6 text-sm text-fg-2 leading-relaxed">{description}</p>
            <div class="md:col-span-3 md:text-right">
                <a href=link_href class="btn-ghost text-sm">{link_text}</a>
            </div>
        </div>
    }
}
