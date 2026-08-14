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

use crate::components::Reveal::RevealOnView;
use leptos::prelude::*;

#[component]
pub fn CustomersSection() -> impl IntoView {
    view! {
        <section id="customers" aria-labelledby="adoption-title" class="px-6 md:px-10 py-24 md:py-32">
            <div class="max-w-content mx-auto">
                <RevealOnView>
                    <p class="section-index" aria-hidden="true">"05 — Adoption"</p>
                    <h2 id="adoption-title" class="text-h2 text-fg mt-4">"Growing where it's measured"</h2>
                    <p class="text-body-lg text-fg-2 mt-4 max-w-2xl">"Numbers sourced straight from the repository."</p>
                </RevealOnView>

                // Readout row — large figures on one ruled strip, hairline dividers.
                <div class="grid md:grid-cols-3 border border-line rounded-panel overflow-hidden divide-y md:divide-y-0 md:divide-x divide-line mt-12">
                    <Readout number="1.7K" unit="GitHub stars" />
                    <Readout number="170" unit="Forks" />
                    <Readout number="490+" unit="Commits" />
                </div>

                {testimonials_section()}
            </div>
        </section>
    }
}

#[component]
fn Readout(number: &'static str, unit: &'static str) -> impl IntoView {
    view! {
        <div class="px-6 py-10 text-center">
            <div class="text-4xl md:text-5xl font-medium tracking-tight text-fg">{number}</div>
            <div class="eyebrow mt-3">{unit}</div>
        </div>
    }
}

#[cfg(feature = "testimonials")]
fn testimonials_section() -> impl IntoView {
    view! {
        <div class="grid md:grid-cols-2 gap-6 mt-6">
            <TestimonialCard
                quote="The WebTransport implementation makes a real difference in latency, and the reliability has been exceptional."
                author="Sarah Chen"
                role="Tech Lead, DevCorp"
            />
            <TestimonialCard
                quote="Open source and built in Rust gives us confidence in both the security and the performance of the platform."
                author="Mark Thompson"
                role="CTO, StartupX"
            />
        </div>
    }
}

#[cfg(not(feature = "testimonials"))]
fn testimonials_section() -> impl IntoView {
    // Testimonials are disabled; enable with the "testimonials" feature flag.
}

#[cfg(feature = "testimonials")]
#[component]
fn TestimonialCard(quote: &'static str, author: &'static str, role: &'static str) -> impl IntoView {
    view! {
        <div class="panel">
            <p class="text-fg-2 text-lg leading-relaxed">{quote}</p>
            <div class="data mt-6">{author}" · "{role}</div>
        </div>
    }
}
