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

use leptos::*;
use leptos_router::A;

#[component]
pub fn HeroHeader() -> impl IntoView {
    let code_html = r##"<span class="hl-comment"># Stream video to a room</span>
<span class="hl-fn">videocall-cli</span> stream \
  <span class="hl-keyword">--user-id</span> <span class="hl-string">"robot-01"</span> \
  <span class="hl-keyword">--meeting-id</span> <span class="hl-string">"control-room"</span> \
  <span class="hl-keyword">--video-device-index</span> <span class="hl-type">0</span> \
  <span class="hl-keyword">--resolution</span> <span class="hl-type">1280x720</span> \
  <span class="hl-keyword">--fps</span> <span class="hl-type">30</span>"##;

    view! {
        <MobileMenuProvider>
            <nav class="sticky top-0 z-50 backdrop-blur-xl bg-background/70 border-b border-white/[0.06]">
                <div class="max-w-6xl mx-auto px-6">
                    <div class="flex justify-between items-center h-14">
                        <A href="/" class="flex-shrink-0 transition-opacity hover:opacity-80">
                            <img class="h-10 w-auto" src="/images/videocall_logo.svg" alt="VideoCall.rs" />
                        </A>

                        <div class="hidden md:flex items-center gap-8">
                            <NavLink href="#supported-platforms" text="Platforms" />
                            <NavLink href="#developers" text="Developers" />
                            <NavLink href="#company" text="Company" />
                            <NavLink href="#pricing" text="Pricing" />
                        </div>

                        <div class="flex items-center gap-3">
                            <a href="https://discord.gg/XRdt6WfZyf" class="text-white/40 hover:text-white/80 transition-colors" aria-label="Discord">
                                <img class="h-5 w-5 opacity-60 hover:opacity-100 transition-opacity" src="/images/discord_logo.svg" alt="Discord" />
                            </a>
                            <a href="https://github.com/security-union/videocall-rs"
                               class="flex items-center gap-1.5 px-3 py-1 rounded-full bg-white/[0.06] border border-white/[0.08] hover:bg-white/[0.1] transition-colors text-white/60 hover:text-white/90"
                            >
                                <img class="h-3.5 w-3.5" src="/images/github_logo.svg" alt="GitHub" />
                                <span class="text-xs font-medium">"1.7k"</span>
                            </a>
                            <MobileMenuButton />
                        </div>
                    </div>
                </div>
                <MobileMenu />
            </nav>

            <section class="relative pt-20 pb-4 md:pt-28 md:pb-8">
                <div class="max-w-4xl mx-auto px-6 text-center">
                    <h1 class="text-5xl md:text-6xl lg:text-7xl font-bold tracking-tight leading-[1.08] mb-6">
                        "Ultra-low-latency"
                        <br/>
                        <span class="hero-gradient">"video calls."</span>
                    </h1>
                    <p class="text-xl md:text-2xl text-white/50 font-normal max-w-2xl mx-auto mb-10 leading-relaxed">
                        "Open-source video infrastructure built with Rust."
                        <br class="hidden sm:block" />
                        "WebTransport-first. Built for robotics, IoT, and real-time apps."
                    </p>

                    <div class="flex flex-col sm:flex-row gap-4 justify-center items-center mb-16">
                        <a href="https://app.videocall.rs" class="hero-btn-primary">"Live Demo"</a>
                        <a href="https://github.com/security-union/videocall-rs" class="hero-btn-secondary">"View on GitHub"</a>
                    </div>
                </div>

                // Code block - the hero visual
                <div class="max-w-3xl mx-auto px-6 mb-16">
                    <div class="relative group">
                        <div class="absolute -inset-px rounded-2xl bg-gradient-to-b from-white/[0.12] to-white/[0.04] pointer-events-none"></div>
                        <div class="absolute -inset-4 rounded-3xl bg-blue-500/[0.07] blur-2xl pointer-events-none group-hover:bg-blue-500/[0.12] transition-all duration-700"></div>
                        <div class="relative rounded-2xl bg-[#0d0d0f] overflow-hidden">
                            <div class="flex items-center px-4 py-3 border-b border-white/[0.06]">
                                <div class="flex gap-1.5">
                                    <div class="w-2.5 h-2.5 rounded-full bg-[#ff5f57]"></div>
                                    <div class="w-2.5 h-2.5 rounded-full bg-[#febc2e]"></div>
                                    <div class="w-2.5 h-2.5 rounded-full bg-[#28c840]"></div>
                                </div>
                                <span class="ml-3 text-[11px] text-white/30 font-mono tracking-wide">"terminal"</span>
                            </div>
                            <pre class="p-6 text-[15px] leading-7 font-mono overflow-x-auto">
                                <code inner_html=code_html></code>
                            </pre>
                        </div>
                    </div>
                </div>

                // Trust strip
                <div class="max-w-3xl mx-auto px-6">
                    <div class="flex flex-wrap justify-center gap-x-10 gap-y-3 text-[13px] text-white/35 font-medium tracking-wide">
                        <span class="flex items-center gap-2">
                            <span class="w-1.5 h-1.5 rounded-full bg-emerald-400"></span>
                            "SUB-50MS LATENCY"
                        </span>
                        <span class="flex items-center gap-2">
                            <span class="w-1.5 h-1.5 rounded-full bg-blue-400"></span>
                            "WEBTRANSPORT + WEBSOCKET"
                        </span>
                        <span class="flex items-center gap-2">
                            <span class="w-1.5 h-1.5 rounded-full bg-amber-400"></span>
                            "JWT + SSO AUTH"
                        </span>
                        <span class="flex items-center gap-2">
                            <span class="w-1.5 h-1.5 rounded-full bg-purple-400"></span>
                            "MIT LICENSE"
                        </span>
                    </div>
                </div>
            </section>
        </MobileMenuProvider>
    }
}

