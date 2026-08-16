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

use crate::components::CountUp::CountUp;
use crate::components::Reveal::RevealOnView;
use leptos::prelude::*;

#[component]
pub fn CustomersSection() -> impl IntoView {
    view! {
        // Move C — the one inverted bone band. Palette flip to bone/near-black
        // resets the eye mid-page; every token clears WCAG AA on #EDEBE6.
        <section id="customers" aria-labelledby="adoption-title" class="band-bone px-6 md:px-10 py-16 md:py-24">
            <div class="max-w-content mx-auto">
                <RevealOnView class="">
                    <p class="section-index bone-ink-3" aria-hidden="true">"07 — Adoption"</p>
                    <h2 id="adoption-title" class="text-h2 bone-ink mt-4">"Growing where it's measured"</h2>
                    <p class="text-body-lg bone-ink-2 mt-4 max-w-xl">"Open source, and used in production. Numbers sourced straight from the repository."</p>
                </RevealOnView>

                // Readout row — big figures sit directly on the bone with vertical
                // hairline seams only. No box, no surface fill: a printed stat page.
                <div class="grid sm:grid-cols-2 md:grid-cols-4 divide-y sm:divide-y-0 sm:divide-x bone-divide mt-12">
                    <Readout target=1.7 decimals=1 suffix="K" unit="GitHub stars" />
                    <Readout target=170.0 decimals=0 suffix="" unit="Forks" />
                    <Readout target=490.0 decimals=0 suffix="+" unit="Commits" />
                    <Readout target=20.0 decimals=0 suffix="+" unit="Contributors" />
                </div>

                {testimonials_section()}
            </div>
        </section>
    }
}

#[component]
fn Readout(
    target: f64,
    decimals: u8,
    #[prop(into)] suffix: String,
    unit: &'static str,
) -> impl IntoView {
    view! {
        <div class="px-6 py-10 text-center">
            <div class="text-4xl md:text-5xl font-medium tracking-tight bone-ink">
                <CountUp target=target decimals=decimals suffix=suffix />
            </div>
            <div class="eyebrow bone-ink-3 mt-3">{unit}</div>
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
