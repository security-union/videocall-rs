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

use crate::components::SecondaryButton;
use leptos::*;

#[component]
pub fn SolutionsSection() -> impl IntoView {
    view! {
        <section id="solutions" class="w-full py-24 bg-background relative overflow-hidden">
            {/* Full-width background elements */}
            <div class="absolute inset-0 bg-gradient-to-b from-background to-background-light/20 pointer-events-none"></div>
            <div class="absolute inset-0 bg-grid-pattern opacity-5 pointer-events-none"></div>

            {/* Constrained content width */}
            <div class="px-6 max-w-4xl mx-auto relative z-10">
                <h2 class="text-8xl !text-8xl font-black tracking-tight mb-16 text-left gradient-text" style="font-size: 3.84rem;">{"Solutions"}</h2>
                <div class="grid md:grid-cols-3 gap-8 md:gap-8 lg:gap-12">
                    <div class="group sharp-card accent-glow p-8 md:p-10 lg:p-12 rounded-xl backdrop-blur-sm" style="margin-bottom: 1em;">
                        <div class="absolute top-0 left-0 w-full h-1 bg-gradient-to-r from-primary to-primary/20"></div>
                        <h3 class="text-2xl font-semibold mb-6 text-foreground">{"Why WebTransport?"}</h3>
                        <p class="text-foreground-muted mb-10 text-lg leading-relaxed">{"A modern, simpler alternative to WebRTC that's designed for scalability and performance."}</p>
                        <ul class="space-y-6 mb-10">
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"No STUN/TURN complexity"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"HTTP/3 based scaling"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"Simpler server infrastructure"}
                            </li>
                        </ul>
                        <SecondaryButton
                            title="Learn More"
                            href=Some("https://github.com/security-union/videocall-rs".to_string())
                            class="mt-4"
                        />
                    </div>
                    <div class="group sharp-card accent-glow p-8 md:p-10 lg:p-12 rounded-xl backdrop-blur-sm" style="margin-bottom: 1em;">
                        <div class="absolute top-0 left-0 w-full h-1 bg-gradient-to-r from-secondary to-secondary/20"></div>
                        <h3 class="text-2xl font-semibold mb-6 text-foreground">{"Developers"}</h3>
                        <p class="text-foreground-muted mb-10 text-lg leading-relaxed">{"Build custom video experiences with our WebTransport-powered SDK and comprehensive documentation."}</p>
                        <ul class="space-y-6 mb-10">
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-secondary/10 mr-3">
                                    <span class="text-secondary text-sm">{"✓"}</span>
                                </span>
                                {"WebTransport SDK"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-secondary/10 mr-3">
                                    <span class="text-secondary text-sm">{"✓"}</span>
                                </span>
                                {"Comprehensive docs"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-secondary/10 mr-3">
                                    <span class="text-secondary text-sm">{"✓"}</span>
                                </span>
                                {"Example projects"}
                            </li>
                        </ul>
                        <SecondaryButton
                            title="View Documentation"
                            href=Some("https://github.com/security-union/videocall-rs".to_string())
                            class="mt-4"
                        />
                    </div>
                    <div class="group sharp-card accent-glow p-8 md:p-10 lg:p-12 rounded-xl backdrop-blur-sm" style="margin-bottom: 1em;">
                        <div class="absolute top-0 left-0 w-full h-1 bg-gradient-to-r from-accent to-accent/20"></div>
                        <h3 class="text-2xl font-semibold mb-6 text-foreground">{"Robotics"}</h3>
                        <p class="text-foreground-muted mb-10 text-lg leading-relaxed">{"Beyond video calls: control robots remotely with ultra-low latency streaming and real-time command capabilities."}</p>
                        <ul class="space-y-6 mb-10">
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-accent/10 mr-3">
                                    <span class="text-accent text-sm">{"✓"}</span>
                                </span>
                                {"Ultra-low latency control"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-accent/10 mr-3">
                                    <span class="text-accent text-sm">{"✓"}</span>
                                </span>
                                {"Teleoperation capabilities"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-accent/10 mr-3">
                                    <span class="text-accent text-sm">{"✓"}</span>
                                </span>
                                {"Secure data transmission"}
                            </li>
                        </ul>
                        <SecondaryButton
                            title="Learn More"
                            href=Some("https://github.com/security-union/videocall-rs".to_string())
                            class="mt-4"
                        />
                    </div>
                </div>
            </div>
        </section>
    }
}
