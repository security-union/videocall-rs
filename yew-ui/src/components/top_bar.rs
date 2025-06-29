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
use crate::context::{clear_username_from_storage, UsernameCtx};
use gloo_utils::window;
use yew::prelude::*;

#[function_component(TopBar)]
pub fn top_bar() -> Html {
    let username_ctx = use_context::<UsernameCtx>();
    let change_username = if let Some(ctx) = &username_ctx {
        let ctx = ctx.clone();
        Some(Callback::from(move |_| {
            ctx.set(None);
            clear_username_from_storage();
            let _ = window().location().reload();
        }))
    } else {
        None
    };

    html! {
        <div class="top-bar">
            <div class="flex space-x-2 align-middle">
                <a href="https://github.com/security-union/videocall-rs" class="m-auto" target="_blank">
                    <img src="https://img.shields.io/github/stars/security-union/videocall-rs?style=social" class="w-16" alt="GitHub stars" />
                </a>
                <a href="https://www.youtube.com/@SecurityUnion" class="m-auto" target="_blank">
                    <div class="w-8">
                        <YoutubeIcon />
                    </div>
                </a>
                <a href="https://discord.gg/JP38NRe4CJ" class="m-auto" target="_blank">
                    <div class="w-8">
                        <DiscordIcon />
                    </div>
                </a>
                <a href="https://m.do.co/c/6de4e19c5193" class="m-auto" target="_blank">
                    <div class="w-16">
                        <DigitalOceanIcon />
                    </div>
                </a>
                {
                    if let Some(onclick) = change_username {
                        html! {
                            <button class="change-username-btn text-sm px-3 py-1 border rounded" onclick={onclick}>{"Change name"}</button>
                        }
                    } else { html!{} }
                }
            </div>
            <span>{ "Made with ❤️ by awesome developers from all over the world 🌏, hosted by Security Union 🛡️." }</span>
        </div>
    }
}
