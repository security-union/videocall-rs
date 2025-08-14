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
use yew::prelude::*;

#[function_component(TopBar)]
pub fn top_bar() -> Html {
    html! {
        <div class="top-bar" style="position:fixed; top:0; left:0; right:0; display:flex; align-items:center; justify-content:space-between; padding:6px 10px; background:rgba(28,28,30,0.6); backdrop-filter:saturate(180%) blur(10px); border-bottom:1px solid #38383A; z-index:100;">
            <div class="flex items-center align-middle" style="opacity:0.9; gap:10px;">
                { html!{} }
            </div>
        </div>
    }
}
