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
                <h2 class="text-4xl font-bold mb-16 text-center gradient-text">{"Pricing"}</h2>
                
                <div class="grid md:grid-cols-3 gap-8">
                    {/* Free tier */}
                    <div class="relative group overflow-hidden">
                        {/* Card */}
                        <div class="sharp-card p-8 relative h-full flex flex-col">
                            {/* Plan label */}
                            <div class="absolute -right-14 top-6 bg-background-light/50 px-10 py-1 transform rotate-45 text-xs font-semibold text-foreground-muted">{"STARTER"}</div>
                            
                            <h3 class="text-xl font-semibold mb-2 text-foreground flex items-center">
                                <span class="w-7 h-7 rounded-full bg-gray-400/20 flex items-center justify-center mr-3">
                                    <svg class="w-3.5 h-3.5 text-gray-400" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 10V3L4 14h7v7l9-11h-7z" />
                                    </svg>
                                </span>
                                {"Free"}
                            </h3>
                            
                            <div class="mt-4 mb-6">
                                <span class="text-3xl font-bold text-foreground">{"$0"}</span>
                                <span class="text-foreground-subtle text-sm ml-1">{"/ forever"}</span>
                            </div>
                            
                            <ul class="space-y-4 mb-8 text-foreground-muted">
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
                    <div class="relative group overflow-hidden -mt-4 mb-4">
                        {/* Highlight accent */}
                        <div class="absolute inset-0 bg-gradient-to-b from-primary/5 to-secondary/5 rounded-xl transform scale-[1.03] group-hover:scale-[1.05] transition-transform duration-300"></div>
                        
                        {/* Card with glow effect */}
                        <div class="sharp-card accent-glow p-8 relative h-full flex flex-col">
                            {/* Highlight banner */}
                            <div class="absolute top-0 right-0 bg-primary text-white text-xs font-bold px-3 py-1.5 rounded-bl-lg rounded-tr-lg">
                                {"POPULAR"}
                            </div>
                            
                            <h3 class="text-xl font-semibold mb-2 text-foreground flex items-center">
                                <span class="w-7 h-7 rounded-full bg-primary/20 flex items-center justify-center mr-3">
                                    <svg class="w-3.5 h-3.5 text-primary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 10V3L4 14h7v7l9-11h-7z" />
                                    </svg>
                                </span>
                                {"Pro"}
                            </h3>
                            
                            <div class="mt-4 mb-6">
                                <span class="text-3xl font-bold bg-gradient-to-r from-primary to-secondary inline-block text-transparent bg-clip-text">{"$19"}</span>
                                <span class="text-foreground-subtle text-sm ml-1">{"/ user / month"}</span>
                            </div>
                            
                            <ul class="space-y-4 mb-8 text-foreground-muted">
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
                    <div class="relative group overflow-hidden">
                        {/* Card */}
                        <div class="sharp-card p-8 relative h-full flex flex-col">
                            {/* Plan label */}
                            <div class="absolute -right-14 top-6 bg-background-light/50 px-10 py-1 transform rotate-45 text-xs font-semibold text-foreground-muted">{"CUSTOM"}</div>
                            
                            <h3 class="text-xl font-semibold mb-2 text-foreground flex items-center">
                                <span class="w-7 h-7 rounded-full bg-secondary/20 flex items-center justify-center mr-3">
                                    <svg class="w-3.5 h-3.5 text-secondary" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 10V3L4 14h7v7l9-11h-7z" />
                                    </svg>
                                </span>
                                {"Enterprise"}
                            </h3>
                            
                            <div class="mt-4 mb-6">
                                <span class="text-3xl font-bold text-foreground">{"Custom"}</span>
                                <span class="text-foreground-subtle text-sm ml-1">{"/ tailored"}</span>
                            </div>
                            
                            <ul class="space-y-4 mb-8 text-foreground-muted">
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