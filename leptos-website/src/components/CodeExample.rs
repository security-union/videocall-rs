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

use leptos::*;

#[component]
pub fn CodeExample(
    #[prop(into)] code: String,
    #[prop(into, default = "rust".to_string())] language: String,
) -> impl IntoView {
    view! {
        <div class="code-block relative group">
            <div class="absolute top-3 right-3 text-xs text-foreground-secondary opacity-50 uppercase tracking-wider">
                {language}
            </div>
            <pre class="code-block-inner overflow-x-auto p-4 text-sm leading-relaxed">
                <code class="language-rust" inner_html=code>
                </code>
            </pre>
        </div>
    }
}
