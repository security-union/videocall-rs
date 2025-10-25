// SPDX-License-Identifier: MIT OR Apache-2.0

use leptos::prelude::*;

#[component]
pub fn ConfigError(message: String) -> impl IntoView {
    view! {
        <div class="error-container">
            <p class="error-message">{message}</p>
            <img src="/assets/instructions.gif" alt="Permission instructions" class="instructions-gif" />
        </div>
    }
}
