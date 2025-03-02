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
                            alt="videocall.rs"
                        />
                    </div>

                    {/* Navigation links - improved layout */}
                    <nav class="w-full md:w-auto">
                        <ul class="grid grid-cols-2 sm:grid-cols-3 md:flex md:flex-row gap-x-10 gap-y-6 text-foreground-muted">
                            <li>
                                <a href="#solutions" class="relative hover:text-foreground transition-colors group block">
                                    <span>{"Solutions"}</span>
                                    <span class="absolute -bottom-1 left-0 w-0 h-[1px] bg-primary group-hover:w-full transition-all duration-300"></span>
                                </a>
                            </li>
                            <li>
                                <a href="#developers" class="relative hover:text-foreground transition-colors group block">
                                    <span>{"Developers"}</span>
                                    <span class="absolute -bottom-1 left-0 w-0 h-[1px] bg-primary group-hover:w-full transition-all duration-300"></span>
                                </a>
                            </li>
                            <li>
                                <a href="#company" class="relative hover:text-foreground transition-colors group block">
                                    <span>{"Company"}</span>
                                    <span class="absolute -bottom-1 left-0 w-0 h-[1px] bg-primary group-hover:w-full transition-all duration-300"></span>
                                </a>
                            </li>
                            <li>
                                <a href="#customers" class="relative hover:text-foreground transition-colors group block">
                                    <span>{"Customers"}</span>
                                    <span class="absolute -bottom-1 left-0 w-0 h-[1px] bg-primary group-hover:w-full transition-all duration-300"></span>
                                </a>
                            </li>
                            <li>
                                <a href="#pricing" class="relative hover:text-foreground transition-colors group block">
                                    <span>{"Pricing"}</span>
                                    <span class="absolute -bottom-1 left-0 w-0 h-[1px] bg-primary group-hover:w-full transition-all duration-300"></span>
                                </a>
                            </li>
                            <li>
                                <a href="https://github.com/videocall-rs" class="relative hover:text-foreground transition-colors group block">
                                    <span class="flex items-center">
                                        <svg class="w-4 h-4 mr-1" fill="currentColor" viewBox="0 0 24 24">
                                            <path fill-rule="evenodd" d="M12 2C6.477 2 2 6.484 2 12.017c0 4.425 2.865 8.18 6.839 9.504.5.092.682-.217.682-.483 0-.237-.008-.868-.013-1.703-2.782.605-3.369-1.343-3.369-1.343-.454-1.158-1.11-1.466-1.11-1.466-.908-.62.069-.608.069-.608 1.003.07 1.531 1.032 1.531 1.032.892 1.53 2.341 1.088 2.91.832.092-.647.35-1.088.636-1.338-2.22-.253-4.555-1.113-4.555-4.951 0-1.093.39-1.988 1.029-2.688-.103-.253-.446-1.272.098-2.65 0 0 .84-.27 2.75 1.026A9.564 9.564 0 0112 6.844c.85.004 1.705.115 2.504.337 1.909-1.296 2.747-1.027 2.747-1.027.546 1.379.202 2.398.1 2.651.64.7 1.028 1.595 1.028 2.688 0 3.848-2.339 4.695-4.566 4.943.359.309.678.92.678 1.855 0 1.338-.012 2.419-.012 2.747 0 .268.18.58.688.482A10.019 10.019 0 0022 12.017C22 6.484 17.522 2 12 2z" clip-rule="evenodd" />
                                        </svg>
                                        {"GitHub"}
                                    </span>
                                    <span class="absolute -bottom-1 left-0 w-0 h-[1px] bg-primary group-hover:w-full transition-all duration-300"></span>
                                </a>
                            </li>
                        </ul>
                    </nav>
                </div>

                {/* Social media icons - redesigned */}
                <div class="mb-12 flex justify-center">
                    <div class="inline-flex items-center p-1.5 rounded-full bg-background-light/10 backdrop-blur-sm">
                        <a
                            href="https://twitter.com"
                            class="group relative w-10 h-10 rounded-full flex items-center justify-center overflow-hidden"
                            aria-label="Follow us on Twitter"
                        >
                            {/* Hover background */}
                            <div class="absolute inset-0 opacity-0 group-hover:opacity-100 bg-gradient-to-br from-[#1DA1F2]/90 to-[#1DA1F2]/70 transition-opacity duration-300"></div>

                            {/* Icon */}
                            <svg class="w-5 h-5 text-foreground-muted group-hover:text-white relative z-10 transition-colors duration-300" fill="currentColor" viewBox="0 0 24 24">
                                <path d="M8.29 20.251c7.547 0 11.675-6.253 11.675-11.675 0-.178 0-.355-.012-.53A8.348 8.348 0 0022 5.92a8.19 8.19 0 01-2.357.646 4.118 4.118 0 001.804-2.27 8.224 8.224 0 01-2.605.996 4.107 4.107 0 00-6.993 3.743 11.65 11.65 0 01-8.457-4.287 4.106 4.106 0 001.27 5.477A4.072 4.072 0 012.8 9.713v.052a4.105 4.105 0 003.292 4.022 4.095 4.095 0 01-1.853.07 4.108 4.108 0 003.834 2.85A8.233 8.233 0 012 18.407a11.616 11.616 0 006.29 1.84" />
                            </svg>
                        </a>

                        <a
                            href="https://github.com/videocall-rs"
                            class="group relative w-10 h-10 rounded-full flex items-center justify-center overflow-hidden mx-2"
                            aria-label="Visit our GitHub"
                        >
                            {/* Hover background */}
                            <div class="absolute inset-0 opacity-0 group-hover:opacity-100 bg-gradient-to-br from-[#2B3137]/90 to-[#2B3137]/70 transition-opacity duration-300"></div>

                            {/* Icon */}
                            <svg class="w-5 h-5 text-foreground-muted group-hover:text-white relative z-10 transition-colors duration-300" fill="currentColor" viewBox="0 0 24 24">
                                <path fill-rule="evenodd" d="M12 2C6.477 2 2 6.484 2 12.017c0 4.425 2.865 8.18 6.839 9.504.5.092.682-.217.682-.483 0-.237-.008-.868-.013-1.703-2.782.605-3.369-1.343-3.369-1.343-.454-1.158-1.11-1.466-1.11-1.466-.908-.62.069-.608.069-.608 1.003.07 1.531 1.032 1.531 1.032.892 1.53 2.341 1.088 2.91.832.092-.647.35-1.088.636-1.338-2.22-.253-4.555-1.113-4.555-4.951 0-1.093.39-1.988 1.029-2.688-.103-.253-.446-1.272.098-2.65 0 0 .84-.27 2.75 1.026A9.564 9.564 0 0112 6.844c.85.004 1.705.115 2.504.337 1.909-1.296 2.747-1.027 2.747-1.027.546 1.379.202 2.398.1 2.651.64.7 1.028 1.595 1.028 2.688 0 3.848-2.339 4.695-4.566 4.943.359.309.678.92.678 1.855 0 1.338-.012 2.419-.012 2.747 0 .268.18.58.688.482A10.019 10.019 0 0022 12.017C22 6.484 17.522 2 12 2z" clip-rule="evenodd" />
                            </svg>
                        </a>

                        <a
                            href="https://discord.gg/XRdt6WfZyf"
                            class="group relative w-10 h-10 rounded-full flex items-center justify-center overflow-hidden"
                            aria-label="Join our Discord"
                        >
                            {/* Hover background */}
                            <div class="absolute inset-0 opacity-0 group-hover:opacity-100 bg-gradient-to-br from-[#5865F2]/90 to-[#5865F2]/70 transition-opacity duration-300"></div>

                            {/* Icon */}
                            <svg class="w-5 h-5 text-foreground-muted group-hover:text-white relative z-10 transition-colors duration-300" fill="currentColor" viewBox="0 0 24 24">
                                <path d="M20.317 4.3698a19.7913 19.7913 0 00-4.8851-1.5152.0741.0741 0 00-.0785.0371c-.211.3753-.4447.8648-.6083 1.2495-1.8447-.2762-3.68-.2762-5.4868 0-.1636-.3847-.4058-.8742-.6177-1.2495a.077.077 0 00-.0785-.037 19.7363 19.7363 0 00-4.8852 1.515.0699.0699 0 00-.0321.0277C.5334 9.0458-.319 13.5799.0992 18.0578a.0824.0824 0 00.0312.0561c2.0528 1.5076 4.0413 2.4228 5.9929 3.0294a.0777.0777 0 00.0842-.0276c.4616-.6304.8731-1.2952 1.226-1.9942a.076.076 0 00-.0416-.1057c-.6528-.2476-1.2743-.5495-1.8722-.8923a.077.077 0 01-.0076-.1277c.1258-.0943.2517-.1923.3718-.2914a.0743.0743 0 01.0776-.0105c3.9278 1.7933 8.18 1.7933 12.0614 0a.0739.0739 0 01.0785.0095c.1202.099.246.1981.3728.2924a.077.077 0 01-.0066.1276 12.2986 12.2986 0 01-1.873.8914.0766.0766 0 00-.0407.1067c.3604.698.7719 1.3628 1.225 1.9932a.076.076 0 00.0842.0286c1.961-.6067 3.9495-1.5219 6.0023-3.0294a.077.077 0 00.0313-.0552c.5004-5.177-.8382-9.6739-3.5485-13.6604a.061.061 0 00-.0312-.0286zM8.02 15.3312c-1.1825 0-2.1569-1.0857-2.1569-2.419 0-1.3332.9555-2.4189 2.157-2.4189 1.2108 0 2.1757 1.0952 2.1568 2.419 0 1.3332-.9555 2.4189-2.1569 2.4189zm7.9748 0c-1.1825 0-2.1569-1.0857-2.1569-2.419 0-1.3332.9554-2.4189 2.1569-2.4189 1.2108 0 2.1757 1.0952 2.1568 2.419 0 1.3332-.946 2.4189-2.1568 2.4189Z" />
                            </svg>
                        </a>
                    </div>
                </div>

                <div class="pt-8 flex flex-col md:flex-row justify-between items-center relative">
                    {/* Subtle divider */}
                    <div class="absolute top-0 left-0 right-0 h-[1px] bg-gradient-to-r from-transparent via-primary/10 to-transparent"></div>

                    <p class="text-foreground-subtle text-sm mb-4 md:mb-0">
                        {"Copyright 2024 videocall.rs. All rights reserved."}
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
