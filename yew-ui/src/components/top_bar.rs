use yew::prelude::*;

#[function_component(TopBar)]
pub fn top_bar() -> Html {
    html! {
        <div class="top-bar">
            <a href="https://github.com/security-union/zoom-rs" target="_blank">
                <img src="https://img.shields.io/github/stars/security-union/zoom-rs?style=social" alt="GitHub stars" />
            </a>
            <span>{ "Made with ❤️ by awesome developers from all over the world 🌏, hosted by Security Union 🛡️." }</span>
        </div>
    }
}
