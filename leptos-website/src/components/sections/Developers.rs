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
            <div class="text-center mb-20">
                <h2 class="text-headline text-foreground mb-6">"For Developers"</h2>
                <p class="text-body-large text-foreground-secondary max-w-3xl mx-auto">"Built with modern technologies and open-source principles"</p>
            </div>

            <div class="grid md:grid-cols-3 gap-8 lg:gap-12 mb-16">
                <DeveloperCard
                    title="videocall-rs"
                    description="The core library for building video calling applications. Includes WebRTC, WebTransport, and more."
                    icon_path="M10 20l4-16m4 4l4 4-4 4M6 16l-4-4 4-4"
                    link_text="Explore on GitHub"
                    link_href="https://github.com/security-union/videocall-rs"
                />
                <DeveloperCard
                    title="videocall-cli"
                    description="Stream video from the command line on Raspberry Pi, robots, and servers. Small, fast, and works with videocall.rs."
                    icon_path="M4 6h16M4 18h16M8 10l-4 3 4 3M12 13h6"
                    link_text="Install from crates.io"
                    link_href="https://crates.io/crates/videocall-cli"
                />
                <DeveloperCard
                    title="Open Source"
                    description="Our video calling platform is entirely open source, built with Rust for speed and reliability."
                    icon_path="M10 20l4-16m4 4l4 4-4 4M6 16l-4-4 4-4"
                    link_text="Explore on GitHub"
                    link_href="https://github.com/security-union/videocall-rs"
                />

                <DeveloperCard
                    title="High Performance"
                    description="Written in Rust to ensure exceptional performance and reliability for your video calls."
                    icon_path="M9 19v-6a2 2 0 00-2-2H5a2 2 0 00-2 2v6a2 2 0 002 2h2a2 2 0 002-2zm0 0V9a2 2 0 012-2h2a2 2 0 012 2v10m-6 0a2 2 0 002 2h2a2 2 0 002-2m0 0V5a2 2 0 012-2h2a2 2 0 012 2v14a2 2 0 01-2 2h-2a2 2 0 01-2-2z"
                    link_text="Read the docs"
                    link_href="https://github.com/security-union/videocall-rs"
                />

                <DeveloperCard
                    title="WebTransport"
                    description="Modern alternative to WebRTC with simplified architecture and better performance."
                    icon_path="M13 10V3L4 14h7v7l9-11h-7z"
                    link_text="Learn More"
                    link_href="https://github.com/security-union/videocall-rs"
                />
            </div>

            // GitHub Community Card
            <div class="card-apple">
                <div class="flex flex-col md:flex-row items-center justify-between gap-8">
                    <div class="max-w-2xl">
                        <h3 class="text-2xl font-semibold mb-4 text-foreground">"Join our GitHub community"</h3>
                        <p class="text-foreground-secondary mb-6">"Contribute to our open-source project, report issues, or just star the repo to show your support. We welcome developers of all experience levels!"</p>

                        <div class="flex flex-wrap gap-4 mb-6">
                            <div class="flex items-center bg-background-secondary px-4 py-2 rounded-lg">
                                <svg class="w-5 h-5 text-primary mr-2" fill="currentColor" viewBox="0 0 20 20">
                                    <path fill-rule="evenodd" d="M10 1a9 9 0 100 18A9 9 0 0010 1zm0 16a7 7 0 100-14 7 7 0 000 14zm1-11a1 1 0 10-2 0v4a1 1 0 00.293.707l2.828 2.829a1 1 0 101.415-1.415L11 9.586V6z" clip-rule="evenodd" />
                                </svg>
                                <span class="text-sm text-foreground-secondary">"280+ commits"</span>
                            </div>
                            <div class="flex items-center bg-background-secondary px-4 py-2 rounded-lg">
                                <svg class="w-5 h-5 text-primary mr-2" fill="currentColor" viewBox="0 0 20 20">
                                    <path fill-rule="evenodd" d="M5 2a1 1 0 011 1v1h1a1 1 0 010 2H6v1a1 1 0 01-2 0V6H3a1 1 0 010-2h1V3a1 1 0 011-1zm0 10a1 1 0 011 1v1h1a1 1 0 110 2H6v1a1 1 0 11-2 0v-1H3a1 1 0 110-2h1v-1a1 1 0 011-1zM12 2a1 1 0 01.967.744L14.146 7.2 17.5 9.134a1 1 0 010 1.732l-3.354 1.935-1.18 4.455a1 1 0 01-1.933 0L9.854 12.8 6.5 10.866a1 1 0 010-1.732l3.354-1.935 1.18-4.455A1 1 0 0112 2z" clip-rule="evenodd" />
                                </svg>
                                <span class="text-sm text-foreground-secondary">"1.6k+ stars"</span>
                            </div>
                            <div class="flex items-center bg-background-secondary px-4 py-2 rounded-lg">
                                <svg class="w-5 h-5 text-primary mr-2" fill="currentColor" viewBox="0 0 20 20">
                                    <path d="M10 12a2 2 0 100-4 2 2 0 000 4z" />
                                    <path fill-rule="evenodd" d="M.458 10C1.732 5.943 5.522 3 10 3s8.268 2.943 9.542 7c-1.274 4.057-5.064 7-9.542 7S1.732 14.057.458 10zM14 10a4 4 0 11-8 0 4 4 0 018 0z" clip-rule="evenodd" />
                                </svg>
                                <span class="text-sm text-foreground-secondary">"120+ watchers"</span>
                            </div>
                        </div>
                    </div>

                    <CTAButton
                        variant=ButtonVariant::Secondary
                        size=ButtonSize::Medium
                        href=Some("https://github.com/security-union/videocall-rs".to_string())
                    >
                        "Visit GitHub Repository"
                    </CTAButton>
                </div>
            </div>
        </section>
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
        <div class="card-apple h-full group hover:scale-[1.02] transition-transform duration-200">
            <div class="mb-6">
                <div class="w-12 h-12 rounded-lg bg-primary/10 flex items-center justify-center mb-4">
                    <svg class="w-6 h-6 text-primary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d=icon_path />
                    </svg>
                </div>
                <h3 class="text-xl font-semibold mb-3 text-foreground">{title}</h3>
                <p class="text-foreground-secondary mb-4">{description}</p>
            </div>

            <CTAButton
                variant=ButtonVariant::Secondary
                size=ButtonSize::Small
                href=Some(link_href.to_string())
                class="mt-auto".to_string()
            >
                {link_text}
            </CTAButton>
        </div>
    }
}
