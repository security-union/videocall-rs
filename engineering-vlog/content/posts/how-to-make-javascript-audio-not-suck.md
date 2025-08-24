+++
title = "Real‑Time Audio in the Browser: Making an AudioWorklet Fast Enough for Low‑End Android"
date = 2025-08-15
description = "When your audio worklet becomes the bottleneck and you have to outsmart the garbage collector to make real-time audio work on low-end Android devices"
[taxonomies]
tags = ["javascript", "audio", "performance", "optimization", "android", "webassembly"]
authors = ["Dario Lencina Talarico"]
+++

# Real‑Time Audio in the Browser: Making an AudioWorklet Fast Enough for Low‑End Android

## Executive Summary

Real-time audio on the web is hard. Real-time audio on low-end Android devices using JavaScript is basically asking the universe to hate you. This is the story of how we took a PCM audio worklet that was choking harder than a '90s dial-up modem and turned it into something that actually works. Spoiler alert: it involved fighting the garbage collector, rewriting circular buffers, and discovering that sometimes the simplest solution is also the hardest one to implement correctly.

## The Problem That Made Users Want to Throw Their Phones

Picture this: You're building a video calling service. Everything works beautifully on your MacBook Pro with its fancy M-series chip. Your tests pass, your demo flows, your investors are happy. Then real users start connecting from real devices – specifically, an Infinix HOT 10i running Android Chrome – and suddenly your audio sounds like it's being transmitted through a potato.

The error message that haunted our dreams:

```
Safari PCM buffer full! Available: 1984, trying to add: 480, max: 2048
```

Wait, Safari? On Android? Yeah, turns out the worklet was originally written for Safari compatibility and kept the name. Classic engineering move.

This wasn't just a small glitch. This was the "your service is unusable on devices that actual humans own" kind of problem. The kind that makes you question your life choices...

## The Traditional Approach: Throwing Hardware at Software Problems

Most engineers, when faced with audio performance issues, reach for the obvious solutions:

1. **"Just buy a better phone"** – Great advice, except you can't control what devices your users have
2. **"Use WebAssembly for everything"** – Sure, if you enjoy debugging WASM loading issues in AudioWorklets
3. **"Make the buffer bigger"** – The classic "more RAM" solution that treats symptoms, not causes
4. **"Use a different framework"** – Because rewriting everything is definitely easier than fixing the actual problem

We tried the buffer size approach first. Doubled it from 2048 to 4096 samples. The user reported it was "almost usable," which in engineering terms means "still broken, but now with different timing."

That's when we realized we weren't dealing with a buffer size problem. We were dealing with a **performance problem disguised as a buffer size problem**.

## The Root of All Evil: Garbage Collection and Micro-Allocations

In the original PCM worklet, we were doing several things that looked harmless but were deadly at real‑time rates:

- **Per-callback allocations**: New arrays every audio callback (hundreds of times per second), driving GC pressure.
- **Modulo in hot loops**: Hidden divisions in tight paths slow things down unnecessarily.
- **I/O on the audio thread**: Logging/timestamps in code that must finish in ~2.7–2.9 ms per callback.
- **Interleaved processing**: Cache‑unfriendly memory access.

On low‑end devices, GC pauses and extra CPU cost made the audio thread miss its deadline, causing buffer overruns and that lovely "PCM buffer full" error.

## The WebAssembly Detour: When the "Obvious" Solution Isn't

Like any self-respecting systems engineers, our first instinct was obvious: "JavaScript is slow, let's rewrite it in Rust!"

We spent time crafting a beautiful WASM implementation with proper circular buffers, zero-copy operations, and all the performance goodness you'd expect from native code:

```rust
#[wasm_bindgen]
pub struct RustPCMProcessor {
    buffer: WasmAudioBuffer,
    // ... beautiful, fast, native code
}

impl RustPCMProcessor {
    #[wasm_bindgen]
    pub fn process_audio(&self, left_output: &mut [f32], right_output: &mut [f32]) -> bool {
        // Blazingly fast native audio processing
        self.buffer.pull_to_arrays(left_output, right_output)
    }
}
```

Then reality hit us like a brick wall made of WebAPI restrictions.

### The AudioWorklet Loading Reality

AudioWorklets run in a restricted environment: `fetch()` and dynamic `import()` are forbidden in `AudioWorkletGlobalScope`, and `importScripts()` isn’t available. **The AudioWorklet sandbox forbids network and dynamic imports; you can pass pre‑fetched bytes from the main thread, but that adds complexity and cross‑origin constraints.** We could have gone that route (potentially with SharedArrayBuffer and cross‑origin isolation), but at that point we were clearly overengineering the problem for our needs.

### The Moment of Engineering Clarity

After wrestling with WASM loading for hours, we had an epiphany: **Maybe the problem isn't that JavaScript is inherently slow. Maybe the problem is that we were writing slow JavaScript.**

What if instead of fighting the browser's security model, we just... wrote faster JavaScript?

## The Solution: Outsmarting the Garbage Collector

So we pivoted. Two choices: continue the WASM wrestling match, or make JavaScript fast enough that it didn't matter. Being pragmatic engineers who'd already burned too much time on WASM loading restrictions, we chose option two.

### 1. Pre-Allocate Everything

The first rule of high-performance JavaScript: **never allocate memory in hot paths**. We pre-allocated large typed arrays once (constructor/init) and avoided creating any new objects inside `process()`.

