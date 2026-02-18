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

    let json_ld = r#"
    {
        "@context": "https://schema.org",
        "@type": "SoftwareApplication",
        "name": "videocall.rs",
        "operatingSystem": "Any",
        "applicationCategory": "DeveloperApplication",
        "offers": {
            "@type": "Offer",
            "price": "0",
            "priceCurrency": "USD"
        },
        "description": "Open source, ultra-low-latency video conferencing API and platform built with Rust. Supports WebTransport and WebSocket.",
        "aggregateRating": {
            "@type": "AggregateRating",
            "ratingValue": "5",
            "ratingCount": "1"
        }
    }
    "#;

    view! {
        <Html lang="en"/>
        <Stylesheet id="leptos" href="/pkg/leptos_website.css"/>
        <Title formatter/>
        <Meta
            name="description"
            content="Open source, ultra-low-latency video conferencing API and platform built with Rust. Perfect for software professionals, robotics, and embedded devices. Supports WebTransport with WebSocket fallback."
        />
        <Meta
            name="keywords"
            content="video conferencing api, rust video streaming, webtransport, websocket, low latency video, robotics video control, embedded video streaming, open source video platform, software professionals, video robotics"
        />
        
        // Open Graph / Facebook
        <Meta property="og:type" content="website"/>
        <Meta property="og:site_name" content="videocall.rs"/>
        <Meta property="og:url" content="https://videocall.rs/"/>
        <Meta property="og:title" content="videocall.rs - Ultra-low-latency Video Conferencing API"/>
        <Meta property="og:description" content="Open source, ultra-low-latency video conferencing API and platform built with Rust. Perfect for software professionals, robotics, and embedded devices."/>
        <Meta property="og:image" content="https://videocall.rs/images/og-image.png"/>

        // Twitter
        <Meta property="twitter:card" content="summary_large_image"/>
        <Meta property="twitter:site" content="@videocallrs"/>
        <Meta property="twitter:creator" content="@videocallrs"/>
        <Meta property="twitter:url" content="https://videocall.rs/"/>
        <Meta property="twitter:title" content="videocall.rs - Ultra-low-latency Video Conferencing API"/>
        <Meta property="twitter:description" content="Open source, ultra-low-latency video conferencing API and platform built with Rust. Perfect for software professionals, robotics, and embedded devices."/>
        <Meta property="twitter:image" content="https://videocall.rs/images/og-image.png"/>

        <Router>
            <Routes>
                <Route path="" view=Home ssr=SsrMode::Async/>
            </Routes>
        </Router>
        <script type="application/ld+json">
            {json_ld}
        </script>
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
