use crate::pages::Home::*;
use leptos::*;
use leptos_meta::*;
use leptos_router::*;

#[component]
pub fn App() -> impl IntoView {
    let formatter = |text| format!("{text} - VideoCall.rs");
    provide_meta_context();

    view! {
        <Html lang="en"/>
        <Stylesheet id="leptos" href="/pkg/leptos_website.css"/>
        <Title formatter/>
        <Meta
            name="description"
            content="Leptos is a cutting-edge Rust web framework designed for building fast, reliable, web applications."
        />
        <Router>
            <Routes>
                <Route path="" view=Home ssr=SsrMode::Async/>
            </Routes>
        </Router>
        <script defer data-domain="leptos.dev" src="https://plausible.io/js/script.js"></script>
    }
}
