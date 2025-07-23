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

use crate::components::icons::{
    digital_ocean::DigitalOceanIcon, discord::DiscordIcon, youtube::YoutubeIcon,
};
use crate::context::UsernameCtx;
use web_sys::window;
use yew::prelude::*;

#[function_component(TopBar)]
pub fn top_bar() -> Html {
    let username_ctx = use_context::<UsernameCtx>();
    let change_username = if let Some(ctx) = &username_ctx {
        let ctx = ctx.clone();
        Some(Callback::from(move |_| {
            // Mark that we are resetting the username so that after the
            // user confirms the new name we can reload the page and flush
            // any lingering connections.
            if let Some(storage) = window().and_then(|w| w.local_storage().ok().flatten()) {
                let _ = storage.set_item("vc_username_reset", "1");
            }

            // Clear only the in-memory context so the UI re-renders to the
            // username prompt. We intentionally keep the cached value in
            // localStorage so it appears pre-filled for convenience.
            ctx.set(None);
        }))
    } else {
        None
    };

    html! {
        <div class="top-bar">
            <div class="flex space-x-2 align-middle">
            {
                if let Some(onclick) = change_username {
                    html! {
                        <button class="button change-username-btn text-sm px-3 py-1 border rounded" onclick={onclick} alt="Click to change username">
                            { username_ctx.as_ref().and_then(|ctx| ctx.as_ref().cloned()).unwrap_or_default() }
                        </button>
                    }
                } else { html!{} }
                }
            </div>
            // <span>{ "Made with ❤️ by awesome developers from all over the world 🌏, hosted by Security Union 🛡️." }</span>
        </div>
    }
}
