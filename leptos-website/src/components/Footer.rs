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
use leptos::prelude::*;

#[component]
pub fn Footer() -> impl IntoView {
    view! {
        <footer class="border-t border-line px-6 md:px-10 py-12">
            <div class="max-w-content mx-auto">
                <div class="flex flex-col md:flex-row justify-between items-start gap-8 mb-10">
                    <div>
                        <img class="h-8 w-auto opacity-70" src="/images/videocall_logo.svg" alt="videocall.rs" />
                        <p class="eyebrow mt-4">"Real-time audio + video infrastructure · MIT / Apache-2.0"</p>
                    </div>

                    <nav aria-label="Footer">
                        <ul class="flex flex-wrap gap-6">
                            <li><a href="#supported-platforms" class="eyebrow hover:text-fg transition-colors">{"Platforms"}</a></li>
                            <li><a href="#developers" class="eyebrow hover:text-fg transition-colors">{"Developers"}</a></li>
                            <li><a href="#company" class="eyebrow hover:text-fg transition-colors">{"Company"}</a></li>
                            <li><a href="#pricing" class="eyebrow hover:text-fg transition-colors">{"Pricing"}</a></li>
                            <li><a href="https://github.com/security-union/videocall-rs" class="eyebrow hover:text-fg transition-colors">{"GitHub"}</a></li>
                        </ul>
                    </nav>
                </div>

                <div class="flex justify-center mb-8">
                    <a href="https://m.do.co/c/6de4e19c5193" class="opacity-30 hover:opacity-50 transition-opacity grayscale" aria-label="Powered by DigitalOcean">
                        <div class="h-5 w-24">
                            <DigitalOceanIcon />
                        </div>
                    </a>
                </div>

                <div class="flex flex-col md:flex-row justify-between items-center gap-4 pt-6 border-t border-line">
                    <p class="text-xs text-fg-3">{"© 2026 videocall.rs · Security Union LLC"}</p>
                    <div class="flex gap-6 text-xs text-fg-3">
                        <a href="https://github.com/security-union/videocall-rs/blob/main/LICENSE-MIT" class="hover:text-fg transition-colors">{"MIT License"}</a>
                        <a href="https://github.com/security-union/videocall-rs/blob/main/LICENSE-APACHE" class="hover:text-fg transition-colors">{"Apache-2.0 License"}</a>
                    </div>
                </div>
            </div>
        </footer>
    }
}
