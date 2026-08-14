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

use crate::error_template::ErrorTemplate;
use crate::pages::Home::Home;
use leptos::prelude::*;
use leptos_meta::{provide_meta_context, Link, Meta, MetaTags, Stylesheet, Title};
use leptos_router::{
    components::{Route, Router, Routes},
    SsrMode, StaticSegment,
};

/// The full HTML document shell. Leptos 0.7+ renders the entire document from
/// Rust: `HydrationScripts islands=true` wires up islands hydration and
/// `MetaTags` is where leptos_meta injects the `<Title>`/`<Meta>`/`<Stylesheet>`
/// declared inside `App` during SSR.
pub fn shell(options: LeptosOptions) -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <AutoReload options=options.clone() />
                <HydrationScripts options islands=true/>
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
    // Identity formatter: the page <Title> already carries the brand, and the
    // title tag must stay under ~60 characters.
    let formatter = |text: String| text;
    provide_meta_context();

    // Clean JSON-LD graph — no fabricated ratings. SoftwareApplication +
    // Organization + a FAQPage whose answers track the repository README.
    let json_ld = r#"{
  "@context": "https://schema.org",
  "@graph": [
    {
      "@type": "SoftwareApplication",
      "name": "videocall.rs",
      "applicationCategory": "DeveloperApplication",
      "operatingSystem": "Any",
      "url": "https://videocall.rs/",
      "description": "Open-source Rust video infrastructure. WebTransport over QUIC with a WebSocket fallback, forwarded by a relay server — no ICE, STUN, TURN, or SDP.",
      "codeRepository": "https://github.com/security-union/videocall-rs",
      "license": [
        "https://opensource.org/licenses/MIT",
        "https://www.apache.org/licenses/LICENSE-2.0"
      ],
      "offers": {
        "@type": "Offer",
        "price": "0",
        "priceCurrency": "USD"
      },
      "author": { "@id": "https://securityunion.dev/#organization" }
    },
    {
      "@type": "Organization",
      "@id": "https://securityunion.dev/#organization",
      "name": "Security Union LLC",
      "url": "https://securityunion.dev",
      "logo": "https://videocall.rs/images/videocall_logo.svg",
      "sameAs": [
        "https://github.com/security-union/videocall-rs",
        "https://discord.gg/JP38NRe4CJ",
        "https://www.youtube.com/@dario.lencina"
      ]
    },
    {
      "@type": "FAQPage",
      "mainEntity": [
        {
          "@type": "Question",
          "name": "Is videocall.rs a WebRTC replacement?",
          "acceptedAnswer": {
            "@type": "Answer",
            "text": "For most server-mediated video, yes. videocall.rs does not use WebRTC's peer-connection stack. Media flows over WebTransport (QUIC/HTTP3), or a WebSocket fallback, to a Rust relay server that forwards packets to other participants. It drops ICE, STUN/TURN, and SDP negotiation entirely."
          }
        },
        {
          "@type": "Question",
          "name": "Can I self-host it?",
          "acceptedAnswer": {
            "@type": "Answer",
            "text": "Yes. The repository ships Helm charts for Kubernetes and a fully native local dev stack. Production container images are built reproducibly with Nix."
          }
        },
        {
          "@type": "Question",
          "name": "Does it work in the browser?",
          "acceptedAnswer": {
            "@type": "Answer",
            "text": "Yes, on Chromium-based browsers (Chrome, Edge, Brave) and Safari on macOS and iOS. Firefox is not currently supported."
          }
        },
        {
          "@type": "Question",
          "name": "How is it different from LiveKit, Jitsi, or mediasoup?",
          "acceptedAnswer": {
            "@type": "Answer",
            "text": "Those stacks are WebRTC selective-forwarding units. videocall.rs forwards media over WebTransport/QUIC instead of WebRTC, is written end-to-end in Rust, and requires no STUN/TURN/ICE infrastructure."
          }
        },
        {
          "@type": "Question",
          "name": "Can I stream from a Raspberry Pi?",
          "acceptedAnswer": {
            "@type": "Answer",
            "text": "Yes. videocall-cli is a headless native client that streams from a camera on Raspberry Pi, Jetson Nano, and other embedded Linux devices."
          }
        },
        {
          "@type": "Question",
          "name": "Is the media encrypted?",
          "acceptedAnswer": {
            "@type": "Answer",
            "text": "Yes. Media is end-to-end encrypted between participants with a hybrid RSA/AES scheme. The relay server forwards ciphertext only and has no access to keys or plaintext."
          }
        }
      ]
    }
  ]
}"#;

    view! {
        <Stylesheet id="leptos" href="/pkg/leptos_website.css"/>
        <Title formatter/>
        <Meta
            name="description"
            content="Open-source Rust video infrastructure: WebTransport over QUIC with a WebSocket fallback, no ICE/STUN/TURN/SDP. Self-hostable, MIT/Apache-2.0."
        />
        <Meta
            name="keywords"
            content="webtransport video, rust video streaming, quic video, webrtc alternative, websocket fallback, self-hosted video conferencing, robotics video streaming, embedded video, open source video infrastructure"
        />
        <Link rel="canonical" href="https://videocall.rs/"/>
        // Machine-readable summary for LLM crawlers.
        <Link rel="alternate" type_="text/markdown" href="/llms.txt"/>

        // Open Graph / Facebook
        <Meta property="og:type" content="website"/>
        <Meta property="og:site_name" content="videocall.rs"/>
        <Meta property="og:url" content="https://videocall.rs/"/>
        <Meta property="og:title" content="videocall.rs — Rust WebTransport video infrastructure"/>
        <Meta property="og:description" content="Open-source Rust transport for live video. WebTransport over QUIC with a WebSocket fallback — no ICE, STUN, TURN, or SDP. Self-hostable, MIT/Apache-2.0."/>
        <Meta property="og:image" content="https://videocall.rs/images/og-image.png"/>

        // Twitter
        <Meta property="twitter:card" content="summary_large_image"/>
        <Meta property="twitter:site" content="@videocallrs"/>
        <Meta property="twitter:creator" content="@videocallrs"/>
        <Meta property="twitter:url" content="https://videocall.rs/"/>
        <Meta property="twitter:title" content="videocall.rs — Rust WebTransport video infrastructure"/>
        <Meta property="twitter:description" content="Open-source Rust transport for live video. WebTransport over QUIC with a WebSocket fallback — no ICE, STUN, TURN, or SDP. Self-hostable, MIT/Apache-2.0."/>
        <Meta property="twitter:image" content="https://videocall.rs/images/og-image.png"/>

        <Router>
            <Routes fallback=|| view! { <ErrorTemplate/> }>
                <Route path=StaticSegment("") view=Home ssr=SsrMode::Async/>
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
