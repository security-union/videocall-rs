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
use crate::components::Reveal::RevealOnView;
use leptos::prelude::*;

#[component]
pub fn CompanySection() -> impl IntoView {
    view! {
        <section id="company" aria-labelledby="company-title" class="px-6 md:px-10 py-16 md:py-24">
            <div class="max-w-content mx-auto">
                <RevealOnView class="">
                    <p class="section-index" aria-hidden="true">"06 — Company"</p>
                    <h2 id="company-title" class="text-h2 text-fg mt-4">"Open source, built in Rust"</h2>
                    <p class="text-body-lg text-fg-2 mt-4 max-w-2xl">"Transparent development, from the transport layer up. Read the code, run it yourself, and extend it."</p>
                </RevealOnView>

                <RevealOnView class="mt-12">
                // Mission — a bare editorial block. The two principles keep their
                // draw-y hairline rules (seams, not cards).
                <div class="max-w-2xl">
                    <h3 class="text-h3 text-fg">"Our mission"</h3>
                    <p class="text-fg-2 mt-4 leading-relaxed">
                        "Make real-time audio and video accessible, performant, and reliable through open-source infrastructure that anyone can read, run, and extend."
                    </p>

                    // The left vertical rules draw downward on reveal, staggered.
                    <dl class="mt-8 divide-y divide-line">
                        <div class="relative pl-4 py-4">
                            <span
                                class="draw-y absolute left-0 top-0 bottom-0 w-px bg-line-strong"
                                style="transition-delay:120ms"
                                aria-hidden="true"
                            ></span>
                            <span class="section-index" aria-hidden="true">"Principle / 01"</span>
                            <dt class="text-fg font-medium mt-2">"Open source first"</dt>
                            <dd class="text-fg-2 mt-1">"Transparency and community-driven development, in the open."</dd>
                        </div>
                        <div class="relative pl-4 py-4">
                            <span
                                class="draw-y absolute left-0 top-0 bottom-0 w-px bg-line-strong"
                                style="transition-delay:240ms"
                                aria-hidden="true"
                            ></span>
                            <span class="section-index" aria-hidden="true">"Principle / 02"</span>
                            <dt class="text-fg font-medium mt-2">"Built with Rust"</dt>
                            <dd class="text-fg-2 mt-1">"One language from server to browser, for performance and reliability."</dd>
                        </div>
                    </dl>
                </div>
                </RevealOnView>

                // Join — a bare row with a hairline top-seam and the two CTAs.
                <div class="border-t border-line pt-8 mt-12 md:flex md:items-center md:justify-between gap-6">
                    <div class="md:max-w-xl">
                        <h3 class="text-h3 text-fg">"Join us"</h3>
                        <p class="text-fg-2 mt-4 leading-relaxed">
                            "We are always looking for engineers who care about real-time systems and open infrastructure."
                        </p>
                    </div>

                    <div class="flex flex-col sm:flex-row gap-3 mt-6 md:mt-0 shrink-0">
                        <CTAButton
                            variant=ButtonVariant::Primary
                            size=ButtonSize::Medium
                            href=Some("https://github.com/security-union/videocall-rs".to_string())
                            class="w-full sm:w-auto".to_string()
                        >
                            "View open positions"
                        </CTAButton>
                        <CTAButton
                            variant=ButtonVariant::Secondary
                            size=ButtonSize::Medium
                            href=Some("https://discord.gg/JP38NRe4CJ".to_string())
                            class="w-full sm:w-auto gap-2".to_string()
                        >
                            <svg class="w-4 h-4 text-fg-3" fill="none" viewBox="0 0 24 24" stroke="currentColor" aria-hidden="true">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5" d="M8 12h.01M12 12h.01M16 12h.01M21 12c0 4.418-4.03 8-9 8a9.863 9.863 0 01-4.255-.949L3 20l1.395-3.72C3.512 15.042 3 13.574 3 12c0-4.418 4.03-8 9-8s9 3.582 9 8z" />
                            </svg>
                            "Join Discord"
                        </CTAButton>
                    </div>
                </div>
            </div>
        </section>
    }
}
