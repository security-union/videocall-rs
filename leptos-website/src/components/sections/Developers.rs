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

// Shared inline-link styling for the payoff lines — underline only, no oxide.
// Declared once so the static `ShipLink` and the TRY rung's reactive links stay
// visually identical.
const LINK_CLASS: &str =
    "text-fg underline decoration-line hover:decoration-fg underline-offset-4 transition-colors";

// The meeting id shown before hydration (no JS, or the split-second before the
// island mounts). Never a real uuid: two no-JS visitors must not be handed the
// same room, so the SSR markup carries an obvious placeholder to swap out.
const PLACEHOLDER_MEETING_ID: &str = "your-meeting-id";

/// Builds the TRY rung's two-line terminal block for a given meeting id.
/// Declared once so the visible `<pre>`, the clipboard payload, and the fallback
/// render can never drift apart. Verified against the repo: `cargo install` +
/// `videocall-cli stream` from the README Usage section.
fn try_command(meeting_id: &str) -> String {
    format!(
        "cargo install videocall-cli\n\
         videocall-cli stream --user-id cam-01 --meeting-id {meeting_id} --video-device-index 0"
    )
}

// The RUN and BUILD commands. Declared once so the visible <pre> and the
// CopyButton's clipboard payload can never drift apart, and verified against the
// repo: `git clone && make dev` from the README dev loop and the Makefile `dev`
// target, and the videocall-client constructor from that crate's lib.rs docs.
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
                    <TryRow />
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
        <a href=href class=LINK_CLASS>
            {children()}
        </a>
    }
}

/// The TRY rung, as its own island so it can mint a fresh, per-visit demo
/// meeting id on the client. Everything a visitor could copy or click — the
/// `--meeting-id` in the command, the clipboard payload, and the watch link —
/// derives from the single `demo_id` signal, so the two can never collide with
/// another visitor's room or diverge from each other.
///
/// Fails safe like the page's other islands: the SSR markup ships the
/// `your-meeting-id` placeholder and a link to the app root, and the id is
/// swapped in only *after* hydration (inside an effect), so no SSR/client diff
/// ever sees a mismatched uuid. Without JavaScript the visitor simply picks
/// their own meeting id — never a fake uuid two no-JS visitors would share.
#[island]
fn TryRow() -> impl IntoView {
    // `None` until the client mints an id → the fail-safe placeholder render.
    let demo_id = RwSignal::new(None::<String>);
    // Copy control state, mirroring `CopyButton`: hidden until the async
    // clipboard API is confirmed, and a 1.5s COPY→COPIED label flip.
    let armed = RwSignal::new(false);
    let copied = RwSignal::new(false);
    let copy_node: NodeRef<leptos::html::Button> = NodeRef::new();

    #[cfg(feature = "hydrate")]
    {
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;

        // Mint the per-visit id once the island mounts. Done in an effect
        // (post-hydration) so the SSR markup's placeholder is never diffed
        // against the uuid — the swap is a plain reactive update afterwards.
        Effect::new(move |_| {
            demo_id.set(Some(fresh_meeting_id()));
        });

        // Wire the copy control. The clipboard payload is rebuilt from the same
        // `demo_id` signal the <pre> renders, so what copies always matches what
        // the reader sees — including the freshly minted uuid.
        Effect::new(move |_| {
            let Some(btn) = copy_node.get() else {
                return;
            };
            let Some(win) = web_sys::window() else {
                return;
            };
            // No async clipboard API: stay hidden rather than offer a dead button.
            let clipboard = win.navigator().clipboard();
            armed.set(true);

            let click = Closure::wrap(Box::new(move || {
                let id = demo_id.get_untracked();
                let cmd = try_command(id.as_deref().unwrap_or(PLACEHOLDER_MEETING_ID));
                // Fire-and-forget, optimistic label flip — see CopyButton.
                let _ = clipboard.write_text(&cmd);
                copied.set(true);

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
        <div class="grid md:grid-cols-12 gap-x-8 gap-y-6 py-10">
            <div class="md:col-span-4 lg:col-span-3">
                <div class="flex items-baseline gap-3">
                    <span class="section-index" aria-hidden="true">"01"</span>
                    <span class="section-index text-fg" aria-hidden="true">"TRY"</span>
                </div>
                <h3 class="text-h3 text-fg mt-3">"Stream a camera in two commands"</h3>
                <p class="text-sm text-fg-2 mt-2 leading-relaxed">"For the roboticist with a Pi on the desk. No browser, no build — just a camera and a meeting id."</p>
            </div>
            <div class="md:col-span-8 lg:col-span-9">
                <div class="relative">
                    <pre class="p-4 pr-20 bg-bg-code border border-line overflow-x-auto text-[13px] leading-6 font-mono text-fg-2"><code>{move || try_command(demo_id.get().as_deref().unwrap_or(PLACEHOLDER_MEETING_ID))}</code></pre>
                    <button
                        node_ref=copy_node
                        type="button"
                        class:hidden=move || !armed.get()
                        class="absolute top-3 right-3 data hover:text-fg transition-colors cursor-pointer"
                        aria-live="polite"
                    >
                        {move || if copied.get() { "COPIED" } else { "COPY" }}
                    </button>
                </div>
                <p class="text-sm text-fg-2 mt-4 leading-relaxed">
                    {move || match demo_id.get() {
                        Some(id) => view! {
                            "Then open "
                            <a href=format!("https://app.videocall.rs/meeting/{id}") class=LINK_CLASS>{format!("app.videocall.rs/meeting/{id}")}</a>
                            " in any browser. Same meeting id, so the Pi's camera and your browser land in the same call — and the id is fresh on every visit, so you get a private room."
                        }.into_any(),
                        None => view! {
                            "Then open "
                            <a href="https://app.videocall.rs" class=LINK_CLASS>"app.videocall.rs"</a>
                            " in any browser and pick any meeting id — swap it in for "
                            <span class="font-mono text-fg">{PLACEHOLDER_MEETING_ID}</span>
                            " in the command above so the Pi's camera and your browser land in the same call."
                        }.into_any(),
                    }}
                </p>
            </div>
        </div>
    }
}

/// Mints a fresh v4 meeting id on the client. Prefers the platform CSPRNG —
/// `crypto.randomUUID()` is available in every secure context, and the site is
/// served over https — falling back to a `Math.random()`-assembled v4 only in
/// the rare non-secure context, where collision odds for a throwaway demo room
/// are still negligible.
#[cfg(feature = "hydrate")]
fn fresh_meeting_id() -> String {
    if let Some(win) = web_sys::window() {
        if let Ok(crypto) = win.crypto() {
            return crypto.random_uuid();
        }
    }
    v4_from_math_random()
}

/// Assembles a v4-shaped uuid from `Math.random()`. Fallback only — not
/// cryptographically strong, but sufficient to keep two visitors' demo rooms
/// apart when `crypto.randomUUID()` is unavailable.
#[cfg(feature = "hydrate")]
fn v4_from_math_random() -> String {
    let mut bytes = [0u8; 16];
    for b in bytes.iter_mut() {
        *b = (js_sys::Math::random() * 256.0) as u8;
    }
    // Stamp the version (4) and variant (10xx) bits per RFC 4122.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}
