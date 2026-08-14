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

use crate::components::LiveCallPanel::LiveCallPanel;
use leptos::prelude::*;
use leptos_router::components::A;

/// Sticky site header: wordmark, primary nav, and the community cluster. The
/// mobile menu island shares its open/closed state through context, so the
/// provider wraps only the header.
#[component]
pub fn SiteNav() -> impl IntoView {
    view! {
        <MobileMenuProvider>
            <header>
                <nav
                    aria-label="Primary"
                    class="sticky top-0 z-50 backdrop-blur-xl bg-bg/80 border-b border-line"
                >
                    <div class="max-w-content mx-auto px-6 md:px-10">
                        <div class="flex justify-between items-center h-14">
                            <A href="/" attr:class="flex-shrink-0 transition-opacity hover:opacity-80" attr:aria-label="videocall.rs home">
                                <img class="h-8 w-auto" src="/images/videocall_logo.svg" alt="videocall.rs" />
                            </A>

                            <div class="hidden md:flex items-center gap-8">
                                <NavLink href="#supported-platforms" text="Platforms" />
                                <NavLink href="#developers" text="Developers" />
                                <NavLink href="#company" text="Company" />
                                <NavLink href="#pricing" text="Pricing" />
                            </div>

                            <div class="flex items-center gap-3">
                                <a
                                    href="https://discord.gg/JP38NRe4CJ"
                                    class="opacity-60 hover:opacity-100 grayscale transition-opacity"
                                    aria-label="Join the Discord community"
                                >
                                    <img class="h-5 w-5" src="/images/discord_logo.svg" alt="" aria-hidden="true" />
                                </a>
                                <a
                                    href="https://github.com/security-union/videocall-rs"
                                    class="flex items-center gap-1.5 text-fg-3 hover:text-fg transition-colors"
                                    aria-label="Star videocall-rs on GitHub"
                                >
                                    <img class="h-3.5 w-3.5 grayscale" src="/images/github_logo.svg" alt="" aria-hidden="true" />
                                    <span class="font-mono text-xs">"1.7k"</span>
                                </a>
                                <MobileMenuButton />
                            </div>
                        </div>
                    </div>
                    <MobileMenu />
                </nav>
            </header>
        </MobileMenuProvider>
    }
}

/// Left-aligned editorial hero. Type, a hairline rule, and the live-call
/// instrument panel carry the whole thing — no raster art, no glow, no gradient
/// fills. The panel is the page's one aliveness island (the CLI snippet it
/// replaced lives on in the System bento below).
#[component]
pub fn Hero() -> impl IntoView {
    view! {
        <section aria-labelledby="hero-title" class="px-6 md:px-10 pt-16 pb-20 md:pt-24 md:pb-28">
            // Text is contained; the media below breaks wider so it, not the
            // copy, is the dominant element. Text above, media below, media larger.
            <div class="max-w-content mx-auto">
                <p class="eyebrow">"01 — Real-time audio + video infrastructure"</p>

                <h1 id="hero-title" class="text-display text-fg mt-6 max-w-4xl">
                    "Real-time audio and video. The whole system, in Rust."
                </h1>

                <p class="text-body-lg text-fg-2 mt-6 max-w-2xl">
                    "videocall.rs is an opinionated, full-stack system for streaming live audio and video. Batteries included: Rust relay media servers and a meetings API for auth and host controls, a browser client, a native CLI, metrics, and a Helm deploy. Run a video conference for your team, or stream from embedded devices in the field with the CLI. You deploy a system, not a codec."
                </p>

                <div class="flex flex-col sm:flex-row gap-3 mt-8">
                    <a href="https://app.videocall.rs" class="btn-solid px-5 py-2.5">"Live demo"</a>
                    <a href="https://github.com/security-union/videocall-rs" class="btn-line px-5 py-2.5">"View source"</a>
                </div>

                // Dividing rule carrying the single live node — the one moment of
                // color above the fold; it also anchors the live-call panel.
                <div class="flex items-center gap-3 mt-14">
                    <span class="live-dot" aria-hidden="true"></span>
                    <span class="eyebrow text-signal">"Live"</span>
                    <span class="rule flex-1"></span>
                </div>
            </div>

            // The live-call instrument panel — the primary aliveness move. It
            // widens past the text measure toward full-bleed (the section
            // gutters are the only inset) so it reads as the dominant media, not
            // a boxed inset.
            <div class="max-w-[1360px] mx-auto mt-8">
                <LiveCallPanel/>
            </div>

            // Spec row — mono facts, hairline dividers, one ink color.
            <div class="max-w-content mx-auto">
                <div class="flex flex-wrap gap-y-3 mt-8">
                    <span class="eyebrow pr-4">"AUDIO + VIDEO"</span>
                    <span class="eyebrow border-l border-line px-4">"OPUS + VP9, PURE RUST"</span>
                    <span class="eyebrow border-l border-line px-4">"WEBTRANSPORT / WS"</span>
                    <span class="eyebrow border-l border-line px-4">"E2E ENCRYPTED"</span>
                    <span class="eyebrow border-l border-line px-4">"MIT / APACHE-2.0"</span>
                </div>
            </div>
        </section>
    }
}

