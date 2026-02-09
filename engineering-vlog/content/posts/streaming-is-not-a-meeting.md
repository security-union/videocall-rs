+++
title = "Streaming is Not a Meeting"
date = 2026-02-09
description = "videocall-client tries to be a transport library and a meeting client at the same time. We're fixing that. Here's the plan."
[taxonomies]
tags = ["rust", "architecture", "videocall-client", "webtransport", "open-source", "engineering"]
authors = ["Dario Lencina Talarico"]
+++

# Streaming is Not a Meeting

This is what streaming a robot camera should look like:

```bash
cargo install videocall-cli
videocall stream --server wss://myserver.com --channel robot-arm-1
```

No SDK. No framework. No meeting to create. Just an address and a stream.

And this is what a telehealth app should look like -- where you *do* need sessions, permissions, and access control:

```rust
// Your backend creates the session and issues a token (Layer 1)
let token = meeting_api.create_session("dr-jones-patient-4821", grants)?;

// Your frontend connects with that token (Layer 0)
let client = VideoCallClient::new(VideoCallClientOptions {
    channel_id: token.channel_id,
    server_url: "wss://hipaa-compliant.example.com".into(),
    auth_token: token.jwt,
    on_peer_added: Rc::new(|peer_id| render_remote_video(peer_id)),
    on_peer_removed: Rc::new(|peer_id| remove_remote_video(peer_id)),
    ..Default::default()
});
client.connect()?;
camera.start();
```

Same transport library in both cases. The robot doesn't need a session. The doctor does. The library doesn't care -- it handles transport, encryption, and codecs. Your application decides how much structure to put on top.

We're not there yet. This article is about how we get there.

## The Principle

When you stream video, three things happen. First, packets flow from a producer to consumers through a named channel -- that's **routing**. Second, the server tracks who's connected so it knows where to forward media -- that's **participant awareness**. Third, someone decides who owns the room, who has permission to join, and what happens when the host leaves -- that's **meeting management**.

The first two are transport. They exist whether you're a robot streaming LIDAR data or a team on a Monday standup. The third is business logic. A roboticist never needs it.

