use crate::components::icons::{
    digital_ocean::DigitalOceanIcon, discord::DiscordIcon, youtube::YoutubeIcon,
};
use yew::prelude::*;

#[function_component(TopBar)]
pub fn top_bar() -> Html {
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
            </div>
            <span>{ "Made with ❤️ by awesome developers from all over the world 🌏, hosted by Security Union 🛡️." }</span>
        </div>
    }
}
