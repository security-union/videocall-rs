use leptos::*;

#[component]
pub fn Footer() -> impl IntoView {
    view! { 
        <footer class="py-16 px-6 relative overflow-hidden">
            {/* Background gradient */}
            <div class="absolute inset-0 bg-gradient-to-t from-background-light/40 to-background/90 pointer-events-none"></div>
            
            {/* Subtle grid pattern */}
            <div class="absolute inset-0 bg-grid-pattern opacity-5 pointer-events-none"></div>
            
            {/* Top border with gradient */}
            <div class="absolute top-0 left-0 right-0 h-[1px] bg-gradient-to-r from-transparent via-primary/30 to-transparent"></div>
            
            <div class="max-w-4xl mx-auto relative z-10">
                <div class="flex flex-col md:flex-row justify-between items-center mb-12">
                    <div class="mb-8 md:mb-0">
                        <img
                            class="h-10 w-auto"
                            src="/images/videocall_logo.svg"
                            alt="VideoCall.rs"
                        />
                    </div>
                    <div class="flex flex-wrap gap-8 text-foreground-muted">
                        <a href="#solutions" class="relative hover:text-foreground transition-colors group">
                            <span>{"Solutions"}</span>
                            <span class="absolute -bottom-1 left-0 w-0 h-[1px] bg-primary group-hover:w-full transition-all duration-300"></span>
                        </a>
                        <a href="#developers" class="relative hover:text-foreground transition-colors group">
                            <span>{"Developers"}</span>
                            <span class="absolute -bottom-1 left-0 w-0 h-[1px] bg-primary group-hover:w-full transition-all duration-300"></span>
                        </a>
                        <a href="#company" class="relative hover:text-foreground transition-colors group">
                            <span>{"Company"}</span>
                            <span class="absolute -bottom-1 left-0 w-0 h-[1px] bg-primary group-hover:w-full transition-all duration-300"></span>
                        </a>
                        <a href="#customers" class="relative hover:text-foreground transition-colors group">
                            <span>{"Customers"}</span>
                            <span class="absolute -bottom-1 left-0 w-0 h-[1px] bg-primary group-hover:w-full transition-all duration-300"></span>
                        </a>
                        <a href="https://github.com/security-union/videocall-rs" class="relative hover:text-foreground transition-colors group">
                            <span>{"GitHub"}</span>
                            <span class="absolute -bottom-1 left-0 w-0 h-[1px] bg-primary group-hover:w-full transition-all duration-300"></span>
                        </a>
                    </div>
                </div>
                <div class="pt-8 flex flex-col md:flex-row justify-between items-center relative">
                    {/* Subtle divider */}
                    <div class="absolute top-0 left-0 right-0 h-[1px] bg-gradient-to-r from-transparent via-primary/10 to-transparent"></div>
                    
                    <p class="text-foreground-subtle text-sm mb-4 md:mb-0">
                        {"Copyright 2024 VideoCall.rs. All rights reserved."}
                    </p>
                    <div class="flex gap-6">
                        <a href="#" class="text-foreground-subtle hover:text-foreground transition-colors text-sm relative group">
                            <span>{"Privacy Policy"}</span>
                            <span class="absolute -bottom-1 left-0 w-0 h-[1px] bg-primary/50 group-hover:w-full transition-all duration-300"></span>
                        </a>
                        <a href="#" class="text-foreground-subtle hover:text-foreground transition-colors text-sm relative group">
                            <span>{"Terms of Service"}</span>
                            <span class="absolute -bottom-1 left-0 w-0 h-[1px] bg-primary/50 group-hover:w-full transition-all duration-300"></span>
                        </a>
                    </div>
                </div>
            </div>
        </footer>
    }
}
