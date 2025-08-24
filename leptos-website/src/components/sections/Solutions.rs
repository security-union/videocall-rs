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

// TODO: put back when we have a use case for it
#[allow(dead_code)]
#[component]
pub fn SolutionsSection() -> impl IntoView {
    view! {
        <section id="solutions" class="relative">
            <div class="text-center mb-20">
                <h2 class="text-headline text-foreground mb-6">
                    "Solutions"
                </h2>
                <p class="text-body-large text-foreground-secondary max-w-3xl mx-auto">
                    "Modern video communication built for the next generation of applications"
                </p>
            </div>

            <div class="grid md:grid-cols-3 gap-8 lg:gap-12">
                <SolutionCard
                    title="Why WebTransport?"
                    description="A modern, simpler alternative to WebRTC that's designed for scalability and performance."
                    features=vec![
                        "No STUN/TURN complexity".to_string(),
                        "HTTP/3 based scaling".to_string(),
                        "Simpler server infrastructure".to_string(),
                    ]
                    link_text="Learn More"
                    link_href="https://github.com/security-union/videocall-rs/blob/main/ARCHITECTURE.md"
                />

                <SolutionCard
                    title="Ultra-Low Latency"
                    description="Experience near-instant video communication with optimized protocols and advanced encoding."
                    features=vec![
                        "Sub-100ms latency".to_string(),
                        "Adaptive bitrate streaming".to_string(),
                        "Real-time optimization".to_string(),
                    ]
                    link_text="See Performance"
                    link_href="https://github.com/security-union/videocall-rs/blob/main/ARCHITECTURE.md"
                />

                <SolutionCard
                    title="Rust-Powered"
                    description="Built with Rust for maximum performance, reliability, and memory safety."
                    features=vec![
                        "Memory-safe architecture".to_string(),
                        "Zero-copy operations".to_string(),
                        "Cross-platform compatibility".to_string(),
                    ]
                    link_text="View Code"
                    link_href="https://github.com/security-union/videocall-rs"
                />
            </div>
        </section>
    }
}

#[component]
fn SolutionCard(
    #[prop(into)] title: String,
    #[prop(into)] description: String,
    features: Vec<String>,
    #[prop(into)] link_text: String,
    #[prop(into)] link_href: String,
) -> impl IntoView {
    view! {
        <div class="card-apple group hover:shadow-lg transition-all duration-300">
            <h3 class="text-subheadline text-foreground mb-4">
                {title}
            </h3>
            <p class="text-body text-foreground-secondary mb-8 leading-relaxed">
                {description}
            </p>
            <ul class="space-y-4 mb-8">
                {features.into_iter().map(|feature| view! {
                    <li class="flex items-center text-foreground-secondary">
                        <div class="w-5 h-5 rounded-full bg-primary/10 flex items-center justify-center mr-3 flex-shrink-0">
                            <svg class="w-3 h-3 text-primary" fill="currentColor" viewBox="0 0 20 20">
                                <path fill-rule="evenodd" d="M16.707 5.293a1 1 0 010 1.414l-8 8a1 1 0 01-1.414 0l-4-4a1 1 0 011.414-1.414L8 12.586l7.293-7.293a1 1 0 011.414 0z" clip-rule="evenodd" />
                            </svg>
                        </div>
                        <span class="text-sm">{feature}</span>
                    </li>
                }).collect_view()}
            </ul>
            <CTAButton
                variant=ButtonVariant::Tertiary
                size=ButtonSize::Small
                href=Some(link_href)
            >
                {link_text}
            </CTAButton>
        </div>
    }
}
