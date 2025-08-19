+++
title = "The Dario Way: Know Your Limits Before Your Customers Do (And How We Caught a 50% Performance Bug)"
date = 2025-01-27
description = "Using synthetic client bots to load test WebTransport video calls and discovering hidden bottlenecks before they hit production"
[taxonomies]
tags = ["rust", "load-testing", "performance", "webtransport", "synthetic-clients", "engineering"]
authors = ["Dario Lencina Talarico"]
+++

# The Dario Way: Know Your Limits Before Your Customers Do (And How We Caught a 50% Performance Bug)

## Executive Summary

Real-time video calling systems fail in spectacular ways when they hit their limits. Instead of waiting for your customers to discover these limits during peak usage (usually at the worst possible moment), we built synthetic client bots that hammer your system with realistic traffic patterns. This article walks through building load testing infrastructure that actually works, catches real bottlenecks, and saves your on-call rotation from 3 AM disasters.

## The Problem That Haunts Every Engineer

Picture this: It's Black Friday, your video calling service is getting hammered, and suddenly users are reporting "the call quality is terrible." Your monitoring dashboard shows everything green – CPU usage normal, memory fine, all services responding. But your support queue is filling up with angry customers who sound like they're calling from the bottom of a well.

You know what's worse than a performance problem? A performance problem you can't reproduce. You know what's even worse than that? Finding out your system has a 50% performance degradation bug that's been lurking in production for months, discovered only when some poor engineer decides to actually stress test the thing properly.

This is the story of how we built synthetic client bots that caught a critical bottleneck before our customers did, and why every real-time system needs this kind of load testing infrastructure.

## The Traditional Approach: Cross Your Fingers and Hope

Most engineering teams approach load testing like they approach fire drills – something they know they should do, but always find excuses to postpone. When they finally get around to it, the approach usually looks like this:

1. **Apache Bench or similar** – great for HTTP endpoints, useless for WebTransport video streams
2. **Synthetic HTTP load** – tells you nothing about real-time media performance  
3. **Manual testing** – "Hey, can everyone join this call and pretend to be users?"
4. **Production monitoring** – also known as "let's find out together with our customers"

These approaches fail because they don't simulate what actually matters: **concurrent clients streaming real audio and video data over WebTransport connections for extended periods.**

## What This Actually Solves

Before you dismiss this as "over-engineering," let's talk about what this load testing infrastructure prevents:

- **The "Everything looks fine" disaster**: When your dashboard is green but users are suffering
- **The "We can't reproduce it" nightmare**: When performance issues only happen at scale
- **The "It worked in dev" problem**: When your localhost testing doesn't reveal concurrency bottlenecks
- **The "Black Friday surprise"**: When peak traffic reveals architecture flaws
- **The "Let's add more servers" bandaid**: When you scale horizontally to fix vertical problems

This approach doesn't just find problems – it finds the right problems, at the right time, with enough detail to actually fix them.

## The Dario Way: Synthetic Reality

Instead of hoping for the best, we built something that simulates worst-case scenarios with surgical precision. Here's the approach:

### 1. YAML-Driven Configuration

We configure our synthetic clients through YAML because infrastructure as code isn't just for deployments – it's for load testing too.

```yaml
ramp_up_delay_ms: 1000
server_url: "https://webtransport-us-east.webtransport.video"
insecure: true

clients:
  - user_id: "bot001"
    meeting_id: "stress-test-alpha"
    enable_audio: true
    enable_video: true
  - user_id: "bot002"
    meeting_id: "stress-test-alpha"
    enable_audio: false
    enable_video: true
```

Notice what we're not doing: we're not simulating fake traffic. Each bot is a real WebTransport client sending real audio (Opus-encoded) and real video streams. The server can't tell the difference between our bots and actual users, which is exactly the point.

### 2. Realistic Media Streaming

Our bots don't send random data – they stream actual media:

- **Audio**: Loops real WAV files, encodes to Opus at 50fps (20ms packets)
- **Video**: Cycles through JPEG images, sends mock video packets at 30fps
- **Protocol**: Uses the exact same WebTransport streams as real clients

This matters because network behavior changes dramatically between "send some JSON" and "stream 30 frames per second of video data while maintaining sub-50ms latency."

### 3. Graduated Load Testing

We don't go from zero to hero. Our bots start up with configurable ramp-up delays:

```rust
// Linear ramp-up delay between client starts
if index < total_clients - 1 {
    info!("Waiting {}ms before starting next client", ramp_up_delay.as_millis());
    time::sleep(ramp_up_delay).await;
}
```

This reveals different types of problems: some systems fail during connection storms, others fail under sustained load, and some fail during the ramp-up period itself.

## The Bug That Almost Got Away

Here's where it gets spicy. While running our synthetic client tests with increasing load, we noticed something disturbing: **performance was degrading far earlier than expected**. With just 20 concurrent clients, latency was spiking and frame drops were happening.

