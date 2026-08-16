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

use crate::components::CopyButton::CopyButton;
use crate::components::Reveal::RevealOnView;
use leptos::prelude::*;

// The exact commands each rung runs. Declared once so the visible <pre> and the
// CopyButton's clipboard payload can never drift apart, and verified against the
// repo: `cargo install` + `videocall-cli stream` from the README Usage section,
// `git clone && make dev` from the README dev loop and the Makefile `dev`
// target, and the videocall-client constructor from that crate's lib.rs docs.
const TRY_CMD: &str = "cargo install videocall-cli\n\
videocall-cli stream --user-id cam-01 --meeting-id demo --video-device-index 0";

const RUN_CMD: &str = "git clone https://github.com/security-union/videocall-rs.git\n\
cd videocall-rs\n\
make dev";

const BUILD_CMD: &str = "cargo add videocall-client\n\n\
// then, in your Rust/WASM app:\n\
use videocall_client::{VideoCallClient, VideoCallClientOptions};\n\n\
let mut client = VideoCallClient::new(VideoCallClientOptions {\n    \
    user_id: \"cam-01\".into(),\n    \
    meeting_id: \"demo\".into(),\n    \
    webtransport_urls: vec![\"https://localhost:4433\".into()],\n    \
    websocket_urls: vec![\"ws://localhost:8080\".into()],\n    \
    enable_webtransport: true,\n    \
    // …peer + connection callbacks and tuning — see docs.rs\n\
});\n\
client.connect().unwrap();";

#[component]
pub fn DevelopersSection() -> impl IntoView {
    view! {
        <section id="developers" aria-labelledby="developers-title" class="px-6 md:px-10 py-16 md:py-24">
            <div class="max-w-content mx-auto">
                <RevealOnView class="">
                    <p class="section-index" aria-hidden="true">"02 — Test"</p>
                    <h2 id="developers-title" class="text-h2 text-fg mt-4">"Three ways to test"</h2>
                    <p class="text-body-lg text-fg-2 mt-4 max-w-2xl">"Try it, run it, build on it. Three ways in, ordered by how far you want to go: stream from a device, run the whole stack, or embed the client."</p>
                </RevealOnView>

                // A commitment ladder as hairline-separated editorial rows — no
                // cards, no cell borders. Each rung: mono index + label, who it
                // is for, a copy-pasteable terminal block, and the payoff in
                // plain text.
                <div class="mt-10 md:mt-12 border-t border-line">
                    <ShipRow
                        index="01"
                        label="TRY"
                        title="Stream a camera in two commands"
                        audience="For the roboticist with a Pi on the desk. No browser, no build — just a camera and a meeting id."
                        code=TRY_CMD
                    >
                        "Then open "
                        <ShipLink href="https://app.videocall.rs/meeting/you/demo">"app.videocall.rs/meeting/you/demo"</ShipLink>
                        " in any browser. The Pi's camera is in the call — same meeting id, a viewer name of your own."
                    </ShipRow>
                    <div class="rule"></div>
                    <ShipRow
                        index="02"
                        label="RUN"
                        title="The whole system on your machine"
                        audience="For evaluating the full stack end to end — media servers, meeting API, and UI — before you commit to anything."
                        code=RUN_CMD
                    >
                        "The entire stack — Postgres, NATS, the relays, meeting-api, and the UI — runs natively with hot reload. Open "
                        <ShipLink href="http://localhost:3001/meeting/you/demo">"localhost:3001/meeting/you/demo"</ShipLink>
                        ". You are in the call. Going to production? The repo ships "
                        <ShipLink href="https://github.com/security-union/videocall-rs/tree/main/helm">"Helm charts"</ShipLink>
                        "."
                    </ShipRow>
                    <div class="rule"></div>
                    <ShipRow
                        index="03"
                        label="BUILD"
                        title="Embed the client in your own app"
                        audience="For building a custom client on the same transport and media pipeline, compiled to WebAssembly."
                        code=BUILD_CMD
                    >
                        "Transport negotiation, encoding, and peer rendering are handled; you own the UI. The full options struct and callbacks are on "
                        <ShipLink href="https://docs.rs/videocall-client">"docs.rs"</ShipLink>
                        "."
                    </ShipRow>
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

/// One rung of the ladder: the mono index + label and title on the left, the
/// terminal block and its plain-text payoff (passed as `children`) on the right.
#[component]
fn ShipRow(
    index: &'static str,
    label: &'static str,
    title: &'static str,
    audience: &'static str,
    code: &'static str,
    children: Children,
) -> impl IntoView {
    view! {
        <div class="grid md:grid-cols-12 gap-x-8 gap-y-6 py-10">
            <div class="md:col-span-4 lg:col-span-3">
                <div class="flex items-baseline gap-3">
                    <span class="section-index" aria-hidden="true">{index}</span>
                    <span class="section-index text-fg" aria-hidden="true">{label}</span>
                </div>
                <h3 class="text-h3 text-fg mt-3">{title}</h3>
                <p class="text-sm text-fg-2 mt-2 leading-relaxed">{audience}</p>
            </div>
            <div class="md:col-span-8 lg:col-span-9">
                // relative wrapper anchors the copy button; pr-20 keeps code
                // clear of it. Copy button is absent without JS (see CopyButton).
                <div class="relative">
                    <pre class="p-4 pr-20 bg-bg-code border border-line overflow-x-auto text-[13px] leading-6 font-mono text-fg-2"><code>{code}</code></pre>
                    <CopyButton text=code />
                </div>
                <p class="text-sm text-fg-2 mt-4 leading-relaxed">{children()}</p>
            </div>
        </div>
    }
}

/// Inline monochrome text link for the payoff lines — underline only, no oxide
/// (the accent is reserved for live/active/focus state elsewhere on the page).
#[component]
fn ShipLink(href: &'static str, children: Children) -> impl IntoView {
    view! {
        <a
            href=href
            class="text-fg underline decoration-line hover:decoration-fg underline-offset-4 transition-colors"
        >
            {children()}
        </a>
    }
}