#[component]
fn NavLink(href: &'static str, text: &'static str) -> impl IntoView {
    view! {
        <a href=href class="text-white/40 hover:text-white/90 transition-colors text-[13px] font-medium tracking-wide">
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
    let (menu_open, set_menu_open) = expect_context::<RwSignal<bool>>().split();
    view! {
        <button
            class="md:hidden p-2 text-white/50 hover:text-white transition-colors"
            on:click=move |_| set_menu_open.update(|n| *n = !*n)
            aria-label="Toggle navigation menu"
        >
            <svg class="h-5 w-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                <path
                    class=move || if menu_open() { "hidden" } else { "" }
                    stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5"
                    d="M4 6h16M4 12h16M4 18h16"
                />
                <path
                    class=move || if menu_open() { "" } else { "hidden" }
                    stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5"
                    d="M6 18L18 6M6 6l12 12"
                />
            </svg>
        </button>
    }
}

#[island]
fn MobileMenu() -> impl IntoView {
    let menu_open = expect_context::<RwSignal<bool>>().read_only();
    let set_menu_open = expect_context::<RwSignal<bool>>().write_only();
    view! {
        <div class=move || format!(
            "md:hidden absolute top-full left-0 right-0 bg-[#0d0d0f]/95 backdrop-blur-xl border-b border-white/[0.06] transition-all duration-300 ease-out {}",
            if menu_open() { "opacity-100 translate-y-0" } else { "opacity-0 -translate-y-2 pointer-events-none" }
        )>
            <div class="px-6 py-5 space-y-4">
                <MobileNavLink href="#supported-platforms" text="Platforms" on_click=move || set_menu_open.set(false) />
                <MobileNavLink href="#developers" text="Developers" on_click=move || set_menu_open.set(false) />
                <MobileNavLink href="#company" text="Company" on_click=move || set_menu_open.set(false) />
                <MobileNavLink href="#pricing" text="Pricing" on_click=move || set_menu_open.set(false) />
            </div>
        </div>
    }
}

#[component]
fn MobileNavLink<F>(href: &'static str, text: &'static str, on_click: F) -> impl IntoView
where F: Fn() + 'static {
    view! {
        <a href=href class="block text-white/50 hover:text-white transition-colors text-base font-medium py-1" on:click=move |_| on_click()>
            {text}
        </a>
    }
}
