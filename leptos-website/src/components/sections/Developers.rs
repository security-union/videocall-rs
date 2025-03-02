use leptos::*;
use crate::components::SecondaryButton;

#[component]
pub fn DevelopersSection() -> impl IntoView {
    view! {
        <section id="developers" class="w-full py-24 relative overflow-hidden">
            {/* Full-width background elements */}
            <div class="absolute inset-0 bg-gradient-to-br from-background/50 to-background-light/5 pointer-events-none"></div>
            <div class="absolute inset-0 bg-grid-pattern opacity-[0.03] pointer-events-none"></div>
            
            {/* Full-width decorative elements */}
            <div class="absolute top-40 -right-20 w-96 h-96 rounded-full bg-primary/10 blur-3xl opacity-20 animate-pulse-slow"></div>
            <div class="absolute bottom-40 -left-20 w-80 h-80 rounded-full bg-secondary/10 blur-3xl opacity-20 animate-pulse-slow"></div>
            
            {/* Constrained content width */}
            <div class="px-6 max-w-4xl mx-auto relative z-10">
                <h2 class="text-8xl !text-8xl font-black tracking-tight mb-16 text-left gradient-text" style="font-size: 3.84rem;">{"For Developers"}</h2>
                
                <div class="grid md:grid-cols-3 gap-12 md:gap-16 lg:gap-20 mb-16">
                    {/* Feature Card 1 */}
                    <div class="group hover:scale-[1.02] transition-transform duration-300" style="margin-bottom: 1em;">
                        <div class="sharp-card accent-glow h-full p-8 md:p-10 lg:p-12 relative overflow-hidden bg-background-light/5 backdrop-blur-sm">
                            {/* Card accent */}
                            <div class="absolute top-0 left-0 w-full h-1 bg-gradient-to-r from-primary to-primary/5"></div>
                            
                            {/* Highlight accent */}
                            <div class="absolute inset-0 bg-gradient-to-b from-primary/5 to-transparent rounded-xl transform scale-[1.01] group-hover:scale-[1.03] transition-transform duration-300"></div>
                            {/* Hover glow effect */}
                            <div class="absolute -inset-0.5 bg-gradient-to-r from-primary/20 via-primary/5 to-primary/20 opacity-0 group-hover:opacity-100 rounded-xl blur transition-all duration-500"></div>
                            
                            <div class="relative">
                                <div class="w-12 h-12 rounded-lg bg-primary/10 flex items-center justify-center mb-4">
                                    <svg class="w-6 h-6 text-primary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10 20l4-16m4 4l4 4-4 4M6 16l-4-4 4-4" />
                                    </svg>
                                </div>
                                
                                <h3 class="text-xl font-semibold mb-3 text-foreground">{"Open Source"}</h3>
                                <p class="text-foreground-muted mb-4">{"Our video calling platform is entirely open source, built with Rust for speed and reliability."}</p>
                                <SecondaryButton
                                    title="Explore on GitHub"
                                    href=Some("https://github.com/security-union/videocall-rs".to_string())
                                    class="mt-4"
                                />
                            </div>
                        </div>
                    </div>
                    
                    {/* Feature Card 2 */}
                    <div class="group hover:scale-[1.02] transition-transform duration-300" style="margin-bottom: 1em;">
                        <div class="sharp-card accent-glow h-full p-8 md:p-10 lg:p-12 relative overflow-hidden bg-background-light/5 backdrop-blur-sm">
                            {/* Card accent */}
                            <div class="absolute top-0 left-0 w-full h-1 bg-gradient-to-r from-secondary to-secondary/5"></div>
                            
                            {/* Highlight accent */}
                            <div class="absolute inset-0 bg-gradient-to-b from-secondary/5 to-transparent rounded-xl transform scale-[1.01] group-hover:scale-[1.03] transition-transform duration-300"></div>
                            
                            {/* Hover glow effect */}
                            <div class="absolute -inset-0.5 bg-gradient-to-r from-secondary/20 via-secondary/5 to-secondary/20 opacity-0 group-hover:opacity-100 rounded-xl blur transition-all duration-500"></div>
                            
                            <div class="relative">
                                <div class="w-12 h-12 rounded-lg bg-secondary/10 flex items-center justify-center mb-4">
                                    <svg class="w-6 h-6 text-secondary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 19v-6a2 2 0 00-2-2H5a2 2 0 00-2 2v6a2 2 0 002 2h2a2 2 0 002-2zm0 0V9a2 2 0 012-2h2a2 2 0 012 2v10m-6 0a2 2 0 002 2h2a2 2 0 002-2m0 0V5a2 2 0 012-2h2a2 2 0 012 2v14a2 2 0 01-2 2h-2a2 2 0 01-2-2z" />
                                    </svg>
                                </div>
                                
                                <h3 class="text-xl font-semibold mb-3 text-foreground">{"High Performance"}</h3>
                                <p class="text-foreground-muted mb-4">{"Written in Rust to ensure exceptional performance and reliability for your video calls."}</p>
                                <SecondaryButton
                                    title="Read the docs"
                                    href=Some("https://github.com/security-union/videocall-rs".to_string())
                                    class="mt-4"
                                />
                            </div>
                        </div>
                    </div>
                    
                    
                </div>
                
                {/* GitHub card */}
                <div class="sharp-card accent-glow relative overflow-hidden group hover:scale-[1.02] transition-transform duration-300 p-8 md:p-10 lg:p-12 bg-background-light/5 backdrop-blur-sm">
                    {/* Card accent */}
                    <div class="absolute top-0 left-0 w-full h-1 bg-gradient-to-r from-primary to-primary/5"></div>
                    
                    {/* Highlight accent */}
                    <div class="absolute inset-0 bg-gradient-to-b from-primary/5 to-transparent rounded-xl transform scale-[1.01] group-hover:scale-[1.03] transition-transform duration-300"></div>
                    
                    {/* Decorative elements */}
                    <div class="absolute -top-16 -right-16 w-32 h-32 bg-grid-pattern opacity-5 transform rotate-12"></div>
                    <div class="absolute -bottom-16 -left-16 w-32 h-32 bg-grid-pattern opacity-5 transform -rotate-12"></div>
                    
                    {/* Hover glow effect */}
                    <div class="absolute -inset-0.5 bg-gradient-to-r from-primary/20 via-primary/5 to-primary/20 opacity-0 group-hover:opacity-100 rounded-xl blur transition-all duration-500"></div>
                    
                    <div class="relative z-10 flex flex-col md:flex-row items-center justify-between gap-8">
                        <div class="max-w-xl">
                            <h3 class="text-2xl font-bold mb-4 text-foreground">{"Join our GitHub community"}</h3>
                            <p class="text-foreground-muted mb-6">{"Contribute to our open-source project, report issues, or just star the repo to show your support. We welcome developers of all experience levels!"}</p>
                            
                            <div class="flex flex-wrap gap-4 mb-6">
                                <div class="flex items-center bg-background-light/20 px-4 py-2 rounded-lg">
                                    <svg class="w-5 h-5 text-primary mr-2" fill="currentColor" viewBox="0 0 20 20">
                                        <path fill-rule="evenodd" d="M10 1a9 9 0 100 18A9 9 0 0010 1zm0 16a7 7 0 100-14 7 7 0 000 14zm1-11a1 1 0 10-2 0v4a1 1 0 00.293.707l2.828 2.829a1 1 0 101.415-1.415L11 9.586V6z" clip-rule="evenodd" />
                                    </svg>
                                    <span class="text-sm text-foreground-muted">{"280+ commits"}</span>
                                </div>
                                
                                <div class="flex items-center bg-background-light/20 px-4 py-2 rounded-lg">
                                    <svg class="w-5 h-5 text-secondary mr-2" fill="currentColor" viewBox="0 0 20 20">
                                        <path d="M10 12a2 2 0 100-4 2 2 0 000 4z" />
                                        <path fill-rule="evenodd" d="M.458 10C1.732 5.943 5.522 3 10 3s8.268 2.943 9.542 7c-1.274 4.057-5.064 7-9.542 7S1.732 14.057.458 10zM14 10a4 4 0 11-8 0 4 4 0 018 0z" clip-rule="evenodd" />
                                    </svg>
                                    <span class="text-sm text-foreground-muted">{"120+ watchers"}</span>
                                </div>
                                
                                <div class="flex items-center bg-background-light/20 px-4 py-2 rounded-lg">
                                    <svg class="w-5 h-5 text-accent mr-2" fill="currentColor" viewBox="0 0 20 20">
                                        <path fill-rule="evenodd" d="M5 2a1 1 0 011 1v1h1a1 1 0 010 2H6v1a1 1 0 01-2 0V6H3a1 1 0 010-2h1V3a1 1 0 011-1zm0 10a1 1 0 011 1v1h1a1 1 0 110 2H6v1a1 1 0 11-2 0v-1H3a1 1 0 110-2h1v-1a1 1 0 011-1zM12 2a1 1 0 01.967.744L14.146 7.2 17.5 9.134a1 1 0 010 1.732l-3.354 1.935-1.18 4.455a1 1 0 01-1.933 0L9.854 12.8 6.5 10.866a1 1 0 010-1.732l3.354-1.935 1.18-4.455A1 1 0 0112 2z" clip-rule="evenodd" />
                                    </svg>
                                    <span class="text-sm text-foreground-muted">{"1.5k+ stars"}</span>
                                </div>
                            </div>
                        </div>
                        
                        <div class="flex-shrink-0">
                            <SecondaryButton
                                title="Visit GitHub Repository"
                                href=Some("https://github.com/videocall-rs".to_string())
                                class="flex items-center"
                            />
                        </div>
                    </div>
                </div>
            </div>
        </section>
    }
}