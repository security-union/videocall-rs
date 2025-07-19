+++
title = "Fame.chat & Videocall.rs: Engineering the Lowest-Latency Video Platform"
date = 2025-06-30
# Set to `true` while drafting; switch to `false` once published
draft = true
description = "Why videocall.rs aims to become the default videoconferencing platform for every open-source project and how we plan to get there."
tags = ["vision", "roadmap", "open source", "videocall.rs"]
+++

Our immediate North Star is **fame.chat**—a fast-growing platform that needs deterministic, ultra-low-latency video rooms for fan-talent meet-ups across oceans.

Concrete KPIs for the 2025 launch:

* **P95 end-to-end latency:** < `PLACEHOLDER_LATENCY_MS` ms between Los Angeles ↔ Manila.
* **Frame loss under sustained 5 % packet loss:** < `PLACEHOLDER_FRAME_LOSS` %.
* **Join-time (cold start):** < `PLACEHOLDER_JOIN_TIME` s from "Go live" to first rendered frame.

From day one we ship:
• Docker `famechat-sfu up` that spins an SFU locally in <30 s.  
• JS `createRoom()` that returns a `callId` in <50 ms.  
• Rust API crate for back-of-house automation.

If we can delight fame.chat users during a 04:00 AM meet-and-greet between LA and Manila, we can delight anyone.

The same engine—shipped as binaries, crates, and a managed edge service—becomes the building block for any product that refuses to compromise on video quality.

## Why This Matters

1. **Accelerating Innovation** – A composable, hackable video stack lets researchers and product teams experiment with codecs, network transports, and UX without asking for permission.
2. **Digital Sovereignty** – Self-hosting keeps sensitive communications within your jurisdiction and under your encryption keys.
3. **Economic Efficiency** – Running your own infra on commodity hardware is dramatically cheaper at scale than per-seat SaaS plans.
4. **Ecosystem Multiplier** – When every open-source project can embed reliable video in a pull request review, a design doc, or a multiplayer IDE session, collaboration itself changes.

## A Service First, Not Just a Library

Libraries are useful; services are transformative. Videocall.rs will ship **batteries-included binaries**—ready to deploy on bare metal, Kubernetes, or as serverless edges—so teams can move from prototype to production in minutes.

Under the hood we guarantee service quality via:

• **Edge SFU Autopilot** – new nodes spin up via 
  `flyctl scale count +1` in ≤30 s when regional RTT > threshold.  
• **Hot-swappable Codec Pipeline** – reload `libvideocall_encode.so` without dropping packets.  
• **Traffic Pacing in Kernel Bypass UDP** – 30 % lower jitter vs. standard socket path.

## Milestones on the Road to Ubiquity

| Timeline | Milestone | What Success Looks Like |
| --- | --- | --- |
| **Q3 2025** | **0.3 "Sulaco" Fame.chat Launch** | *Tech:* Chrome & Safari (desktop/iOS); latency test-rig proving KPIs above. *Ops:* fame.chat in prod with SLA-backed USA↔PH rooms. |
| **Q3 2025** | **0.35 "Hudson" Observability & SLA** | *Tech:* status.<domain> page; per-session MOS scoring + Grafana dashboards. *Ops:* real-time alerts when SLA placeholders breached. |
| **Q4 2025** | **0.7 "Ripley" Creator Power-Ups** | *Tech:* Group rooms ≤50 active / 1 000 passive; backstage green-rooms; tipping overlay SDK. *Ops:* SFU validated on 5 continents. |
| **Q1 2026** | **1.0 LTS "Bishop"** | *Tech:* external security audit pass; auto-scale to 5 k concurrent rooms. *DevRel:* semver contract + public RFC process.

These milestones keep fame.chat as the lighthouse deployment, forcing every feature through the crucible of real-world traffic and clearly measurable SLAs.

## How Great Open-Source Projects Pull It Off

1. **Radical Transparency** – All discussions, roadmaps, and RFCs live in public. Users become contributors when they can see the why, not just the what.
2. **Modular Core** – Like *Kubernetes* and *Rust*, a small, highly cohesive kernel surrounded by plugins invites experimentation without destabilising the base.
3. **Dogfood Relentlessly** – Every stand-up, design review, and project update already happens on videocall.rs. The product improves because our pain is our users' pain.
4. **Sustainable Funding** – Grants, sponsorships, and premium services keep maintainers paid and burnout at bay.

## Why Adopt Now

• **First pure-Rust QUIC media plane** – Inspect packets with `cargo flamegraph`, tweak congestion control in one commit.  
• **Deterministic Resource Footprint** – ≤`PLACEHOLDER_MB` MB RAM + ≤`PLACEHOLDER_CPU` % CPU per HD stream on bare-metal AMD EPYC.  
• **WASM Portability** – Same core runs in browsers, edge workers, and native apps.

## Calling All Staff Engineers

You crave problems where nanosecond optimisations translate to happier humans. Videocall.rs offers:

• **Real-Time Systems** – Tackle head-of-line blocking, jitter buffers, and adaptive bitrate in production.

• **Web-Native APIs** – Contribute to the emerging WebTransport and WebCodecs standards.

• **Edge Computing** – Deploy media planes across global POPs with zero-copy packet routing.

• **Community Leadership** – Shape the RFC process, mentor newcomers, and influence a tool your future self will rely on.

## Join Us

We're hunting for engineers who obsess over packet pacing, jitter buffers, and flamegraphs.  
• Join the discussion on **[Discord](https://discord.gg/JP38NRe4CJ)** – ping `@griff` if you're into FEC or QUIC congestion control.  
• Dive into the code or open an issue at **[GitHub](https://github.com/security-union/videocall-rs)** 