NATS was labeling webtransport server as a [slow consumer](https://docs.nats.io/running-a-nats-service/nats_admin/slow_consumers) of messages, this meant that the server was not able to process all the messages it was receiving. 

The monitoring dashboard looked fine. CPU usage was reasonable. Memory was stable. But the synthetic clients were reporting terrible performance. This is exactly the scenario where traditional load testing fails – everything looks normal except for the thing that actually matters: user experience.

### The Investigation

We dug into the WebTransport server code and found the culprit: a pattern that's embarrassingly common in concurrent systems.

**The Problem**: Multiple tasks were sharing an `Arc<AtomicBool>` called `should_run` that was being checked in tight loops across different connection handlers. Every packet processing loop was calling `should_run.load(Ordering::Acquire)`, creating massive contention on a single memory location.

Think about it: with 20 concurrent clients streaming at 30fps each, that's 600 atomic operations per second on the same memory location. With 100 clients, you're looking at 3,000 atomic operations per second all fighting over the same cache line.

**The Fix**: We replaced the atomic boolean with Rust's `CancellationToken` from `tokio_util`:

```rust
// Before: Contention nightmare
loop {
    if !should_run.load(Ordering::Acquire) {
        break;
    }
    // Process packet...
}

// After: Efficient cancellation
loop {
    tokio::select! {
        _ = cancellation_token.cancelled() => {
            debug!("Task cancelled");
            break;
        }
        result = process_packet() => {
            // Handle packet...
        }
    }
}
```

### The Results

The performance improvement was dramatic:

- **50% improvement in concurrent client capacity**
- **Latency spikes eliminated** under normal load
- **Memory contention reduced** by orders of magnitude
- **Clean shutdown** behavior as a bonus

But here's the kicker: **this bug would never have been caught by traditional monitoring**. CPU usage was fine, memory was fine, all the metrics looked healthy. It was only visible under concurrent load with realistic traffic patterns.

## Why This Actually Matters

### For Your Users
- **Calls that don't degrade** under normal load
- **Predictable performance** even during peak usage  
- **No mysterious quality drops** during busy periods

### For Your Business
- **Customer complaints prevented** before they happen
- **Infrastructure costs optimized** because you know actual limits
- **Peak traffic handling** without surprise outages

### For Your Engineers
- **Sleep better** knowing your limits are tested, not guessed
- **Debug faster** with realistic reproduction environments
- **Ship confidently** because performance regressions are caught early

### For Your Wallet
- **Right-sized infrastructure** based on actual measurements
- **No emergency scaling** during unexpected load spikes
- **Prevented outages** that cost more than the testing infrastructure

## The Hidden Benefits

Building this synthetic client infrastructure gave us superpowers we didn't expect:

1. **Regression detection**: Every deploy gets load tested automatically
2. **Capacity planning**: We know exactly how many users each server can handle
3. **Architecture validation**: We can test new features under realistic load
4. **Debugging playground**: Reproduce production issues on demand

## Implementation Reality Check

### The Bot Architecture

Our bot system consists of several key components:

- **WebTransport Client**: Connects using the exact same protocol as real clients
- **Audio Producer**: Loops WAV files and encodes to Opus in real-time
- **Video Producer**: Cycles through images and sends video packets at 30fps
- **Configuration System**: YAML-driven client scenarios

### The Media Pipeline

Each bot maintains realistic media streams:

```rust
// Audio: 50fps Opus packets (20ms each)
let audio_producer = AudioProducer::from_wav_file(
    user_id, "BundyBests2.wav", packet_tx
);

// Video: 30fps mock video packets  
let video_producer = VideoProducer::from_image_sequence(
    user_id, image_directory, packet_tx
);
```

The beauty is in the realism: from the server's perspective, these bots are indistinguishable from real clients.

## Common Objections (And Why They're Wrong)

### "But load testing is expensive!"
**Reality**: You know what's expensive? Finding out your system can only handle 20 concurrent users when you expected 200, discovered during your product launch.

### "We can just monitor production!"
**Reality**: Production monitoring tells you what already happened. Load testing tells you what's going to happen.

### "Our system scales horizontally, we can just add servers!"
**Reality**: Some problems don't scale horizontally. That atomic boolean contention? Adding more servers makes it worse, not better.

### "This is overkill for our use case!"
**Reality**: If you have real-time systems with concurrent users, this isn't overkill – it's necessary. If you don't, then yes, skip this.

## When NOT to Use This Approach

Don't build synthetic client infrastructure if:
- Your system doesn't have concurrent real-time connections
- You're building CRUD applications without performance requirements
- Your traffic patterns are completely predictable and well-understood
- You have unlimited budget for over-provisioning

## The Bottom Line

The "Dario Way" is really just measuring what matters before your customers have to. Instead of hoping your system can handle load, you test it with realistic synthetic clients that behave exactly like real users.

It requires upfront investment in testing infrastructure, but in exchange, you get:
- **Real performance data** from realistic load scenarios
- **Early bottleneck detection** before they hit production  
- **Confident capacity planning** based on actual measurements
- **Sleep at night** knowing your limits are tested, not guessed
- **The satisfaction** of catching critical bugs before your customers do

Plus, when someone asks "How many concurrent users can your system handle?", you get to say "We stress test with synthetic clients every deploy, so exactly 847 users per server." And that beats "probably a lot?" every time.

## Want to See the Code?

All the synthetic client implementation is in the [videocall-rs repository](https://github.com/security-union/videocall-rs/tree/main/bot). Check out the `AudioProducer` and `VideoProducer` modules for the media pipeline, and the YAML configuration system for easy scenario setup.

The WebTransport server improvements using cancellation tokens are in the `actix-api` crate – search for `CancellationToken` to see how we replaced the problematic atomic boolean patterns.

Remember: the best performance testing is the kind that finds real problems before your customers do.

*Now go forth and load test all the things (with synthetic clients that actually matter).*