### 2. Eliminate Modulo Operations

Modulo looks innocent but is division in disguise. We replaced per-sample modulo with branchy fast/slow paths and bulk copies. **`TypedArray.set` is highly optimized and typically implemented as a bulk copy**, which makes the fast path effectively a `memcpy`.


### 3. Remove All I/O from Hot Paths

We eliminated logging and timestamping from `process()`. Any stats/diagnostics are deferred to non‑RT contexts (e.g., ring‑buffer to main thread).

### Memory budget and startup watermarks

- We increased the internal PCM buffer size incrementally until underruns stopped on the target device, then doubled it as a safety margin. That’s how we arrived at our current budget.
- We also added a small startup watermark before unmuting playback to avoid initial underruns on slow devices.

## The Results: From "Almost Usable" to "Actually Works"

The transformation was dramatic:

### Before (The JavaScript Struggle)
- **Infinix HOT 10i**: "PCM buffer full" errors every few seconds
- **GC pressure**: 375 allocations per second in the audio thread
- **CPU usage**: Maxing out on low-end devices
- **User experience**: "Sounds like dial-up internet"
- **Developer confidence**: "Maybe we should rewrite everything in C++"

### After (The Optimized Reality)
- **Infinix HOT 10i**: Smooth audio, zero buffer overruns
- **GC pressure**: Zero allocations in hot paths
- **CPU usage**: Barely registering on the same devices
- **User experience**: "This actually works!"
- **Developer confidence**: "JavaScript can do real-time audio after all"

The user who reported the original issue said it went from unusable to "almost usable" after we doubled the buffer size, then to "actually works great" after the optimization pass.

## The Hidden Lesson: Micro-Optimizations Matter

This experience taught us something important: **micro-optimizations matter when you're doing them ~344–375 times per second** (44.1 kHz vs 48 kHz at 128‑sample blocks).

In most application code, premature optimization is the root of all evil. But in real-time audio code, every microsecond counts. When your code runs hundreds of times per second and has to complete in under ~3 ms each time, those “insignificant” allocations and function calls add up to the difference between working and not working.

## When NOT to Do This

Before you go optimizing every JavaScript function you've ever written, remember:

- **Most code doesn't need this level of optimization**: We did this because we were in a real-time audio callback
- **Readable code is usually better**: These optimizations make the code harder to understand
- **Profile first**: Make sure you're actually optimizing the bottleneck
- **Consider the deployment environment**: AudioWorklets have severe restrictions that might make WASM impractical

## Lessons from the WASM Detour

Our failed attempt to use WebAssembly taught us some valuable lessons:

### 1. Browser Security Models Are Non-Negotiable
AudioWorklets are sandboxed for good reason, and while you can pass pre‑fetched bytes into the worklet, network and dynamic imports are forbidden. The browser vendors aren't being difficult – they're preventing malicious audio code from compromising user security.

### 2. Sometimes the "Simple" Solution Is Actually Simpler
Writing fast JavaScript turned out to be less complex than fighting WASM loading restrictions. The optimized JavaScript solution:
- Has zero external dependencies
- Loads instantly with no async initialization
- Debugs easily in browser dev tools
- Works identically across all browsers

### 3. Know Your Performance Ceiling Before You Start
We could have saved time by profiling the theoretical maximum performance of JavaScript first. If optimized JavaScript could meet our requirements, why add WASM complexity?

### 4. The "Native is Always Faster" Myth
Modern JavaScript engines are incredibly sophisticated. V8 can often optimize well-written JavaScript to near-native performance, especially for numeric operations like audio processing. WASM isn't automatically faster – it's just more predictable.

## The Takeaway

Real-time audio in JavaScript is possible, but it requires thinking like a systems programmer while writing in a garbage-collected language. The key principles:

1. **Pre-allocate everything** in non-critical paths
2. **Never allocate memory** in hot paths  
3. **Use bulk operations** instead of loops when possible
4. **Remove all I/O** from real-time code
5. **Think about cache locality** and memory access patterns
6. **Profile on your worst-case hardware**, not your development machine

The result is JavaScript that performs like native code, which is what real-time audio demands.

### Engineer’s checklist

- **Pre‑allocate** large typed arrays; no allocations in `process()`
- **Replace modulo** in hot paths with branchy fast/slow copies
- **Use bulk operations** (`TypedArray.set`) for contiguous copies
- **Planarize storage** to match Web Audio channel arrays
- **Zero I/O** in the audio thread; defer stats/logs to main thread
- **Startup watermark** before unmuting playback
- **Overflow/underflow policy**: choose and document (drop, overwrite, or silence)
- **Profile on target hardware** (callback time p95/p99, GC pause)
- **Measure before/after** (overruns per minute, CPU, buffer fill levels)

## Want to See the Code?

The full implementation is in the [videocall-rs repository](https://github.com/security-union/videocall-rs). Check out [`yew-ui/scripts/pcmPlayerWorker.js`](https://github.com/security-union/videocall-rs/blob/main/yew-ui/scripts/pcmPlayerWorker.js) for the complete optimized audio worklet.

And remember: if someone tells you JavaScript can't do real-time audio, show them this article. It absolutely can – you just have to outsmart the garbage collector first.

*Now go forth and make your audio not suck.*