#[component]
fn NavLink(href: &'static str, text: &'static str) -> impl IntoView {
    view! {
        <a href=href class="eyebrow nav-link py-2">
            {text}
        </a>
    }
}

#[island]
fn MobileMenuProvider(children: Children) -> impl IntoView {
    provide_context(RwSignal::new(false));
    children()
}

#[island]
fn MobileMenuButton() -> impl IntoView {
    // `RwSignal` is `Copy` in Leptos 0.7+, so the island can share the context
    // signal directly.
    let menu_open = expect_context::<RwSignal<bool>>();
    view! {
        <button
            class="md:hidden p-2 text-fg-3 hover:text-fg transition-colors"
            on:click=move |_| menu_open.update(|n| *n = !*n)
            aria-label="Toggle navigation menu"
        >
            <svg class="h-5 w-5" fill="none" viewBox="0 0 24 24" stroke="currentColor" aria-hidden="true">
                <path
                    class=move || if menu_open.get() { "hidden" } else { "" }
                    stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5"
                    d="M4 6h16M4 12h16M4 18h16"
                />
                <path
                    class=move || if menu_open.get() { "" } else { "hidden" }
                    stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5"
                    d="M6 18L18 6M6 6l12 12"
                />
            </svg>
        </button>
    }
}

#[island]
fn MobileMenu() -> impl IntoView {
    let menu_open = expect_context::<RwSignal<bool>>();
    view! {
        // Solid opaque surface (not a translucent + backdrop-blur panel): nested
        // inside the nav's own backdrop-filter, a semi-transparent layer fails to
        // occlude in Chromium, so the hero bled through. A solid bg-bg is the fix.
        <div class=move || format!(
            "md:hidden absolute top-full left-0 right-0 bg-bg border-b border-line transition-all duration-300 ease-out {}",
            if menu_open.get() { "opacity-100 translate-y-0" } else { "opacity-0 -translate-y-2 pointer-events-none" }
        )>
            <div class="px-6 py-5 space-y-1">
                <MobileNavLink href="#supported-platforms" text="Platforms" on_click=move || menu_open.set(false) />
                <MobileNavLink href="#developers" text="Developers" on_click=move || menu_open.set(false) />
                <MobileNavLink href="#company" text="Company" on_click=move || menu_open.set(false) />
                <MobileNavLink href="#pricing" text="Pricing" on_click=move || menu_open.set(false) />
            </div>
        </div>
    }
}

#[component]
fn MobileNavLink<F>(href: &'static str, text: &'static str, on_click: F) -> impl IntoView
where
    F: Fn() + 'static,
{
    view! {
        <a href=href class="block text-fg-2 hover:text-fg transition-colors text-base py-3" on:click=move |_| on_click()>
            {text}
        </a>
    }
}
