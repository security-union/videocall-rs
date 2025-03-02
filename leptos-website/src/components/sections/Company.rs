use leptos::*;

#[component]
pub fn CompanySection() -> impl IntoView {
    view! {
        <section id="company" class="w-full py-24 relative overflow-hidden">
            {/* Full-width background elements */}
            <div class="absolute inset-0 bg-gradient-to-br from-background to-background-light/20 pointer-events-none"></div>
            <div class="absolute inset-0 bg-dot-pattern opacity-5 pointer-events-none"></div>
            
            {/* Full-width decorative circle */}
            <div class="absolute top-20 -right-32 w-96 h-96 bg-primary/5 rounded-full blur-3xl"></div>
            <div class="absolute -bottom-32 -left-32 w-80 h-80 bg-secondary/5 rounded-full blur-3xl"></div>

            {/* Constrained content width */}
            <div class="px-6 max-w-4xl mx-auto relative z-10">
                <h2 class="text-8xl !text-8xl font-black tracking-tight mb-16 text-left gradient-text" style="font-size: 3.84rem;">{"Company"}</h2>
                
                <div class="grid md:grid-cols-2 gap-12 md:gap-16 lg:gap-20">
                    {/* Our Mission Card */}
                    <div class="group hover:scale-[1.02] transition-transform duration-300" style="margin-bottom: 1em;">
                        {/* Card background with 3D effect */}
                        <div class="absolute inset-0 bg-background-light/10 transform group-hover:-translate-y-2 transition-transform duration-300 rounded-xl"></div>
                        <div class="absolute inset-0 bg-background-light/20 transform group-hover:-translate-y-4 transition-transform duration-300 rounded-xl"></div>
                        
                        {/* Main card */}
                        <div class="sharp-card accent-glow p-8 md:p-10 lg:p-12 relative z-10 bg-background-light/5 backdrop-blur-sm">
                            {/* Accent line */}
                            <div class="absolute top-0 left-0 right-0 h-1 bg-gradient-to-r from-primary to-primary/5"></div>
                            
                            {/* Highlight accent */}
                            <div class="absolute inset-0 bg-gradient-to-b from-primary/5 to-transparent rounded-xl transform scale-[1.01] group-hover:scale-[1.03] transition-transform duration-300"></div>
                            
                            {/* Hover glow effect */}
                            <div class="absolute -inset-0.5 bg-gradient-to-r from-primary/20 via-primary/5 to-primary/20 opacity-0 group-hover:opacity-100 rounded-xl blur transition-all duration-500"></div>
                            
                            {/* Content */}
                            <div class="flex flex-col h-full">
                                <h3 class="text-2xl font-semibold mb-8 text-foreground flex items-center">
                                    <span class="w-8 h-8 rounded-full bg-primary/20 flex items-center justify-center mr-3">
                                        <svg class="w-4 h-4 text-primary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 3v4M3 5h4M6 17v4m-2-2h4m5-16l2 2m0 0l2 2m-2-2v12" />
                                        </svg>
                                    </span>
                                    {"Our Mission"}
                                </h3>
                                
                                <p class="text-foreground-muted text-lg mb-10 leading-relaxed">
                                    {"We're building the future of real-time communication. Our mission is to make video conferencing more accessible, performant, and reliable through open-source innovation."}
                                </p>
                                
                                <div class="space-y-8 mt-auto">
                                    <div class="relative overflow-hidden p-4 rounded-lg bg-background-light/10 backdrop-blur-sm group-hover:bg-background-light/20 transition-all">
                                        <div class="absolute top-0 left-0 w-2 h-full bg-primary"></div>
                                        <h4 class="text-xl font-semibold text-primary mb-2 pl-3">{"Open Source First"}</h4>
                                        <p class="text-foreground-muted leading-relaxed pl-3">{"We believe in transparency and community-driven development."}</p>
                                    </div>
                                    
                                    <div class="relative overflow-hidden p-4 rounded-lg bg-background-light/10 backdrop-blur-sm group-hover:bg-background-light/20 transition-all">
                                        <div class="absolute top-0 left-0 w-2 h-full bg-secondary"></div>
                                        <h4 class="text-xl font-semibold text-secondary mb-2 pl-3">{"Built with Rust"}</h4>
                                        <p class="text-foreground-muted leading-relaxed pl-3">{"Leveraging Rust's performance and reliability for better video calls."}</p>
                                    </div>
                                </div>
                            </div>
                        </div>
                    </div>
                    
                    {/* Join Us Card */}
                    <div class="group hover:scale-[1.02] transition-transform duration-300" style="margin-bottom: 1em;">
                        {/* Ring decoration that pulses on hover */}
                        <div class="absolute top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 w-[140%] h-[140%] rounded-full border-2 border-dashed border-primary/10 group-hover:border-primary/20 group-hover:scale-110 transition-all duration-1000"></div>
                        <div class="absolute top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 w-[120%] h-[120%] rounded-full border-2 border-dashed border-secondary/10 group-hover:border-secondary/20 group-hover:scale-100 transition-all duration-700"></div>
                        
                        {/* Card content */}
                        <div class="glass-card accent-glow p-8 md:p-10 lg:p-12 relative z-10 bg-background-light/5 backdrop-blur-sm">
                            {/* Accent line */}
                            <div class="absolute top-0 left-0 right-0 h-1 bg-gradient-to-r from-secondary to-secondary/5"></div>
                            
                            {/* Highlight accent */}
                            <div class="absolute inset-0 bg-gradient-to-b from-secondary/5 to-transparent rounded-xl transform scale-[1.01] group-hover:scale-[1.03] transition-transform duration-300"></div>
                            
                            {/* Hover glow effect */}
                            <div class="absolute -inset-0.5 bg-gradient-to-r from-secondary/20 via-secondary/5 to-secondary/20 opacity-0 group-hover:opacity-100 rounded-xl blur transition-all duration-500"></div>
                            
                            <h3 class="text-2xl font-semibold mb-8 text-foreground flex items-center">
                                <span class="w-8 h-8 rounded-full bg-secondary/20 flex items-center justify-center mr-3">
                                    <svg class="w-4 h-4 text-secondary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M17 20h5v-2a3 3 0 00-5.356-1.857M17 20H7m10 0v-2c0-.656-.126-1.283-.356-1.857M7 20H2v-2a3 3 0 015.356-1.857M7 20v-2c0-.656.126-1.283.356-1.857m0 0a5.002 5.002 0 019.288 0M15 7a3 3 0 11-6 0 3 3 0 016 0zm6 3a2 2 0 11-4 0 2 2 0 014 0zM7 10a2 2 0 11-4 0 2 2 0 014 0z" />
                                    </svg>
                                </span>
                                {"Join Us"}
                            </h3>
                            
                            <p class="text-foreground-muted text-lg mb-10 leading-relaxed">
                                {"We're always looking for talented individuals who share our passion for building great software."}
                            </p>
                            
                            <div class="space-y-6 mt-auto">
                                <a 
                                    href="https://github.com/security-union/videocall-rs" 
                                    class="group/btn relative flex w-full items-center justify-center py-3 px-6 overflow-hidden rounded-lg transition-all"
                                >
                                    {/* Button background with animated gradient */}
                                    <div class="absolute inset-0 bg-gradient-to-r from-primary to-secondary opacity-90 group-hover/btn:opacity-100 transition-opacity"></div>
                                    
                                    {/* Button shine effect */}
                                    <div class="absolute top-0 -inset-full h-full w-1/3 z-5 block transform -skew-x-12 bg-gradient-to-r from-transparent to-white opacity-20 group-hover/btn:animate-shine"></div>
                                    
                                    {/* Button text */}
                                    
                                </a>
                                
                                <a 
                                    href="https://discord.gg/XRdt6WfZyf" 
                                    class="group/btn relative flex w-full items-center justify-center py-3 px-6 overflow-hidden rounded-lg transition-all"
                                >
                                    {/* Button background */}
                                    <div class="absolute inset-0 bg-background border border-primary/20 group-hover/btn:border-primary/40 transition-colors"></div>
                                    
                                    {/* Button text */}
                                    <span class="relative z-10 flex items-center text-foreground-muted group-hover/btn:text-foreground transition-colors font-medium">
                                        <svg class="w-5 h-5 mr-2 text-primary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 12h.01M12 12h.01M16 12h.01M21 12c0 4.418-4.03 8-9 8a9.863 9.863 0 01-4.255-.949L3 20l1.395-3.72C3.512 15.042 3 13.574 3 12c0-4.418 4.03-8 9-8s9 3.582 9 8z" />
                                        </svg>
                                        {"Join our Discord"}
                                    </span>
                                </a>
                            </div>
                        </div>
                    </div>
                </div>
            </div>
        </section>
    }
} 