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

use crate::icons::DigitalOceanIcon;
use leptos::*;
use leptos_router::A;

#[component]
pub fn HeroHeader() -> impl IntoView {
    view! {
        <MobileMenuProvider>
            // Apple-style translucent navigation
            <nav class="sticky top-0 z-50 backdrop-blur-md bg-background/90 border-b border-border/10">
                <div class="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8">
                    <div class="flex justify-between items-center h-16">
                        // Logo
                        <A href="/" class="flex-shrink-0 transition-opacity hover:opacity-80">
                            <img
                                class="h-14 w-auto brightness-100 contrast-100"
                                src="/images/videocall_logo.svg"
                                alt="VideoCall.rs"
                                style="filter: drop-shadow(0 0 1px rgba(255,255,255,0.1));"
                            />
                        </A>

                        // Desktop Navigation
                        <div class="hidden md:flex items-center space-x-8">
                            <NavLink href="#developers" text="Developers" />
                            <NavLink href="#company" text="Company" />
                            <NavLink href="#customers" text="Customers" />
                            <NavLink href="#pricing" text="Pricing" />
                        </div>

                        // Right side icons
                        <div class="flex items-center space-x-4">
                            <SocialLinks />
                            <MobileMenuButton />
                        </div>
                    </div>
                </div>

                // Mobile Navigation Menu
                <MobileMenu />
            </nav>

            // Hero Section - Apple-style
            <section class="relative overflow-hidden bg-background">
                <div class="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8">
                    <div class="pt-24 pb-32 lg:pt-32 lg:pb-40">
                        <div class="text-center max-w-4xl mx-auto">
                            <h1 class="text-hero text-foreground mb-6">
                                "Ultra-low-latency "
                                <span class="text-primary">"video calls"</span>
                                " for web, mobile, and embedded devices"
                            </h1>
                            <p class="text-body-large text-foreground-secondary mb-12 max-w-2xl mx-auto">
                                "Build cameras, kiosks, drones, and robots with the same ultra-low-latency engine — open source and production-ready"
                            </p>
                            <div class="flex flex-col sm:flex-row gap-4 justify-center items-center">
                                <a
                                    href="https://app.videocall.rs"
                                    class="btn-primary text-lg px-8 py-4"
                                >
                                    "Create a meeting"
                                </a>
                                <a
                                    href="https://www.youtube.com/watch?v=XQoynxQJajk"
                                    class="btn-secondary text-lg px-8 py-4"
                                >
                                    "Watch How It Works"
                                </a>
                            </div>
                        </div>
                    </div>
                </div>

                // Subtle background pattern
                <div class="absolute inset-0 bg-gradient-to-b from-transparent via-transparent to-background-secondary/10 pointer-events-none"></div>
            </section>
        </MobileMenuProvider>
    }
}

#[component]
fn NavLink(href: &'static str, text: &'static str) -> impl IntoView {
    view! {
        <a
            href=href
            class="text-foreground-secondary hover:text-foreground transition-colors duration-200 text-sm font-medium"
        >
            {text}
        </a>
    }
}

#[component]
fn SocialLinks() -> impl IntoView {
    view! {
        <div class="flex items-center space-x-3">
            <a
                href="https://discord.gg/XRdt6WfZyf"
                class="text-foreground-tertiary hover:text-foreground-secondary transition-colors"
                aria-label="Discord"
            >
                <img class="h-5 w-5" src="/images/discord_logo.svg" alt="Discord" />
            </a>
            <a
                href="https://github.com/security-union/videocall-rs"
                class="text-foreground-tertiary hover:text-foreground-secondary transition-colors"
                aria-label="GitHub"
            >
                <img class="h-5 w-5" src="/images/github_logo.svg" alt="GitHub" />
            </a>
            <a
                href="https://m.do.co/c/6de4e19c5193"
                class="text-foreground-tertiary hover:text-foreground-secondary transition-colors"
                aria-label="DigitalOcean"
            >
                <div class="h-10 w-32">
                    <DigitalOceanIcon />
                </div>
            </a>
        </div>
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
            class="md:hidden p-2 text-foreground-secondary hover:text-foreground transition-colors"
            on:click=move |_| set_menu_open.update(|n| *n = !*n)
            aria-label="Toggle navigation menu"
        >
            <svg
                class="h-6 w-6"
                fill="none"
                viewBox="0 0 24 24"
                stroke="currentColor"
            >
                <path
                    class=move || if menu_open() { "hidden" } else { "" }
                    stroke-linecap="round"
                    stroke-linejoin="round"
                    stroke-width="2"
                    d="M4 6h16M4 12h16M4 18h16"
                />
                <path
                    class=move || if menu_open() { "" } else { "hidden" }
                    stroke-linecap="round"
                    stroke-linejoin="round"
                    stroke-width="2"
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
        <div
            class=move || format!(
                "md:hidden absolute top-full left-0 right-0 bg-background-secondary/95 backdrop-blur-md border-b border-border transition-all duration-300 ease-out {}",
                if menu_open() {
                    "opacity-100 translate-y-0"
                } else {
                    "opacity-0 -translate-y-2 pointer-events-none"
                }
            )
        >
            <div class="px-4 py-6 space-y-4">
                <MobileNavLink
                    href="#developers"
                    text="Developers"
                    on_click=move || set_menu_open.set(false)
                />
                <MobileNavLink
                    href="#company"
                    text="Company"
                    on_click=move || set_menu_open.set(false)
                />
                <MobileNavLink
                    href="#customers"
                    text="Customers"
                    on_click=move || set_menu_open.set(false)
                />
                <MobileNavLink
                    href="#pricing"
                    text="Pricing"
                    on_click=move || set_menu_open.set(false)
                />
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
        <a
            href=href
            class="block text-foreground-secondary hover:text-foreground transition-colors duration-200 text-base font-medium py-2"
            on:click=move |_| on_click()
        >
            {text}
        </a>
    }
}
