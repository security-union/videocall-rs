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

use crate::components::CTAButton::{CTAButton, ButtonSize, ButtonVariant};
use leptos::*;

#[component]
pub fn CompanySection() -> impl IntoView {
    view! {
        <section id="company" class="relative">
            <div class="text-center mb-20">
                <h2 class="text-headline text-foreground mb-6">"Company"</h2>
                <p class="text-body-large text-foreground-secondary max-w-3xl mx-auto">"Building the future of real-time communication"</p>
            </div>

            <div class="grid md:grid-cols-2 gap-8 lg:gap-12">
                // Our Mission Card
                <div class="card-apple h-full">
                    <div class="mb-6">
                        <div class="flex items-center mb-6">
                            <div class="w-10 h-10 rounded-full bg-primary/10 flex items-center justify-center mr-4">
                                <svg class="w-5 h-5 text-primary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 3v4M3 5h4M6 17v4m-2-2h4m5-16l2 2m0 0l2 2m-2-2v12" />
                                </svg>
                            </div>
                            <h3 class="text-2xl font-semibold text-foreground">"Our Mission"</h3>
                        </div>
                        
                        <p class="text-foreground-secondary text-lg mb-8 leading-relaxed">
                            "We're building the future of real-time communication. Our mission is to make video conferencing more accessible, performant, and reliable through open-source innovation."
                        </p>
                    </div>

                    <div class="space-y-6">
                        <div class="border-l-4 border-primary pl-4">
                            <h4 class="text-lg font-semibold text-foreground mb-2">"Open Source First"</h4>
                            <p class="text-foreground-secondary">"We believe in transparency and community-driven development."</p>
                        </div>
                        
                        <div class="border-l-4 border-primary pl-4">
                            <h4 class="text-lg font-semibold text-foreground mb-2">"Built with Rust"</h4>
                            <p class="text-foreground-secondary">"Leveraging Rust's performance and reliability for better video calls."</p>
                        </div>
                    </div>
                </div>

                // Join Us Card
                <div class="card-apple h-full">
                    <div class="mb-6">
                        <div class="flex items-center mb-6">
                            <div class="w-10 h-10 rounded-full bg-primary/10 flex items-center justify-center mr-4">
                                <svg class="w-5 h-5 text-primary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M17 20h5v-2a3 3 0 00-5.356-1.857M17 20H7m10 0v-2c0-.656-.126-1.283-.356-1.857M7 20H2v-2a3 3 0 015.356-1.857M7 20v-2c0-.656.126-1.283.356-1.857m0 0a5.002 5.002 0 019.288 0M15 7a3 3 0 11-6 0 3 3 0 016 0zm6 3a2 2 0 11-4 0 2 2 0 014 0zM7 10a2 2 0 11-4 0 2 2 0 014 0z" />
                                </svg>
                            </div>
                            <h3 class="text-2xl font-semibold text-foreground">"Join Us"</h3>
                        </div>
                        
                        <p class="text-foreground-secondary text-lg mb-8 leading-relaxed">
                            "We're always looking for talented individuals who share our passion for building great software."
                        </p>
                    </div>

                    <div class="space-y-4 mt-auto">
                        <CTAButton
                            variant=ButtonVariant::Primary
                            size=ButtonSize::Medium
                            href=Some("https://github.com/security-union/videocall-rs".to_string())
                            class="w-full justify-center".to_string()
                        >
                            "View Open Positions"
                        </CTAButton>
                        
                        <CTAButton
                            variant=ButtonVariant::Secondary
                            size=ButtonSize::Medium
                            href=Some("https://discord.gg/XRdt6WfZyf".to_string())
                            class="w-full justify-center".to_string()
                        >
                            <div class="flex items-center">
                                <svg class="w-5 h-5 mr-2 text-primary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 12h.01M12 12h.01M16 12h.01M21 12c0 4.418-4.03 8-9 8a9.863 9.863 0 01-4.255-.949L3 20l1.395-3.72C3.512 15.042 3 13.574 3 12c0-4.418 4.03-8 9-8s9 3.582 9 8z" />
                                </svg>
                                "Join our Discord"
                            </div>
                        </CTAButton>
                    </div>
                </div>
            </div>
        </section>
    }
}