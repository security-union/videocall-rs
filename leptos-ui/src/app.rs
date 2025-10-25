use leptos::prelude::*;
use leptos_meta::{provide_meta_context, MetaTags, Stylesheet, Title};
use leptos_router::{components::*, *};

pub fn shell(options: LeptosOptions) -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <AutoReload options=options.clone() />
                <HydrationScripts options/>
                <MetaTags/>
            </head>
            <body>
                <App/>
            </body>
        </html>
    }
}

#[component]
pub fn App() -> impl IntoView {
    // Provides context that manages stylesheets, titles, meta tags, etc.
    provide_meta_context();

    view! {
        // injects a stylesheet into the document <head>
        // id=leptos means cargo-leptos will hot-reload this stylesheet
        <Stylesheet id="leptos" href="/pkg/leptos-ui.css"/>
        <Stylesheet id="yew-compat-1" href="/static/style.css"/>
        <Stylesheet id="yew-compat-2" href="/static/global.css"/>

        // sets the document title
        <Title text="videocall.rs"/>

        // content for this welcome page
        <Router>
            <main>
                <Routes fallback=|| "Page not found.".into_view()>
                    <Route path=StaticSegment("") view=crate::pages::home::HomePage/>
                    <Route path=StaticSegment("login") view=crate::pages::home::LoginPage/>
                    <Route path=("meeting", ParamSegment("id")) view=crate::pages::meeting::MeetingRoute/>
                    <Route path=("meeting", ParamSegment("id"), ParamSegment("webtransport_enabled")) view=crate::pages::meeting::MeetingRoute/>
                </Routes>
            </main>
        </Router>
    }
}