[videocall-rs](https://github.com/security-union/videocall-rs) currently treats all three as one thing. `videocall-client`'s `lib.rs` opens with *"This crate intends to make no assumptions about the UI."* Then 11 source files import `yew::Callback<T>`. The `VideoCallClientOptions` struct includes `on_meeting_ended` -- business logic in the transport layer. On the backend, `actix-api` routes media AND decides that when a host disconnects, the session dies for everyone. Routing and business logic share a codebase, a dependency graph, and an API surface.

The stated contract and the code disagree. That's what we're fixing.

## The Evidence

This isn't a new problem. Other projects have faced the same architectural question, and the ones that got the boundary right built ecosystems.

[Pion](https://github.com/pion/webrtc) is a pure WebRTC stack in Go. `pion/ice`, `pion/dtls`, `pion/srtp`, `pion/sctp` -- each a standalone module. Zero opinions about meetings or rooms. Over 1,400 packages import it. When [LiveKit](https://github.com/livekit/livekit) needed a WebRTC stack, they imported Pion. When Cloudflare needed one for Calls, same decision. Pion handles routing and nothing else.

LiveKit then added the business logic layer on top. Their SFU has rooms (routing requires grouping) and tracks participants (routing requires knowing who's listening). But the Room Service API -- CreateRoom, DeleteRoom, permissions, admin operations -- is a separate layer. Access control uses JWT tokens with explicit grants: `roomCreate`, `roomJoin`, `roomAdmin`. A "host" isn't the first person who showed up -- it's whoever the token says has `roomAdmin`. That separation enabled SDKs for React, Swift, Android, Flutter, and Unity. Clean layers, clean ecosystem.

[Jitsi](https://github.com/jitsi/jitsi-meet) proved open-source video conferencing works at scale -- billions of minutes served, a genuine success story. Their `lib-jitsi-meet` is framework-agnostic. JVB handles media routing. Jicofo handles meeting orchestration. In theory, the same split. In practice, signaling bleeds through Prosody/XMPP across all layers. That's not a criticism of the engineering, which is excellent -- it's what happens when layers share infrastructure organically. We're learning from that.

The pattern: routing and business logic separated cleanly leads to ecosystems. Routing and business logic entangled leads to monoliths.

## The Plan

Four layers. One rule: Layer 1 is optional.

**Layer 0 -- videocall-client.** Transport and routing. Connect to a channel. Send and receive media. Encode, decode, encrypt. Know who's connected. That's it. We replace `yew::Callback<T>` with plain Rust closures (`Rc<dyn Fn(T)>`) so any framework -- Yew, Leptos, Dioxus, plain JS, a headless CLI -- can consume the API without friction. We drop `yew-websocket` and `yew-webtransport` in favor of `web-sys` directly for WASM targets, with `tokio`+`quinn` for native targets behind feature flags. The `WebMedia` trait already abstracts the right thing; we just cut the framework-specific wrappers underneath. `on_meeting_ended` and `on_meeting_info` move out -- they're business logic and don't belong here.

This is the highest-ROI move. It directly unblocks anyone who wants streaming without a meeting: roboticists, CLI tools (`videocall-cli` already exists), native apps, anyone building with a non-Yew framework.

**Layer 1 -- meeting-api (OPTIONAL).** A separate crate in the workspace. Meeting lifecycle, ownership, permissions. Token-based grants like LiveKit's model -- `roomCreate`, `roomJoin`, `roomAdmin` as explicit permissions, not the current `creator_id` inference where the first person to connect becomes host. Lifecycle policies (what happens when the host leaves) configurable per-room, not hardcoded in the server. Gabriel's meeting ownership work ([#503](https://github.com/security-union/videocall-rs/pull/503)) and the existing `FEATURE_MEETING_MANAGEMENT` flag are already steps in this direction. We formalize the boundary.

**Layer 2 -- actix-api.** The streaming server. Routes media. Validates tokens if meeting-api is configured. Stays dumb about business logic. Without meeting-api: raw streaming mode -- accept connections, route packets, done. With meeting-api: full conferencing with rooms, hosts, permissions. Deploy with Helm. Turnkey. No strings attached.

**Layer 3 -- any frontend.** Yew, Leptos, Dioxus, plain JS, a Python script, a CLI. All first-class consumers because Layer 0 speaks plain closures, not framework callbacks. The frontend is no longer the only way into the system.

**The real design decision isn't "Layer 1 is optional."** Most production use cases need *some* form of session management. The telehealth app from the intro needs per-channel access control and lifecycle policy. A gaming lobby needs matchmaking. A classroom needs instructor roles. A roboticist monitoring a fleet might just need a server-level API key and nothing else.

The point is that **Layer 1 is an implementation, not THE implementation.** Our `meeting-api` crate is one way to do room management. But because Layer 0 is clean transport with no opinions about sessions, a telehealth company can write their own Layer 1 with HIPAA-specific semantics. A gaming company writes theirs with matchmaking logic. An education platform writes theirs with classroom roles. They all use the same videocall-client underneath.

That's the Pion playbook. Pion doesn't ship room management. LiveKit built one on top. Cloudflare built a different one. Dozens of others built their own. Pion enabled all of them by not forcing a meeting model into the transport. That's what we're building toward.

## The Team

This work doesn't happen without the people building it. [Antonio Estrada](https://github.com/testradav) from HCL has landed 79 commits -- Firefox support ([#522](https://github.com/security-union/videocall-rs/pull/522)), self-video as a canvas tile ([#517](https://github.com/security-union/videocall-rs/pull/517)), audio device toggles ([#557](https://github.com/security-union/videocall-rs/pull/557)), peer canvas cropping ([#492](https://github.com/security-union/videocall-rs/pull/492)). [Jay Boyd](https://github.com/jboyd01) overhauled the Helm charts ([#531](https://github.com/security-union/videocall-rs/pull/531)), fixed GLIBC compatibility ([#519](https://github.com/security-union/videocall-rs/pull/519)), and simplified k3s deployment ([#458](https://github.com/security-union/videocall-rs/pull/458)). [Chris Heltzel](https://github.com/cheltzel-hcl) provides technical input, testing, and keeps weekly design discussions sharp. Michael Alexander contributes on infrastructure.

[Gabriel](https://github.com/iamgabrielsoft) has been an exceptional open-source collaborator -- the meeting ownership project ([#503](https://github.com/security-union/videocall-rs/pull/503)), CI fixes ([#559](https://github.com/security-union/videocall-rs/pull/559)), call duration timer ([#425](https://github.com/security-union/videocall-rs/pull/425)), pre-push scripts ([#502](https://github.com/security-union/videocall-rs/pull/502)). Breadth across features, CI, and DX from a single contributor.

## What's Next

Jitsi proved open-source video works at scale. Pion proved the right abstraction enables an ecosystem. LiveKit proved you can layer a product on top of clean boundaries. We respect what they built. We're taking those lessons and building with Rust, WebTransport, and the right boundaries from the start.

The backend is turnkey. The client library will work with any framework or none at all. The code is open. The roadmap is public.

Let's build.
