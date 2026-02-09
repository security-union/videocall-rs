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
seo_keywords = ["rust cross platform", "video call sdk architecture", "rust wasm native", "uniffi rust", "crux framework", "livekit rust sdk", "libwebrtc architecture", "videocall-client refactor", "rust ios android", "cross platform video calling"]

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

Also, videocall-client is tightly coupled with the web libraries, it would be great to be able to run it on iOS and Android without having to rewrite the entire library.

And it paid off, we have a working library that is used by many different projects, by many different developers.

The problem is that even when I love Yew, I am not fullfilling my own vision, I am not building a library that is easy to use for everyone, I am building a library that is easy to use for Yew users.

Now as we add more features, like meeting management, we are starting to see the limitations of the current architecture.

# The Challenges

Refactoring the codebase to remove the Yew specific code is not going to be easy, but it is necessary.

Right now, `videocall-client` is WASM-only. Every encoder, decoder, and connection type talks directly to `web-sys` browser APIs. The `MicrophoneEncoderTrait` has `set_error_callback(&mut self, on_error: yew::Callback<String>)` baked right into the trait definition. `VideoCallClientOptions` uses `yew::Callback` for `on_peer_added`, `on_peer_removed`, and every other event. That means if you want to use our library from Leptos, Dioxus, plain JavaScript, or a native iOS app, you are out of luck.

Before I go rewrite everything, I want to understand how others have solved this problem. Here are the projects I am studying.

# How Others Solved Cross-Platform

## 1. libWebRTC — The Layered Architecture

[libWebRTC](https://webrtc.github.io/webrtc-org/architecture/) is the C++ engine behind every browser's WebRTC implementation. Its architecture has two layers:

- **Native C++ API**: The core media engine (VoiceEngine, VideoEngine, Transport) written in portable C++ with no UI framework dependencies whatsoever.
- **Platform bindings**: Objective-C wrappers for iOS/macOS, Java/JNI wrappers for Android, and the browser's JavaScript API on the web.

The key insight is that **the core media logic knows nothing about the platform it runs on**. Encoding, decoding, jitter buffering, echo cancellation — all of it lives in pure C++ behind stable interfaces. The platform shells are thin adapters that bridge to native media APIs (CoreAudio, AudioTrack, etc.).

This is the gold standard, but it is also a massive C++ codebase maintained by Google. I don't have that budget. What I can take from it: **separate the protocol and media logic from the platform I/O**.

## 2. LiveKit Rust SDK — Crate-Level Separation

[LiveKit's Rust SDK](https://github.com/livekit/rust-sdks) is the closest analog to what I am building. They ship a family of crates:

- `livekit` — the high-level client SDK (Room, Participant, Track abstractions)
- `livekit-webrtc` — a Rust abstraction over libwebrtc with platform-specific backends
- `livekit-ffi` — generates foreign-language bindings for Swift, Kotlin, Python, Unity, etc.
- `livekit-protocol` — protobuf types shared across all platforms

Their media abstractions (`audio_source`, `video_source`, `audio_stream`, `video_stream`) are defined as **platform-agnostic traits** in the core crate. The actual WebRTC implementation uses libwebrtc under the hood, but the SDK consumer never touches it directly.

What I can take from this: **define media traits in a core crate, push platform-specific implementations into separate crates or feature-gated modules**.

## 3. Crux — Core/Shell Architecture in Rust

[Crux](https://redbadger.github.io/crux/) is a Rust framework that takes the most radical approach to the problem. It splits every app into:

- **Core**: Pure Rust, side-effect free, compiled to all targets including WASM. Contains all business logic and state management. Uses event sourcing — the core receives events and produces effects (requests to the outside world), but never executes I/O itself.
- **Shell**: Platform-native (SwiftUI, Jetpack Compose, React, Yew). The shell renders UI, handles I/O, and feeds results back to the core.

The core and shell communicate through message passing with cross-language type checking via FFI. Crux compiles the same Rust core as a `staticlib` for iOS (linked via Xcode), a `cdylib` for Android (loaded via JNA), and `wasm32-unknown-unknown` for the web.

The side-effect-free core is a powerful idea: because it never touches the network or the camera directly, it is trivially testable and completely portable. The shell is intentionally thin.

What I can take from this: **videocall-client's state machine (connection lifecycle, peer tracking, meeting management) should be a pure-logic core that requests effects rather than executing them directly**.

## 4. Mozilla UniFFI — The Binding Generator

[UniFFI](https://mozilla.github.io/uniffi-rs/) is what Mozilla uses internally for Firefox mobile. You write your library in Rust, describe the interface with proc-macros or a WebIDL-based definition file, and UniFFI generates idiomatic bindings for Kotlin (Android), Swift (iOS), Python, and more. Third-party support exists for C# and JavaScript.

UniFFI does not solve the architecture problem — it solves the distribution problem. Once you have a clean, platform-agnostic Rust core, UniFFI lets you ship it to every platform without hand-writing FFI glue.

What I can take from this: **once the core is decoupled from web-sys and Yew, UniFFI can generate the Swift and Kotlin bindings I need for iOS and Android**.

## 5. Daily.co — The Primitives Approach

[Daily](https://docs.daily.co/guides/architecture-and-monitoring/intro-to-video-arch) is a commercial video calling platform. Their cross-platform SDK design centers on a small set of universal primitives: **Rooms**, **Participants**, **Tracks**, and **Tokens**. Every platform SDK (web, iOS, Android, React Native) exposes the same primitives with the same semantics.

The publish/subscribe model is framework-agnostic by nature: participants publish tracks, other participants subscribe to them. The SDK doesn't care whether the subscriber renders the video in a SwiftUI `View`, a Jetpack Compose composable, or a `<video>` element.

What I can take from this: **design the public API around media primitives (tracks, participants, rooms) rather than framework-specific callbacks**.

# Where videocall-client Stands Today

Looking at the codebase honestly, here is where the coupling lives:

1. **`yew::Callback<T>` everywhere** — `VideoCallClientOptions`, `MicrophoneEncoderTrait`, encoder state callbacks, device selection callbacks. This is the most pervasive coupling.
2. **`web-sys` as the only I/O backend** — `HtmlVideoElement`, `HtmlCanvasElement`, `MediaStreamTrack`, WebCodecs API. Every encoder and decoder talks directly to browser APIs.
3. **`yew-websocket` and `yew-webtransport`** — the connection layer is Yew-specific rather than using a generic async transport trait.

The good news is that the existing trait abstractions (`MediaDecoderTrait`, `MicrophoneEncoderTrait`, `AudioPeerDecoderTrait`) show the *intent* to abstract. They just leak platform types through their signatures.

# What I Am Thinking

I don't have a concrete plan yet — I am still studying these projects. But the direction that keeps emerging from every example above is the same:

1. **Extract a pure-logic core** (protocol, state machine, peer management) with no I/O dependencies.
2. **Define platform traits** for the I/O boundaries (transport, media capture, media rendering).
3. **Implement platform backends** behind those traits (web-sys for WASM, CoreAudio/VideoToolbox for iOS, etc.).
4. **Use callbacks or channels, not framework types** for the event interface — `Box<dyn Fn(Event)>` or `tokio::sync::mpsc` instead of `yew::Callback`.
5. **Ship bindings** via UniFFI for mobile, wasm-bindgen for the web, and a C API for everything else.

This is a big refactor, and I want to get it right. I will be writing more as I dig deeper into each of these projects and start prototyping the new architecture.

If you have experience with any of these approaches, I would love to hear from you. Open an issue on [videocall.rs](https://github.com/aspect-build/aspect-workflows) or find me on the project's Discord.

