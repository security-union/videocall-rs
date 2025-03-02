use crate::components::CTAButton::*;
use leptos::*;

#[component]
pub fn PricingSection() -> impl IntoView {
    view! {
        <section id="pricing" class="w-full py-24 relative overflow-hidden">
            {/* Full-width background elements */}
            <div class="absolute inset-0 bg-gradient-to-br from-background to-background-light/10 pointer-events-none"></div>
            <div class="absolute inset-0 bg-dot-pattern opacity-5 pointer-events-none"></div>

            {/* Full-width decorative elements */}
            <div class="absolute top-1/3 right-0 w-80 h-80 bg-primary/5 rounded-full blur-3xl opacity-50"></div>
            <div class="absolute bottom-1/3 left-0 w-60 h-60 bg-secondary/5 rounded-full blur-3xl opacity-50"></div>

            {/* Constrained content width */}
            <div class="px-6 max-w-4xl mx-auto relative z-10">
                <h2 class="text-8xl !text-8xl font-black tracking-tight mb-16 text-left gradient-text" style="font-size: 3.84rem;">{"Pricing"}</h2>

                <div class="grid md:grid-cols-3 gap-8 md:gap-12 lg:gap-16" style="margin-bottom: 1em;">
                    <div class="group sharp-card accent-glow p-8 md:p-10 lg:p-12 rounded-xl backdrop-blur-sm" style="margin-bottom: 1em;">
                        <div class="absolute top-0 left-0 w-full h-1 bg-gradient-to-r from-primary to-primary/20"></div>
                        <h3 class="text-4xl font-semibold mb-6 text-foreground flex items-center">
                            <span>{"Free"}</span>
                            <span class="inline-block w-4"></span>
                            <sup class="text-[15px] font-bold text-primary/80 tracking-wide">{"[SELF-HOSTED]"}</sup>
                        </h3>
                        <p class="text-foreground-muted mb-10 text-lg leading-relaxed">{"Full-featured but you're responsible for hosting, maintenance, and uptime."}</p>
                        <ul class="space-y-6 mb-16">
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"Run on your own infrastructure"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"Community support only"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"You manage updates and security"}
                            </li>
                        </ul>
                        <CTAButton
                            title="Get the Helm Chart".to_string()
                            icon=IconProps {
                                path: "M8 5v14l11-7z".into(),
                                size: "w-5 h-5".into(),
                            }
                            animated=true
                            href=Some("https://github.com/security-union/videocall-rs".to_string())
                            class="mt-4".to_string()
                        />
                    </div>
                    <div class="group sharp-card accent-glow p-8 md:p-10 lg:p-12 rounded-xl backdrop-blur-sm relative z-10 scale-105" style="margin-bottom: 1em;">
                        <div class="absolute top-0 left-0 w-full h-1 bg-gradient-to-r from-primary to-secondary"></div>
                        <h3 class="text-4xl font-semibold mb-6 text-foreground">
                            {"Pro"}
                            <sup class="ml-6 text-[15px] font-bold text-primary/80 tracking-wide">{"[POPULAR]"}</sup>
                        </h3>
                        <p class="text-foreground-muted mb-10 text-lg leading-relaxed">{"We host and manage everything for you with guaranteed uptime and reliability."}</p>
                        <ul class="space-y-6 mb-16">
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"Fully managed cloud hosting"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"99.99% uptime SLA"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"Automatic updates and security patches"}
                            </li>
                        </ul>
                        <CTAButton
                            title="Get Started".to_string()
                            icon=IconProps {
                                path: "M8 5v14l11-7z".into(),
                                size: "w-5 h-5".into(),
                            }
                            animated=false
                            href=Some("https://github.com/security-union/videocall-rs".to_string())
                            class="primary mt-4".to_string()
                        />
                    </div>
                    <div class="group sharp-card accent-glow p-8 md:p-10 lg:p-12 rounded-xl backdrop-blur-sm" style="margin-bottom: 1em;">
                        <div class="absolute top-0 left-0 w-full h-1 bg-gradient-to-r from-secondary to-secondary/20"></div>
                        <h3 class="text-4xl font-semibold mb-6 text-foreground">
                            {"Enterprise"}
                            <sup class="ml-6 text-[15px] font-bold text-secondary/80 tracking-wide">{"[DEDICATED]"}</sup>
                        </h3>
                        <p class="text-foreground-muted mb-10 text-lg leading-relaxed">{"Dedicated hosting with custom SLAs, advanced security, and white-glove support."}</p>
                        <ul class="space-y-6 mb-16">
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-secondary/10 mr-3">
                                    <span class="text-secondary text-sm">{"✓"}</span>
                                </span>
                                {"Dedicated infrastructure"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-secondary/10 mr-3">
                                    <span class="text-secondary text-sm">{"✓"}</span>
                                </span>
                                {"Custom integrations"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-secondary/10 mr-3">
                                    <span class="text-secondary text-sm">{"✓"}</span>
                                </span>
                                {"Enterprise-grade HELM chart with end-to-end encryption"}
                            </li>
                        </ul>
                        <CTAButton
                            title="Contact Sales".to_string()
                            icon=IconProps {
                                path: "M8 5v14l11-7z".into(),
                                size: "w-5 h-5".into(),
                            }
                            animated=false
                            href=Some("mailto:social@securityunion.dev?subject=Enterprise%20Plan%20Inquiry".to_string())
                            class="secondary mt-4".to_string()
                        />
                    </div>
                </div>
            </div>
        </section>
    }
}
