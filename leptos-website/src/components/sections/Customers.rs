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

#[component]
pub fn CustomersSection() -> impl IntoView {
    view! {
        <section id="customers" class="relative">
            <div class="text-center mb-20">
                <h2 class="text-headline text-foreground mb-6">"Trusted By"</h2>
                <p class="text-body-large text-foreground-secondary max-w-3xl mx-auto">"Growing community of developers and organizations"</p>
            </div>

            // Stats Grid
            <div class="grid md:grid-cols-3 gap-8 lg:gap-12 mb-20">
                <StatCard
                    number="500+"
                    label="Active Users"
                    icon_path="M12 4.354a4 4 0 110 5.292M15 21H3v-1a6 6 0 0112 0v1zm0 0h6v-1a6 6 0 00-9-5.197M13 7a4 4 0 11-8 0 4 4 0 018 0z"
                />

                <StatCard
                    number="1.6K+"
                    label="GitHub Stars"
                    icon_path="M11.049 2.927c.3-.921 1.603-.921 1.902 0l1.519 4.674a1 1 0 00.95.69h4.915c.969 0 1.371 1.24.588 1.81l-3.976 2.888a1 1 0 00-.363 1.118l1.518 4.674c.3.922-.755 1.688-1.538 1.118l-3.976-2.888a1 1 0 00-1.176 0l-3.976 2.888c-.783.57-1.838-.197-1.538-1.118l1.518-4.674a1 1 0 00-.363-1.118l-3.976-2.888c-.784-.57-.38-1.81.588-1.81h4.914a1 1 0 00.951-.69l1.519-4.674z"
                />

                <StatCard
                    number="280+"
                    label="Commits"
                    icon_path="M10 1a9 9 0 100 18A9 9 0 0010 1zm0 16a7 7 0 100-14 7 7 0 000 14zm1-11a1 1 0 10-2 0v4a1 1 0 00.293.707l2.828 2.829a1 1 0 101.415-1.415L11 9.586V6z"
                />
            </div>

            {testimonials_section()}
        </section>
    }
}

#[cfg(feature = "testimonials")]
fn testimonials_section() -> impl IntoView {
    view! {
        <div class="grid md:grid-cols-2 gap-8 lg:gap-12">
            <TestimonialCard
                quote="The performance and reliability of videocall.rs has been exceptional. The WebTransport implementation makes a real difference in latency."
                author="Sarah Chen"
                role="Tech Lead at DevCorp"
            />

            <TestimonialCard
                quote="Being open source and built with Rust gives us confidence in both the security and performance of the platform."
                author="Mark Thompson"
                role="CTO at StartupX"
            />
        </div>
    }
}

#[cfg(not(feature = "testimonials"))]
fn testimonials_section() -> impl IntoView {
    view! {
        // Testimonials section disabled - enable with "testimonials" feature flag
    }
}

#[component]
fn StatCard(number: &'static str, label: &'static str, icon_path: &'static str) -> impl IntoView {
    view! {
        <div class="card-apple text-center group hover:scale-[1.02] transition-transform duration-200">
            <div class="w-16 h-16 rounded-full bg-primary/10 flex items-center justify-center mx-auto mb-6">
                <svg class="w-8 h-8 text-primary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d=icon_path />
                </svg>
            </div>
            <div class="text-4xl font-bold text-foreground mb-2 group-hover:text-primary transition-colors">{number}</div>
            <div class="text-foreground-secondary">{label}</div>
        </div>
    }
}

#[cfg(feature = "testimonials")]
#[component]
fn TestimonialCard(quote: &'static str, author: &'static str, role: &'static str) -> impl IntoView {
    view! {
        <div class="card-apple group hover:scale-[1.02] transition-transform duration-200">
            <div class="mb-6">
                <svg class="w-8 h-8 text-primary/30 mb-4" fill="currentColor" viewBox="0 0 24 24">
                    <path d="M14.017 21v-7.391c0-5.704 3.731-9.57 8.983-10.609l.995 2.151c-2.432.917-3.995 3.638-3.995 5.849h4v10h-9.983zm-14.017 0v-7.391c0-5.704 3.748-9.57 9-10.609l.996 2.151c-2.433.917-3.996 3.638-3.996 5.849h4v10h-10z"/>
                </svg>
                <p class="text-foreground-secondary text-lg leading-relaxed mb-6">{quote}</p>
            </div>

            <div class="flex items-center">
                <div class="w-12 h-12 rounded-full bg-primary/10 flex items-center justify-center mr-4">
                    <svg class="w-6 h-6 text-primary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5.121 17.804A13.937 13.937 0 0112 16c2.5 0 4.847.655 6.879 1.804M15 10a3 3 0 11-6 0 3 3 0 016 0zm6 2a9 9 0 11-18 0 9 9 0 0118 0z" />
                    </svg>
                </div>
                <div>
                    <div class="font-semibold text-foreground">{author}</div>
                    <div class="text-sm text-foreground-secondary">{role}</div>
                </div>
            </div>
        </div>
    }
}
