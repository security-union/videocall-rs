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

use crate::components::CTAButton::{ButtonSize, ButtonVariant, CTAButton};
use leptos::*;

#[component]
pub fn DevelopersSection() -> impl IntoView {
    view! {
        <section id="developers" class="relative">
            <div class="text-center mb-16">
                <h2 class="text-4xl md:text-5xl font-semibold tracking-tight mb-4">"For Developers"</h2>
                <p class="text-lg md:text-xl text-white/50 max-w-2xl mx-auto">"Built with modern technologies and open-source principles"</p>
            </div>

            <div class="grid md:grid-cols-2 lg:grid-cols-3 gap-6 mb-12">
                <DeveloperCard
                    title="videocall-rs"
                    description="Core Rust library for scalable video apps. WebTransport support, WebSocket fallback, and low-level media control."
                    icon_path="M10 20l4-16m4 4l4 4-4 4M6 16l-4-4 4-4"
                    link_text="Explore on GitHub"
                    link_href="https://github.com/security-union/videocall-rs"
                />
                <DeveloperCard
                    title="videocall-cli"
                    description="Headless video streaming CLI for robotics and IoT. Stream from Raspberry Pi, Jetson Nano, and servers."
                    icon_path="M4 6h16M4 18h16M8 10l-4 3 4 3M12 13h6"
                    link_text="Install from crates.io"
                    link_href="https://crates.io/crates/videocall-cli"
                />
                <DeveloperCard
                    title="WebTransport"
                    description="QUIC-based transport for sub-50ms latency. Automatic WebSocket fallback for maximum compatibility."
                    icon_path="M13 10V3L4 14h7v7l9-11h-7z"
                    link_text="Learn More"
                    link_href="https://github.com/security-union/videocall-rs"
                />
            </div>

            <div class="card-apple">
                <div class="flex flex-col md:flex-row items-center justify-between gap-8">
                    <div class="max-w-2xl">
                        <h3 class="text-2xl font-semibold mb-4 text-foreground">"Open Source Community"</h3>
                        <p class="text-foreground-secondary mb-6">"Contribute to the project, report issues, or star the repo. We welcome developers of all experience levels."</p>

                        <div class="flex flex-wrap gap-3">
                            <StatBadge label="280+ commits" />
                            <StatBadge label="1.7k+ stars" />
                            <StatBadge label="120+ watchers" />
                        </div>
                    </div>

                    <CTAButton
                        variant=ButtonVariant::Secondary
                        size=ButtonSize::Medium
                        href=Some("https://github.com/security-union/videocall-rs".to_string())
                    >
                        "View on GitHub"
                    </CTAButton>
                </div>
            </div>
        </section>
    }
}

#[component]
fn StatBadge(label: &'static str) -> impl IntoView {
    view! {
        <span class="inline-flex items-center px-3 py-1.5 rounded-full bg-white/[0.06] text-sm text-white/50">
            {label}
        </span>
    }
}

#[component]
fn DeveloperCard(
    title: &'static str,
    description: &'static str,
    icon_path: &'static str,
    link_text: &'static str,
    link_href: &'static str,
) -> impl IntoView {
    view! {
        <div class="card-apple h-full flex flex-col">
            <div class="mb-4">
                <div class="w-10 h-10 rounded-lg bg-primary/10 flex items-center justify-center mb-4">
                    <svg class="w-5 h-5 text-primary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d=icon_path />
                    </svg>
                </div>
                <h3 class="text-lg font-semibold mb-2 text-foreground">{title}</h3>
                <p class="text-sm text-foreground-secondary leading-relaxed">{description}</p>
            </div>

            <div class="mt-auto pt-4">
                <CTAButton
                    variant=ButtonVariant::Tertiary
                    size=ButtonSize::Small
                    href=Some(link_href.to_string())
                >
                    {link_text}
                </CTAButton>
            </div>
        </div>
    }
}
