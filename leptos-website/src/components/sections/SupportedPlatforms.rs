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
pub fn SupportedPlatformsSection() -> impl IntoView {
    view! {
        <section id="supported-platforms" class="relative">
            <div class="text-center mb-12">
                <h2 class="text-headline text-foreground mb-4">"Supported platforms and Browsers"</h2>
                <p class="text-body-large text-foreground-secondary max-w-3xl mx-auto">
                    "Runs beautifully on modern browsers and embedded devices"
                </p>
            </div>

            <PlatformsCarousel/>

            <div class="flex flex-col sm:flex-row gap-4 justify-center items-center">
                <CTAButton
                    variant=ButtonVariant::Primary
                    size=ButtonSize::Medium
                    href=Some("https://app.videocall.rs".to_string())
                >
                    "Try it now"
                </CTAButton>
                <CTAButton
                    variant=ButtonVariant::Secondary
                    size=ButtonSize::Medium
                    href=Some("https://crates.io/crates/videocall-cli".to_string())
                >
                    "Install videocall-cli"
                </CTAButton>
            </div>
        </section>
    }
}

#[island]
fn PlatformsCarousel() -> impl IntoView {
    // Use Wikimedia thumbnail endpoints (PNG) for reliability and CORS-friendliness
    #[derive(Clone, Copy)]
    struct PlatformItem {
        name: &'static str,
        src: &'static str,
    }

    const ITEMS: [PlatformItem; 10] = [
        PlatformItem {
            name: "Chrome",
            src: "/images/platforms/chrome.svg",
        },
        PlatformItem {
            name: "Safari",
            src: "/images/platforms/safari.svg",
        },
        PlatformItem {
            name: "Brave",
            src: "/images/platforms/brave.svg",
        },
        PlatformItem {
            name: "Edge",
            src: "/images/platforms/edge.svg",
        },
        PlatformItem {
            name: "Raspberry Pi",
            src: "/images/platforms/raspberry-pi.svg",
        },
        PlatformItem {
            name: "Linux",
            src: "/images/platforms/linux.svg",
        },
        PlatformItem {
            name: "Chromium",
            src: "/images/platforms/chromium.svg",
        },
        PlatformItem {
            name: "Mac OS",
            src: "/images/platforms/apple.svg",
        },
        PlatformItem {
            name: "iOS",
            src: "/images/platforms/ios.svg",
        },
        PlatformItem {
            name: "Android",
            src: "/images/platforms/android.svg",
        },
    ];

    view! {
        <div class="relative mb-12">
            <div class="overflow-hidden mask-edge-fade">
                <div class="flex gap-4 animate-platforms-scroll will-change-transform">
                    {move || {
                        ITEMS
                            .iter()
                            .chain(ITEMS.iter())
                            .map(|item| view! {
                                <div class="card-apple group flex-shrink-0 w-44 p-6 flex flex-col items-center justify-center">
                                    <div class="flex items-center justify-center w-full h-24">
                                        <img
                                            src=item.src
                                            alt=item.name
                                            class="h-16 w-auto opacity-90 grayscale group-hover:grayscale-0 group-hover:opacity-100 transition-all duration-300"
                                            loading="lazy"
                                        />
                                    </div>
                                    <div class="mt-4 text-sm text-foreground-secondary">{item.name}</div>
                                </div>
                            })
                            .collect_view()
                    }}
                </div>
            </div>
            <style>
                {"@keyframes platforms-scroll {{ 0% {{ transform: translateX(0); }} 100% {{ transform: translateX(-50%); }} }} .animate-platforms-scroll {{ animation: platforms-scroll 20s linear infinite; }} .mask-edge-fade {{ -webkit-mask-image: linear-gradient(to right, rgba(0,0,0,0) 0, rgba(0,0,0,1) 48px, rgba(0,0,0,1) calc(100% - 48px), rgba(0,0,0,0) 100%); mask-image: linear-gradient(to right, rgba(0,0,0,0) 0, rgba(0,0,0,1) 48px, rgba(0,0,0,1) calc(100% - 48px), rgba(0,0,0,0) 100%); -webkit-mask-repeat: no-repeat; mask-repeat: no-repeat; -webkit-mask-size: 100% 100%; mask-size: 100% 100%; }}"}
            </style>
        </div>
    }
}
