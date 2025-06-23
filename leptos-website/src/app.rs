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


use crate::pages::Home::*;
use leptos::*;
use leptos_meta::*;
use leptos_router::*;

#[component]
pub fn App() -> impl IntoView {
    let formatter = |text| format!("{text} - videocall.rs");
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
        <script>
            "var _paq = window._paq = window._paq || [];
            _paq.push([\"setDocumentTitle\", document.domain + \"/\" + document.title]);
            _paq.push([\"setCookieDomain\", \"*.videocall.rs\"]);
            _paq.push(['trackPageView']);
            _paq.push(['enableLinkTracking']);
            (function() {
                var u=\"//matomo.videocall.rs/\";
                _paq.push(['setTrackerUrl', u+'matomo.php']);
                _paq.push(['setSiteId', '1']);
                var d=document, g=d.createElement('script'), s=d.getElementsByTagName('script')[0];
                g.async=true; g.src=u+'matomo.js'; s.parentNode.insertBefore(g,s);
            })();"
        </script>
    }
}
