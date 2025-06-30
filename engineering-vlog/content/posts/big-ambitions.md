+++
title = "Big ambitions: I hope this article ages well"
date = 2025-06-30
# Set to `true` while drafting; switch to `false` once published
draft = false
description = "Why videocall.rs aims to become the default videoconferencing platform for every open-source project and how we plan to get there."
tags = ["vision", "roadmap", "open source", "videocall.rs"]
+++

> "Dream no small dreams for they have no power to move the hearts of people." — Johann Wolfgang von Goethe

## The Vision

Videocall.rs started as a weekend experiment in **pure-Rust WebTransport**. Today it has enough momentum to aim much higher: **becoming the de-facto, drop-in videoconferencing layer for every open-source project and engineering team on the planet**.

Closed-source incumbents have set a very high bar for audio/video quality, latency, security, and reliability—but they come with strings attached: vendor lock-in, pay-per-minute pricing, and opaque security practices. We believe the open-source world deserves better.

*Imagine a world where starting a secure, low-latency video call is as easy as `cargo add videocall-rs && videocall up`.* That is what we are building.

## Why This Matters

1. **Accelerating Innovation** – A composable, hackable video stack lets researchers and product teams experiment with codecs, network transports, and UX without asking for permission.
2. **Digital Sovereignty** – Self-hosting keeps sensitive communications within your jurisdiction and under your encryption keys.
3. **Economic Efficiency** – Running your own infra on commodity hardware is dramatically cheaper at scale than per-seat SaaS plans.
4. **Ecosystem Multiplier** – When every open-source project can embed reliable video in a pull request review, a design doc, or a multiplayer IDE session, collaboration itself changes.

## A Service First, Not Just a Library

Libraries are useful; services are transformative. Videocall.rs will ship **batteries-included binaries**—ready to deploy on bare metal, Kubernetes, or as serverless edges—so teams can move from prototype to production in minutes.

For investors, this positions us to layer a sustainable business on top of the core open source:

• **Managed Cloud** – zero-ops clusters with SLAs.
• **Enterprise Support** – dedicated channels, on-prem audits, and feature backports.
• **Marketplace** – revenue-sharing plugins for analytics, transcription, and AI assistants.

A thriving commercial ecosystem guarantees the open-source core remains healthy, well-maintained, and audaciously ambitious.

## Milestones on the Road to Ubiquity

| Timeline | Milestone | What Success Looks Like |
| --- | --- | --- |
| **Q3 2025** | **0.3 "Sulaco" Developer Preview** | Works seamlessly on Chrome and Safari (desktop & iOS); integrated latency torture-suite proving sub-150 ms median under 5 % packet loss and 300 ms jitter; one-click Docker compose demo; CI publishing to crates.io & npm; production deployment powering fame.chat with SLA-backed USA↔Philippines rooms and real-time monitoring dashboards. |
| **Q4 2025** | **0.7 "Ripley" Feature Parity** | Group rooms ≤ 50 active participants; 1K passive participants; SFU architecture proven around the world. |
| **Q1 2026** | **1.0 LTS "Bishop"** | External security audit pass; semantic-versioning contract; HA reference deployment guide; real-time QoS metrics for every call; auto-scaling clusters proven at 5 k concurrent rooms. |
| **2027** | **"LV-426" Planetary Scale** | 10 k concurrent rooms on a single cluster; multi-region media relay; adaptive mesh for peer-to-peer edge cases; edge transcoding. |
| **Beyond** | **The "Weyland-Yutani Moment"** | Videocall.rs becomes the default pick for hobbyists, Fortune 500s, hackathons, and classrooms alike. |

These milestones are ambitious by design. They give contributors a clear north star and investors a quantifiable de-risked path.

## How Great Open-Source Projects Pull It Off

1. **Radical Transparency** – All discussions, roadmaps, and RFCs live in public. Users become contributors when they can see the why, not just the what.
2. **Modular Core** – Like *Kubernetes* and *Rust*, a small, highly cohesive kernel surrounded by plugins invites experimentation without destabilising the base.
3. **Dogfood Relentlessly** – Every stand-up, design review, and investor update already happens on videocall.rs. The product improves because our pain is our users' pain.
4. **Sustainable Funding** – Grants, sponsorships, and premium services keep maintainers paid and burnout at bay.

## Why Invest Now

• **First-Mover in Rust** – We are the only pure-Rust stack targeting WebTransport and QUIC end-to-end. Safety, performance, and WASM portability are default.

• **Compounding Developer Love** – Early Rust crates that win hearts (Tokio, Serde) become infrastructure decades later. Video is next.

• **Clear Monetisation Path** – Managed hosting and compliance certifications unlock enterprise budgets without compromising the open-core ethos.

## Calling All Staff Engineers

You crave problems where nanosecond optimisations translate to happier humans. Videocall.rs offers:

• **Real-Time Systems** – Tackle head-of-line blocking, jitter buffers, and adaptive bitrate in production.

• **Web-Native APIs** – Contribute to the emerging WebTransport and WebCodecs standards.

• **Edge Computing** – Deploy media planes across global POPs with zero-copy packet routing.

• **Community Leadership** – Shape the RFC process, mentor newcomers, and influence a tool your future self will rely on.

## Join Us

We have **one moon-shot goal**: make high-quality video communication a commodity for every open-source team. If that resonates—whether you write cheques or code—**now is the moment to get involved**.

🚀 *Let's build the pixels that bring the world closer—together.* 