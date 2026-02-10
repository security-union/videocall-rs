+++
title = "Getting the Abstractions Right in videocall.rs"
date = 2025-07-05
# Set to `true` while drafting; switch to `false` once published
draft = true
slug = "getting-the-abstractions-right"
description = "How videocall-client can evolve from a Yew-only WASM library into a cross-platform SDK that works on web, iOS, Android, and desktop."
tags = ["rust", "architecture", "videocall-client", "webtransport", "open-source", "engineering", "cross-platform"]
authors = ["Dario Lencina Talarico"]

[extra]
seo_keywords = ["rust cross platform", "video call sdk architecture", "rust wasm native", "uniffi rust", "livekit rust sdk", "libwebrtc architecture", "pion webrtc go", "jitsi architecture", "agora sdk", "dyte sdk", "videocall-client refactor", "cross platform video calling"]

[taxonomies]
tags = ["rust", "architecture", "cross-platform", "software engineering", "video calling", "sdk design"]
authors = ["Dario Lencina Talarico"]
+++

<p style="text-align:center; margin-top:1em; margin-bottom:1em;">
    <img src="/images/Bleeding_Gums_Murphy.webp" alt="2 AM coding session" style="max-width:600px; width:100%; height:auto; border-radius:4px;" />
</p>

Since I started coding, I always loathed the architecture astronauts, the ones who spend all their time building castles in the air, and never actually ship anything.

In particular, it annoyed me that the abstractions that they came up with were always wrong, because they were too detached from the code, always outdated, it seemed like they were paid by the complexity of the solution vs building the actual product.

Fast forward 15 years, and now I am responsible for building good abstractions, it is not about building a particular consumer product, I am building a library that will be used by many different projects, by many different developers.

# The Problems

When I started videocall.rs I knew exactly how I wanted to use it for streaming a robot camera:

```bash
cargo install videocall-cli
videocall stream --server wss://myserver.com --channel robot-arm-1
```

Done. Simple. 

Maybe the CLI will negotiate a JWT with the server, but that would be done behind the scenes, no friction for the user.

And for the web app, I wanted to use it like this for a telehealth app:
```rust
let client = VideoCallClient::new(VideoCallClientOptions {
    server_url: "wss://hipaa-compliant.example.com".into(),
    channel_id: token.channel_id,
    auth_token: token.jwt,
    on_peer_added: Rc::new(|peer_id| render_remote_video(peer_id)),
    on_peer_removed: Rc::new(|peer_id| remove_remote_video(peer_id)),
    ..Default::default()
});
client.connect()?;
camera.start();
```

I wanted the user to use any framework they wanted, at least javascript/typescript and rust.

But then I started to commit a bunch of sins in the name of progress, like embedding a bunch of yew specific code in the library, like the `yew::Callback<T>` type, I would rather using a streaming architecture that is agnostic to the framework.

<aside>
Callbacks are terrible anyways.

Also, videocall-client is tightly coupled with the web libraries, it would be great to be able to run it on iOS and Android without having to rewrite the entire library.
</aside>

And it paid off, we have a working library that is used by many different projects, by many different developers.

The problem is that even when I love Yew, I am not fullfilling my own vision, I am not building a library that is easy to use for everyone, I am building a library that is easy to use for Yew users, and even when I really like Yew, the reality is that there are many other great UI frameworks out there, and I don't want them to be locked out of using our library.

Now as we add more features, like meeting management, we are starting to see the limitations of the current architecture.

# The Challenges

Refactoring the codebase to remove the Yew specific code is not going to be easy, but it is necessary.

Right now, `videocall-client` is WASM-only. Every encoder, decoder, and connection type talks directly to `web-sys` browser APIs. The `MicrophoneEncoderTrait` has `set_error_callback(&mut self, on_error: yew::Callback<String>)` baked right into the trait definition. `VideoCallClientOptions` uses `yew::Callback` for `on_peer_added`, `on_peer_removed`, and every other event. That means if you want to use our library from Leptos, Dioxus, plain JavaScript, or a native iOS app, you are out of luck.

Before we go rewrite everything, I want to understand how others have solved this problem. Here are the projects I am studying.

# How Others Solved Cross-Platform

## 1. libWebRTC — The Layered Architecture

