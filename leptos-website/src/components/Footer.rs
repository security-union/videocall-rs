/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use crate::icons::DigitalOceanIcon;
use leptos::*;

#[component]
pub fn Footer() -> impl IntoView {
    view! {
        <footer class="py-12 px-6 border-t border-white/[0.06]">
            <div class="max-w-6xl mx-auto">
                <div class="flex flex-col md:flex-row justify-between items-center gap-8 mb-10">
                    <img class="h-8 w-auto opacity-70" src="/images/videocall_logo.svg" alt="videocall.rs" />

                    <nav>
                        <ul class="flex flex-wrap justify-center gap-6 text-[13px] text-white/40">
                            <li><a href="#supported-platforms" class="hover:text-white/80 transition-colors">{"Platforms"}</a></li>
                            <li><a href="#developers" class="hover:text-white/80 transition-colors">{"Developers"}</a></li>
                            <li><a href="#company" class="hover:text-white/80 transition-colors">{"Company"}</a></li>
                            <li><a href="#pricing" class="hover:text-white/80 transition-colors">{"Pricing"}</a></li>
                            <li>
                                <a href="https://github.com/security-union/videocall-rs" class="hover:text-white/80 transition-colors flex items-center gap-1">
                                    <svg class="w-3.5 h-3.5" fill="currentColor" viewBox="0 0 24 24">
                                        <path fill-rule="evenodd" d="M12 2C6.477 2 2 6.484 2 12.017c0 4.425 2.865 8.18 6.839 9.504.5.092.682-.217.682-.483 0-.237-.008-.868-.013-1.703-2.782.605-3.369-1.343-3.369-1.343-.454-1.158-1.11-1.466-1.11-1.466-.908-.62.069-.608.069-.608 1.003.07 1.531 1.032 1.531 1.032.892 1.53 2.341 1.088 2.91.832.092-.647.35-1.088.636-1.338-2.22-.253-4.555-1.113-4.555-4.951 0-1.093.39-1.988 1.029-2.688-.103-.253-.446-1.272.098-2.65 0 0 .84-.27 2.75 1.026A9.564 9.564 0 0112 6.844c.85.004 1.705.115 2.504.337 1.909-1.296 2.747-1.027 2.747-1.027.546 1.379.202 2.398.1 2.651.64.7 1.028 1.595 1.028 2.688 0 3.848-2.339 4.695-4.566 4.943.359.309.678.92.678 1.855 0 1.338-.012 2.419-.012 2.747 0 .268.18.58.688.482A10.019 10.019 0 0022 12.017C22 6.484 17.522 2 12 2z" clip-rule="evenodd" />
                                    </svg>
                                    {"GitHub"}
                                </a>
                            </li>
                        </ul>
                    </nav>
                </div>

                <div class="flex justify-center mb-8">
                    <a href="https://m.do.co/c/6de4e19c5193" class="opacity-30 hover:opacity-50 transition-opacity" aria-label="Powered by DigitalOcean">
                        <div class="h-5 w-24">
                            <DigitalOceanIcon />
                        </div>
                    </a>
                </div>

                <div class="flex flex-col md:flex-row justify-between items-center gap-4 pt-6 border-t border-white/[0.04]">
                    <p class="text-[12px] text-white/20">{"Copyright 2024 videocall.rs. All rights reserved."}</p>
                    <div class="flex gap-6 text-[12px] text-white/20">
                        <a href="https://github.com/security-union/videocall-rs/blob/main/LICENSE-MIT" class="hover:text-white/40 transition-colors">{"Privacy Policy"}</a>
                        <a href="https://github.com/security-union/videocall-rs/blob/main/LICENSE-MIT" class="hover:text-white/40 transition-colors">{"Terms of Service"}</a>
                    </div>
                </div>
            </div>
        </footer>
    }
}
