use leptos::*;

#[component]
pub fn SolutionsSection() -> impl IntoView {
    view! {
        <section id="solutions" class="py-24 bg-background relative overflow-hidden">
            <div class="absolute inset-0 bg-gradient-to-b from-background to-background-light/20 pointer-events-none"></div>
            
            {/* Subtle background pattern */}
            <div class="absolute inset-0 bg-grid-pattern opacity-5 pointer-events-none"></div>
            
            <div class="max-w-4xl mx-auto px-6 relative z-10">
                <h2 class="text-4xl font-bold mb-12 text-center bg-gradient-to-r from-primary/90 to-secondary/90 inline-block text-transparent bg-clip-text">{"Solutions"}</h2>
                <div class="grid md:grid-cols-2 gap-8">
                    <div class="group sharp-card accent-glow p-8 rounded-xl backdrop-blur-sm">
                        <div class="absolute top-0 left-0 w-full h-1 bg-gradient-to-r from-primary to-primary/20"></div>
                        <h3 class="text-2xl font-semibold mb-4 text-foreground">{"Enterprise"}</h3>
                        <p class="text-foreground-muted mb-8 text-lg leading-relaxed">{"Secure, scalable video conferencing for your organization with custom branding and integration options."}</p>
                        <ul class="space-y-4 mb-8">
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"Custom branding"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"API integration"}
                            </li>
                            <li class="flex items-center text-foreground-muted">
                                <span class="inline-flex items-center justify-center h-6 w-6 rounded-full bg-primary/10 mr-3">
                                    <span class="text-primary text-sm">{"✓"}</span>
                                </span>
                                {"Advanced security"}
                            </li>
                        </ul>
                        <a href="#contact" class="inline-flex items-center text-primary hover:text-foreground transition-colors group-hover:translate-x-1 transform transition-transform">
                            {"Contact Sales"} 
                            <span class="ml-2">{"→"}</span>
                        </a>
                    </div>
                    <div class="group sharp-card accent-glow p-8 rounded-xl backdrop-blur-sm">
                        <div class="absolute top-0 left-0 w-full h-1 bg-gradient-to-r from-secondary to-secondary/20"></div>
                        <h3 class="text-2xl font-semibold mb-4 text-foreground">{"Developers"}</h3>
                        <p class="text-foreground-muted mb-8 text-lg leading-relaxed">{"Build custom video experiences with our WebTransport-powered SDK and comprehensive documentation."}</p>
                        <ul class="space-y-4 mb-8">
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
                        <a href="https://github.com/security-union/videocall-rs" class="inline-flex items-center text-secondary hover:text-foreground transition-colors group-hover:translate-x-1 transform transition-transform">
                            {"View Documentation"}
                            <span class="ml-2">{"→"}</span>
                        </a>
                    </div>
                </div>
            </div>
        </section>
    }
} 