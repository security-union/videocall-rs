use leptos::*;

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
                                
                                <a href="https://github.com/videocall-rs" class="inline-flex items-center text-primary hover:text-primary-bright group/link">
                                    <span class="transition-all duration-300 border-b border-transparent group-hover/link:border-primary">{"Explore on GitHub"}</span>
                                    <svg class="w-4 h-4 ml-1 transform transition-transform duration-300 group-hover/link:translate-x-1" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 7l5 5m0 0l-5 5m5-5H6" />
                                    </svg>
                                </a>
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
                                
                                <a href="#" class="inline-flex items-center text-secondary hover:text-secondary-bright group/link">
                                    <span class="transition-all duration-300 border-b border-transparent group-hover/link:border-secondary">{"Read the docs"}</span>
                                    <svg class="w-4 h-4 ml-1 transform transition-transform duration-300 group-hover/link:translate-x-1" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 7l5 5m0 0l-5 5m5-5H6" />
                                    </svg>
                                </a>
                            </div>
                        </div>
                    </div>
                    
                    {/* Feature Card 3 */}
                    <div class="group hover:scale-[1.02] transition-transform duration-300" style="margin-bottom: 1em;">
                        <div class="sharp-card accent-glow h-full p-8 md:p-10 lg:p-12 relative overflow-hidden bg-background-light/5 backdrop-blur-sm">
                            {/* Card accent */}
                            <div class="absolute top-0 left-0 w-full h-1 bg-gradient-to-r from-accent to-accent/5"></div>
                            
                            {/* Highlight accent */}
                            <div class="absolute inset-0 bg-gradient-to-b from-accent/5 to-transparent rounded-xl transform scale-[1.01] group-hover:scale-[1.03] transition-transform duration-300"></div>
                            
                            {/* Hover glow effect */}
                            <div class="absolute -inset-0.5 bg-gradient-to-r from-accent/20 via-accent/5 to-accent/20 opacity-0 group-hover:opacity-100 rounded-xl blur transition-all duration-500"></div>
                            
                            <div class="relative">
                                <div class="w-12 h-12 rounded-lg bg-accent/10 flex items-center justify-center mb-4">
                                    <svg class="w-6 h-6 text-accent" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 15v2m-6 4h12a2 2 0 002-2v-6a2 2 0 00-2-2H6a2 2 0 00-2 2v6a2 2 0 002 2zm10-10V7a4 4 0 00-8 0v4h8z" />
                                    </svg>
                                </div>
                                
                                <h3 class="text-xl font-semibold mb-3 text-foreground">{"End-to-End Encrypted"}</h3>
                                <p class="text-foreground-muted mb-4">{"Your conversations stay private with our robust end-to-end encryption implementation."}</p>
                                
                                <a href="#" class="inline-flex items-center text-accent hover:text-accent-bright group/link">
                                    <span class="transition-all duration-300 border-b border-transparent group-hover/link:border-accent">{"Learn about security"}</span>
                                    <svg class="w-4 h-4 ml-1 transform transition-transform duration-300 group-hover/link:translate-x-1" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 7l5 5m0 0l-5 5m5-5H6" />
                                    </svg>
                                </a>
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
                                    <span class="text-sm text-foreground-muted">{"580+ stars"}</span>
                                </div>
                            </div>
                        </div>
                        
                        <div class="flex-shrink-0">
                            <a 
                                href="https://github.com/videocall-rs" 
                                class="group/btn inline-flex items-center justify-center space-x-2 relative overflow-hidden px-8 py-4 rounded-lg"
                            >
                                {/* Button background with shine */}
                                <div class="absolute inset-0 bg-foreground dark:bg-foreground-light group-hover/btn:bg-primary transition-colors duration-300"></div>
                                
                                {/* Button shine effect */}
                                <div class="absolute top-0 -inset-full h-full w-1/3 z-5 block transform -skew-x-12 bg-gradient-to-r from-transparent to-white opacity-10 group-hover/btn:animate-shine"></div>
                                
                                {/* Button content */}
                                <span class="relative z-10 flex items-center">
                                    <svg class="w-6 h-6 mr-2 text-background" fill="currentColor" viewBox="0 0 24 24">
                                        <path fill-rule="evenodd" d="M12 2C6.477 2 2 6.484 2 12.017c0 4.425 2.865 8.18 6.839 9.504.5.092.682-.217.682-.483 0-.237-.008-.868-.013-1.703-2.782.605-3.369-1.343-3.369-1.343-.454-1.158-1.11-1.466-1.11-1.466-.908-.62.069-.608.069-.608 1.003.07 1.531 1.032 1.531 1.032.892 1.53 2.341 1.088 2.91.832.092-.647.35-1.088.636-1.338-2.22-.253-4.555-1.113-4.555-4.951 0-1.093.39-1.988 1.029-2.688-.103-.253-.446-1.272.098-2.65 0 0 .84-.27 2.75 1.026A9.564 9.564 0 0112 6.844c.85.004 1.705.115 2.504.337 1.909-1.296 2.747-1.027 2.747-1.027.546 1.379.202 2.398.1 2.651.64.7 1.028 1.595 1.028 2.688 0 3.848-2.339 4.695-4.566 4.943.359.309.678.92.678 1.855 0 1.338-.012 2.419-.012 2.747 0 .268.18.58.688.482A10.019 10.019 0 0022 12.017C22 6.484 17.522 2 12 2z" clip-rule="evenodd" />
                                    </svg>
                                    <span class="text-background font-medium">{"Visit GitHub Repository"}</span>
                                </span>
                            </a>
                        </div>
                    </div>
                </div>
            </div>
        </section>
    }
} 