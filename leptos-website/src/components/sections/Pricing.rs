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
                
                <div class="grid md:grid-cols-3 gap-12 md:gap-16 lg:gap-20">
                    {/* Free tier */}
                    <div class="relative group overflow-hidden" style="margin-bottom: 1em;">
                        {/* Card */}
                        <div class="sharp-card accent-glow p-8 md:p-10 lg:p-12 relative h-full flex flex-col bg-background-light/5 backdrop-blur-sm">
                            {/* Accent line */}
                            <div class="absolute top-0 left-0 right-0 h-1 bg-gradient-to-r from-gray-400 to-gray-400/5"></div>
                            
                            {/* Highlight accent */}
                            <div class="absolute inset-0 bg-gradient-to-b from-gray-400/5 to-transparent rounded-xl transform scale-[1.01] group-hover:scale-[1.03] transition-transform duration-300"></div>
                            
                            {/* Hover glow effect */}
                            <div class="absolute -inset-0.5 bg-gradient-to-r from-gray-400/20 via-gray-400/5 to-gray-400/20 opacity-0 group-hover:opacity-100 rounded-xl blur transition-all duration-500"></div>
                            
                            <h3 class="text-2xl font-semibold mb-2 text-foreground flex items-center">
                                <span class="w-7 h-7 rounded-full bg-gray-400/20 flex items-center justify-center mr-3">
                                    <svg class="w-3.5 h-3.5 text-gray-400" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 10V3L4 14h7v7l9-11h-7z" />
                                    </svg>
                                </span>
                                <span>{"Free"}<sup class="text-sm font-bold text-primary ml-1">{"[STARTER]"}</sup></span>
                            </h3>
                            
                            <div class="mt-4 mb-8">
                                <span class="text-3xl font-bold text-foreground">{"$0"}</span>
                                <span class="text-foreground-subtle text-sm ml-1">{"/ forever"}</span>
                            </div>
                            
                            <ul class="space-y-6 mb-10 text-foreground-muted">
                                <li class="flex items-start">
                                    <svg class="w-5 h-5 text-success mr-3 mt-0.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                                    </svg>
                                    <span>{"Up to 4 participants"}</span>
                                </li>
                                <li class="flex items-start">
                                    <svg class="w-5 h-5 text-success mr-3 mt-0.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                                    </svg>
                                    <span>{"30 minute meetings"}</span>
                                </li>
                                <li class="flex items-start">
                                    <svg class="w-5 h-5 text-success mr-3 mt-0.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                                    </svg>
                                    <span>{"Basic features"}</span>
                                </li>
                                <li class="flex items-start">
                                    <svg class="w-5 h-5 text-success mr-3 mt-0.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                                    </svg>
                                    <span>{"Community support"}</span>
                                </li>
                            </ul>
                            
                            <a 
                                href="#" 
                                class="mt-auto group/btn relative flex w-full items-center justify-center py-3 px-6 overflow-hidden rounded-lg transition-all"
                            >
                                {/* Button background */}
                                <div class="absolute inset-0 bg-background-light/20 group-hover/btn:bg-background-light/30 transition-colors"></div>
                                
                                {/* Button text */}
                                <span class="relative z-10 flex items-center text-foreground font-medium">
                                    {"Get Started"}
                                </span>
                            </a>
                        </div>
                    </div>
                    
                    {/* Pro tier */}
                    <div class="relative group overflow-hidden -mt-4 md:-mt-6 mb-4 md:mb-6" style="margin-bottom: 1em;">
                        {/* Highlight accent */}
                        <div class="absolute inset-0 bg-gradient-to-b from-primary/5 to-secondary/5 rounded-xl transform scale-[1.03] group-hover:scale-[1.05] transition-transform duration-300"></div>
                        
                        {/* Card with glow effect */}
                        <div class="sharp-card accent-glow p-8 md:p-10 lg:p-12 relative h-full flex flex-col bg-background-light/5 backdrop-blur-sm">
                            {/* Accent line */}
                            <div class="absolute top-0 left-0 right-0 h-1 bg-gradient-to-r from-primary to-secondary"></div>
                            
                            {/* Hover glow effect */}
                            <div class="absolute -inset-0.5 bg-gradient-to-r from-primary/20 via-primary/5 to-primary/20 opacity-0 group-hover:opacity-100 rounded-xl blur transition-all duration-500"></div>
                            
                            <h3 class="text-2xl font-semibold mb-2 text-foreground flex items-center">
                                <span class="w-7 h-7 rounded-full bg-primary/20 flex items-center justify-center mr-3">
                                    <svg class="w-3.5 h-3.5 text-primary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 10V3L4 14h7v7l9-11h-7z" />
                                    </svg>
                                </span>
                                <span>{"Pro"}<sup class="text-sm font-bold text-primary ml-1">{"[POPULAR]"}</sup></span>
                            </h3>
                            
                            <div class="mt-4 mb-8">
                                <span class="text-3xl font-bold bg-gradient-to-r from-primary to-secondary inline-block text-transparent bg-clip-text">{"$19"}</span>
                                <span class="text-foreground-subtle text-sm ml-1">{"/ user / month"}</span>
                            </div>
                            
                            <ul class="space-y-6 mb-10 text-foreground-muted">
                                <li class="flex items-start">
                                    <svg class="w-5 h-5 text-primary mr-3 mt-0.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                                    </svg>
                                    <span>{"Up to 50 participants"}</span>
                                </li>
                                <li class="flex items-start">
                                    <svg class="w-5 h-5 text-primary mr-3 mt-0.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                                    </svg>
                                    <span>{"Unlimited meeting duration"}</span>
                                </li>
                                <li class="flex items-start">
                                    <svg class="w-5 h-5 text-primary mr-3 mt-0.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                                    </svg>
                                    <span>{"Advanced features"}</span>
                                </li>
                                <li class="flex items-start">
                                    <svg class="w-5 h-5 text-primary mr-3 mt-0.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                                    </svg>
                                    <span>{"Priority support"}</span>
                                </li>
                            </ul>
                            
                            <a 
                                href="#" 
                                class="mt-auto group/btn relative flex w-full items-center justify-center py-3 px-6 overflow-hidden rounded-lg transition-all"
                            >
                                {/* Button background with animated gradient */}
                                <div class="absolute inset-0 bg-gradient-to-r from-primary to-secondary opacity-90 group-hover/btn:opacity-100 transition-opacity"></div>
                                
                                {/* Button shine effect */}
                                <div class="absolute top-0 -inset-full h-full w-1/3 z-5 block transform -skew-x-12 bg-gradient-to-r from-transparent to-white opacity-20 group-hover/btn:animate-shine"></div>
                                
                                {/* Button text */}
                                <span class="relative z-10 flex items-center text-white font-medium">
                                    <svg class="w-5 h-5 mr-2" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 15v2m-6 4h12a2 2 0 002-2v-6a2 2 0 00-2-2H6a2 2 0 00-2 2v6a2 2 0 002 2zm10-10V7a4 4 0 00-8 0v4h8z" />
                                    </svg>
                                    {"Start Free Trial"}
                                </span>
                            </a>
                        </div>
                    </div>
                    
                    {/* Enterprise tier */}
                    <div class="relative group overflow-hidden" style="margin-bottom: 1em;">
                        {/* Card */}
                        <div class="sharp-card accent-glow p-8 md:p-10 lg:p-12 relative h-full flex flex-col bg-background-light/5 backdrop-blur-sm">
                            {/* Accent line */}
                            <div class="absolute top-0 left-0 right-0 h-1 bg-gradient-to-r from-secondary to-secondary/5"></div>
                            
                            {/* Highlight accent */}
                            <div class="absolute inset-0 bg-gradient-to-b from-secondary/5 to-transparent rounded-xl transform scale-[1.01] group-hover:scale-[1.03] transition-transform duration-300"></div>
                            
                            {/* Hover glow effect */}
                            <div class="absolute -inset-0.5 bg-gradient-to-r from-secondary/20 via-secondary/5 to-secondary/20 opacity-0 group-hover:opacity-100 rounded-xl blur transition-all duration-500"></div>
                            
                            <h3 class="text-2xl font-semibold mb-2 text-foreground flex items-center">
                                <span class="w-7 h-7 rounded-full bg-secondary/20 flex items-center justify-center mr-3">
                                    <svg class="w-3.5 h-3.5 text-secondary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 10V3L4 14h7v7l9-11h-7z" />
                                    </svg>
                                </span>
                                <span>{"Enterprise"}<sup class="text-sm font-bold text-primary ml-1">{"[CUSTOM]"}</sup></span>
                            </h3>
                            
                            <div class="mt-4 mb-8">
                                <span class="text-3xl font-bold text-foreground">{"Custom"}</span>
                                <span class="text-foreground-subtle text-sm ml-1">{"/ tailored"}</span>
                            </div>
                            
                            <ul class="space-y-6 mb-10 text-foreground-muted">
                                <li class="flex items-start">
                                    <svg class="w-5 h-5 text-secondary mr-3 mt-0.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                                    </svg>
                                    <span>{"Unlimited participants"}</span>
                                </li>
                                <li class="flex items-start">
                                    <svg class="w-5 h-5 text-secondary mr-3 mt-0.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                                    </svg>
                                    <span>{"Unlimited meeting duration"}</span>
                                </li>
                                <li class="flex items-start">
                                    <svg class="w-5 h-5 text-secondary mr-3 mt-0.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                                    </svg>
                                    <span>{"Custom features"}</span>
                                </li>
                                <li class="flex items-start">
                                    <svg class="w-5 h-5 text-secondary mr-3 mt-0.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                                    </svg>
                                    <span>{"Dedicated support"}</span>
                                </li>
                                <li class="flex items-start">
                                    <svg class="w-5 h-5 text-secondary mr-3 mt-0.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                                    </svg>
                                    <span>{"SLA guarantees"}</span>
                                </li>
                            </ul>
                            
                            <a 
                                href="#" 
                                class="mt-auto group/btn relative flex w-full items-center justify-center py-3 px-6 overflow-hidden rounded-lg transition-all"
                            >
                                {/* Button background */}
                                <div class="absolute inset-0 bg-background-light/20 group-hover/btn:bg-background-light/30 transition-colors"></div>
                                
                                {/* Button text */}
                                <span class="relative z-10 flex items-center text-foreground font-medium">
                                    <svg class="w-5 h-5 mr-2 text-secondary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 10h.01M12 10h.01M16 10h.01M9 16H5a2 2 0 01-2-2V6a2 2 0 012-2h14a2 2 0 012 2v8a2 2 0 01-2 2h-5l-5 5v-5z" />
                                    </svg>
                                    {"Contact Sales"}
                                </span>
                            </a>
                        </div>
                    </div>
                </div>
            </div>
        </section>
    }
} 