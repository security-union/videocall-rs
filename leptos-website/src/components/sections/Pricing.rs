use leptos::*;

#[component]
pub fn PricingSection() -> impl IntoView {
    view! {
        <section id="pricing" class="py-24 px-6 relative overflow-hidden">
            {/* Background elements */}
            <div class="absolute inset-0 bg-gradient-to-br from-background to-background-light/10 pointer-events-none"></div>
            <div class="absolute inset-0 bg-dot-pattern opacity-5 pointer-events-none"></div>
            
            {/* Decorative elements */}
            <div class="absolute top-1/3 right-0 w-80 h-80 bg-primary/5 rounded-full blur-3xl opacity-50"></div>
            <div class="absolute bottom-1/3 left-0 w-60 h-60 bg-secondary/5 rounded-full blur-3xl opacity-50"></div>
            
            <div class="max-w-4xl mx-auto relative z-10">
                <h2 class="text-8xl !text-8xl font-black tracking-tight mb-16 text-left gradient-text" style="font-size: 3.84rem;">{"Pricing"}</h2>
                
                <div class="grid md:grid-cols-3 gap-8 md:gap-12 lg:gap-16" style="margin-bottom: 1em;">
                    <div class="group sharp-card accent-glow p-8 md:p-10 lg:p-12 rounded-xl backdrop-blur-sm" style="margin-bottom: 1em;">
                        <div class="absolute top-0 left-0 w-full h-1 bg-gradient-to-r from-primary to-primary/20"></div>
                        <h3 class="text-4xl font-semibold mb-6 text-foreground flex items-center">
                            <span>{"Free"}</span>
                            <span class="inline-block w-4"></span>
                            <sup class="text-[15px] font-bold text-primary/80 tracking-wide">{"[STARTER]"}</sup>
                        <h3 class="text-4xl font-semibold mb-6 text-foreground">
                            {"Free"}
                            <sup class="ml-6 text-[15px] font-bold text-primary/80 tracking-wide">{"[STARTER]"}</sup>
                        </h3>
                        <p class="text-foreground-muted mb-10 text-lg leading-relaxed">{"Perfect for small teams and individuals who need reliable video conferencing."}</p>
                        <ul class="space-y-6 mb-10">
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"Up to 4 participants"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"40-minute time limit"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"Screen sharing"}
                            </li>
                        </ul>
                        <a href="#" class="group/btn relative inline-flex items-center justify-center py-4 px-6 overflow-hidden rounded-lg transition-all bg-gradient-to-r from-gray-700 via-gray-600 to-gray-700 hover:from-gray-600 hover:via-gray-500 hover:to-gray-600 border border-gray-700 hover:border-gray-500">
                            {/* Button shine effect */}
                            <div class="absolute top-0 -inset-full h-full w-1/3 z-5 block transform -skew-x-12 bg-gradient-to-r from-transparent to-white opacity-20 group-hover/btn:animate-shine"></div>
                            
                            {/* Button text */}
                            <span class="relative z-10 flex items-center text-white font-semibold">
                                {"Get Started"}
                                <svg class="w-5 h-5 ml-2 transition-transform group-hover/btn:translate-x-1" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M17 8l4 4m0 0l-4 4m4-4H3" />
                                </svg>
                            </span>
                        </a>
                    </div>
                    <div class="group sharp-card accent-glow p-8 md:p-10 lg:p-12 rounded-xl backdrop-blur-sm relative z-10 scale-105" style="margin-bottom: 1em;">
                        <div class="absolute top-0 left-0 w-full h-1 bg-gradient-to-r from-primary to-secondary"></div>
                        <h3 class="text-4xl font-semibold mb-6 text-foreground">
                            {"Pro"}
                            <sup class="ml-6 text-[15px] font-bold text-primary/80 tracking-wide">{"[POPULAR]"}</sup>
                        </h3>
                        <p class="text-foreground-muted mb-10 text-lg leading-relaxed">{"Enhanced features for professionals and growing teams who need more flexibility."}</p>
                        <ul class="space-y-6 mb-10">
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"Up to 50 participants"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"Unlimited meeting duration"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"Cloud recording"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"Analytics dashboard"}
                            </li>
                        </ul>
                        <a href="#" class="group/btn relative inline-flex items-center justify-center py-4 px-6 overflow-hidden rounded-lg transition-all bg-gradient-to-r from-primary via-primary/90 to-primary hover:from-primary/90 hover:via-primary/80 hover:to-primary/90 shadow-md shadow-primary/20 hover:shadow-lg hover:shadow-primary/30">
                            {/* Button shine effect */}
                            <div class="absolute top-0 -inset-full h-full w-1/3 z-5 block transform -skew-x-12 bg-gradient-to-r from-transparent to-white opacity-20 group-hover/btn:animate-shine"></div>
                            
                            {/* Button text */}
                            <span class="relative z-10 flex items-center text-white font-semibold">
                                {"Start Free Trial"}
                                <svg class="w-5 h-5 ml-2 transition-transform group-hover/btn:translate-x-1" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M17 8l4 4m0 0l-4 4m4-4H3" />
                                </svg>
                            </span>
                        </a>
                    </div>
                    <div class="group sharp-card accent-glow p-8 md:p-10 lg:p-12 rounded-xl backdrop-blur-sm" style="margin-bottom: 1em;">
                        <div class="absolute top-0 left-0 w-full h-1 bg-gradient-to-r from-secondary to-secondary/20"></div>
                        <h3 class="text-4xl font-semibold mb-6 text-foreground">
                            {"Enterprise"}
                            <sup class="ml-6 text-[15px] font-bold text-secondary/80 tracking-wide">{"[CUSTOM]"}</sup>
                        </h3>
                        <p class="text-foreground-muted mb-10 text-lg leading-relaxed">{"Tailored solutions for organizations that need advanced security and control."}</p>
                        <ul class="space-y-6 mb-10">
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-secondary/10 mr-3">
                                    <span class="text-secondary text-sm">{"✓"}</span>
                                </span>
                                {"Unlimited participants"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-secondary/10 mr-3">
                                    <span class="text-secondary text-sm">{"✓"}</span>
                                </span>
                                {"Dedicated support"}
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
                                {"Advanced security"}
                            </li>
                        </ul>
                        <a href="#contact" class="group/btn relative inline-flex items-center justify-center py-4 px-6 overflow-hidden rounded-lg transition-all bg-gradient-to-r from-secondary via-secondary/90 to-secondary hover:from-secondary/90 hover:via-secondary/80 hover:to-secondary/90 shadow-md shadow-secondary/20 hover:shadow-lg hover:shadow-secondary/30">
                            {/* Button shine effect */}
                            <div class="absolute top-0 -inset-full h-full w-1/3 z-5 block transform -skew-x-12 bg-gradient-to-r from-transparent to-white opacity-20 group-hover/btn:animate-shine"></div>
                            
                            {/* Button text */}
                            <span class="relative z-10 flex items-center text-white font-semibold">
                                {"Contact Sales"}
                                <svg class="w-5 h-5 ml-2 transition-transform group-hover/btn:translate-x-1" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M17 8l4 4m0 0l-4 4m4-4H3" />
                                </svg>
                            </span>
                        </a>
                    </div>
                </div>
            </div>
        </section>
    }
} 