[libWebRTC](https://webrtc.github.io/webrtc-org/architecture/) is the C++ engine behind every browser's WebRTC implementation. Its architecture has two layers:

- **Native C++ API**: The core media engine (VoiceEngine, VideoEngine, Transport) written in portable C++ with no UI framework dependencies whatsoever.
- **Platform bindings**: Objective-C wrappers for iOS/macOS, Java/JNI wrappers for Android, and the browser's JavaScript API on the web.

The key insight is that **the core media logic knows nothing about the platform it runs on**. Encoding, decoding, jitter buffering, echo cancellation — all of it lives in pure C++ behind stable interfaces. The platform shells are thin adapters that bridge to native media APIs (CoreAudio, AudioTrack, etc.).

This is the gold standard, but it is also a massive C++ codebase maintained by Google. I don't have that budget. What I can take from it: **separate the protocol and media logic from the platform I/O**.

## 2. Pion — Pure Go WebRTC

[Pion](https://github.com/pion/webrtc) is a pure Go implementation of the WebRTC API. No C dependencies, no libwebrtc, no cgo. It compiles to a single binary on Windows, Mac, Linux, and even WASM.

Its architecture is modular at the package level:
- `pion/ice` — ICE agent (RFC 8445)
- `pion/dtls` — DTLS encryption for data traffic
- `pion/srtp` — SRTP encryption for media traffic
- `pion/sctp` — data channels
- `pion/webrtc` — the top-level PeerConnection API that ties everything together

The `MediaEngine` component handles codec negotiation (Opus, VP8, VP9, H.264, G722, etc.) and the whole stack provides direct RTP/RTCP access for server-side media processing.

Why Pion matters here: it proved that you can rewrite WebRTC from scratch in a memory-safe language without depending on Google's C++ codebase, and that the resulting library can be the foundation for production systems. Which brings us to LiveKit.

## 3. LiveKit — Pion + Rust, Full Stack

[LiveKit](https://github.com/livekit/livekit) is an open-source video platform whose **server is a Go SFU built on Pion**. Their server handles the hard parts — simulcast, SVC codecs (AV1/VP9), speaker detection, selective subscription — all powered by Pion's pure Go WebRTC stack.

But their **client SDKs** are where it gets interesting for us. LiveKit's [Rust SDK](https://github.com/livekit/rust-sdks) is designed with two explicit goals from their own README:

1. Build a standalone, cross-platform LiveKit client SDK for Rustaceans.
2. Build a **common core for other platform-specific SDKs** (Unity, Unreal, iOS, Android).

They ship a family of crates:

- `livekit` — the high-level client SDK (Room, Participant, Track abstractions)
- `libwebrtc` — Rust bindings to Google's libwebrtc with platform-specific backends
- `livekit-ffi` (v0.12.46) — the mature FFI layer that powers their Swift, Kotlin, Python, Unity, and Node.js SDKs. Uses protobuf for the interface, compiles as `staticlib` + `cdylib`.
- `livekit-uniffi` (v0.1.0, experimental, unpublished) — a newer, parallel effort using [Mozilla's UniFFI](https://mozilla.github.io/uniffi-rs/) to generate idiomatic Swift/Kotlin bindings automatically. Similar to what we are experimenting with in videocall.rs.
- `livekit-protocol` — protobuf types shared across all platforms

Their media abstractions (`audio_source`, `video_source`, `audio_stream`, `video_stream`) are defined as platform-agnostic types in the core crate. The WebRTC implementation uses libwebrtc under the hood, but the SDK consumer never touches it directly. They support hardware encoding/decoding via VideoToolbox on macOS/iOS, NVIDIA/AMD GPUs on Linux, and Jetson boards.

The key challenge they call out in their README is exactly the one we face: *"There's a significant amount of business/control logic in our signaling protocol and WebRTC. Currently, this logic needs to be implemented in every new platform we support."* Their Rust SDK is their answer to that duplication.

What I can take from this: **define media traits in a core crate, push platform-specific implementations into separate crates. Use FFI (protobuf-based or UniFFI) to generate bindings for mobile and other languages rather than hand-writing them per platform.**

## 4. Jitsi Meet — JavaScript Core, Native Wrappers

[Jitsi Meet](https://jitsi.github.io/handbook/docs/architecture) takes a different approach. The core is a JavaScript/React application:

- **Jitsi Videobridge (JVB)** — a Java-based WebRTC SFU that routes video streams
- **lib-jitsi-meet** — the low-level JavaScript library that handles all WebRTC logic
- **Jitsi Meet web app** — React frontend built on lib-jitsi-meet

For cross-platform, they wrap the web application in native shells:
- **Android SDK** and **iOS SDK** — native wrappers around the JavaScript core
- **React Native SDK** — shared JavaScript logic with native bridges
- **Flutter SDK** — Dart wrappers
- **Electron SDK** — desktop wrapper

The shared component model uses React/React Native feature folders where code is organized by feature (chat, video, participants) with shared logic across Android, iOS, and web.

This is the "JavaScript everywhere" approach. It works, Jitsi is widely deployed, but it means the media logic lives in JavaScript and you inherit browser limitations on every platform. For us in Rust, this is the opposite direction — we want to push *more* logic into the compiled core, not less.

What I can take from this: **Jitsi shows that even a JavaScript-first project eventually needs native SDKs for every platform. Starting from Rust gives us a better foundation for that journey.**

## 5. Agora — Proprietary Native Engine

[Agora](https://docs.agora.io/en/video-calling/overview/core-concepts) is the largest proprietary real-time video platform. Their architecture centers on a closed-source native RTC engine that compiles to every platform: Android, iOS, macOS, Windows, Electron, Unity, Flutter, React Native, and web.

The engine provides a single abstraction layer that standardizes audio formats (Opus, G722, AAC) and video formats (H.264, JPEG) across platforms. Behind it sits their Software-Defined Real-Time Network (SD-RTN) — a global private network with data centers in 200+ countries.

For developers, Agora ships per-platform SDKs that wrap the same native engine. The API surface is consistent: initialize engine, join channel, publish/subscribe to tracks.

What I can take from this: **Agora proves the commercial viability of the "one native core, many platform wrappers" model. That is the right architecture — we just need to do it with an open-source Rust core instead of a proprietary C++ one.**

## 6. Dyte — Kotlin Multiplatform for Mobile

[Dyte](https://dyte.io/blog/dyte-web-core/) is a newer entrant that made an interesting technical choice for mobile. Their web SDK ("Web-Core") is a ~11,000 line JavaScript data layer that abstracts away WebRTC complexity. But for mobile, they initially used React Native and then **migrated to Kotlin Multiplatform** to share business logic (networking, state management) across Android and iOS while keeping platform-native code where needed.

Their architecture explicitly separates UI kits from core SDKs — you can use their pre-built UI components or build your own on top of the core.

What I can take from this: **even companies that start with web-first eventually realize they need shared compiled logic for mobile. Dyte chose Kotlin Multiplatform for that shared layer; we can use Rust, which gives us web (via WASM) for free in addition to native.**

# Where videocall-client Stands Today

Looking at the codebase honestly, here is where the coupling lives:

1. **`yew::Callback<T>` everywhere** — `VideoCallClientOptions`, `MicrophoneEncoderTrait`, encoder state callbacks, device selection callbacks. This is the most pervasive coupling.
2. **`web-sys` as the only I/O backend** — `HtmlVideoElement`, `HtmlCanvasElement`, `MediaStreamTrack`, WebCodecs API. Every encoder and decoder talks directly to browser APIs.
3. **`yew-websocket` and `yew-webtransport`** — the connection layer is Yew-specific rather than using a generic async transport trait.

The good news is that the existing trait abstractions (`MediaDecoderTrait`, `MicrophoneEncoderTrait`, `AudioPeerDecoderTrait`) show the *intent* to abstract. They just leak platform types through their signatures.

# What I Am Thinking

I don't have a concrete plan yet — I am still studying these projects. But a pattern keeps emerging from every single one of them, whether they are written in C++ (libWebRTC), Go (Pion/LiveKit), Java (Jitsi), or proprietary C (Agora):

**Separate the protocol and media logic from platform I/O, then wrap the core for each platform.**

Every successful video platform arrives at this architecture eventually. The question is just which language and tooling you use for the core:

| Project | Server | Client SDKs | Mobile Strategy | Web Strategy |
|---------|--------|-------------|-----------------|--------------|
| libWebRTC | N/A (engine) | C++ core | Obj-C / JNI wrappers | Browser-native |
| Pion | Go | Go (server-side focus) | N/A | WASM (experimental) |
| LiveKit | Go (Pion) | Swift, Kotlin, JS, Flutter, etc. + Rust SDK as emerging unified core | Per-platform native SDKs + Rust FFI bridge | JS SDK (separate from Rust) |
| Jitsi | Java (JVB) | JavaScript / React | Native wrappers around JS core | JS-native |
| Agora | Proprietary | C++ native engine | Native engine per platform | Web SDK |
| Dyte | Proprietary | JS (web) / Kotlin (mobile) | Kotlin Multiplatform | JS-native |
| **videocall.rs** | **Rust** | **Rust (WASM-only today)** | **??? (this article)** | **WASM (current)** |

We are already in Rust, which gives us WASM for free and native compilation for everything else. LiveKit's *goal* with their Rust SDK is the closest to what we want to become — a single compiled core that powers all platform SDKs via FFI. But today, most of their client SDKs (Swift, Kotlin, JS, Flutter) are still independently implemented per platform, with the Rust SDK as an emerging unified foundation rather than the reality powering everything yet. They are also experimenting with UniFFI alongside their more mature protobuf-based FFI layer. The key difference from us is that they wrap libwebrtc, while we have our own protocol — which means we have fewer external dependencies to manage but more protocol logic to keep portable.

This is a big refactor, and I want to get it right. I will be writing more as I dig deeper into each of these projects and start prototyping the new architecture.

If you have experience with any of these approaches, I would love to hear from you. Open an issue on [videocall.rs](https://github.com/aspect-build/aspect-workflows) or find me on the project's Discord.

