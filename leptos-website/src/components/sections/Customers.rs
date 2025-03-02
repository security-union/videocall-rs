use leptos::*;

#[component]
pub fn CustomersSection() -> impl IntoView {
    view! {
        <section id="customers" class="w-full py-24 relative overflow-hidden">
            {/* Full-width background elements */}
            <div class="absolute inset-0 bg-gradient-to-br from-background to-background-light/10 pointer-events-none"></div>

            {/* Full-width decorative elements */}
            <div class="absolute top-0 left-0 w-full h-1 bg-gradient-to-r from-transparent via-primary/10 to-transparent"></div>
            <div class="absolute bottom-0 left-0 w-full h-1 bg-gradient-to-r from-transparent via-secondary/10 to-transparent"></div>

            {/* Constrained content width */}
            <div class="px-6 max-w-4xl mx-auto relative z-10">
                <h2 class="text-8xl !text-8xl font-black tracking-tight mb-16 text-left gradient-text" style="font-size: 3.84rem;">{"Trusted By"}</h2>

                {/* Stats with 3D floating cards */}
                <div class="grid md:grid-cols-3 gap-12 md:gap-16 lg:gap-20 mb-24">
                    <div class="float-card glass-card accent-glow p-8 md:p-10 lg:p-12 text-center group hover:scale-[1.02] transition-transform duration-300" style="margin-bottom: 1em;">
                        {/* Accent line */}
                        <div class="absolute top-0 left-0 right-0 h-1 bg-gradient-to-r from-primary to-primary/5"></div>

                        {/* Highlight accent */}
                        <div class="absolute inset-0 bg-gradient-to-b from-primary/5 to-transparent rounded-xl transform scale-[1.01] group-hover:scale-[1.03] transition-transform duration-300"></div>

                        <div class="absolute inset-0 bg-background-light/5 backdrop-blur-sm rounded-lg"></div>

                        <div class="relative z-10">
                            <div class="inline-flex items-center justify-center w-16 h-16 rounded-full bg-primary/10 mb-6 mx-auto">
                                <svg class="w-8 h-8 text-primary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 4.354a4 4 0 110 5.292M15 21H3v-1a6 6 0 0112 0v1zm0 0h6v-1a6 6 0 00-9-5.197M13 7a4 4 0 11-8 0 4 4 0 018 0z" />
                                </svg>
                            </div>
                            <div class="text-5xl font-bold bg-gradient-to-r from-primary to-primary/70 inline-block text-transparent bg-clip-text mb-3 group-hover:scale-110 transform transition-transform">{"500+"}</div>
                            <div class="text-foreground-muted text-lg">{"Active Users"}</div>
                        </div>
                    </div>

                    <div class="float-card glass-card accent-glow p-8 md:p-10 lg:p-12 text-center group hover:scale-[1.02] transition-transform duration-300" style="margin-bottom: 1em;">
                        {/* Accent line */}
                        <div class="absolute top-0 left-0 right-0 h-1 bg-gradient-to-r from-accent to-accent/5"></div>

                        {/* Highlight accent */}
                        <div class="absolute inset-0 bg-gradient-to-b from-accent/5 to-transparent rounded-xl transform scale-[1.01] group-hover:scale-[1.03] transition-transform duration-300"></div>

                        <div class="absolute inset-0 bg-background-light/5 backdrop-blur-sm rounded-lg"></div>

                        <div class="relative z-10">
                            <div class="inline-flex items-center justify-center w-16 h-16 rounded-full bg-accent/10 mb-6 mx-auto">
                                <svg class="w-8 h-8 text-accent" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M11.049 2.927c.3-.921 1.603-.921 1.902 0l1.519 4.674a1 1 0 00.95.69h4.915c.969 0 1.371 1.24.588 1.81l-3.976 2.888a1 1 0 00-.363 1.118l1.518 4.674c.3.922-.755 1.688-1.538 1.118l-3.976-2.888a1 1 0 00-1.176 0l-3.976 2.888c-.783.57-1.838-.197-1.538-1.118l1.518-4.674a1 1 0 00-.363-1.118l-3.976-2.888c-.784-.57-.38-1.81.588-1.81h4.914a1 1 0 00.951-.69l1.519-4.674z" />
                                </svg>
                            </div>
                            <div class="text-5xl font-bold bg-gradient-to-r from-accent to-accent/70 inline-block text-transparent bg-clip-text mb-3 group-hover:scale-110 transform transition-transform">{"1.5K+"}</div>
                            <div class="text-foreground-muted text-lg">{"GitHub Stars"}</div>
                        </div>
                    </div>
                </div>

                {/* Testimonials - hidden but preserved in the DOM */}
                <div class="hidden">
                    <h3 class="text-7xl !text-7xl font-black tracking-tight mb-16 text-foreground text-left flex items-center" style="font-size: 2.88rem;">
                        <span class="w-16 h-16 rounded-full bg-primary/20 flex items-center justify-center mr-6">
                            <svg class="w-8 h-8 text-primary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 10h.01M12 10h.01M16 10h.01M9 16H5a2 2 0 01-2-2V6a2 2 0 012-2h14a2 2 0 012 2v8a2 2 0 01-2 2h-5l-5 5v-5z" />
                            </svg>
                        </span>
                        {"What People Are Saying"}
                    </h3>

                    <div class="grid md:grid-cols-2 gap-12 md:gap-16 lg:gap-20">
                        {/* First testimonial */}
                        <div class="relative group hover:scale-[1.02] transition-transform duration-300" style="margin-bottom: 1em;">
                            {/* Floating quotation mark */}
                            <div class="absolute -top-6 -left-6 text-7xl text-primary/20 font-serif pointer-events-none group-hover:text-primary/30 transition-colors">{"❝"}</div>

                            {/* Card */}
                            <div class="sharp-card accent-glow p-8 md:p-10 lg:p-12 relative bg-background-light/5 backdrop-blur-sm">
                                {/* Accent line */}
                                <div class="absolute top-0 left-0 right-0 h-1 bg-gradient-to-r from-primary to-primary/5"></div>

                                {/* Highlight accent */}
                                <div class="absolute inset-0 bg-gradient-to-b from-primary/5 to-transparent rounded-xl transform scale-[1.01] group-hover:scale-[1.03] transition-transform duration-300"></div>

                                <p class="text-foreground-muted text-lg mb-6 leading-relaxed relative z-10">{"The performance and reliability of videocall.rs has been exceptional. The WebTransport implementation makes a real difference in latency."}</p>

                                <div class="flex items-center">
                                    <div class="w-12 h-12 rounded-full bg-primary/20 mr-4 flex items-center justify-center overflow-hidden">
                                        <svg class="w-6 h-6 text-primary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5.121 17.804A13.937 13.937 0 0112 16c2.5 0 4.847.655 6.879 1.804M15 10a3 3 0 11-6 0 3 3 0 016 0zm6 2a9 9 0 11-18 0 9 9 0 0118 0z" />
                                        </svg>
                                    </div>
                                    <div>
                                        <div class="text-lg font-medium text-foreground group-hover:text-primary transition-colors">{"Sarah Chen"}</div>
                                        <div class="text-foreground-subtle">{"Tech Lead at DevCorp"}</div>
                                    </div>
                                </div>
                            </div>
                        </div>

                        {/* Second testimonial */}
                        <div class="relative group hover:scale-[1.02] transition-transform duration-300" style="margin-bottom: 1em;">
                            {/* Floating quotation mark */}
                            <div class="absolute -top-6 -left-6 text-7xl text-secondary/20 font-serif pointer-events-none group-hover:text-secondary/30 transition-colors">{"❝"}</div>

                            {/* Card */}
                            <div class="sharp-card accent-glow p-8 md:p-10 lg:p-12 relative bg-background-light/5 backdrop-blur-sm">
                                {/* Accent line */}
                                <div class="absolute top-0 left-0 right-0 h-1 bg-gradient-to-r from-secondary to-secondary/5"></div>

                                {/* Highlight accent */}
                                <div class="absolute inset-0 bg-gradient-to-b from-secondary/5 to-transparent rounded-xl transform scale-[1.01] group-hover:scale-[1.03] transition-transform duration-300"></div>

                                <p class="text-foreground-muted text-lg mb-6 leading-relaxed relative z-10">{"Being open source and built with Rust gives us confidence in both the security and performance of the platform."}</p>

                                <div class="flex items-center">
                                    <div class="w-12 h-12 rounded-full bg-secondary/20 mr-4 flex items-center justify-center overflow-hidden">
                                        <svg class="w-6 h-6 text-secondary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5.121 17.804A13.937 13.937 0 0112 16c2.5 0 4.847.655 6.879 1.804M15 10a3 3 0 11-6 0 3 3 0 016 0zm6 2a9 9 0 11-18 0 9 9 0 0118 0z" />
                                        </svg>
                                    </div>
                                    <div>
                                        <div class="text-lg font-medium text-foreground group-hover:text-secondary transition-colors">{"Mark Thompson"}</div>
                                        <div class="text-foreground-subtle">{"CTO at StartupX"}</div>
                                    </div>
                                </div>
                            </div>
                        </div>
                    </div>
                </div>
            </div>
        </section>
    }
}
